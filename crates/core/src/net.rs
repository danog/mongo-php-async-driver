//! Connections and the I/O-backend layer.
//!
//! A [`Conn`] is one TCP connection to a server. It is used in one of two modes:
//! - blocking (the default, what ext-mongodb does): `send`/`recv` block the PHP thread, bounded
//!   by the socket timeout (SO_SNDTIMEO/SO_RCVTIMEO, set only when the value changes);
//! - non-blocking, driven by an event loop: `try_send`/`try_recv` make progress until the socket
//!   would block, and a [`Reactor`] reports when to try again.
//!
//! [`Reactor`] is the I/O-backend layer: it watches connections for readiness and hands back the
//! tokens of the ready ones. Backend 1 is [`MioReactor`] (one epoll/kqueue fd, which the PHP side
//! watches with Revolt; Unix only). A per-socket backend for Windows (Revolt watchers on each
//! connection socket, no threads) implements the same trait later.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

use crate::wire;

/// The largest reply accepted before the server's maxMessageSizeBytes is known (libmongoc's
/// default MONGOC_DEFAULT_MAX_MSG_SIZE, 48 MB).
pub const DEFAULT_MAX_MSG_SIZE: usize = 48_000_000;
/// The hard upper bound on any reply, whatever a server advertises (every MongoDB server
/// advertises 48 MB; libmongoc trusts the advertised int32, we bound it).
pub const HARD_MAX_MSG_SIZE: usize = 64 * 1024 * 1024;
/// The receive buffer grows by at most this much per read, so a declared messageLength
/// reserves memory only as bytes actually arrive.
const READ_CHUNK: usize = 1024 * 1024;

/// The reply-size cap for a server advertising `advertised` maxMessageSizeBytes: libmongoc caps
/// replies at the advertised value (mongoc-cluster.c); a non-positive value is ignored (the
/// default applies) and the result never exceeds HARD_MAX_MSG_SIZE nor goes below the smallest
/// possible OP_MSG reply.
pub fn max_message_size_for(advertised: i32) -> usize {
    if advertised <= 0 {
        return DEFAULT_MAX_MSG_SIZE;
    }
    (advertised as usize).clamp(wire::HEADER_LEN + 5, HARD_MAX_MSG_SIZE)
}

#[derive(Debug)]
pub enum NetError {
    /// Name resolution failed or gave no address.
    Resolve(String),
    /// Connecting failed (refused, unreachable, timed out: see the io::Error).
    Connect(io::Error),
    /// A send or receive timed out.
    Timeout,
    /// The server closed the connection.
    Closed,
    Io(io::Error),
    /// A malformed or oversized message.
    Protocol(&'static str),
    /// The TLS handshake failed (the text after "TLS handshake failed: ").
    Tls(String),
    /// Setting up a UNIX domain socket failed: libmongoc's error code and message.
    Unix(u32, String),
    /// The wait was interrupted (crate::aio::Interrupted): the fiber is being destroyed or an
    /// exception was thrown into it. Nothing is known about the server.
    Interrupted,
}

impl std::fmt::Display for NetError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            NetError::Resolve(s) => write!(f, "{s}"),
            NetError::Connect(e) | NetError::Io(e) => write!(f, "{e}"),
            NetError::Timeout => write!(f, "socket timeout"),
            NetError::Closed => write!(f, "connection closed"),
            NetError::Protocol(s) => write!(f, "{s}"),
            NetError::Tls(s) => write!(f, "TLS handshake failed: {s}"),
            NetError::Unix(_, s) => write!(f, "{s}"),
            NetError::Interrupted => write!(f, "{}", crate::aio::Interrupted),
        }
    }
}

/// A handshake I/O error: rustls's own errors (verification) carry their description.
pub(crate) fn tls_io_err(e: io::Error) -> NetError {
    if let Some(r) = e.get_ref().and_then(|i| i.downcast_ref::<rustls::Error>()) {
        return NetError::Tls(crate::tls::describe(r));
    }
    match io_err(e) {
        NetError::Timeout => NetError::Tls("Connection timed out".into()),
        NetError::Closed => NetError::Tls("Connection reset by peer".into()),
        NetError::Io(e) => NetError::Tls(e.to_string()),
        other => other,
    }
}

