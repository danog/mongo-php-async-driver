//! The waits of the driver's I/O: where a blocking driver blocks the PHP thread, these may
//! instead suspend the calling PHP fiber (the driver installs a [`Backend`] that does it through
//! the event loop). Without a backend, or when the backend says the call may not suspend, every
//! wait is the plain blocking one (poll(2), thread::sleep), so synchronous use keeps its exact
//! syscalls: the socket code asks [`active`] once per send or receive and stays on its blocking
//! path when it is false.

use std::io;
use std::cell::Cell;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::time::{Duration, Instant};

use crate::net::Interest;

/// The event-loop side of the waits (installed by the PHP extension).
pub struct Backend {
    /// Whether the current call may suspend instead of blocking.
    pub active: fn() -> bool,
    /// poll(2) semantics over `fds` (revents filled, the count of ready ones; 0: the deadline
    /// passed), suspending the caller meanwhile.
    pub poll: fn(&mut [crate::os::PollFd], Option<Instant>) -> io::Result<usize>,
    /// Sleeps, suspending the caller meanwhile.
    pub sleep: fn(Duration) -> io::Result<()>,
    /// Before an operation's first I/O: an error message when the call would have to suspend
    /// but cannot (a fiber without the event loop).
    pub require: fn() -> Result<(), &'static str>,
    /// Suspends until `unpark(id)` or `deadline` (false): `enqueue` receives the id first.
    pub park: fn(&mut dyn FnMut(u64), Option<Instant>) -> io::Result<bool>,
    /// Resumes the parked caller `id` (if it still waits).
    pub unpark: fn(u64),
    /// The current fiber's identity (0: {main}).
    pub current: fn() -> usize,
    /// A socket the waits may have watched is about to be closed (its registration must go: a
    /// socket that reuses its number is a new one).
    pub forget: fn(crate::os::RawSock),
}

static BACKEND: AtomicPtr<Backend> = AtomicPtr::new(std::ptr::null_mut());

/// The words of the synchronous fast path, per thread (each PHP thread of a thread-safe PHP
/// has its own executor globals):
/// - `fiber`: the address of a pointer that is null while no fiber runs (the extension's
///   &EG(active_fiber)); null: unknown (every call asks the backend);
/// - `classes`: the address of a counter that changes whenever the event loop may have become
///   available (the extension's class-table size: Revolt\EventLoop can only appear with a new
///   class);
/// - `no_loop_at`: the counter's value when the backend last found no event loop in {main}
///   (u32::MAX: not known, or an event loop is there). Maintained by the backend.
struct SyncWords {
    fiber: Cell<*const *const ()>,
    classes: Cell<*const u32>,
    no_loop_at: Cell<u32>,
}

thread_local! {
    static SYNC: SyncWords = const { SyncWords { fiber: Cell::new(std::ptr::null()), classes: Cell::new(std::ptr::null()), no_loop_at: Cell::new(u32::MAX) } };
}

/// Sets the words [`quiet`] reads (null pointers: none, until they are set again).
pub fn set_sync_words(fiber: *const *const (), classes: *const u32) {
    SYNC.with(|w| {
        w.fiber.set(fiber);
        w.classes.set(classes);
    });
}

/// The counter value at which the backend last found no event loop (u32::MAX: none).
#[inline]
pub fn no_loop_at() -> u32 {
    SYNC.with(|w| w.no_loop_at.get())
}

pub fn set_no_loop_at(v: u32) {
    SYNC.with(|w| w.no_loop_at.set(v));
}

/// The synchronous fast path: a {main} call while no event loop is loaded (and none can have
/// appeared since the backend last looked), so no wait may suspend. A few loads, inlined, so
/// synchronous use does not call into the backend per send/receive.
#[inline(always)]
fn quiet() -> bool {
    SYNC.with(|w| {
        let (f, c) = (w.fiber.get(), w.classes.get());
        // SAFETY: the extension's executor-globals words of this thread, valid while set
        !f.is_null() && !c.is_null() && unsafe { std::ptr::read_volatile(f) }.is_null() && unsafe { std::ptr::read_volatile(c) } == w.no_loop_at.get()
    })
}

