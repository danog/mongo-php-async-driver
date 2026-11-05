//! Event-loop glue without threads: the sockets of in-flight asynchronous operations sit in one
//! epoll set (EPOLLONESHOT, data = operation token) with a timerfd for deadlines; PHP watches the
//! epoll fd through a socket stream with Revolt `EventLoop::onReadable(..., fn () =>
//! Client::wakeup())`. wakeup() drives the operations (client::on_event / on_timer: send, receive,
//! getMore, hand the connection to the next queued operation) and then resumes the fibers whose
//! operation finished. Operations progress whatever the fibers do: a fiber that is resumed but
//! not yet run (a scheduler may resume it lazily) holds no connection.
//!
//! Everything runs on the PHP thread. No RefCell borrow is held across a suspension or a PHP call.

use crate::client;
use crate::error::{Error, R};
use ext_php_rs::{
    boxed::ZBox,
    exception::throw_object,
    ffi,
    types::{ZendObject, Zval},
    zend::{ExecutorGlobals, Function},
};
use std::cell::{Cell, RefCell};
use std::ffi::{c_char, c_int, c_void};
use std::os::fd::RawFd;
use std::time::Instant;

pub const TIMER: u64 = u64::MAX;

struct Aio {
    epfd: RawFd,
    timerfd: RawFd,
    timer_at: Option<Instant>,
    get_suspension: Option<Function>,
}

thread_local! {
    static AIO: RefCell<Option<Aio>> = const { RefCell::new(None) };
    /// Incremented at the end of every request in which the driver ran (see `hook_request`).
    pub static GEN: Cell<u64> = const { Cell::new(1) };
    static HOOKED: Cell<bool> = const { Cell::new(false) };
    /// Suspensions of finished operations, resumed at the end of wakeup()/pump().
    static READY: RefCell<Vec<Zval>> = const { RefCell::new(Vec::new()) };
}

fn os_err(what: &str) -> Error {
    Error::Net(format!("{what}: {}", std::io::Error::last_os_error()))
}

fn with<T>(f: impl FnOnce(&mut Aio) -> R<T>) -> R<T> {
    AIO.with_borrow_mut(|a| {
        if a.is_none() {
            let epfd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
            if epfd < 0 {
                return Err(os_err("epoll_create1"));
            }
            let timerfd = unsafe { libc::timerfd_create(libc::CLOCK_MONOTONIC, libc::TFD_CLOEXEC | libc::TFD_NONBLOCK) };
            if timerfd < 0 {
                return Err(os_err("timerfd_create"));
            }
            let mut ev = libc::epoll_event { events: libc::EPOLLIN as u32, u64: TIMER };
            unsafe { libc::epoll_ctl(epfd, libc::EPOLL_CTL_ADD, timerfd, &mut ev) };
            *a = Some(Aio { epfd, timerfd, timer_at: None, get_suspension: None });
        }
        f(a.as_mut().unwrap())
    })
}

// ---- request lifecycle --------------------------------------------------------------------

extern "C" {
    fn zend_register_list_destructors_ex(
        ld: Option<extern "C" fn(*mut ffi::zend_resource)>,
        pld: Option<extern "C" fn(*mut ffi::zend_resource)>,
        type_name: *const c_char,
        module_number: c_int,
    ) -> c_int;
    fn zend_register_resource(rsrc_pointer: *mut c_void, rsrc_type: c_int) -> *mut ffi::zend_resource;
    fn php_file_le_stream() -> c_int;
    fn _php_stream_sock_open_from_socket(socket: c_int, persistent_id: *const c_char) -> *mut ffi::php_stream;
    fn zend_is_unwind_exit(ex: *const ffi::zend_object) -> bool;
    fn zend_is_graceful_exit(ex: *const ffi::zend_object) -> bool;
}

/// Runs at the end of the request (resources are destroyed after RSHUTDOWN, before objects).
extern "C" fn request_end(_: *mut ffi::zend_resource) {
    HOOKED.set(false);
    GEN.set(GEN.get() + 1);
    // operations still in flight: their connections are in an unknown state, closed
    client::abort_ops();
    READY.with_borrow_mut(|r| r.clear());
    AIO.with_borrow_mut(|a| {
        if let Some(a) = a {
            a.get_suspension = None;
            a.timer_at = None;
            disarm_timer(a);
        }
    });
}

/// Registers the end-of-request hook once per request: call before any operation.
pub fn hook_request() {
    if HOOKED.get() {
        return;
    }
    static TYPE: std::sync::OnceLock<c_int> = std::sync::OnceLock::new();
    let t = *TYPE.get_or_init(|| unsafe { zend_register_list_destructors_ex(Some(request_end), None, c"mongo-php-async-driver".as_ptr(), 0) });
    unsafe { zend_register_resource(std::ptr::null_mut(), t) };
    HOOKED.set(true);
}

/// A new PHP socket stream on a duplicate of the epoll fd, for Revolt's onReadable.
pub fn php_stream() -> R<Zval> {
    hook_request();
    let fd = with(|a| Ok(a.epfd))?;
    let dup = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
    if dup < 0 {
        return Err(os_err("dup"));
    }
    let stream = unsafe { _php_stream_sock_open_from_socket(dup, std::ptr::null()) };
    let mut z = Zval::new();
    unsafe {
        z.set_resource((*stream).res);
        (*stream).set___exposed(1);
    }
    let _ = php_file_le_stream;
    Ok(z)
}

// ---- epoll ----------------------------------------------------------------------------------