fn io_err(e: io::Error) -> NetError {
    if crate::aio::is_interrupted(&e) {
        return NetError::Interrupted;
    }
    match e.kind() {
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => NetError::Timeout,
        io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset | io::ErrorKind::BrokenPipe => NetError::Closed,
        _ => NetError::Io(e),
    }
}

/// The connection's byte stream: plain TCP, or TLS over it (one match per read or write).
enum Stream {
    Plain(TcpStream),
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>),
}

impl Stream {
    fn tcp(&self) -> &TcpStream {
        match self {
            Stream::Plain(s) => s,
            Stream::Tls(t) => &t.sock,
        }
    }
}

impl Read for Stream {
    #[inline]
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Stream::Plain(s) => s.read(buf),
            Stream::Tls(t) => t.read(buf),
        }
    }
}

impl Write for Stream {
    #[inline]
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Stream::Plain(s) => s.write(buf),
            Stream::Tls(t) => t.write(buf),
        }
    }
    #[inline]
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Stream::Plain(_) => Ok(()),
            Stream::Tls(t) => t.flush(),
        }
    }
    #[inline]
    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        match self {
            Stream::Plain(s) => s.write_all(buf),
            // the records are only on the wire after the flush
            Stream::Tls(t) => t.write_all(buf).and_then(|()| t.flush()),
        }
    }
}

pub struct Conn {
    stream: Stream,
    /// The server address as the topology names it ("host:port").
    pub addr: String,
    /// The message being received (complete after `recv` / `try_recv` returned it).
    rbuf: Vec<u8>,
    max_msg: usize,
    nonblocking: bool,
    read_timeout: Option<Duration>,
    write_timeout: Option<Duration>,
    /// Pool generation at creation (CMAP).
    pub generation: u64,
    /// Registered with a reactor (backend-specific bookkeeping).
    pub registered: bool,
    /// The negotiated compressor and the zlib level (OP_COMPRESSED), None: uncompressed.
    pub compressor: Option<(u8, i32)>,
}

/// Resolves `host:port` (all addresses, in resolver order, as libmongoc tries them).
pub fn resolve(host: &str, port: u16) -> Result<Vec<SocketAddr>, NetError> {
    let addrs: Vec<SocketAddr> = (host, port).to_socket_addrs().map_err(|e| NetError::Resolve(e.to_string()))?.collect();
    if addrs.is_empty() {
        return Err(NetError::Resolve(format!("no address for {host}")));
    }
    Ok(addrs)
}

impl Conn {
    /// Drops the connection without touching its socket (after a fork the descriptor belongs to
    /// the parent: it is never written to, read, shut down or closed). Only the memory is freed.
    pub fn abandon(self) {
        let Conn { stream, addr, rbuf, .. } = self;
        drop((addr, rbuf));
        // the TLS state is freed without sending anything (no close_notify)
        let stream = match stream {
            Stream::Plain(s) => s,
            Stream::Tls(t) => {
                let t = *t;
                drop(t.conn);
                t.sock
            }
        };
        #[cfg(unix)]
        {
            use std::os::fd::IntoRawFd;
            let _ = stream.into_raw_fd();
        }
        #[cfg(not(unix))]
        std::mem::forget(stream);
    }

    /// Connects to the first address of `host:port` that accepts within `timeout` each
    /// (connectTimeoutMS; None = the OS default).
    pub fn connect(host: &str, port: u16, timeout: Option<Duration>) -> Result<Conn, NetError> {
        let mut last = None;
        for sa in resolve(host, port)? {
            let r = match timeout {
                Some(t) if !t.is_zero() => TcpStream::connect_timeout(&sa, t),
                _ => TcpStream::connect(sa),
            };
            match r {
                Ok(stream) => {
                    stream.set_nodelay(true).map_err(NetError::Connect)?;
                    let addr = if host.contains(':') { format!("[{host}]:{port}") } else { format!("{host}:{port}") };
                    return Ok(Conn {
                        stream: Stream::Plain(stream),
                        addr,
                        rbuf: Vec::new(),
                        max_msg: DEFAULT_MAX_MSG_SIZE,
                        nonblocking: false,
                        read_timeout: None,
                        write_timeout: None,
                        generation: 0,
                        registered: false,
                        compressor: None,
                    });
                }
                Err(e) => last = Some(e),
            }
        }
        Err(NetError::Connect(last.unwrap()))
    }

