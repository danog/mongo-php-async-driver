//! The event-loop backend of mongo_core::aio: inside a PHP Fiber, a driver call that waits for
//! the network suspends its fiber through Revolt (revolt/event-loop, a userland library looked
//! up by name, never a dependency of the extension), so other fibers run meanwhile.
//!
//! - When: a call inside a fiber (EG(active_fiber)) waits by suspending; a call in {main} does
//!   so only while fibers are waiting in the driver (then Revolt's {main} suspension runs the
//!   loop and they progress); otherwise, and wherever a fiber may not switch (object
//!   destruction, shutdown, zend_fiber_switch_blocked()), the plain blocking waits are used.
//! - How (Unix, backend 1): one epoll/kqueue instance per PHP thread (mongo_core::reactor:
//!   sockets registered once, edge-triggered) reports the sockets being waited for, token =
//!   waiter slot; its fd (a dup) is a PHP socket stream watched by one Revolt onReadable
//!   callback, referenced only while waits are pending. A wait names its slot as the waiter of
//!   its sockets, stores `EventLoop::getSuspension()` in the slot and suspends; the callback
//!   drains the reactor and resumes the ready slots.
//! - The watcher and the timer belong to the driver of the first wait; a wait after
//!   EventLoop::setDriver() moves them to the new driver.
//! - How (Windows, backend 2: IOCP has no handle a PHP stream could wrap): a wait wraps each of
//!   its sockets in a PHP socket stream (whose socket handle is reset before the stream goes,
//!   so PHP never shuts down or closes the driver's socket) and watches it with
//!   onReadable/onWritable; the callback resumes the watcher's slot; the wait cancels its
//!   watchers when it ends. The loop is kept alive while waits are pending by a referenced
//!   watcher on a UDP socket nobody writes to (backend 1's watcher, without readiness).
//! - Deadlines: one unreferenced Revolt delay() for the earliest one (a sleep is a wait without
//!   sockets).
//! - The callbacks are Closures over a static internal function (zend_create_fake_closure):
//!   nothing new is visible to Reflection.
//! - Never across a PHP call: a borrow of the state here (PHP may re-enter).

use crate::zend::{call, sys, zval};
use std::cell::{Cell, RefCell};
use std::ffi::c_char;
use std::io;
use std::ptr::{addr_of, null_mut};
use std::time::{Duration, Instant};

extern "C" {
    fn zend_fiber_switch_blocked() -> bool;
    fn zend_create_fake_closure(res: *mut sys::zval, func: *mut sys::zend_function, scope: *mut sys::zend_class_entry, called_scope: *mut sys::zend_class_entry, this_ptr: *mut sys::zval);
    fn zend_internal_run_time_cache_reserved_size() -> usize;
    fn _php_stream_sock_open_from_socket(socket: mongo_core::os::RawSock, persistent_id: *const c_char) -> *mut sys::php_stream;
    fn zend_read_static_property_ex(scope: *mut sys::zend_class_entry, name: *mut sys::zend_string, silent: bool) -> *mut sys::zval;
    fn zend_weakrefs_hash_add(ht: *mut sys::HashTable, key: *mut sys::zend_object, data: *mut sys::zval) -> *mut sys::zval;
    fn zend_weakrefs_hash_del(ht: *mut sys::HashTable, key: *mut sys::zend_object) -> sys::zend_result;
}

/// A weak reference to an object, without holding it: a table whose one weak key is the object
/// (zend_weakrefs_hash_add), which the engine empties when the object is freed.
struct WeakObj {
    obj: *mut sys::zend_object,
    ht: *mut sys::HashTable,
}

impl WeakObj {
    unsafe fn new(obj: *mut sys::zend_object) -> WeakObj {
        let ht = crate::zend::hash::new(1);
        let mut v = zval::null();
        zend_weakrefs_hash_add(ht, obj, &mut v);
        WeakObj { obj, ht }
    }

    /// The object; null once it was freed.
    fn get(&self) -> *mut sys::zend_object {
        if unsafe { crate::zend::hash::count(self.ht) } == 1 {
            self.obj
        } else {
            null_mut()
        }
    }
}

impl Drop for WeakObj {
    fn drop(&mut self) {
        unsafe {
            if !self.get().is_null() {
                zend_weakrefs_hash_del(self.ht, self.obj);
            }
            crate::zend::hash::release(self.ht);
        }
    }
}

#[cfg(not(php86))]
extern "C" {
    fn zend_exception_save();
    fn zend_exception_restore();
    fn zend_call_known_function(func: *mut sys::zend_function, object: *mut sys::zend_object, called_scope: *mut sys::zend_class_entry, retval: *mut sys::zval, param_count: u32, params: *mut sys::zval, named_params: *mut sys::HashTable);
}

// PHP 8.6: zend_call_known_function is an inline wrapper of zend_call_known_function_ex (with
// consumed_args 0), and zend_exception_save/restore (EG(prev_exception)) are gone
#[cfg(php86)]
extern "C" {
    fn zend_call_known_function_ex(func: *mut sys::zend_function, object: *mut sys::zend_object, called_scope: *mut sys::zend_class_entry, retval: *mut sys::zval, param_count: u32, params: *mut sys::zval, named_params: *mut sys::HashTable, consumed_args: u32);
    fn zend_exception_set_previous(exception: *mut sys::zend_object, add_previous: *mut sys::zend_object);
}

