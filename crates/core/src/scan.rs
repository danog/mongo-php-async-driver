//! One server check of a scan as a non-blocking state machine: libmongoc's mongoc_async_cmd_t
//! (mongoc-async-cmd.c) as the topology scanner uses it. The scan loop (Client::run_scan, =
//! mongoc_async_run) polls every check's socket at once, so a scan takes as long as its slowest
//! check instead of the sum of all of them.
//!
//! States and messages are libmongoc's: a check is pending until its happy-eyeballs delay
//! expires, then connects (non-blocking), runs the TLS handshake if any, sends the hello and
//! receives the reply. Its deadline is `connectTimeoutMS` after the connect started, and again
//! after the hello was sent (_acmd_reset_elapsed); the time from the end of the send to the reply
//! is the check's duration and round-trip time.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::error::{code, domain, Error};
use crate::net::{Conn, Interest, NetError};

/// libmongoc's HAPPY_EYEBALLS_DELAY: each further address of a host starts this much later.
pub const HAPPY_EYEBALLS_DELAY: Duration = Duration::from_millis(250);

/// mongoc_async_cmd_state_t
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    PendingConnect,
    /// the TLS handshake (stream setup)
    Setup,
    Send,
    Recv,
    Error,
    Cancelled,
}

/// What `step` did.
pub enum Step {
    Progress,
    /// the reply's body document
    Success(Vec<u8>),
    Failed,
}