    /// Connects to the UNIX domain socket at `path` (mongoc_topology_scanner_node_connect_unix:
    /// a blocking connect, without connectTimeoutMS, and libmongoc's messages).
    #[cfg(unix)]
    pub fn connect_unix(path: &str) -> Result<Conn, NetError> {
        use crate::error::code;
        use std::os::fd::{FromRawFd, IntoRawFd};
        // sockaddr_un.sun_path, and libmongoc's check (`snprintf` into sizeof - 1 bytes must
        // not reach sizeof - 1)
        const SUN_PATH: usize = if cfg!(any(target_os = "linux", target_os = "android")) { 108 } else { 104 };
        if path.len() >= SUN_PATH - 1 || path.contains('\0') {
            return Err(NetError::Unix(code::STREAM_SOCKET, "Failed to define socket address path.".into()));
        }
        let unix = std::os::unix::net::UnixStream::connect(path).map_err(|e| {
            // bson_strerror_r: the system's text without Rust's " (os error N)"
            let text = e.to_string();
            let text = text.rsplit_once(" (os error ").map_or(text.as_str(), |(t, _)| t).to_owned();
            NetError::Unix(code::STREAM_CONNECT, format!("Failed to connect to UNIX domain socket: {text}"))
        })?;
        // SAFETY: an owned, connected stream socket. TcpStream is used for the generic socket
        // calls only (read, write, SO_RCVTIMEO/SO_SNDTIMEO, O_NONBLOCK, close), which behave the
        // same on AF_UNIX sockets; no TCP-only call (set_nodelay, addresses) is made on it. This
        // keeps one plain-stream type, so the TCP path has no extra branch.
        let stream = unsafe { TcpStream::from_raw_fd(unix.into_raw_fd()) };
        Ok(Conn {
            stream: Stream::Plain(stream),
            addr: path.to_owned(),
            rbuf: Vec::new(),
            max_msg: DEFAULT_MAX_MSG_SIZE,
            nonblocking: false,
            read_timeout: None,
            write_timeout: None,
            generation: 0,
            registered: false,
            compressor: None,
        })
    }

    /// The server's maxMessageSizeBytes, once the handshake told it (bounded, see
    /// `max_message_size_for`).
    pub fn set_max_message_size(&mut self, advertised: i32) {
        self.max_msg = max_message_size_for(advertised);
    }

    /// The largest message accepted from the server.
    pub fn max_message_size(&self) -> usize {
        self.max_msg
    }

    /// Runs the TLS handshake on the connected socket (mongoc_stream_tls_handshake_block),
    /// bounded by `timeout` per read and write (connectTimeoutMS): the connection over TLS.
    /// Certificate and host name verification happen in the handshake; on failure the socket
    /// is closed.
    pub fn into_tls(mut self, config: std::sync::Arc<rustls::ClientConfig>, name: rustls::pki_types::ServerName<'static>, timeout: Option<Duration>) -> Result<Conn, NetError> {
        self.set_blocking()?;
        let t = timeout.filter(|t| !t.is_zero());
        let mut sock = match self.stream {
            Stream::Plain(s) => s,
            Stream::Tls(_) => return Err(NetError::Tls("already TLS".into())),
        };
        sock.set_read_timeout(t).map_err(NetError::Io)?;
        sock.set_write_timeout(t).map_err(NetError::Io)?;
        let mut conn = rustls::ClientConnection::new(config, name).map_err(|e| NetError::Tls(crate::tls::describe(&e)))?;
        while conn.is_handshaking() {
            conn.complete_io(&mut sock).map_err(tls_io_err)?;
        }
        Ok(Conn { stream: Stream::Tls(Box::new(rustls::StreamOwned::new(conn, sock))), read_timeout: t, write_timeout: t, ..self })
    }

