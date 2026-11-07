//! The waits of the driver's I/O: where a blocking driver blocks the PHP thread, these may
//! instead suspend the calling PHP fiber (the driver installs a [`Backend`] that does it through
//! the event loop). Without a backend, or when the backend says the call may not suspend, every
//! wait is the plain blocking one (poll(2), thread::sleep), so synchronous use keeps its exact
//! syscalls: the socket code asks [`active`] once per send or receive and stays on its blocking
//! path when it is false.

use std::io;
use std::sync::atomic::{AtomicPtr, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use crate::net::Interest;

/// The event-loop side of the waits (installed by the PHP extension).
pub struct Backend {
    /// Whether the current call may suspend instead of blocking.
    pub active: fn() -> bool,
    /// poll(2) semantics over `fds` (revents filled, the count of ready ones; 0: the deadline
    /// passed), suspending the caller meanwhile.
    pub poll: fn(&mut [libc::pollfd], Option<Instant>) -> io::Result<usize>,
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
}

static BACKEND: AtomicPtr<Backend> = AtomicPtr::new(std::ptr::null_mut());

/// The address of a pointer that is null while no fiber runs (the extension's
/// &EG(active_fiber)); null: unknown (every call asks the backend).
static FIBER: AtomicPtr<*const ()> = AtomicPtr::new(std::ptr::null_mut());

/// The address of a counter that changes whenever the event loop may have become available
/// (the extension's class-table size: Revolt\EventLoop can only appear with a new class).
static CLASSES: AtomicPtr<u32> = AtomicPtr::new(std::ptr::null_mut());

/// The counter's value when the backend last found no event loop in {main} (u32::MAX: not
/// known, or an event loop is there). Maintained by the backend.
pub static NO_LOOP_AT: AtomicU32 = AtomicU32::new(u32::MAX);

/// Sets the words [`quiet`] reads.
pub fn set_sync_words(fiber: *const *const (), classes: *const u32) {
    FIBER.store(fiber as *mut *const (), Ordering::Release);
    CLASSES.store(classes as *mut u32, Ordering::Release);
}

/// The synchronous fast path: a {main} call while no event loop is loaded (and none can have
/// appeared since the backend last looked), so no wait may suspend. A few loads, inlined, so
/// synchronous use does not call into the backend per send/receive.
#[inline(always)]
fn quiet() -> bool {
    let f = FIBER.load(Ordering::Relaxed);
    let c = CLASSES.load(Ordering::Relaxed);
    // SAFETY: the extension's executor-globals words, valid for the process lifetime
    !f.is_null() && !c.is_null() && unsafe { std::ptr::read_volatile(f) }.is_null() && unsafe { std::ptr::read_volatile(c) } == NO_LOOP_AT.load(Ordering::Relaxed)
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

/// Suspends until another fiber calls [`unpark`] with the id `enqueue` was given, or until
/// `deadline` (Ok(false)). Only when [`active`]: never blocks the thread.
pub fn park(enqueue: &mut dyn FnMut(u64), deadline: Option<Instant>) -> io::Result<bool> {
    match backend() {
        Some(b) => (b.park)(enqueue, deadline),
        None => Ok(false),
    }
}

/// The calling fiber's identity (0: {main}, or no event-loop backend).
pub fn current() -> usize {
    backend().map_or(0, |b| (b.current)())
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
#[cfg(unix)]
pub fn poll(fds: &mut [libc::pollfd], deadline: Option<Instant>) -> io::Result<usize> {
    match backend() {
        Some(b) if active() => (b.poll)(fds, deadline),
        _ => crate::net::poll_fds(fds, deadline),
    }
}

/// Waits until `fd` is ready for `interest`: false when `deadline` passed first.
#[cfg(unix)]
pub fn wait_fd(fd: std::os::fd::RawFd, interest: Interest, deadline: Option<Instant>) -> io::Result<bool> {
    let events = match interest {
        Interest::Read => libc::POLLIN,
        Interest::Write => libc::POLLOUT,
    };
    let mut p = [libc::pollfd { fd, events, revents: 0 }];
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
#[cfg(unix)]
pub struct WaitStream {
    pub sock: std::net::TcpStream,
    pub timeout: Option<Duration>,
}

#[cfg(unix)]
impl WaitStream {
    fn wait(&self, interest: Interest) -> io::Result<()> {
        use std::os::fd::AsRawFd;
        let deadline = self.timeout.map(|t| Instant::now() + t);
        if wait_fd(self.sock.as_raw_fd(), interest, deadline)? {
            Ok(())
        } else {
            Err(io::ErrorKind::TimedOut.into())
        }
    }
}

#[cfg(unix)]
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

#[cfg(unix)]
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
/// non-blocking socket (TcpStream::connect_timeout's errors: TimedOut, the socket error).
#[cfg(unix)]
pub fn connect(sa: &std::net::SocketAddr, timeout: Duration) -> io::Result<std::net::TcpStream> {
    use std::os::fd::{AsRawFd, FromRawFd};
    let family = if sa.is_ipv4() { libc::AF_INET } else { libc::AF_INET6 };
    // SAFETY: plain socket creation; the descriptor is owned by the TcpStream right away
    let fd = unsafe { libc::socket(family, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let s = unsafe { std::net::TcpStream::from_raw_fd(fd) };
    crate::net::socket_defaults(fd);
    s.set_nonblocking(true)?;
    let (st, len) = crate::net::sockaddr(sa);
    // SAFETY: a sockaddr of the given length
    if unsafe { libc::connect(fd, (&st as *const libc::sockaddr_storage).cast(), len) } < 0 {
        let e = io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::EINPROGRESS) {
            return Err(e);
        }
        if !wait_fd(s.as_raw_fd(), Interest::Write, Some(Instant::now() + timeout))? {
            return Err(io::ErrorKind::TimedOut.into());
        }
        if let Some(e) = s.take_error()? {
            return Err(e);
        }
    }
    Ok(s)
}

/// A client socket of the control-plane connections (KMS, HTTP): a blocking TcpStream as
/// before, or a [`WaitStream`] when the call may suspend (decided at connect).
#[cfg(unix)]
pub enum Socket {
    Plain(std::net::TcpStream),
    Wait(WaitStream),
}

#[cfg(unix)]
impl Socket {
    /// TcpStream::connect_timeout, or [`connect`] when [`active`].
    pub fn connect(sa: &std::net::SocketAddr, timeout: Duration) -> io::Result<Socket> {
        if active() {
            return Ok(Socket::Wait(WaitStream { sock: connect(sa, timeout)?, timeout: Some(timeout) }));
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

#[cfg(unix)]
impl io::Read for Socket {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Socket::Plain(s) => s.read(buf),
            Socket::Wait(w) => w.read(buf),
        }
    }
}

#[cfg(unix)]
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
