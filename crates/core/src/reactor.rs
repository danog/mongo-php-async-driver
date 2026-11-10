//! The readiness reactor behind the suspending waits on Unix: one epoll (Linux) or kqueue
//! (macOS, BSD) instance per thread, whose fd the event loop watches (the driver's backend arms
//! it for a wait and drains it when that fd is readable).
//!
//! A socket is registered with the kernel at its first wait and stays registered until it is
//! closed, so a wait makes no control call. Readiness is edge-triggered (EPOLLET; EV_CLEAR for
//! kqueue's read filter): an event reports a change once. That is sound because a wait only
//! starts after the socket's last read or write returned EAGAIN (or right after a request was
//! sent, before its reply can be there), and an event that arrives while nobody waits for it is
//! kept as "ready" bits that the next wait of that socket consumes: that wait returns at once and
//! its caller retries the I/O. kqueue's write filter is one-shot per wait instead (macOS reports
//! an edge-triggered write filter at every ACK that frees send-buffer space).
//!
//! The bookkeeping is per fd number: a closed socket must be [`forget`]-ten, else a new socket
//! that gets the same number would be taken for registered and never report. The owners of the
//! driver's sockets do it when they drop them (net::Conn, aio::WaitStream). The kernel drops a
//! closed socket's registration by itself (epoll: once no process holds it any more).

use std::cell::RefCell;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use crate::os::PollFd;

const IN: i16 = libc::POLLIN;
const OUT: i16 = libc::POLLOUT;
const ERRS: i16 = libc::POLLERR | libc::POLLHUP;

/// The state of one fd number.
#[derive(Clone, Copy, Default)]
struct Entry {
    /// the waiting token + 1 (0: nobody waits) and its generation
    waiter: u32,
    gen: u32,
    /// what the waiter waits for (POLLIN / POLLOUT)
    want: i16,
    /// readiness reported while nobody waited for it
    ready: i16,
    /// registered with the kernel
    registered: bool,
}

struct Reactor {
    kfd: OwnedFd,
    fds: Vec<Entry>,
    #[cfg(any(target_os = "linux", target_os = "android"))]
    evs: Vec<libc::epoll_event>,
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    evs: Vec<libc::kevent>,
}

thread_local! {
    static REACTOR: RefCell<Option<Reactor>> = const { RefCell::new(None) };
}

fn cvt(r: libc::c_int) -> io::Result<libc::c_int> {
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r)
    }
}

/// Runs `f` on this thread's reactor (created on first use). Never across anything that may
/// drop a socket: [`forget`] borrows it too.
fn with<R>(f: impl FnOnce(&mut Reactor) -> io::Result<R>) -> io::Result<R> {
    REACTOR.with_borrow_mut(|r| {
        if r.is_none() {
            *r = Some(Reactor::new()?);
        }
        f(r.as_mut().unwrap())
    })
}

/// The fd the event loop watches: readable when a waited-for socket may be ready.
pub fn raw_fd() -> io::Result<RawFd> {
    with(|r| Ok(r.kfd.as_raw_fd()))
}

/// Watches `fds` for the wait (`token`, `gen`). Ok(true): one of them is already known to be
/// ready (its bits are consumed, nothing is watched): the caller does not suspend but retries
/// its I/O, or polls the set when there are several.
pub fn arm(fds: &[PollFd], token: usize, gen: u32) -> io::Result<bool> {
    with(|r| r.arm(fds, token, gen))
}

/// The wait (`token`) over `fds` ended.
pub fn disarm(fds: &[PollFd], token: usize) {
    REACTOR.with_borrow_mut(|r| {
        if let Some(r) = r.as_mut() {
            for p in fds {
                if let Some(e) = r.fds.get_mut(p.fd as usize) {
                    if e.waiter == token as u32 + 1 {
                        e.waiter = 0;
                    }
                }
            }
        }
    });
}

/// Reads the events without blocking: `take(token, gen)` is called for the wait each one is for
/// and says whether that wait took it (it was still pending); the readiness of events nobody
/// took is kept for the socket's next wait.
pub fn drain(take: &mut dyn FnMut(usize, u32) -> bool) {
    let _ = with(|r| {
        r.drain(take);
        Ok(())
    });
}