/// The TLS client of a new connection: its configuration and server name, or the error that
/// fails the check before it connects.
pub type TlsSetup = Option<Result<(Arc<rustls::ClientConfig>, rustls::pki_types::ServerName<'static>), Error>>;

pub struct Probe {
    /// the scanner node (server id)
    pub node: u32,
    /// the address this check connects to (None: an existing connection or a UNIX socket)
    pub addr: Option<SocketAddr>,
    /// a new connection: its reply completes the handshake
    pub fresh: bool,
    pub conn: Option<Conn>,
    pub state: State,
    /// POLLOUT or POLLIN
    pub events: i16,
    /// _start_time: the deadline and the elapsed time count from here
    pub start: Instant,
    pub delay_until: Instant,
    timeout: Option<Duration>,
    msg: Vec<u8>,
    sent: usize,
    tls: TlsSetup,
    /// the server address as the topology names it
    name: String,
    pub error: Option<Error>,
    /// finished: removed at the end of the loop iteration
    pub done: bool,
}

fn stream_error(code: u32, msg: &str) -> Error {
    Error::new(domain::STREAM, code, msg)
}

impl Probe {
    /// A check on an existing connection (or a UNIX socket connected during the setup): it
    /// starts with the TLS handshake (`tls`) or the send.
    pub fn with_stream(node: u32, mut conn: Conn, msg: Vec<u8>, timeout: Option<Duration>, fresh: bool, tls: TlsSetup) -> Probe {
        let now = Instant::now();
        let name = conn.addr.clone();
        let mut p = Probe { node, addr: None, fresh, conn: None, state: State::Send, events: crate::os::POLLOUT, start: now, delay_until: now, timeout, msg, sent: 0, tls: None, name, error: None, done: false };
        if let Err(e) = conn.set_nonblocking() {
            p.fail(NetError::Io(std::io::Error::other(e.to_string())), "Failed to write rpc bytes.");
            return p;
        }
        p.conn = Some(conn);
        if let Some(t) = tls {
            p.tls = Some(t);
            p.begin_tls();
        }
        p
    }

    /// A check that connects to `addr` once `delay` has passed.
    pub fn connect(node: u32, name: &str, addr: SocketAddr, delay: Duration, msg: Vec<u8>, timeout: Option<Duration>, tls: TlsSetup) -> Probe {
        let now = Instant::now();
        Probe { node, addr: Some(addr), fresh: true, conn: None, state: State::PendingConnect, events: crate::os::POLLOUT, start: now, delay_until: now + delay, timeout, msg, sent: 0, tls, name: name.to_owned(), error: None, done: false }
    }

    fn fail(&mut self, _e: NetError, msg: &str) {
        self.error = Some(stream_error(code::STREAM_SOCKET, msg));
        self.state = State::Error;
    }

    /// Wraps the connection in TLS (mongoc_stream_tls_new_with_hostname): the handshake runs as
    /// the stream setup.
    fn begin_tls(&mut self) {
        let Some(tls) = self.tls.take() else { return };
        let conn = self.conn.take().expect("connected");
        match tls.and_then(|(cfg, name)| conn.tls_begin(cfg, name).map_err(|e| net_error(&e))) {
            Ok(c) => {
                self.conn = Some(c);
                self.state = State::Setup;
                self.events = crate::os::POLLOUT;
            }
            Err(e) => {
                self.error = Some(e);
                self.state = State::Error;
            }
        }
    }

    /// _mongoc_async_cmd_phase_connect: false when no connection could be started (the check
    /// failed; `error` may be unset: "connection error").
    pub fn initiate(&mut self) -> bool {
        // a TLS configuration that failed to load fails the check before it connects
        if let Some(Err(e)) = &self.tls {
            self.error = Some(e.clone());
            return false;
        }
        let Some(sa) = self.addr else { return false };
        match Conn::connect_nonblocking(sa, &self.name) {
            Ok(c) => {
                self.conn = Some(c);
                self.start = Instant::now();
                self.state = State::Send;
                self.events = crate::os::POLLOUT;
                if self.tls.is_some() {
                    self.begin_tls();
                    if self.state == State::Error {
                        return false;
                    }
                }
                true
            }
            Err(_) => false,
        }
    }

    pub fn fd(&self) -> Option<crate::os::RawSock> {
        self.conn.as_ref().map(Conn::raw_fd)
    }

    /// When the check times out (None: never, connectTimeoutMS 0).
    pub fn deadline(&self) -> Option<Instant> {
        self.timeout.map(|t| self.start + t)
    }

    pub fn timed_out(&self, now: Instant) -> bool {
        self.deadline().is_some_and(|d| d <= now)
    }

    /// POLLERR/POLLHUP on the socket (mongoc_async_run's messages).
    pub fn poll_error(&mut self, hup: bool) {
        let e = if self.state == State::Send {
            stream_error(code::STREAM_CONNECT, if hup { "connection refused" } else { "unknown connection error" })
        } else {
            stream_error(code::STREAM_SOCKET, if hup { "connection closed" } else { "unknown socket error" })
        };
        self.error = Some(e);
        self.state = State::Error;
    }

    /// The deadline passed (mongoc_async_run): "connection timeout" while connecting, else
    /// "socket timeout".
    pub fn timeout_error(&mut self) {
        self.error = Some(stream_error(code::STREAM_CONNECT, if self.state == State::Send { "connection timeout" } else { "socket timeout" }));
    }

    /// _acmd_cancel
    pub fn cancel(&mut self) {
        self.state = State::Cancelled;
    }

    /// One phase of mongoc_async_cmd_run (the caller reports a connection first when the state
    /// is Send).
    pub fn step(&mut self) -> Step {
        let Some(conn) = self.conn.as_mut() else { return Step::Failed };
        match self.state {
            State::Setup => match conn.try_handshake() {
                Ok(None) => {
                    self.state = State::Send;
                    self.events = crate::os::POLLOUT;
                    Step::Progress
                }
                Ok(Some(i)) => {
                    self.events = if i == Interest::Read { crate::os::POLLIN } else { crate::os::POLLOUT };
                    Step::Progress
                }
                Err(e) => {
                    self.error = Some(net_error(&e));
                    Step::Failed
                }
            },
            State::Send => match conn.try_send(&self.msg, &mut self.sent).and_then(|all| if all { conn.try_flush() } else { Ok(false) }) {
                Ok(true) => {
                    conn.begin_recv();
                    self.state = State::Recv;
                    self.events = crate::os::POLLIN;
                    self.start = Instant::now();
                    Step::Progress
                }
                Ok(false) => Step::Progress,
                Err(_) => {
                    self.error = Some(stream_error(code::STREAM_SOCKET, "Failed to write rpc bytes."));
                    Step::Failed
                }
            },
            State::Recv => match conn.try_recv() {
                Ok(false) => Step::Progress,
                Ok(true) => match crate::handshake::reply_body(conn.message(), crate::net::DEFAULT_MAX_MSG_SIZE) {
                    Ok(body) => Step::Success(body),
                    Err(_) => {
                        self.error = Some(Error::new(domain::PROTOCOL, code::PROTOCOL_INVALID_REPLY, "Invalid reply from server."));
                        Step::Failed
                    }
                },
                Err(NetError::Closed) => {
                    self.error = Some(stream_error(code::STREAM_SOCKET, "Server closed connection."));
                    Step::Failed
                }
                Err(NetError::Protocol(_)) => {
                    self.error = Some(Error::new(domain::PROTOCOL, code::PROTOCOL_INVALID_REPLY, "Invalid reply from server."));
                    Step::Failed
                }
                Err(_) => {
                    self.error = Some(stream_error(code::STREAM_SOCKET, "Failed to receive rpc bytes from server."));
                    Step::Failed
                }
            },
            State::PendingConnect | State::Error | State::Cancelled => Step::Failed,
        }
    }
}

/// A connection-setup error (TLS) as the check reports it.
fn net_error(e: &NetError) -> Error {
    match e {
        NetError::Tls(m) => Error::new(domain::STREAM, code::STREAM_SOCKET, format!("TLS handshake failed: {m}")),
        e => Error::new(domain::STREAM, code::STREAM_SOCKET, e.to_string()),
    }
}