/// Installs (or removes, with None) the backend.
pub fn set_backend(b: Option<&'static Backend>) {
    BACKEND.store(b.map_or(std::ptr::null_mut(), |b| b as *const Backend as *mut Backend), Ordering::Release);
}

#[inline]
fn backend() -> Option<&'static Backend> {
    let p = BACKEND.load(Ordering::Acquire);
    // SAFETY: only ever set from a &'static Backend
    (!p.is_null()).then(|| unsafe { &*p })
}

/// Whether the current call's waits suspend (else they block).
#[inline]
pub fn active() -> bool {
    !quiet() && backend().is_some_and(|b| (b.active)())
}

/// Before an operation's first I/O (server selection, getting a connection): fails when the
/// call runs where waits would have to suspend but no event loop is available.
#[inline]
pub fn require() -> Result<(), crate::error::Error> {
    if quiet() {
        return Ok(());
    }
    require_slow()
}

#[cold]
fn require_slow() -> Result<(), crate::error::Error> {
    match backend() {
        Some(b) => (b.require)().map_err(|m| crate::error::Error::new(crate::error::domain::CLIENT, crate::error::code::CLIENT_NO_EVENT_LOOP, m)),
        None => Ok(()),
    }
}

/// How a [`park`] ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Parked {
    /// another fiber called [`unpark`] with this wait's id
    Unparked,
    /// the deadline passed first
    TimedOut,
    /// the call cannot suspend (not [`active`]): it did not wait, and `enqueue` was not called.
    /// What it waits for is held by a fiber that cannot run before this call returns, so the
    /// caller must not wait for it at all (no blocking retry loop: it would never end).
    Blocking,
}

/// The wait for something another fiber holds: suspends until that fiber calls [`unpark`] with
/// the id `enqueue` was given, or until `deadline`. Decides itself whether the call can suspend
/// ([`Parked::Blocking`] when not), so a caller cannot wait where it would block the thread.
pub fn park(enqueue: &mut dyn FnMut(u64), deadline: Option<Instant>) -> io::Result<Parked> {
    match backend() {
        Some(b) if active() => Ok(if (b.park)(enqueue, deadline)? { Parked::Unparked } else { Parked::TimedOut }),
        _ => Ok(Parked::Blocking),
    }
}

/// The calling fiber's identity (0: {main}, or no event-loop backend).
pub fn current() -> usize {
    backend().map_or(0, |b| (b.current)())
}

/// A socket of the driver is about to be closed: the backend forgets what it registered for it.
/// Every owner of a socket the waits may watch calls it before closing it (net::Conn,
/// [`WaitStream`]).
pub fn forget(fd: crate::os::RawSock) {
    if let Some(b) = backend() {
        (b.forget)(fd);
    }
}

/// Resumes a [`park`]ed caller.
pub fn unpark(id: u64) {
    if let Some(b) = backend() {
        (b.unpark)(id);
    }
}

/// The error of a wait that ended without its event: the waiting fiber is being destroyed, or
/// an exception was thrown into it. The operation must stop; its connection is in an unknown
/// state and is closed.
#[derive(Debug)]
pub struct Interrupted;

impl std::fmt::Display for Interrupted {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("the operation was interrupted")
    }
}

impl std::error::Error for Interrupted {}

/// The io::Error of an interrupted wait.
pub fn interrupted() -> io::Error {
    io::Error::other(Interrupted)
}

/// Whether `e` is an interrupted wait.
pub fn is_interrupted(e: &io::Error) -> bool {
    e.get_ref().is_some_and(|i| i.is::<Interrupted>())
}