#[cfg(php86)]
#[inline]
unsafe fn zend_call_known_function(func: *mut sys::zend_function, object: *mut sys::zend_object, called_scope: *mut sys::zend_class_entry, retval: *mut sys::zval, param_count: u32, params: *mut sys::zval, named_params: *mut sys::HashTable) {
    zend_call_known_function_ex(func, object, called_scope, retval, param_count, params, named_params, 0)
}

thread_local! {
    /// Object destruction and shutdown in progress: waits block.
    static FORBID: Cell<u32> = const { Cell::new(0) };
    /// revolt/event-loop in this request: 0 not looked up, 1 present, 2 missing (autoloading it
    /// in a fiber failed).
    static REVOLT: Cell<u8> = const { Cell::new(0) };
}

pub static BACKEND: mongo_core::aio::Backend = mongo_core::aio::Backend { active, poll, sleep, require, park, unpark, current };

const NO_LOOP: &str = "MongoDB driver calls inside a Fiber need the revolt/event-loop package: the class Revolt\\EventLoop was not found";

#[inline]
fn in_fiber() -> bool {
    // SAFETY: the executor globals of the calling PHP thread
    unsafe { !(*crate::zend::eg()).active_fiber.is_null() }
}

/// EG_FLAGS_IN_SHUTDOWN
fn shutting_down() -> bool {
    unsafe { (*crate::zend::eg()).flags & 1 != 0 }
}

fn revolt() -> bool {
    match REVOLT.get() {
        1 => true,
        2 => false,
        _ => {
            // autoloaded when needed (composer)
            let name = unsafe { zval::string(b"Revolt\\EventLoop") };
            let ce = unsafe { sys::zend_lookup_class_ex(name.value.str_, null_mut(), 0) };
            let ok = !ce.is_null() && !unsafe { call::has_exception() };
            if ok {
                found_revolt(ce);
            }
            REVOLT.set(if ok { 1 } else { 2 });
            ok
        }
    }
}

/// The class-table size (the counter of mongo_core::aio's synchronous fast path).
fn class_count() -> u32 {
    unsafe { (*(*crate::zend::eg()).class_table).nNumOfElements }
}

/// mongodb.main_event_loop (PHP's INI table: per thread under ZTS).
fn main_event_loop() -> bool {
    // orig: false (an int before PHP 8.6, a bool since)
    unsafe { sys::zend_ini_long(c"mongodb.main_event_loop".as_ptr(), 23, Default::default()) != 0 }
}

/// mongodb.main_event_loop was changed: the remembered "no event loop in {main}" no longer holds.
pub fn main_event_loop_changed() {
    mongo_core::aio::set_no_loop_at(u32::MAX);
}

/// Whether {main}'s waits go through Revolt: mongodb.main_event_loop is On and
/// Revolt\EventLoop is loaded (declared; {main} never autoloads it). A miss is remembered
/// until a class is added (or the setting changes).
fn revolt_loaded() -> bool {
    if !main_event_loop() {
        // the synchronous fast path holds until a class is added or the setting changes
        publish_sync_words();
        mongo_core::aio::set_no_loop_at(class_count());
        return false;
    }
    if REVOLT.get() == 1 {
        return true;
    }
    let count = class_count();
    if count == mongo_core::aio::no_loop_at() {
        return false;
    }
    publish_sync_words();
    let name = unsafe { zval::string(b"Revolt\\EventLoop") };
    let ce = unsafe { sys::zend_lookup_class_ex(name.value.str_, null_mut(), ZEND_FETCH_CLASS_NO_AUTOLOAD) };
    if ce.is_null() {
        mongo_core::aio::set_no_loop_at(count);
        return false;
    }
    found_revolt(ce);
    REVOLT.set(1);
    mongo_core::aio::set_no_loop_at(u32::MAX);
    true
}

/// EG(active_fiber) and the class-table size of this PHP thread, for mongo_core::aio's
/// synchronous fast path (EG(class_table) is set per request: published here, in a request,
/// never at MINIT, and withdrawn at RSHUTDOWN).
fn publish_sync_words() {
    unsafe {
        let eg = crate::zend::eg();
        mongo_core::aio::set_sync_words(addr_of!((*eg).active_fiber).cast(), addr_of!((*(*eg).class_table).nNumOfElements));
    }
}

/// ZEND_FETCH_CLASS_NO_AUTOLOAD
const ZEND_FETCH_CLASS_NO_AUTOLOAD: u32 = 0x80;

/// Whether this call's waits suspend: inside a fiber (Revolt autoloaded if needed: a fiber
/// without it fails in `require`), and in {main} whenever Revolt is loaded (its {main}
/// suspension runs the loop meanwhile); never where PHP forbids fiber switches, in object
/// destruction or at shutdown.
fn active() -> bool {
    if FORBID.get() > 0 || shutting_down() || unsafe { zend_fiber_switch_blocked() } {
        return false;
    }
    if in_fiber() {
        revolt()
    } else {
        revolt_loaded()
    }
}

fn current() -> usize {
    unsafe { (*crate::zend::eg()).active_fiber as usize }
}

fn require() -> Result<(), &'static str> {
    if FORBID.get() == 0 && in_fiber() && !revolt() {
        return Err(NO_LOOP);
    }
    Ok(())
}

/// Runs `f` with suspension forbidden (object free handlers, GC, shutdown): its waits block.
pub fn blocking<R>(f: impl FnOnce() -> R) -> R {
    struct G;
    impl Drop for G {
        fn drop(&mut self) {
            FORBID.set(FORBID.get() - 1);
        }
    }
    FORBID.set(FORBID.get() + 1);
    let _g = G;
    f()
}