    /// Starts a non-blocking connect to `sa` (mongoc_socket_new + mongoc_socket_connect with a
    /// zero timeout, as the topology scanner's TCP initiator does): the connect's own result is
    /// ignored, poll(2) reports completion or failure (an immediate ECONNREFUSED, as macOS gives
    /// on loopback, shows as POLLHUP). `addr` is the server address as the topology names it.
    #[cfg(unix)]
    pub fn connect_nonblocking(sa: std::net::SocketAddr, addr: &str) -> io::Result<Conn> {
        use std::os::fd::FromRawFd;
        let family = if sa.is_ipv4() { libc::AF_INET } else { libc::AF_INET6 };
        // SAFETY: plain socket creation; the descriptor is owned by the TcpStream right away
        let fd = unsafe { libc::socket(family, libc::SOCK_STREAM, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let stream = unsafe { TcpStream::from_raw_fd(fd) };
        socket_defaults(fd);
        stream.set_nonblocking(true)?;
        stream.set_nodelay(true)?;
        let (storage, len) = sockaddr(&sa);
        // SAFETY: a sockaddr of the given length; the result is deliberately ignored
        let _ = unsafe { libc::connect(fd, (&storage as *const libc::sockaddr_storage).cast(), len) };
        Ok(Conn {
            stream: Stream::Plain(stream),
            addr: addr.to_owned(),
            rbuf: Vec::new(),
            max_msg: DEFAULT_MAX_MSG_SIZE,
            nonblocking: true,
            read_timeout: None,
            write_timeout: None,
            generation: 0,
            registered: false,
            compressor: None,
        })
    }

    /// Wraps the (non-blocking) socket in a TLS client whose handshake `try_handshake` drives.
    pub fn tls_begin(self, config: std::sync::Arc<rustls::ClientConfig>, name: rustls::pki_types::ServerName<'static>) -> Result<Conn, NetError> {
        let sock = match self.stream {
            Stream::Plain(s) => s,
            Stream::Tls(_) => return Err(NetError::Tls("already TLS".into())),
        };
        let conn = rustls::ClientConnection::new(config, name).map_err(|e| NetError::Tls(crate::tls::describe(&e)))?;
        Ok(Conn { stream: Stream::Tls(Box::new(rustls::StreamOwned::new(conn, sock))), ..self })
    }

    /// Advances a non-blocking TLS handshake: None once it is complete (nothing left to send),
    /// else what the socket must become ready for.
    pub fn try_handshake(&mut self) -> Result<Option<Interest>, NetError> {
        let Stream::Tls(t) = &mut self.stream else { return Ok(None) };
        loop {
            if t.conn.wants_write() {
                match t.conn.write_tls(&mut t.sock) {
                    Ok(_) => continue,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(Some(Interest::Write)),
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(tls_io_err(e)),
                }
            }
            if !t.conn.is_handshaking() {
                return Ok(None);
            }
            match t.conn.read_tls(&mut t.sock) {
                Ok(0) => return Err(tls_io_err(io::ErrorKind::UnexpectedEof.into())),
                Ok(_) => {
                    if let Err(e) = t.conn.process_new_packets() {
                        // the alert, if the socket takes it
                        let _ = t.conn.write_tls(&mut t.sock);
                        return Err(NetError::Tls(crate::tls::describe(&e)));
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(Some(Interest::Read)),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(tls_io_err(e)),
            }
        }
    }

    /// Non-blocking: writes the TLS records still buffered (plain TCP buffers nothing); true
    /// once everything is on the wire.
    pub fn try_flush(&mut self) -> Result<bool, NetError> {
        let Stream::Tls(t) = &mut self.stream else { return Ok(true) };
        while t.conn.wants_write() {
            match t.conn.write_tls(&mut t.sock) {
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(false),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(io_err(e)),
            }
        }
        Ok(true)
    }

    /// mongoc_socket_check_closed: the peer closed the connection (readable without data, or a
    /// socket error). One poll(2), plus a MSG_PEEK recv when readable.
    #[cfg(unix)]
    pub fn check_closed(&self) -> bool {
        let fd = self.raw_fd();
        let mut p = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        // SAFETY: one valid pollfd; a zero timeout
        if unsafe { libc::poll(&mut p, 1, 0) } <= 0 || p.revents == 0 {
            return false;
        }
        let mut b = 0u8;
        // SAFETY: a one-byte buffer we own; MSG_PEEK leaves the data in the socket
        let r = unsafe { libc::recv(fd, (&mut b as *mut u8).cast(), 1, libc::MSG_PEEK | libc::MSG_DONTWAIT) };
        r < 1
    }

    #[cfg(unix)]
    pub fn raw_fd(&self) -> std::os::fd::RawFd {
        std::os::fd::AsRawFd::as_raw_fd(self.stream.tcp())
    }

    fn set_blocking(&mut self) -> Result<(), NetError> {
        if self.nonblocking {
            self.stream.tcp().set_nonblocking(false).map_err(NetError::Io)?;
            self.nonblocking = false;
        }
        Ok(())
    }

    /// Switches to non-blocking mode (for `try_send` / `try_recv`).
    pub fn set_nonblocking(&mut self) -> Result<(), NetError> {
        if !self.nonblocking {
            self.stream.tcp().set_nonblocking(true).map_err(NetError::Io)?;
            self.nonblocking = true;
        }
        Ok(())
    }

    /// Sends `msg`, blocking up to `timeout` (None or zero: no limit) per write. When the call
    /// may suspend (crate::aio::active), the socket is non-blocking and every wait suspends.
    pub fn send(&mut self, msg: &[u8], timeout: Option<Duration>) -> Result<(), NetError> {
        #[cfg(unix)]
        if crate::aio::active() {
            return self.send_waiting(msg, timeout.filter(|t| !t.is_zero()));
        }
        self.set_blocking()?;
        let t = timeout.filter(|t| !t.is_zero());
        if t != self.write_timeout {
            self.stream.tcp().set_write_timeout(t).map_err(NetError::Io)?;
            self.write_timeout = t;
        }
        self.stream.write_all(msg).map_err(io_err)
    }

    /// Receives one message, blocking up to `timeout` (None or zero: no limit) per read (when
    /// the call may suspend: suspending, see `send`).
    pub fn recv(&mut self, timeout: Option<Duration>) -> Result<&[u8], NetError> {
        #[cfg(unix)]
        if crate::aio::active() {
            return self.recv_waiting(timeout.filter(|t| !t.is_zero()));
        }
        self.set_blocking()?;
        let t = timeout.filter(|t| !t.is_zero());
        if t != self.read_timeout {
            self.stream.tcp().set_read_timeout(t).map_err(NetError::Io)?;
            self.read_timeout = t;
        }
        self.rbuf.clear();
        while !self.read_some()? {}
        Ok(&self.rbuf)
    }

    /// Waits until the socket is ready for `interest`, up to `timeout` (the per-call socket
    /// timeout of the blocking mode): NetError::Timeout when it passes.
    #[cfg(unix)]
    fn wait(&self, interest: Interest, timeout: Option<Duration>) -> Result<(), NetError> {
        match crate::aio::wait_fd(self.raw_fd(), interest, timeout.map(|t| Instant::now() + t)) {
            Ok(true) => Ok(()),
            Ok(false) => Err(NetError::Timeout),
            Err(e) => Err(io_err(e)),
        }
    }

    /// `send` in non-blocking mode, waiting through crate::aio.
    #[cfg(unix)]
    fn send_waiting(&mut self, msg: &[u8], timeout: Option<Duration>) -> Result<(), NetError> {
        self.set_nonblocking()?;
        let mut off = 0;
        if matches!(self.stream, Stream::Tls(_)) {
            // the plaintext goes into TLS records (bounded by rustls's buffer limit), written as
            // the socket takes them
            loop {
                let Stream::Tls(t) = &mut self.stream else { unreachable!() };
                if off < msg.len() {
                    off += t.conn.writer().write(&msg[off..]).map_err(io_err)?;
                }
                let more = off < msg.len();
                if !self.try_flush()? {
                    self.wait(Interest::Write, timeout)?;
                } else if !more {
                    return Ok(());
                }
            }
        }
        while !self.try_send(msg, &mut off)? {
            self.wait(Interest::Write, timeout)?;
        }
        Ok(())
    }

    /// `recv` in non-blocking mode, waiting through crate::aio.
    #[cfg(unix)]
    fn recv_waiting(&mut self, timeout: Option<Duration>) -> Result<&[u8], NetError> {
        self.set_nonblocking()?;
        self.rbuf.clear();
        loop {
            match self.read_some() {
                Ok(true) => return Ok(&self.rbuf),
                Ok(false) => {}
                // the non-blocking socket has nothing yet (io_err maps WouldBlock so)
                Err(NetError::Timeout) => {
                    // TLS records the read produced (alerts, key updates) go out first
                    if !self.try_flush()? {
                        self.wait(Interest::Write, timeout)?;
                        continue;
                    }
                    self.wait(Interest::Read, timeout)?;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Sends `msg` and receives the reply (blocking).
    pub fn roundtrip(&mut self, msg: &[u8], timeout: Option<Duration>) -> Result<&[u8], NetError> {
        self.send(msg, timeout)?;
        self.recv(timeout)
    }

    /// Non-blocking send of `data[*off..]`; Ok(true) once all of it is sent.
    pub fn try_send(&mut self, data: &[u8], off: &mut usize) -> Result<bool, NetError> {
        while *off < data.len() {
            match self.stream.write(&data[*off..]) {
                Ok(0) => return Err(NetError::Closed),
                Ok(n) => *off += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(false),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(io_err(e)),
            }
        }
        Ok(true)
    }

    /// Starts receiving a new message (non-blocking mode).
    pub fn begin_recv(&mut self) {
        self.rbuf.clear();
    }

    /// Non-blocking receive; Ok(true) once the message is complete (see `message`).
    pub fn try_recv(&mut self) -> Result<bool, NetError> {
        match self.read_some() {
            Err(NetError::Timeout) if self.nonblocking => Ok(false),
            r => r,
        }
    }

    /// The last complete message.
    pub fn message(&self) -> &[u8] {
        &self.rbuf
    }

    /// Reads into rbuf (one read call at most once the header is known); true when complete.
    fn read_some(&mut self) -> Result<bool, NetError> {
        loop {
            let have = self.rbuf.len();
            // the first read takes up to 16 KiB (most replies), then exactly what is missing:
            // one reply per request, so bytes beyond it are a protocol error
            let want = if have >= 4 {
                let n = wire::message_length(&self.rbuf);
                // validated before anything beyond the header is read or reserved
                if n < wire::HEADER_LEN as i32 || n as usize > self.max_msg {
                    return Err(NetError::Protocol("invalid reply length"));
                }
                let n = n as usize;
                if have >= n {
                    if have > n {
                        return Err(NetError::Protocol("trailing bytes after reply"));
                    }
                    return Ok(true);
                }
                n - have
            } else {
                16 * 1024 - have
            };
            let want = want.min(READ_CHUNK);
            self.rbuf.reserve(want);
            let spare = self.rbuf.spare_capacity_mut();
            // SAFETY: read() only writes into the buffer; we expose the uninitialized tail as
            // bytes and set the length to what was written.
            let dst = unsafe { std::slice::from_raw_parts_mut(spare.as_mut_ptr() as *mut u8, want) };
            match self.stream.read(dst) {
                Ok(0) => return Err(NetError::Closed),
                Ok(n) => unsafe { self.rbuf.set_len(have + n) },
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(io_err(e)),
            }
        }
    }
}

// ------------------------------------------------------------------ I/O backends

/// A reactor token: the driver's identifier of an in-flight operation.
pub type Token = usize;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Interest {
    Read,
    Write,
}

/// The I/O-backend layer: readiness notification for non-blocking connections.
pub trait Reactor {
    /// Watches `conn` for `interest`, reporting `token` once (re-arm after each report).
    fn arm(&mut self, conn: &mut Conn, token: Token, interest: Interest) -> io::Result<()>;
    /// Stops watching `conn` (before it is closed or used in blocking mode elsewhere).
    fn disarm(&mut self, conn: &mut Conn) -> io::Result<()>;
    /// Appends the tokens that became ready to `ready`, waiting up to `timeout` (None: forever).
    fn poll(&mut self, timeout: Option<Duration>, ready: &mut Vec<Token>) -> io::Result<()>;
}

/// Backend 1: mio's poll (epoll on Linux, kqueue on macOS/BSD). Its fd is what PHP watches.
#[cfg(unix)]
pub struct MioReactor {
    poll: mio::Poll,
    events: mio::Events,
}

#[cfg(unix)]
impl MioReactor {
    pub fn new() -> io::Result<Self> {
        Ok(MioReactor { poll: mio::Poll::new()?, events: mio::Events::with_capacity(64) })
    }

    /// The epoll/kqueue fd: readable when an armed connection is ready.
    pub fn raw_fd(&self) -> std::os::fd::RawFd {
        std::os::fd::AsRawFd::as_raw_fd(&self.poll)
    }
}

#[cfg(unix)]
impl Reactor for MioReactor {
    fn arm(&mut self, conn: &mut Conn, token: Token, interest: Interest) -> io::Result<()> {
        let fd = conn.raw_fd();
        let i = match interest {
            Interest::Read => mio::Interest::READABLE,
            Interest::Write => mio::Interest::WRITABLE,
        };
        let reg = self.poll.registry();
        if conn.registered {
            reg.reregister(&mut mio::unix::SourceFd(&fd), mio::Token(token), i)
        } else {
            reg.register(&mut mio::unix::SourceFd(&fd), mio::Token(token), i)?;
            conn.registered = true;
            Ok(())
        }
    }

    fn disarm(&mut self, conn: &mut Conn) -> io::Result<()> {
        if conn.registered {
            conn.registered = false;
            let fd = conn.raw_fd();
            return self.poll.registry().deregister(&mut mio::unix::SourceFd(&fd));
        }
        Ok(())
    }

    fn poll(&mut self, timeout: Option<Duration>, ready: &mut Vec<Token>) -> io::Result<()> {
        loop {
            match self.poll.poll(&mut self.events, timeout) {
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                r => r?,
            }
            ready.extend(self.events.iter().map(|e| e.token().0));
            return Ok(());
        }
    }
}

/// What std sets on its own sockets: close-on-exec, and (Apple) no SIGPIPE on writes to a
/// closed peer.
#[cfg(unix)]
pub(crate) fn socket_defaults(fd: libc::c_int) {
    // SAFETY: fcntl/setsockopt on a descriptor the caller owns
    unsafe {
        libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
        #[cfg(target_vendor = "apple")]
        {
            let one: libc::c_int = 1;
            libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_NOSIGPIPE, (&one as *const libc::c_int).cast(), std::mem::size_of::<libc::c_int>() as libc::socklen_t);
        }
    }
}

/// The C socket address of `sa`.
#[cfg(unix)]
pub(crate) fn sockaddr(sa: &std::net::SocketAddr) -> (libc::sockaddr_storage, libc::socklen_t) {
    // SAFETY: an all-zero sockaddr_storage is valid; the casts write within its size
    let mut st: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let len = match sa {
        std::net::SocketAddr::V4(a) => {
            let p = unsafe { &mut *(&mut st as *mut libc::sockaddr_storage).cast::<libc::sockaddr_in>() };
            p.sin_family = libc::AF_INET as libc::sa_family_t;
            p.sin_port = a.port().to_be();
            p.sin_addr = libc::in_addr { s_addr: u32::from_ne_bytes(a.ip().octets()) };
            #[cfg(any(target_vendor = "apple", target_os = "freebsd", target_os = "openbsd", target_os = "netbsd", target_os = "dragonfly"))]
            {
                p.sin_len = std::mem::size_of::<libc::sockaddr_in>() as u8;
            }
            std::mem::size_of::<libc::sockaddr_in>()
        }
        std::net::SocketAddr::V6(a) => {
            let p = unsafe { &mut *(&mut st as *mut libc::sockaddr_storage).cast::<libc::sockaddr_in6>() };
            p.sin6_family = libc::AF_INET6 as libc::sa_family_t;
            p.sin6_port = a.port().to_be();
            p.sin6_addr = libc::in6_addr { s6_addr: a.ip().octets() };
            p.sin6_flowinfo = a.flowinfo();
            p.sin6_scope_id = a.scope_id();
            #[cfg(any(target_vendor = "apple", target_os = "freebsd", target_os = "openbsd", target_os = "netbsd", target_os = "dragonfly"))]
            {
                p.sin6_len = std::mem::size_of::<libc::sockaddr_in6>() as u8;
            }
            std::mem::size_of::<libc::sockaddr_in6>()
        }
    };
    (st, len as libc::socklen_t)
}

/// poll(2) over `fds` until one is ready or `deadline` passes (None: no limit); EINTR is
/// retried. The wait of a scan (libmongoc's _mongoc_stream_poll_internal); the timeout is rounded
/// up to whole milliseconds so that an unexpired deadline never spins.
#[cfg(unix)]
pub fn poll_fds(fds: &mut [libc::pollfd], deadline: Option<Instant>) -> io::Result<usize> {
    loop {
        let ms: libc::c_int = match deadline {
            None => -1,
            Some(d) => {
                let left = d.saturating_duration_since(Instant::now());
                left.as_micros().div_ceil(1000).min(libc::c_int::MAX as u128) as libc::c_int
            }
        };
        // SAFETY: a valid, exclusively borrowed pollfd array of the given length
        let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, ms) };
        if n >= 0 {
            return Ok(n as usize);
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

/// Time left until `deadline` (zero once passed).
pub fn remaining(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn advertised_size_is_bounded() {
        assert_eq!(max_message_size_for(0), DEFAULT_MAX_MSG_SIZE);
        assert_eq!(max_message_size_for(-5), DEFAULT_MAX_MSG_SIZE);
        assert_eq!(max_message_size_for(i32::MIN), DEFAULT_MAX_MSG_SIZE);
        assert_eq!(max_message_size_for(i32::MAX), HARD_MAX_MSG_SIZE);
        assert_eq!(max_message_size_for(48_000_000), 48_000_000);
        assert_eq!(max_message_size_for(3), wire::HEADER_LEN + 5);
    }

    /// A peer that sends `bytes` then keeps the connection open.
    fn peer(bytes: Vec<u8>) -> (Conn, std::thread::JoinHandle<()>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let h = std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            s.write_all(&bytes).unwrap();
            std::thread::sleep(Duration::from_millis(200));
        });
        (Conn::connect("127.0.0.1", port, Some(Duration::from_secs(5))).unwrap(), h)
    }

    #[test]
    fn unix_domain_sockets() {
        use std::os::unix::net::UnixListener;
        let dir = std::env::temp_dir().join(format!("mdrop-unix-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("m.sock");
        let l = UnixListener::bind(&path).unwrap();
        // a reply-shaped message from the peer; timeouts and blocking switches work on the socket
        let mut msg = 20i32.to_le_bytes().to_vec();
        msg.extend_from_slice(&[7; 16]);
        let sent = msg.clone();
        let h = std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            s.write_all(&sent).unwrap();
            let mut b = [0u8; 4];
            s.read_exact(&mut b).unwrap();
            assert_eq!(&b, b"ping");
        });
        let mut c = Conn::connect_unix(path.to_str().unwrap()).unwrap();
        assert_eq!(c.addr, path.to_str().unwrap());
        assert_eq!(c.recv(Some(Duration::from_secs(5))).unwrap(), &msg[..]);
        c.set_nonblocking().unwrap();
        c.send(b"ping", Some(Duration::from_secs(5))).unwrap();
        h.join().unwrap();
        let err = |p: &str| match Conn::connect_unix(p) {
            Err(NetError::Unix(code, m)) => (code, m),
            _ => panic!("{p}: connected"),
        };
        let missing = dir.join("missing.sock");
        assert_eq!(err(missing.to_str().unwrap()), (crate::error::code::STREAM_CONNECT, "Failed to connect to UNIX domain socket: No such file or directory".to_owned()));
        let file = dir.join("file.sock");
        std::fs::write(&file, b"").unwrap();
        assert!(err(file.to_str().unwrap()).1.starts_with("Failed to connect to UNIX domain socket: "));
        // libmongoc's bound: the path and its NUL must leave one byte of sun_path free
        let max = if cfg!(target_os = "linux") { 108 } else { 104 } - 2;
        let long = format!("/{}", "a".repeat(max));
        assert_eq!(err(&long), (crate::error::code::STREAM_SOCKET, "Failed to define socket address path.".to_owned()));
        assert_eq!(err(&long[..max]).1, "Failed to connect to UNIX domain socket: No such file or directory");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn oversized_or_negative_message_length_is_rejected() {
        for len in [i32::MAX, (DEFAULT_MAX_MSG_SIZE + 1) as i32, -1, 0, 15] {
            let mut hdr = len.to_le_bytes().to_vec();
            hdr.extend_from_slice(&[0; 12]);
            let (mut c, h) = peer(hdr);
            match c.recv(Some(Duration::from_secs(5))) {
                Err(NetError::Protocol(_)) => {}
                r => panic!("length {len}: {:?}", r.map(|m| m.len())),
            }
            // nothing beyond the first read was reserved for the declared length
            assert!(c.rbuf.capacity() < 1024 * 1024, "length {len}: capacity {}", c.rbuf.capacity());
            h.join().unwrap();
        }
    }

    #[test]
    fn lowered_cap_applies() {
        let mut hdr = 1000i32.to_le_bytes().to_vec();
        hdr.extend_from_slice(&[0; 12]);
        let (mut c, h) = peer(hdr);
        c.set_max_message_size(100);
        assert!(matches!(c.recv(Some(Duration::from_secs(5))), Err(NetError::Protocol(_))));
        h.join().unwrap();
    }
}