/// poll(2) over `fds` until one is ready or `deadline` passes (None: no limit): suspends when
/// [`active`], else blocks.
pub fn poll(fds: &mut [crate::os::PollFd], deadline: Option<Instant>) -> io::Result<usize> {
    match backend() {
        Some(b) if active() => (b.poll)(fds, deadline),
        _ => crate::net::poll_fds(fds, deadline),
    }
}

/// Waits until `fd` is ready for `interest`: false when `deadline` passed first.
pub fn wait_fd(fd: crate::os::RawSock, interest: Interest, deadline: Option<Instant>) -> io::Result<bool> {
    let events = match interest {
        Interest::Read => crate::os::POLLIN,
        Interest::Write => crate::os::POLLOUT,
    };
    let mut p = [crate::os::pollfd(fd, events)];
    Ok(poll(&mut p, deadline)? > 0)
}

/// Sleeps for `d`: suspends when [`active`], else blocks (thread::sleep).
pub fn sleep(d: Duration) -> io::Result<()> {
    match backend() {
        Some(b) if active() => (b.sleep)(d),
        _ => {
            std::thread::sleep(d);
            Ok(())
        }
    }
}

/// A TCP stream whose reads and writes wait through [`wait_fd`] (the socket is non-blocking):
/// blocking `Read`/`Write` semantics for code written against a blocking stream (rustls's
/// StreamOwned, the HTTP client), each read or write bounded by `timeout` as SO_RCVTIMEO /
/// SO_SNDTIMEO would (a timeout is reported as ErrorKind::TimedOut).
pub struct WaitStream {
    pub sock: std::net::TcpStream,
    pub timeout: Option<Duration>,
}

impl WaitStream {
    fn wait(&self, interest: Interest) -> io::Result<()> {
        let deadline = self.timeout.map(|t| Instant::now() + t);
        if wait_fd(crate::os::raw(&self.sock), interest, deadline)? {
            Ok(())
        } else {
            Err(io::ErrorKind::TimedOut.into())
        }
    }
}

impl Drop for WaitStream {
    fn drop(&mut self) {
        // before the socket closes: a socket reusing its number must be registered anew
        forget(crate::os::raw(&self.sock));
    }
}

impl io::Read for WaitStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            match self.sock.read(buf) {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => self.wait(Interest::Read)?,
                r => return r,
            }
        }
    }
}

impl io::Write for WaitStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        loop {
            match self.sock.write(buf) {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => self.wait(Interest::Write)?,
                r => return r,
            }
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// connect(2) to `sa` bounded by `timeout`, waiting through [`wait_fd`]: the connected,
/// non-blocking socket as a [`WaitStream`] with `timeout` (TcpStream::connect_timeout's errors:
/// TimedOut, the socket error).
pub fn connect(sa: &std::net::SocketAddr, timeout: Duration) -> io::Result<WaitStream> {
    let (sock, r) = crate::os::connect_start(sa)?;
    // the WaitStream owns the socket before it waits (it unregisters it when dropped)
    let s = WaitStream { sock, timeout: Some(timeout) };
    if !r? {
        if !wait_fd(crate::os::raw(&s.sock), Interest::Write, Some(Instant::now() + timeout))? {
            return Err(io::ErrorKind::TimedOut.into());
        }
        if let Some(e) = s.sock.take_error()? {
            return Err(e);
        }
    }
    Ok(s)
}

/// A client socket of the control-plane connections (KMS, HTTP): a blocking TcpStream as
/// before, or a [`WaitStream`] when the call may suspend (decided at connect).
pub enum Socket {
    Plain(std::net::TcpStream),
    Wait(WaitStream),
}

impl Socket {
    /// TcpStream::connect_timeout, or [`connect`] when [`active`].
    pub fn connect(sa: &std::net::SocketAddr, timeout: Duration) -> io::Result<Socket> {
        if active() {
            return Ok(Socket::Wait(connect(sa, timeout)?));
        }
        std::net::TcpStream::connect_timeout(sa, timeout).map(Socket::Plain)
    }