/// Registers `fd` (once per connection) in the epoll set, disarmed.
pub fn register(fd: RawFd) -> R<()> {
    with(|a| {
        let mut ev = libc::epoll_event { events: 0, u64: 0 };
        if unsafe { libc::epoll_ctl(a.epfd, libc::EPOLL_CTL_ADD, fd, &mut ev) } < 0 {
            return Err(os_err("epoll_ctl add"));
        }
        Ok(())
    })
}

/// Arms `fd` once for `events` (EPOLLIN/EPOLLOUT), reported with `token`.
pub fn arm(fd: RawFd, events: u32, token: u64) -> R<()> {
    with(|a| {
        let mut ev = libc::epoll_event { events: events | libc::EPOLLONESHOT as u32, u64: token };
        if unsafe { libc::epoll_ctl(a.epfd, libc::EPOLL_CTL_MOD, fd, &mut ev) } < 0 {
            return Err(os_err("epoll_ctl mod"));
        }
        Ok(())
    })
}

/// Makes sure the timer fires at or before `at`.
pub fn timer_at(at: Instant) {
    let _ = with(|a| {
        if a.timer_at.is_none_or(|t| at < t) {
            arm_timer(a, at);
        }
        Ok(())
    });
}

fn arm_timer(a: &mut Aio, at: Instant) {
    let d = at.saturating_duration_since(Instant::now());
    let spec = libc::itimerspec {
        it_interval: libc::timespec { tv_sec: 0, tv_nsec: 0 },
        it_value: libc::timespec { tv_sec: d.as_secs() as _, tv_nsec: (d.subsec_nanos().max(1000)) as _ },
    };
    unsafe { libc::timerfd_settime(a.timerfd, 0, &spec, std::ptr::null_mut()) };
    a.timer_at = Some(at);
}

fn disarm_timer(a: &mut Aio) {
    let spec = libc::itimerspec { it_interval: libc::timespec { tv_sec: 0, tv_nsec: 0 }, it_value: libc::timespec { tv_sec: 0, tv_nsec: 0 } };
    unsafe { libc::timerfd_settime(a.timerfd, 0, &spec, std::ptr::null_mut()) };
}

/// Waits up to `timeout_ms` (-1: forever) for events and dispatches them; true if any came.
fn poll(timeout_ms: c_int) -> bool {
    let Ok(epfd) = with(|a| Ok(a.epfd)) else { return false };
    let mut evs = [libc::epoll_event { events: 0, u64: 0 }; 64];
    let mut any = false;
    let mut t = timeout_ms;
    loop {
        let n = unsafe { libc::epoll_wait(epfd, evs.as_mut_ptr(), 64, t) };
        if n <= 0 {
            break;
        }
        any = true;
        t = 0;
        for ev in &evs[..n as usize] {
            if ev.u64 == TIMER {
                let next = with(|a| {
                    let mut buf = [0u8; 8];
                    unsafe { libc::read(a.timerfd, buf.as_mut_ptr() as *mut c_void, 8) };
                    a.timer_at = None;
                    Ok(())
                });
                if next.is_ok() {
                    if let Some(at) = client::on_timer() {
                        timer_at(at);
                    }
                }
            } else {
                client::on_event(ev.u64);
            }
        }
        if n < 64 {
            break;
        }
    }
    any
}

// ---- fibers ---------------------------------------------------------------------------------

pub fn get_suspension() -> R<Zval> {
    let f = with(|a| {
        if a.get_suspension.is_none() {
            a.get_suspension = Some(
                Function::try_from_method("\\Revolt\\EventLoop", "getSuspension")
                    .ok_or_else(|| Error::Invalid("async mode needs revolt/event-loop".into()))?,
            );
        }
        Ok(a.get_suspension.as_ref().unwrap().try_call(vec![]).map_err(|e| Error::Invalid(format!("getSuspension: {e}"))))
    })?;
    f
}

fn exit_pending() -> bool {
    let ex = ExecutorGlobals::get().exception;
    !ex.is_null() && unsafe { zend_is_unwind_exit(ex) || zend_is_graceful_exit(ex) }
}

/// Suspends the current fiber on `suspension`; Err(Interrupted) if an exception or exit is
/// pending when it comes back.
pub fn suspend(suspension: &Zval) -> R<()> {
    let _ = suspension.try_call_method("suspend", vec![]);
    if exit_pending() {
        return Err(Error::Interrupted);
    }
    if let Some(ex) = ExecutorGlobals::take_exception() {
        rethrow(ex);
        return Err(Error::Interrupted);
    }
    Ok(())
}

fn rethrow(ex: ZBox<ZendObject>) {
    let mut z = Zval::new();
    let mut ex = ex;
    z.set_object(&mut ex);
    let _ = throw_object(z);
}

/// Queues a finished operation's suspension for resumption.
pub fn ready(s: Zval) {
    READY.with_borrow_mut(|r| r.push(s));
}

/// Revolt callback: drives the operations whose socket is ready or whose deadline passed, then
/// resumes the fibers whose operation finished.
pub fn wakeup() {
    poll(0);
    resume_ready();
}

/// Blocks the PHP thread until an event arrives (at most `timeout_ms`) and dispatches it: used by
/// a blocking caller waiting for a connection that asynchronous operations hold.
pub fn pump(timeout_ms: c_int) -> bool {
    let any = poll(timeout_ms);
    resume_ready();
    any
}

fn resume_ready() {
    let ready = READY.with_borrow_mut(std::mem::take);
    let mut first = None;
    for s in ready {
        let _ = s.try_call_method("resume", vec![]);
        if let Some(ex) = ExecutorGlobals::take_exception() {
            first.get_or_insert(ex);
        }
    }
    if let Some(ex) = first {
        rethrow(ex);
    }
}