// ------------------------------------------------------------------ state

#[derive(PartialEq, Clone, Copy)]
enum SlotState {
    Free,
    Pending,
    Ready,
    TimedOut,
}

struct Slot {
    /// bumped at every reuse (park ids name one use of a slot)
    gen: u32,
    state: SlotState,
    suspension: Option<sys::zval>,
    deadline: Option<Instant>,
}

struct Loop {
    /// Windows: the Revolt watchers of the waits (watcher id, slot) and the streams wrapping their
    /// sockets (slot, stream)
    #[cfg(windows)]
    watches: Vec<(sys::zval, usize)>,
    #[cfg(windows)]
    streams: Vec<(usize, *mut sys::php_stream)>,
    slots: Vec<Slot>,
    pending: usize,
    /// request-level PHP state (reset at request end)
    php: Option<Php>,
}

/// The PHP side of the loop in this request, on one Revolt driver.
struct Php {
    /// the driver the watcher and the timer belong to (EventLoop::setDriver() may replace it
    /// between waits: the next wait then moves to the new one); not held from here
    driver: WeakObj,
    /// the onReadable callback id, and whether it is referenced
    watcher: sys::zval,
    referenced: bool,
    /// the deadline timer: its id and due time
    timer: Option<(sys::zval, Instant)>,
    /// Windows: the keep-alive watcher's UDP socket (PHP's stream owns it)
    #[cfg(windows)]
    keepalive: mongo_core::os::RawSock,
}

thread_local! {
    static LOOP: RefCell<Option<Loop>> = const { RefCell::new(None) };
}

fn with<R>(f: impl FnOnce(&mut Loop) -> R) -> io::Result<R> {
    LOOP.with_borrow_mut(|l| {
        if l.is_none() {
            *l = Some(Loop {
                #[cfg(windows)]
                watches: Vec::new(),
                #[cfg(windows)]
                streams: Vec::new(),
                slots: Vec::new(),
                pending: 0,
                php: None,
            });
        }
        Ok(f(l.as_mut().unwrap()))
    })
}

// ------------------------------------------------------------------ PHP calls