    /// SO_RCVTIMEO and SO_SNDTIMEO (each read or write).
    pub fn set_timeouts(&mut self, t: Option<Duration>) -> io::Result<()> {
        match self {
            Socket::Plain(s) => s.set_read_timeout(t).and_then(|()| s.set_write_timeout(t)),
            Socket::Wait(w) => {
                w.timeout = t;
                Ok(())
            }
        }
    }
}

impl io::Read for Socket {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Socket::Plain(s) => s.read(buf),
            Socket::Wait(w) => w.read(buf),
        }
    }
}

impl io::Write for Socket {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Socket::Plain(s) => s.write(buf),
            Socket::Wait(w) => w.write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    thread_local! {
        static FORGOTTEN: std::cell::RefCell<Vec<crate::os::RawSock>> = const { std::cell::RefCell::new(Vec::new()) };
    }

    fn no() -> bool {
        false
    }

    /// A backend that never suspends and records the sockets it is told about.
    static RECORDING: Backend = Backend {
        active: no,
        poll: |fds, d| crate::net::poll_fds(fds, d),
        sleep: |d| {
            std::thread::sleep(d);
            Ok(())
        },
        require: || Ok(()),
        park: |_, _| Ok(false),
        unpark: |_| {},
        current: || 0,
        forget: |fd| FORGOTTEN.with_borrow_mut(|f| f.push(fd)),
    };

    fn forgotten(fd: crate::os::RawSock) -> bool {
        FORGOTTEN.with_borrow_mut(|f| std::mem::take(f)).contains(&fd)
    }

    /// Every socket owner of the driver tells the backend before it closes its socket: a socket
    /// that gets the number of a closed, registered one must be registered anew, or it never
    /// reports.
    #[test]
    fn closed_sockets_are_forgotten() {
        set_backend(Some(&RECORDING));
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        // a blocking connection (net::Conn::connect)
        let c = crate::net::Conn::connect("127.0.0.1", addr.port(), Some(Duration::from_secs(5))).unwrap();
        let fd = c.raw_fd();
        drop(c);
        assert!(forgotten(fd), "Conn::connect");
        // a server check's connection (net::Conn::connect_nonblocking), also over TLS
        let c = crate::net::Conn::connect_nonblocking(addr, "x").unwrap();
        let fd = c.raw_fd();
        drop(c);
        assert!(forgotten(fd), "Conn::connect_nonblocking");
        let c = crate::net::Conn::connect_nonblocking(addr, "x").unwrap();
        let fd = c.raw_fd();
        let config = std::sync::Arc::new(rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(rustls_graviola::default_provider())).with_safe_default_protocol_versions().unwrap().with_root_certificates(rustls::RootCertStore::empty()).with_no_client_auth());
        let c = c.tls_begin(config, rustls::pki_types::ServerName::try_from("localhost").unwrap()).unwrap();
        assert!(!forgotten(fd));
        drop(c);
        assert!(forgotten(fd), "Conn::tls_begin");
        // a UNIX domain socket connection (net::Conn::connect_unix)
        #[cfg(unix)]
        {
            let dir = std::env::temp_dir().join(format!("mdrop-aio-{}", std::process::id()));
            let _ = std::fs::remove_file(&dir);
            let ul = std::os::unix::net::UnixListener::bind(&dir).unwrap();
            let c = crate::net::Conn::connect_unix(dir.to_str().unwrap()).unwrap();
            let fd = c.raw_fd();
            drop(c);
            assert!(forgotten(fd), "Conn::connect_unix");
            drop(ul);
            let _ = std::fs::remove_file(&dir);
        }
        // the KMS / HTTP sockets (WaitStream; connect wraps the socket before it waits)
        let w = connect(&addr, Duration::from_secs(5)).unwrap();
        let fd = crate::os::raw(&w.sock);
        drop(w);
        assert!(forgotten(fd), "aio::WaitStream");
    }
}