/// The socket `fd` is being closed: its registration goes with it.
pub fn forget(fd: RawFd) {
    let _ = REACTOR.try_with(|r| {
        // the borrows of this module never span a socket's drop, so this one cannot fail
        if let Ok(mut r) = r.try_borrow_mut() {
            if let Some(e) = r.as_mut().and_then(|r| r.fds.get_mut(fd as usize)) {
                *e = Entry::default();
            }
        }
    });
}

impl Reactor {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn new() -> io::Result<Reactor> {
        // SAFETY: creates a descriptor owned right away
        let fd = cvt(unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) })?;
        Ok(Reactor { kfd: unsafe { OwnedFd::from_raw_fd(fd) }, fds: Vec::new(), evs: vec![libc::epoll_event { events: 0, u64: 0 }; 256] })
    }

    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    fn new() -> io::Result<Reactor> {
        // SAFETY: creates a descriptor owned right away
        let kfd = unsafe { OwnedFd::from_raw_fd(cvt(libc::kqueue())?) };
        cvt(unsafe { libc::fcntl(kfd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) })?;
        // SAFETY: kevent is plain data
        Ok(Reactor { kfd, fds: Vec::new(), evs: vec![unsafe { std::mem::zeroed() }; 256] })
    }

    fn entry(&mut self, fd: RawFd) -> &mut Entry {
        let i = fd as usize;
        if self.fds.len() <= i {
            self.fds.resize((i + 1).next_power_of_two().max(64), Entry::default());
        }
        &mut self.fds[i]
    }

    fn arm(&mut self, fds: &[PollFd], token: usize, gen: u32) -> io::Result<bool> {
        // poll(2) ignores negative fds; so does this
        let mut hit = false;
        for p in fds.iter().filter(|p| p.fd >= 0) {
            let e = self.entry(p.fd);
            let bits = e.ready & (p.events | ERRS);
            e.ready &= !bits;
            hit |= bits != 0;
        }
        if hit {
            return Ok(true);
        }
        for (k, p) in fds.iter().enumerate().filter(|(_, p)| p.fd >= 0) {
            if let Err(err) = self.watch(p, token, gen) {
                for q in fds[..k].iter().filter(|q| q.fd >= 0) {
                    self.entry(q.fd).waiter = 0;
                }
                return Err(err);
            }
        }
        Ok(false)
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn watch(&mut self, p: &PollFd, token: usize, gen: u32) -> io::Result<()> {
        let kfd = self.kfd.as_raw_fd();
        let e = self.entry(p.fd);
        if !e.registered {
            let mut ev = libc::epoll_event { events: (libc::EPOLLIN | libc::EPOLLOUT | libc::EPOLLRDHUP | libc::EPOLLET) as u32, u64: p.fd as u64 };
            // SAFETY: a valid event for the socket
            match cvt(unsafe { libc::epoll_ctl(kfd, libc::EPOLL_CTL_ADD, p.fd, &mut ev) }) {
                Ok(_) => {}
                Err(err) if err.raw_os_error() == Some(libc::EEXIST) => {}
                Err(err) => return Err(err),
            }
            e.registered = true;
        }
        *e = Entry { waiter: token as u32 + 1, gen, want: p.events, ..*e };
        Ok(())
    }

    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    fn watch(&mut self, p: &PollFd, token: usize, gen: u32) -> io::Result<()> {
        let kfd = self.kfd.as_raw_fd();
        let e = self.entry(p.fd);
        let kev = |filter, flags| libc::kevent { ident: p.fd as usize, filter, flags, fflags: 0, data: 0, udata: std::ptr::null_mut() };
        let ch = [kev(libc::EVFILT_READ, libc::EV_ADD | libc::EV_CLEAR), kev(libc::EVFILT_WRITE, libc::EV_ADD | libc::EV_ONESHOT)];
        let changes: &[libc::kevent] = match (e.registered, p.events & OUT != 0) {
            (false, true) => &ch,
            (false, false) => &ch[..1],
            (true, true) => &ch[1..],
            (true, false) => &[],
        };
        if !changes.is_empty() {
            // SAFETY: a valid changelist, no event list
            cvt(unsafe { libc::kevent(kfd, changes.as_ptr(), changes.len() as libc::c_int, std::ptr::null_mut(), 0, std::ptr::null()) })?;
        }
        *e = Entry { waiter: token as u32 + 1, gen, want: p.events, registered: true, ..*e };
        Ok(())
    }

    /// An event `bits` for `fd`: offered to its waiter, else kept as ready bits.
    fn event(&mut self, fd: usize, bits: i16, take: &mut dyn FnMut(usize, u32) -> bool) {
        let Some(e) = self.fds.get_mut(fd) else { return };
        if e.waiter != 0 && bits & (e.want | ERRS) != 0 && take(e.waiter as usize - 1, e.gen) {
            e.ready |= bits & !(e.want | ERRS);
            return;
        }
        e.ready |= bits;
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn drain(&mut self, take: &mut dyn FnMut(usize, u32) -> bool) {
        loop {
            // SAFETY: the event buffer we own, a zero timeout
            let n = unsafe { libc::epoll_wait(self.kfd.as_raw_fd(), self.evs.as_mut_ptr(), self.evs.len() as libc::c_int, 0) };
            if n <= 0 {
                return;
            }
            for i in 0..n as usize {
                let (fd, ev) = (self.evs[i].u64 as usize, self.evs[i].events as libc::c_int);
                let mut bits = 0;
                if ev & libc::EPOLLIN != 0 {
                    bits |= IN;
                }
                if ev & libc::EPOLLOUT != 0 {
                    bits |= OUT;
                }
                if ev & (libc::EPOLLERR | libc::EPOLLHUP | libc::EPOLLRDHUP) != 0 {
                    bits |= ERRS | IN;
                }
                self.event(fd, bits, take);
            }
            if (n as usize) < self.evs.len() {
                return;
            }
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    fn drain(&mut self, take: &mut dyn FnMut(usize, u32) -> bool) {
        let zero = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        loop {
            // SAFETY: the event buffer we own, a zero timeout
            let n = unsafe { libc::kevent(self.kfd.as_raw_fd(), std::ptr::null(), 0, self.evs.as_mut_ptr(), self.evs.len() as libc::c_int, &zero) };
            if n <= 0 {
                return;
            }
            for i in 0..n as usize {
                let e = &self.evs[i];
                let mut bits = if e.filter == libc::EVFILT_READ { IN } else { OUT };
                if e.flags & (libc::EV_EOF | libc::EV_ERROR) != 0 {
                    bits |= ERRS;
                }
                let fd = e.ident;
                self.event(fd, bits, take);
            }
            if (n as usize) < self.evs.len() {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::{TcpListener, TcpStream};
    use std::time::Duration;

    fn registered(fd: RawFd) -> bool {
        REACTOR.with_borrow(|r| r.as_ref().and_then(|r| r.fds.get(fd as usize)).is_some_and(|e| e.registered))
    }

    /// The tokens the reactor reports once its fd is readable (at most a second later).
    fn wait_tokens() -> Vec<usize> {
        let mut p = [crate::os::pollfd(raw_fd().unwrap(), IN)];
        let _ = crate::os::poll(&mut p, 1000);
        let mut out = Vec::new();
        drain(&mut |t, _| {
            out.push(t);
            true
        });
        out
    }

    fn pair(l: &TcpListener) -> (TcpStream, TcpStream) {
        let c = TcpStream::connect(l.local_addr().unwrap()).unwrap();
        let (s, _) = l.accept().unwrap();
        c.set_nonblocking(true).unwrap();
        (c, s)
    }

    #[test]
    fn registered_once_and_events_kept_for_the_next_wait() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let (c, mut s) = pair(&l);
        let fd = c.as_raw_fd();
        let p = [crate::os::pollfd(fd, IN)];
        assert!(!arm(&p, 7, 1).unwrap());
        assert!(registered(fd));
        s.write_all(b"x").unwrap();
        assert_eq!(wait_tokens(), vec![7]);
        disarm(&p, 7);
        // data arriving while nobody waits: the next wait returns at once
        s.write_all(b"y").unwrap();
        let _ = wait_tokens();
        assert!(arm(&p, 8, 1).unwrap());
        // consumed: the wait after that is a real one
        assert!(!arm(&p, 8, 2).unwrap());
        disarm(&p, 8);
        forget(fd);
    }

    /// Every socket owner of the driver forgets its socket when it drops it: a socket that
    /// gets the number of a closed, registered one must be registered anew, or it never reports.
    #[test]
    fn closed_sockets_are_forgotten() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let armed = |fd: RawFd| {
            assert!(!arm(&[crate::os::pollfd(fd, IN)], 1, 1).unwrap());
            disarm(&[crate::os::pollfd(fd, IN)], 1);
            assert!(registered(fd));
            fd
        };
        // a blocking connection (net::Conn::connect)
        let c = crate::net::Conn::connect("127.0.0.1", addr.port(), Some(Duration::from_secs(5))).unwrap();
        let fd = armed(c.raw_fd());
        drop(c);
        assert!(!registered(fd), "Conn::connect");
        // a server check's connection (net::Conn::connect_nonblocking), also over TLS
        let c = crate::net::Conn::connect_nonblocking(addr, "x").unwrap();
        let fd = armed(c.raw_fd());
        drop(c);
        assert!(!registered(fd), "Conn::connect_nonblocking");
        let c = crate::net::Conn::connect_nonblocking(addr, "x").unwrap();
        let fd = armed(c.raw_fd());
        let config = std::sync::Arc::new(rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(rustls_graviola::default_provider())).with_safe_default_protocol_versions().unwrap().with_root_certificates(rustls::RootCertStore::empty()).with_no_client_auth());
        let c = c.tls_begin(config, rustls::pki_types::ServerName::try_from("localhost").unwrap()).unwrap();
        assert!(registered(fd));
        drop(c);
        assert!(!registered(fd), "Conn::tls_begin");
        // a UNIX domain socket connection (net::Conn::connect_unix)
        let dir = std::env::temp_dir().join(format!("mdrop-reactor-{}", std::process::id()));
        let _ = std::fs::remove_file(&dir);
        let ul = std::os::unix::net::UnixListener::bind(&dir).unwrap();
        let c = crate::net::Conn::connect_unix(dir.to_str().unwrap()).unwrap();
        let fd = armed(c.raw_fd());
        drop(c);
        assert!(!registered(fd), "Conn::connect_unix");
        drop(ul);
        let _ = std::fs::remove_file(&dir);
        // the KMS / HTTP sockets (aio::WaitStream; aio::connect wraps the socket before it waits)
        let w = crate::aio::connect(&addr, Duration::from_secs(5)).unwrap();
        let fd = armed(crate::os::raw(&w.sock));
        drop(w);
        assert!(!registered(fd), "aio::WaitStream");
    }

    /// A socket that reuses the number of a closed (and forgotten) one is registered anew and
    /// gets its events. The sockets are moved to a number above what other tests use, so the
    /// reuse is certain.
    #[test]
    fn reused_fd_numbers_are_registered_anew() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let high = |c: TcpStream| {
            // SAFETY: a new descriptor for the same socket, owned by the returned stream
            let fd = unsafe { libc::fcntl(c.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 900) };
            assert!(fd >= 900);
            unsafe { TcpStream::from_raw_fd(fd) }
        };
        let (c, _s) = pair(&l);
        let c = high(c);
        let old = c.as_raw_fd();
        assert!(!arm(&[crate::os::pollfd(old, IN)], 1, 1).unwrap());
        disarm(&[crate::os::pollfd(old, IN)], 1);
        forget(old);
        drop(c);
        let (c, mut s) = pair(&l);
        let c = high(c);
        assert_eq!(c.as_raw_fd(), old);
        let p = [crate::os::pollfd(old, IN)];
        assert!(!arm(&p, 2, 1).unwrap());
        s.write_all(b"x").unwrap();
        assert_eq!(wait_tokens(), vec![2]);
        disarm(&p, 2);
        forget(old);
    }
}