/// The functions of the Revolt calls, resolved once per request (a class cannot change within
/// one): Revolt\EventLoop's static methods, and the methods of the class of the suspensions.
#[derive(Default)]
struct Fns {
    event_loop: Option<std::ptr::NonNull<sys::zend_class_entry>>,
    statics: Vec<(&'static str, *mut sys::zend_function)>,
    suspension: Option<std::ptr::NonNull<sys::zend_class_entry>>,
    methods: Vec<(&'static [u8], *mut sys::zend_function)>,
    /// the slot of the static property EventLoop::$driver (None: not looked up yet; null: no
    /// such property: EventLoop::getDriver() tells the driver instead)
    driver_slot: Option<*mut sys::zval>,
}

thread_local! {
    static FNS: RefCell<Fns> = RefCell::new(Fns::default());
}

/// Revolt\EventLoop was found: its class, for the calls of this request.
fn found_revolt(ce: *mut sys::zend_class_entry) {
    FNS.with_borrow_mut(|f| {
        if f.event_loop.map(|p| p.as_ptr()) != Some(ce) {
            *f = Fns { event_loop: std::ptr::NonNull::new(ce), ..Fns::default() };
        }
    });
}

/// The public method `name` (lower case) of `ce`, static or not as `want_static` says; null when
/// there is none such (the call then goes by name, as PHP resolves it).
unsafe fn find_method(ce: *mut sys::zend_class_entry, name: &[u8], want_static: bool) -> *mut sys::zend_function {
    let z = sys::zend_hash_str_find(&(*ce).function_table, name.as_ptr().cast(), name.len());
    if z.is_null() {
        return null_mut();
    }
    let f = (*z).value.ptr as *mut sys::zend_function;
    let flags = (*f).common.fn_flags;
    if flags & sys::ZEND_ACC_PUBLIC == 0 || (flags & sys::ZEND_ACC_STATIC != 0) != want_static || flags & sys::ZEND_ACC_ABSTRACT != 0 {
        return null_mut();
    }
    f
}

/// Revolt\EventLoop::<method>(args...)
unsafe fn event_loop(method: &'static str, args: &mut [sys::zval]) -> sys::zval {
    let mut rv = zval::undef();
    let known = FNS.with_borrow_mut(|f| {
        let ce = f.event_loop?.as_ptr();
        let func = match f.statics.iter().find(|(m, _)| *m == method) {
            Some(&(_, func)) => func,
            None => {
                let func = find_method(ce, method.to_ascii_lowercase().as_bytes(), true);
                f.statics.push((method, func));
                func
            }
        };
        (!func.is_null()).then_some((func, ce))
    });
    match known {
        Some((func, ce)) => zend_call_known_function(func, null_mut(), ce, &mut rv, args.len() as u32, args.as_mut_ptr(), null_mut()),
        None => {
            let mut func = zval::string(format!("Revolt\\EventLoop::{method}").as_bytes());
            sys::_call_user_function_impl(null_mut(), &mut func, &mut rv, args.len() as u32, args.as_mut_ptr(), null_mut());
        }
    }
    rv
}

/// The current Revolt driver (null while there is none yet): the value of EventLoop::$driver
/// (revolt/event-loop 1.x keeps it there: its slot is looked up once per request, a static
/// member table lives as long as the request), else what EventLoop::getDriver() returns. The
/// object is held by EventLoop for as long as it is the driver.
unsafe fn driver_now() -> *mut sys::zend_object {
    let (ce, slot) = FNS.with_borrow(|f| (f.event_loop, f.driver_slot));
    let slot = match (ce, slot) {
        (_, Some(slot)) => slot,
        (Some(ce), None) => {
            let name = zval::string(b"driver");
            // silent: no error for an uninitialised typed property (EventLoop sets it on first use)
            let slot = zend_read_static_property_ex(ce.as_ptr(), name.value.str_, true);
            FNS.with_borrow_mut(|f| f.driver_slot = Some(slot));
            slot
        }
        (None, None) => null_mut(),
    };
    if !slot.is_null() {
        let z = zval::deref(slot);
        if zval::is(z, sys::IS_OBJECT) {
            return (*z).value.obj;
        }
        if zval::is(z, sys::IS_UNDEF) {
            return null_mut();
        }
    }
    let d = event_loop("getDriver", &mut []);
    if zval::is(&d, sys::IS_OBJECT) {
        d.value.obj
    } else {
        null_mut()
    }
}

/// `$o-><name>(args...)` (`name` in lower case).
unsafe fn method(o: &sys::zval, name: &'static [u8], args: &mut [sys::zval]) -> sys::zval {
    let obj = o.value.obj;
    let ce = (*obj).ce;
    let func = FNS.with_borrow_mut(|f| {
        if f.suspension.map(|p| p.as_ptr()) != Some(ce) {
            f.suspension = std::ptr::NonNull::new(ce);
            f.methods.clear();
        }
        match f.methods.iter().find(|(m, _)| *m == name) {
            Some(&(_, func)) => func,
            None => {
                let func = find_method(ce, name, false);
                f.methods.push((name, func));
                func
            }
        }
    });
    if func.is_null() {
        return call::method_n(obj, name, args);
    }
    let mut rv = zval::undef();
    zend_call_known_function(func, obj, ce, &mut rv, args.len() as u32, args.as_mut_ptr(), null_mut());
    rv
}

/// The internal functions behind the Closures. Their name is a permanent interned string made at
/// MINIT; the functions themselves are per thread: an internal function's run-time cache (where
/// observers keep their data) must not be shared by the threads of a thread-safe PHP.
struct Closures {
    ready: sys::zend_internal_function,
    timer: sys::zend_internal_function,
    /// Windows: a socket watcher's callback
    #[cfg(windows)]
    socket: sys::zend_internal_function,
    /// the run-time caches the functions point to
    _caches: Vec<Box<[u8]>>,
}

/// The functions' name ("{closure}", interned at MINIT).
static mut CLOSURE_NAME: *mut sys::zend_string = null_mut();

thread_local! {
    static FUNCS: std::cell::OnceCell<Box<std::cell::UnsafeCell<Closures>>> = const { std::cell::OnceCell::new() };
}

/// MINIT: the name of the callbacks' functions; installs the backend.
pub unsafe fn minit() {
    CLOSURE_NAME = crate::zend::class::interned(c"{closure}");
    mongo_core::aio::set_backend(Some(&BACKEND));
}

/// This thread's callback functions (made on first use; they live as long as the thread). An
/// engine restarted in the same process (MSHUTDOWN, then MINIT again: FrankenPHP) interns their
/// name anew.
unsafe fn funcs() -> *mut Closures {
    let c = FUNCS.with(|f| {
        f.get_or_init(|| {
            let mut caches = Vec::new();
            let mut mk = |h: sys::zif_handler| {
                let mut f = std::mem::zeroed::<sys::zend_internal_function>();
                f.type_ = 1; // ZEND_INTERNAL_FUNCTION
                f.fn_flags = sys::ZEND_ACC_PUBLIC;
                f.function_name = CLOSURE_NAME;
                f.handler = h;
                // the run-time cache an internal function declared at run time gets (observers use it)
                let mut cache = vec![0u8; zend_internal_run_time_cache_reserved_size().max(8)].into_boxed_slice();
                f.run_time_cache__ptr = cache.as_mut_ptr().cast();
                caches.push(cache);
                f
            };
            let ready = mk(Some(crate::zh!(on_ready)));
            let timer = mk(Some(crate::zh!(on_timer)));
            #[cfg(windows)]
            let socket = mk(Some(crate::zh!(on_socket)));
            Box::new(std::cell::UnsafeCell::new(Closures {
                ready,
                timer,
                #[cfg(windows)]
                socket,
                _caches: caches,
            }))
        })
        .get()
    });
    if (*c).ready.function_name != CLOSURE_NAME {
        (*c).ready.function_name = CLOSURE_NAME;
        (*c).timer.function_name = CLOSURE_NAME;
        #[cfg(windows)]
        {
            (*c).socket.function_name = CLOSURE_NAME;
        }
    }
    c
}

unsafe fn fake_closure(f: *mut sys::zend_internal_function) -> sys::zval {
    let mut z = zval::undef();
    zend_create_fake_closure(&mut z, f.cast(), null_mut(), null_mut(), null_mut());
    z
}


/// The PHP socket stream of the request's keep-alive watcher: a dup of the reactor's fd
/// (readable when a waited-for socket may be ready).
#[cfg(unix)]
unsafe fn keepalive_stream() -> io::Result<*mut sys::php_stream> {
    let fd = mongo_core::reactor::raw_fd()?;
    let dup = libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0);
    if dup < 0 {
        return Err(io::Error::last_os_error());
    }
    let stream = _php_stream_sock_open_from_socket(dup, std::ptr::null());
    if stream.is_null() {
        libc::close(dup);
        return Err(io::Error::other("cannot watch the event fd"));
    }
    Ok(stream)
}

/// The PHP socket stream of the request's keep-alive watcher: a UDP socket bound to the
/// loopback interface that nothing writes to (whatever arrives is drained), and its handle.
#[cfg(windows)]
unsafe fn keepalive_stream() -> io::Result<(*mut sys::php_stream, mongo_core::os::RawSock)> {
    use std::os::windows::io::IntoRawSocket;
    let udp = std::net::UdpSocket::bind("127.0.0.1:0")?;
    udp.set_nonblocking(true)?;
    let raw = udp.into_raw_socket() as mongo_core::os::RawSock;
    let stream = _php_stream_sock_open_from_socket(raw, std::ptr::null());
    if stream.is_null() {
        use std::os::windows::io::FromRawSocket;
        drop(std::net::UdpSocket::from_raw_socket(raw as u64));
        return Err(io::Error::other("cannot watch the keep-alive socket"));
    }
    Ok((stream, raw))
}

/// The request's keep-alive watcher (created unreferenced on first use, on the current driver):
/// referenced while waits are pending, so that the loop runs until they end.
unsafe fn php_state() -> io::Result<()> {
    let now = driver_now();
    if call::has_exception() {
        return Err(mongo_core::aio::interrupted());
    }
    // the watcher's driver is no longer the current one (EventLoop::setDriver()): it moves
    let old = with(|l| match &l.php {
        Some(p) if !now.is_null() && p.driver.get() != now => Some(l.php.take()),
        Some(_) => None,
        None => Some(None),
    })?;
    match old {
        None => return Ok(()),
        Some(Some(old)) => retire(old),
        Some(None) => {}
    }
    #[cfg(unix)]
    let stream = keepalive_stream()?;
    #[cfg(windows)]
    let (stream, keepalive) = keepalive_stream()?;
    // php_stream_to_zval
    let mut zs = zval::undef();
    ext_php_rs::types::Zval::set_resource(&mut zs, (*stream).res);
    (*stream).set___exposed(1);
    let cb = fake_closure(&mut (*funcs()).ready);
    let mut args = [zs, cb];
    let id = event_loop("onReadable", &mut args);
    if call::has_exception() || !zval::is(&id, sys::IS_STRING) {
        return Err(mongo_core::aio::interrupted());
    }
    let mut a = [zval::copy(&id)];
    event_loop("unreference", &mut a);
    // the driver that has the watcher (onReadable made it if there was none)
    let driver = driver_now();
    if call::has_exception() || driver.is_null() {
        return Err(mongo_core::aio::interrupted());
    }
    let driver = WeakObj::new(driver);
    with(|l| {
        l.php = Some(Php {
            driver,
            watcher: id,
            referenced: false,
            timer: None,
            #[cfg(windows)]
            keepalive,
        })
    })?;
    Ok(())
}

/// The PHP state of a driver that is no longer EventLoop's (EventLoop::setDriver(), allowed only
/// while that driver does not run; it usually frees the old driver, and the watcher and timer
/// with it): if it is still alive, they are cancelled on it. Waits still
/// pending on it (fibers suspended on a loop that is gone) stay pending.
unsafe fn retire(old: Php) {
    let d = old.driver.get();
    if !d.is_null() {
        // still alive (held by someone): what is ours on it goes
        let ids = [Some(&old.watcher), old.timer.as_ref().map(|t| &t.0)];
        for id in ids.into_iter().flatten() {
            call::method_n(d, b"cancel", &mut [zval::copy(id)]);
        }
    }
    drop(old);
}

/// RSHUTDOWN: the request's PHP state goes (waiters left behind belong to destroyed fibers).
pub fn rshutdown() {
    // the PHP values are dropped after the borrow ends: dropping a suspension may destroy its
    // fiber, whose unwinding wait re-enters this state
    let mut drop_later: Vec<sys::zval> = Vec::new();
    let mut php = None;
    let _ = LOOP.try_with(|l| {
        if let Ok(mut l) = l.try_borrow_mut() {
            if let Some(l) = l.as_mut() {
                // waits left behind belong to fibers already destroyed: their slots are freed
                for s in &mut l.slots {
                    drop_later.extend(s.suspension.take());
                    if s.state != SlotState::Free {
                        *s = Slot { gen: s.gen.wrapping_add(1), state: SlotState::Free, suspension: None, deadline: None };
                    }
                }
                l.pending = 0;
                php = l.php.take();
                #[cfg(windows)]
                {
                    // streams of waits that never ended: PHP closes them with the request's
                    // resources, never their (the driver's) sockets
                    for (_, st) in l.streams.drain(..) {
                        unsafe { disown(st) };
                    }
                    drop_later.extend(l.watches.drain(..).map(|(id, _)| id));
                }
            }
        }
    });
    drop(drop_later);
    drop(php);
    REVOLT.set(0);
    FNS.with_borrow_mut(|f| *f = Fns::default());
    mongo_core::aio::set_no_loop_at(u32::MAX);
    mongo_core::aio::set_sync_words(std::ptr::null(), std::ptr::null());
}

// ------------------------------------------------------------------ waits

fn exit_pending() -> bool {
    unsafe { call::has_exception() }
}

fn poll(fds: &mut [mongo_core::os::PollFd], deadline: Option<Instant>) -> io::Result<usize> {
    wait(fds, deadline, None)
}

fn park(enqueue: &mut dyn FnMut(u64), deadline: Option<Instant>) -> io::Result<bool> {
    wait(&mut [], deadline, Some(enqueue)).map(|n| n > 0)
}

fn unpark(id: u64) {
    let (t, gen) = ((id & 0xffff_ffff) as usize, (id >> 32) as u32);
    let s = LOOP.with_borrow_mut(|l| {
        let s = l.as_mut()?.slots.get_mut(t).filter(|s| s.gen == gen && s.state == SlotState::Pending)?;
        let z = s.suspension.take()?;
        s.state = SlotState::Ready;
        Some(z)
    });
    if let Some(s) = s {
        unsafe { resume_all(vec![s]) };
    }
}

/// The wait of a poll (sockets), a sleep (none) or a park (none; `enqueue` gets its id).
fn wait(fds: &mut [mongo_core::os::PollFd], deadline: Option<Instant>, enqueue: Option<&mut dyn FnMut(u64)>) -> io::Result<usize> {
    if exit_pending() {
        return Err(mongo_core::aio::interrupted());
    }
    unsafe { php_state()? };
    // a slot, named as the waiter of the sockets
    let token = with(|l| -> io::Result<Option<usize>> {
        let t = match l.slots.iter().position(|s| s.state == SlotState::Free) {
            Some(t) => t,
            None => {
                l.slots.push(Slot { gen: 0, state: SlotState::Free, suspension: None, deadline: None });
                l.slots.len() - 1
            }
        };
        let gen = l.slots[t].gen.wrapping_add(1);
        #[cfg(unix)]
        if mongo_core::reactor::arm(fds, t, gen)? {
            // a readiness the reactor already saw: no suspension
            return Ok(None);
        }
        l.slots[t] = Slot { gen, state: SlotState::Pending, suspension: None, deadline };
        l.pending += 1;
        Ok(Some(t))
    })??;
    let Some(token) = token else {
        if fds.len() == 1 {
            // the caller retries its I/O, which tells the rest
            fds[0].revents = fds[0].events;
            return Ok(1);
        }
        let n = mongo_core::net::poll_fds(fds, Some(Instant::now()))?;
        if n > 0 {
            return Ok(n);
        }
        // what the reactor saw was consumed meanwhile: a real wait (it now finds nothing known)
        return wait(fds, deadline, enqueue);
    };
    let my_gen = with(|l| l.slots[token].gen)?;
    #[cfg(windows)]
    let streams = match unsafe { watch(token, fds) } {
        Ok(s) => s,
        Err(e) => {
            let _ = with(|l| {
                if l.slots[token].gen == my_gen {
                    l.slots[token] = Slot { gen: my_gen, state: SlotState::Free, suspension: None, deadline: None };
                    l.pending -= 1;
                }
            });
            unsafe { after_wait()? };
            return Err(e);
        }
    };
    if let Some(enqueue) = enqueue {
        enqueue((token as u64) | ((my_gen as u64) << 32));
    }
    let r = unsafe { suspend(token, deadline) };
    #[cfg(windows)]
    unsafe {
        unwatch(token, my_gen, streams)
    };
    #[cfg(unix)]
    mongo_core::reactor::disarm(fds, token);
    let state = with(|l| {
        // the slot is still this wait's unless the request ended meanwhile (rshutdown)
        if l.slots[token].gen != my_gen {
            return SlotState::Pending;
        }
        let s = std::mem::replace(&mut l.slots[token], Slot { gen: my_gen, state: SlotState::Free, suspension: None, deadline: None });
        l.pending -= 1;
        s.state
    })?;
    unsafe { after_wait()? };
    r?;
    match state {
        SlotState::Ready => {
            if fds.is_empty() {
                // unparked
                Ok(1)
            } else if fds.len() == 1 {
                // the caller retries its I/O, which tells the rest
                fds[0].revents = fds[0].events;
                Ok(1)
            } else {
                mongo_core::net::poll_fds(fds, Some(Instant::now()))
            }
        }
        _ => Ok(0),
    }
}

/// Windows: the watchers of a wait's sockets: one PHP socket stream per socket, watched with
/// onReadable / onWritable for its events (callback: `on_socket`). Returns the streams (each
/// zval holds the stream's first reference).
#[cfg(windows)]
unsafe fn watch(token: usize, fds: &[mongo_core::os::PollFd]) -> io::Result<Vec<(sys::zval, *mut sys::php_stream)>> {
    let mut streams: Vec<(sys::zval, *mut sys::php_stream)> = Vec::with_capacity(fds.len());
    let fail = |streams: Vec<(sys::zval, *mut sys::php_stream)>, e: io::Error| {
        let gen = with(|l| l.slots[token].gen).unwrap_or(0);
        unwatch(token, gen, streams);
        Err(e)
    };
    for p in fds {
        let stream = _php_stream_sock_open_from_socket(p.fd, std::ptr::null());
        if stream.is_null() {
            return fail(streams, io::Error::other("cannot watch the socket"));
        }
        // php_stream_to_zval
        let mut zs = zval::undef();
        ext_php_rs::types::Zval::set_resource(&mut zs, (*stream).res);
        (*stream).set___exposed(1);
        let _ = with(|l| l.streams.push((token, stream)));
        streams.push((zs, stream));
        for (bit, method) in [(mongo_core::os::POLLIN, "onReadable"), (mongo_core::os::POLLOUT, "onWritable")] {
            if p.events & bit == 0 {
                continue;
            }
            let mut args = [zval::copy(&streams.last().unwrap().0), fake_closure(&mut (*funcs()).socket)];
            let id = event_loop(method, &mut args);
            if call::has_exception() || !zval::is(&id, sys::IS_STRING) {
                return fail(streams, mongo_core::aio::interrupted());
            }
            let _ = with(|l| l.watches.push((id, token)));
        }
    }
    Ok(streams)
}

/// Windows: a stream no longer owns the driver's socket: its handle is reset, so freeing the
/// stream neither shuts the socket down nor closes it (php_netstream_data_t starts with it).
#[cfg(windows)]
unsafe fn disown(stream: *mut sys::php_stream) {
    let data = (*stream).abstract_ as *mut mongo_core::os::RawSock;
    if !data.is_null() {
        // SOCK_ERR (INVALID_SOCKET)
        *data = !0;
    }
}

/// Windows: the end of a wait: its watchers are cancelled, its streams disowned and released.
/// After the request ended (`rshutdown` took the watchers and disowned the streams), the
/// streams' references are left alone: the request's resources are gone.
#[cfg(windows)]
unsafe fn unwatch(token: usize, gen: u32, streams: Vec<(sys::zval, *mut sys::php_stream)>) {
    let ids = with(|l| {
        if l.slots.get(token).is_none_or(|s| s.gen != gen) {
            return None;
        }
        let mut ids = Vec::new();
        let mut i = 0;
        while i < l.watches.len() {
            if l.watches[i].1 == token {
                ids.push(l.watches.swap_remove(i).0);
            } else {
                i += 1;
            }
        }
        l.streams.retain(|(t, _)| *t != token);
        Some(ids)
    });
    let Ok(Some(ids)) = ids else {
        std::mem::forget(streams);
        return;
    };
    saving_exception(|| {
        for id in ids {
            event_loop("cancel", &mut [id]);
        }
    });
    for (zs, stream) in streams {
        disown(stream);
        drop(zs);
    }
}

/// Windows: a socket watcher's callback (callbackId, stream): resumes the watcher's slot.
#[cfg(windows)]
unsafe extern "C" fn on_socket(ex: *mut sys::zend_execute_data, rv: *mut sys::zval) {
    zval::write(rv, zval::null());
    if call::num_args(ex) < 1 {
        return;
    }
    let id = zval::deref(call::arg(ex, 0));
    if !zval::is(id, sys::IS_STRING) {
        return;
    }
    let id = zval::str_bytes(id);
    let s = LOOP.with_borrow_mut(|l| {
        let l = l.as_mut()?;
        let token = l.watches.iter().find(|(w, _)| zval::str_bytes(w) == id)?.1;
        let s = l.slots.get_mut(token).filter(|s| s.state == SlotState::Pending)?;
        let z = s.suspension.take()?;
        s.state = SlotState::Ready;
        Some(z)
    });
    if let Some(s) = s {
        resume_all(vec![s]);
    }
}

/// Stores the suspension in `token`'s slot, references the watcher, arms the deadline timer,
/// suspends.
unsafe fn suspend(token: usize, deadline: Option<Instant>) -> io::Result<()> {
    let s = event_loop("getSuspension", &mut []);
    if call::has_exception() || !zval::is(&s, sys::IS_OBJECT) {
        return Err(mongo_core::aio::interrupted());
    }
    let (reference, rearm) = with(|l| {
        l.slots[token].suspension = Some(zval::copy(&s));
        let php = l.php.as_mut().expect("php_state");
        let reference = !php.referenced;
        php.referenced = true;
        let rearm = deadline.filter(|d| php.timer.as_ref().is_none_or(|(_, at)| d < at));
        (reference.then(|| zval::copy(&php.watcher)), rearm.map(|d| (d, php.timer.take())))
    })?;
    if let Some(id) = reference {
        event_loop("reference", &mut [id]);
    }
    if let Some((at, old)) = rearm {
        arm_timer(at, old.map(|o| o.0));
    }
    method(&s, b"suspend", &mut []);
    if call::has_exception() {
        return Err(mongo_core::aio::interrupted());
    }
    Ok(())
}

/// A deadline timer due at `at` (an unreferenced delay(); `old` cancelled).
unsafe fn arm_timer(at: Instant, old: Option<sys::zval>) {
    if let Some(id) = old {
        event_loop("cancel", &mut [id]);
    }
    let secs = at.saturating_duration_since(Instant::now()).as_secs_f64();
    let mut args = [zval::double(secs), fake_closure(&mut (*funcs()).timer)];
    let id = event_loop("delay", &mut args);
    if zval::is(&id, sys::IS_STRING) {
        event_loop("unreference", &mut [zval::copy(&id)]);
        let _ = with(|l| {
            if let Some(php) = l.php.as_mut() {
                php.timer = Some((id, at));
            }
        });
    }
}

/// After a wait: the watcher is unreferenced once nothing waits (the loop may end).
unsafe fn after_wait() -> io::Result<()> {
    let id = with(|l| {
        let php = l.php.as_mut()?;
        (l.pending == 0 && php.referenced).then(|| {
            php.referenced = false;
            zval::copy(&php.watcher)
        })
    })?;
    if let Some(id) = id {
        saving_exception(|| event_loop("unreference", &mut [id]));
    }
    Ok(())
}

/// Runs `f`'s PHP calls even while an exception is pending (an interrupted wait: PHP skips any
/// call made while one is pending, and the loop's state must still be updated).
unsafe fn saving_exception<R>(f: impl FnOnce() -> R) -> R {
    if !call::has_exception() {
        return f();
    }
    #[cfg(not(php86))]
    {
        zend_exception_save();
        let r = f();
        zend_exception_restore();
        r
    }
    // what zend_exception_save/restore did, without EG(prev_exception): the pending exception
    // is set aside while `f` runs, then restored (chained as the previous one of any new one)
    #[cfg(php86)]
    {
        let eg = crate::zend::eg();
        let saved = (*eg).exception;
        (*eg).exception = null_mut();
        let r = f();
        if (*eg).exception.is_null() {
            (*eg).exception = saved;
        } else {
            zend_exception_set_previous((*eg).exception, saved);
        }
        r
    }
}

/// The ready slots' suspensions are resumed (outside any borrow).
unsafe fn resume_all(list: Vec<sys::zval>) {
    for s in list {
        method(&s, b"resume", &mut []);
    }
}

/// onReadable callback: drains the reactor, resumes the ready waiters.
unsafe extern "C" fn on_ready(_ex: *mut sys::zend_execute_data, rv: *mut sys::zval) {
    zval::write(rv, zval::null());
    #[cfg(windows)]
    let ready = {
        // the keep-alive socket: whatever arrived is drained (nothing waits on it)
        if let Ok(Some(raw)) = with(|l| l.php.as_ref().map(|p| p.keepalive)) {
            use std::os::windows::io::FromRawSocket;
            let udp = std::mem::ManuallyDrop::new(std::net::UdpSocket::from_raw_socket(raw as u64));
            let mut b = [0u8; 512];
            while udp.recv(&mut b).is_ok() {}
        }
        Vec::new()
    };
    #[cfg(unix)]
    let ready = LOOP.with_borrow_mut(|l| {
        let Some(l) = l.as_mut() else { return Vec::new() };
        let mut out = Vec::new();
        mongo_core::reactor::drain(&mut |t, gen| {
            let Some(s) = l.slots.get_mut(t).filter(|s| s.gen == gen && s.state == SlotState::Pending) else { return false };
            let Some(z) = s.suspension.take() else { return false };
            s.state = SlotState::Ready;
            out.push(z);
            true
        });
        out
    });
    resume_all(ready);
}

/// The deadline timer: expired waiters resume (timed out), the next deadline is armed.
unsafe extern "C" fn on_timer(_ex: *mut sys::zend_execute_data, rv: *mut sys::zval) {
    zval::write(rv, zval::null());
    let now = Instant::now();
    let r = with(|l| {
        if let Some(php) = l.php.as_mut() {
            php.timer = None;
        }
        let mut out = Vec::new();
        let mut next: Option<Instant> = None;
        for s in &mut l.slots {
            if s.state != SlotState::Pending || s.suspension.is_none() {
                continue;
            }
            match s.deadline {
                Some(d) if d <= now => {
                    s.state = SlotState::TimedOut;
                    out.push(s.suspension.take().unwrap());
                }
                Some(d) => next = Some(next.map_or(d, |n: Instant| n.min(d))),
                None => {}
            }
        }
        (out, next)
    });
    if let Ok((out, next)) = r {
        if let Some(at) = next {
            arm_timer(at, None);
        }
        resume_all(out);
    }
}

fn sleep(d: Duration) -> io::Result<()> {
    if exit_pending() {
        return Err(mongo_core::aio::interrupted());
    }
    // a sleep is a wait without sockets (the watcher stays referenced meanwhile)
    poll(&mut [], Some(Instant::now() + d)).map(|_| ())
}

/// The error of a Session used while another fiber's operation holds it.
pub fn session_in_use() -> mongo_core::error::Error {
    use mongo_core::error::{code, domain, Error};
    Error::new(domain::CLIENT, code::CLIENT_SESSION_IN_USE, "The session is in use by another fiber")
}

/// Borrows a Session's state mutably. Another fiber's operation may hold it across a wait:
/// then this fails with [`session_in_use`] (a LogicException) instead of waiting.
pub fn lock<T>(c: &RefCell<T>) -> Result<std::cell::RefMut<'_, T>, mongo_core::error::Error> {
    c.try_borrow_mut().map_err(|_| session_in_use())
}

/// `lock` for a shared borrow (fails only while an operation holds the session).
pub fn lock_ref<T>(c: &RefCell<T>) -> Result<std::cell::Ref<'_, T>, mongo_core::error::Error> {
    c.try_borrow().map_err(|_| session_in_use())
}

/// `lock` that throws the LogicException on failure.
pub unsafe fn lock_or_throw<T>(c: &RefCell<T>) -> Option<std::cell::RefMut<'_, T>> {
    lock(c).map_err(|e| super::throw_error(&e, None)).ok()
}

/// `lock_ref` that throws the LogicException on failure.
pub unsafe fn lock_ref_or_throw<T>(c: &RefCell<T>) -> Option<std::cell::Ref<'_, T>> {
    lock_ref(c).map_err(|e| super::throw_error(&e, None)).ok()
}
