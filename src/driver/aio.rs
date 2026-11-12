//! The event-loop backend of mongo_core::aio: inside a PHP Fiber (and in {main} once the loop is
//! in use), a driver call that waits for the network suspends through the Revolt event loop, so
//! other fibers and callbacks run meanwhile. The loop is revolt-rs's native one: this extension
//! embeds revolt-rs and registers the Revolt classes at MINIT, unless another copy of revolt-rs
//! (the `revolt` extension, another embedder) did; `EventLoop::setDriver()` is refused, so the
//! thread's loop is always that native driver, reached through revolt-rs's embedding ABI (the
//! table of whichever copy registered the classes) without PHP calls.
//!
//! - When: a call inside a fiber (EG(active_fiber)) waits by suspending; a call in {main} does so
//!   once the loop is in use (its driver exists: something called EventLoop) and
//!   mongodb.main_event_loop is on (the {main} suspension runs the loop meanwhile); otherwise,
//!   and wherever a fiber may not switch (object destruction, shutdown,
//!   zend_fiber_switch_blocked()), the plain blocking waits are used.
//! - How: the driver's sockets are *sources* of the loop's driver (revolt-rs's poller itself):
//!   registered edge-triggered at their first wait (one group per driver, i.e. per request),
//!   forgotten when they close. A wait arms its sockets with its park id (slot | generation << 32)
//!   and suspends; during the loop's poll, `src_take` marks the waits whose sockets are ready,
//!   `src_flush` resumes them, once per poll. Readiness seen while nobody waited is kept by the
//!   poller: the next wait of that socket returns at once and its caller retries the I/O. The
//!   group is referenced while waits are pending (the loop runs until they end).
//! - Deadlines: one unreferenced native delay() for the earliest one (a sleep is a wait without
//!   sockets). A wait whose socket is ready resumes as ready even when its deadline passed too:
//!   the poll's events are taken before the timers run.
//! - Another copy of revolt-rs registered the classes with an incompatible embedding ABI (an
//!   older revolt extension, say): no wait can suspend; every call blocks (a warning at MINIT).
//! - Never across a call into the loop: a borrow of the state here (the loop calls back).

use crate::zend::{call, sys, zval};
use revolt_rs::embed::{Abi, NativeLoop, SrcFd};
use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::io;
use std::ptr::{addr_of, null_mut};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

extern "C" {
    fn zend_fiber_switch_blocked() -> bool;
    fn zend_read_static_property_ex(scope: *mut sys::zend_class_entry, name: *mut sys::zend_string, silent: bool) -> *mut sys::zval;
}

thread_local! {
    /// Object destruction and shutdown in progress: waits block.
    static FORBID: Cell<u32> = const { Cell::new(0) };
}

/// The loop can be driven natively (set at MINIT: false when the classes belong to a revolt-rs
/// copy of an incompatible ABI).
static NATIVE: AtomicBool = AtomicBool::new(false);

pub static BACKEND: mongo_core::aio::Backend = mongo_core::aio::Backend { active, poll, sleep, require, park, unpark, current, forget };

#[inline]
fn in_fiber() -> bool {
    // SAFETY: the executor globals of the calling PHP thread
    unsafe { !(*crate::zend::eg()).active_fiber.is_null() }
}

/// EG_FLAGS_IN_SHUTDOWN
fn shutting_down() -> bool {
    unsafe { (*crate::zend::eg()).flags & 1 != 0 }
}

/// The class-table size (the counter of mongo_core::aio's synchronous fast path).
fn class_count() -> u32 {
    unsafe { (*(*crate::zend::eg()).class_table).nNumOfElements }
}

thread_local! {
    /// mongodb.main_event_loop, as its INI handler last set it on this thread (PHP calls it at
    /// startup, for every new ZTS thread, and at every change)
    static MAIN_EVENT_LOOP: Cell<bool> = const { Cell::new(true) };
    /// the slot of the static property EventLoop::$driver (looked up once per request; null:
    /// not yet)
    static DRIVER_SLOT: Cell<*mut sys::zval> = const { Cell::new(null_mut()) };
}

/// mongodb.main_event_loop (read at every {main} wait: no INI table lookup).
fn main_event_loop() -> bool {
    MAIN_EVENT_LOOP.get()
}

/// mongodb.main_event_loop was set to `on`: the remembered "no event loop in {main}" no longer
/// holds.
pub fn main_event_loop_changed(on: bool) {
    MAIN_EVENT_LOOP.set(on);
    mongo_core::aio::set_no_loop_at(u32::MAX);
}

/// Whether {main}'s waits go through the loop: mongodb.main_event_loop is On and the loop is in
/// use (EventLoop's driver exists; revolt-rs keeps EventLoop::$driver unset until it is made). A
/// miss is remembered until the driver is made (or the setting changes): the type word of that
/// static property is the fast path's counter.
fn loop_in_use() -> bool {
    if !main_event_loop() {
        // the synchronous fast path holds until a class is added or the setting changes
        publish_sync_words(class_count_word());
        mongo_core::aio::set_no_loop_at(class_count());
        return false;
    }
    let slot = unsafe { driver_slot() };
    if slot.is_null() {
        return false;
    }
    if unsafe { zval::is(slot, sys::IS_OBJECT) } {
        mongo_core::aio::set_no_loop_at(u32::MAX);
        return true;
    }
    let word = unsafe { addr_of!((*slot).u1.type_info) };
    publish_sync_words(word);
    mongo_core::aio::set_no_loop_at(unsafe { *word });
    false
}

/// The slot of EventLoop::$driver (null: no such class or property), looked up once per request
/// (a static member table lives as long as the request).
unsafe fn driver_slot() -> *mut sys::zval {
    let slot = DRIVER_SLOT.get();
    if !slot.is_null() {
        return slot;
    }
    let name = zval::string(b"Revolt\\EventLoop");
    let ce = sys::zend_lookup_class_ex(name.value.str_, null_mut(), ZEND_FETCH_CLASS_NO_AUTOLOAD);
    if ce.is_null() {
        return null_mut();
    }
    let prop = zval::string(b"driver");
    // silent: no error for an uninitialised typed property (EventLoop sets it on first use)
    let slot = zend_read_static_property_ex(ce, prop.value.str_, true);
    DRIVER_SLOT.set(slot);
    slot
}

/// ZEND_FETCH_CLASS_NO_AUTOLOAD
const ZEND_FETCH_CLASS_NO_AUTOLOAD: u32 = 0x80;

/// The class-table size's address (the default counter of the synchronous fast path).
fn class_count_word() -> *const u32 {
    unsafe { addr_of!((*(*crate::zend::eg()).class_table).nNumOfElements) }
}

/// EG(active_fiber) and a counter of this PHP thread (the class-table size, or the type of
/// EventLoop::$driver), for mongo_core::aio's synchronous fast path (both are per request:
/// published here, in a request, never at MINIT, and withdrawn at RSHUTDOWN).
fn publish_sync_words(counter: *const u32) {
    unsafe {
        let eg = crate::zend::eg();
        mongo_core::aio::set_sync_words(addr_of!((*eg).active_fiber).cast(), counter);
    }
}

/// Whether this call's waits suspend: inside a fiber, and in {main} once the loop is in use;
/// never where PHP forbids fiber switches, in object destruction or at shutdown.
fn active() -> bool {
    if FORBID.get() > 0 || shutting_down() || unsafe { zend_fiber_switch_blocked() } {
        return false;
    }
    if !NATIVE.load(Ordering::Relaxed) {
        return false;
    }
    in_fiber() || loop_in_use()
}

fn current() -> usize {
    unsafe { (*crate::zend::eg()).active_fiber as usize }
}

/// Fibers always have the loop (or block, with an incompatible revolt-rs copy).
fn require() -> Result<(), &'static str> {
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
    /// the suspension of the waiting fiber or {main} (borrowed: the waiting frame holds it);
    /// null until it suspends, and once it was taken for a resume
    suspension: *mut sys::zend_object,
    deadline: Option<Instant>,
}

impl Slot {
    fn free(gen: u32) -> Slot {
        Slot { gen, state: SlotState::Free, suspension: null_mut(), deadline: None }
    }
}

/// The loop's driver of this request and what is ours on it.
struct Nat {
    abi: &'static Abi,
    /// borrowed: EventLoop holds it for the whole request (setDriver() is refused)
    driver: *mut sys::zend_object,
    /// the source group of the driver's sockets
    group: u32,
    /// the waits pending (the group is referenced while there are any)
    here: usize,
    /// the deadline timer: its id and due time
    timer: Option<(u64, Instant)>,
}

struct Loop {
    slots: Vec<Slot>,
    /// this request's driver (reset at RSHUTDOWN)
    nat: Option<Nat>,
    /// the suspensions `src_take` took during the current poll, for `src_flush`
    taken: Vec<*mut sys::zend_object>,
}

thread_local! {
    static LOOP: RefCell<Loop> = const { RefCell::new(Loop { slots: Vec::new(), nat: None, taken: Vec::new() }) };
}

fn with<R>(f: impl FnOnce(&mut Loop) -> R) -> R {
    LOOP.with_borrow_mut(f)
}

/// The park id of a slot's use.
fn park_id(t: usize, gen: u32) -> u64 {
    (t as u64) | ((gen as u64) << 32)
}

/// MINIT: installs the backend; the embedded revolt-rs registers the Revolt classes unless
/// another copy did. With a copy of an incompatible ABI there, the waits block (warning).
pub unsafe fn minit(module_number: i32) {
    mongo_core::aio::set_backend(Some(&BACKEND));
    revolt_rs::module::minit(module_number);
    let native = revolt_rs::embed::active().is_some();
    NATIVE.store(native, Ordering::Relaxed);
    if !native {
        sys::php_error_docref(
            null_mut(),
            sys::E_CORE_WARNING as i32,
            c"The Revolt classes are registered by a revolt-rs copy whose embedding ABI is incompatible with the one in the mongodb extension (an older revolt extension?): MongoDB driver calls block instead of suspending fibers".as_ptr(),
        );
    }
}

/// RINIT of the embedded revolt-rs.
pub unsafe fn rinit() {
    revolt_rs::module::rinit();
}

/// MSHUTDOWN of the embedded revolt-rs.
pub unsafe fn mshutdown() {
    revolt_rs::module::mshutdown();
}

/// The driver of this request (its group made on first use).
unsafe fn nat() -> io::Result<(&'static Abi, *mut sys::zend_object, u32)> {
    if let Some(n) = with(|l| l.nat.as_ref().map(|n| (n.abi, n.driver, n.group))) {
        return Ok(n);
    }
    let Some(nl) = NativeLoop::current() else {
        return Err(mongo_core::aio::interrupted());
    };
    let group = nl.src_group(src_take, src_flush, null_mut());
    if group == 0 || call::has_exception() {
        return Err(mongo_core::aio::interrupted());
    }
    with(|l| l.nat = Some(Nat { abi: nl.abi, driver: nl.driver, group, here: 0, timer: None }));
    Ok((nl.abi, nl.driver, group))
}

/// RSHUTDOWN: the request's state goes (before the driver: revolt-rs's RSHUTDOWN, ours or the
/// owning copy's, runs after this); waits left behind belong to fibers destroyed later, which
/// find their slots freed and touch nothing.
pub fn rshutdown() {
    let _ = LOOP.try_with(|l| {
        if let Ok(mut l) = l.try_borrow_mut() {
            for s in &mut l.slots {
                *s = Slot::free(s.gen.wrapping_add(1));
            }
            l.nat = None;
            l.taken.clear();
        }
    });
    DRIVER_SLOT.set(null_mut());
    mongo_core::aio::set_no_loop_at(u32::MAX);
    mongo_core::aio::set_sync_words(std::ptr::null(), std::ptr::null());
    // the embedded revolt-rs's (the thread's driver goes, if this copy owns the loop)
    unsafe { revolt_rs::module::rshutdown() };
}

/// A socket of the driver is about to be closed: its source goes (a later socket with its number
/// is registered anew).
fn forget(fd: mongo_core::os::RawSock) {
    let n = LOOP.try_with(|l| l.try_borrow().ok().and_then(|l| l.nat.as_ref().map(|n| (n.abi, n.driver)))).ok().flatten();
    if let Some((abi, d)) = n {
        unsafe { (abi.src_forget)(d, fd as i64) };
    }
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
    let s = take_pending(id, SlotState::Ready);
    if !s.is_null() {
        // also while an exception is pending (the unparking call was interrupted and is
        // unwinding, giving back what it held): the waiter, its slot already Ready, must resume
        if let Some((abi, _, _)) = with(|l| l.nat.as_ref().map(|n| (n.abi, n.driver, n.group))) {
            unsafe { resume(abi, s) };
        }
    }
}

/// The suspension of the pending wait `id` (slot, generation), now in `state`; null when it does
/// not wait (any more), or has not suspended yet.
fn take_pending(id: u64, state: SlotState) -> *mut sys::zend_object {
    let (t, gen) = ((id & 0xffff_ffff) as usize, (id >> 32) as u32);
    LOOP.try_with(|l| {
        let Ok(mut l) = l.try_borrow_mut() else { return null_mut() };
        let Some(s) = l.slots.get_mut(t).filter(|s| s.gen == gen && s.state == SlotState::Pending && !s.suspension.is_null()) else {
            return null_mut();
        };
        s.state = state;
        std::mem::replace(&mut s.suspension, null_mut())
    })
    .unwrap_or(null_mut())
}

/// The sockets of a wait as sources.
fn sources(fds: &[mongo_core::os::PollFd]) -> ([SrcFd; 2], Vec<SrcFd>) {
    let conv = |p: &mongo_core::os::PollFd| SrcFd {
        // negative: ignored, as by poll(2) (Windows: INVALID_SOCKET is -1 too)
        fd: p.fd as i64,
        events: (if p.events & mongo_core::os::POLLIN != 0 { revolt_rs::embed::SRC_READ } else { 0 } | if p.events & mongo_core::os::POLLOUT != 0 { revolt_rs::embed::SRC_WRITE } else { 0 }) as u32,
    };
    let mut small = [SrcFd { fd: -1, events: 0 }; 2];
    if fds.len() <= 2 {
        for (i, p) in fds.iter().enumerate() {
            small[i] = conv(p);
        }
        return (small, Vec::new());
    }
    (small, fds.iter().map(conv).collect())
}

/// The wait of a poll (sockets), a sleep (none) or a park (none; `enqueue` gets its id).
fn wait(fds: &mut [mongo_core::os::PollFd], deadline: Option<Instant>, enqueue: Option<&mut dyn FnMut(u64)>) -> io::Result<usize> {
    if exit_pending() {
        return Err(mongo_core::aio::interrupted());
    }
    let (abi, d, group) = unsafe { nat()? };
    let (small, big) = sources(fds);
    let src: &[SrcFd] = if fds.len() <= 2 { &small[..fds.len()] } else { &big };
    // a slot, named as the waiter of the sockets
    let (token, gen) = with(|l| {
        let t = match l.slots.iter().position(|s| s.state == SlotState::Free) {
            Some(t) => t,
            None => {
                l.slots.push(Slot::free(0));
                l.slots.len() - 1
            }
        };
        let gen = l.slots[t].gen.wrapping_add(1);
        l.slots[t].gen = gen;
        (t, gen)
    });
    let id = park_id(token, gen);
    if !src.is_empty() {
        let r = unsafe { (abi.src_arm)(d, group, src.as_ptr(), src.len(), id) };
        if r < 0 {
            return Err(io::Error::from_raw_os_error(-r));
        }
        if r > 0 {
            // a readiness the poller already saw: no suspension
            if fds.len() == 1 {
                // the caller retries its I/O, which tells the rest
                fds[0].revents = fds[0].events;
                return Ok(1);
            }
            let n = mongo_core::net::poll_fds(fds, Some(Instant::now()))?;
            if n > 0 {
                return Ok(n);
            }
            // what the poller saw was consumed meanwhile: a real wait (it now finds nothing)
            return wait(fds, deadline, enqueue);
        }
    }
    let first = with(|l| {
        l.slots[token] = Slot { gen, state: SlotState::Pending, suspension: null_mut(), deadline };
        let n = l.nat.as_mut().expect("nat");
        n.here += 1;
        n.here == 1
    });
    if first {
        unsafe { (abi.src_reference)(d, group, true) };
    }
    if let Some(enqueue) = enqueue {
        enqueue(id);
    }
    let r = unsafe { suspend(abi, d, token, gen, deadline) };
    // the slot is still this wait's unless the request ended meanwhile (rshutdown): then the
    // driver may be gone, nothing is touched
    let state = with(|l| {
        if l.slots[token].gen != gen || l.nat.is_none() {
            return None;
        }
        let s = std::mem::replace(&mut l.slots[token], Slot::free(gen));
        let n = l.nat.as_mut().expect("nat");
        n.here -= 1;
        Some((s.state, n.here == 0))
    });
    let Some((state, last)) = state else {
        return Err(mongo_core::aio::interrupted());
    };
    unsafe {
        if !src.is_empty() {
            (abi.src_disarm)(d, src.as_ptr(), src.len(), id);
        }
        if last {
            (abi.src_reference)(d, group, false);
        }
    }
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

/// Suspends the wait `token` (its suspension stored in the slot), with the deadline timer armed
/// for it if it is the earliest.
unsafe fn suspend(abi: &'static Abi, d: *mut sys::zend_object, token: usize, gen: u32, deadline: Option<Instant>) -> io::Result<()> {
    let o = (abi.suspension)(d);
    if o.is_null() || call::has_exception() {
        return Err(mongo_core::aio::interrupted());
    }
    // this frame holds the suspension until it returns
    let s = zval::obj(o);
    let rearm = with(|l| {
        let slot = &mut l.slots[token];
        if slot.gen == gen {
            slot.suspension = o;
        }
        let n = l.nat.as_mut().expect("nat");
        deadline.filter(|d| n.timer.as_ref().is_none_or(|(_, at)| d < at)).map(|d| (d, n.timer.take()))
    });
    if let Some((at, old)) = rearm {
        arm_timer(abi, d, at, old.map(|o| o.0));
    }
    let mut rv = zval::undef();
    let ok = (abi.suspend)(o, &mut rv) && !call::has_exception();
    drop(rv);
    drop(s);
    if !ok {
        return Err(mongo_core::aio::interrupted());
    }
    Ok(())
}

/// A deadline timer due at `at` (an unreferenced delay(); `old` cancelled).
unsafe fn arm_timer(abi: &'static Abi, d: *mut sys::zend_object, at: Instant, old: Option<u64>) {
    if let Some(id) = old {
        (abi.cancel)(d, id);
    }
    let secs = at.saturating_duration_since(Instant::now()).as_secs_f64();
    let id = (abi.timer)(d, secs, false, on_timer, null_mut(), None, null_mut());
    if id != 0 {
        (abi.unreference)(d, id);
        with(|l| {
            if let Some(n) = l.nat.as_mut() {
                n.timer = Some((id, at));
            }
        });
    }
}

/// Resumes a suspension (a fiber's: as a microtask of the loop; {main}'s: the loop returns).
unsafe fn resume(abi: &'static Abi, s: *mut sys::zend_object) {
    (abi.resume)(s, null_mut());
}

/// During the loop's poll: a source of the wait `token` is ready. Taken (true) when the wait is
/// still pending: its suspension is resumed at the flush.
unsafe extern "C" fn src_take(_data: *mut c_void, token: u64) -> bool {
    let s = take_pending(token, SlotState::Ready);
    if s.is_null() {
        return false;
    }
    with(|l| l.taken.push(s));
    true
}

/// After the poll: the waits taken resume (all of them in one call).
unsafe extern "C" fn src_flush(_data: *mut c_void) {
    let (abi, taken) = with(|l| (l.nat.as_ref().map(|n| n.abi), std::mem::take(&mut l.taken)));
    if let Some(abi) = abi {
        for s in taken.iter() {
            resume(abi, *s);
        }
    }
    // the buffer is kept for the next poll
    with(|l| {
        if l.taken.is_empty() {
            let mut t = taken;
            t.clear();
            l.taken = t;
        }
    });
}

/// The deadline timer: expired waiters resume (timed out), the next deadline is armed. A wait
/// whose socket is ready resumes as ready even when its deadline passed too: the loop's poll
/// takes its event before the timers run, and a loop that could not run before the deadline (a
/// callback or a blocking {main} call held the thread) polls before it runs this.
unsafe extern "C" fn on_timer(_data: *mut c_void, _id: u64) {
    let now = Instant::now();
    let (out, next, nat) = with(|l| {
        if let Some(n) = l.nat.as_mut() {
            n.timer = None;
        }
        let mut out = Vec::new();
        let mut next: Option<Instant> = None;
        for s in &mut l.slots {
            if s.state != SlotState::Pending || s.suspension.is_null() {
                continue;
            }
            match s.deadline {
                Some(d) if d <= now => {
                    s.state = SlotState::TimedOut;
                    out.push(std::mem::replace(&mut s.suspension, null_mut()));
                }
                Some(d) => next = Some(next.map_or(d, |n: Instant| n.min(d))),
                None => {}
            }
        }
        (out, next, l.nat.as_ref().map(|n| (n.abi, n.driver)))
    });
    let Some((abi, d)) = nat else { return };
    if let Some(at) = next {
        arm_timer(abi, d, at, None);
    }
    for s in out {
        resume(abi, s);
    }
}

fn sleep(d: Duration) -> io::Result<()> {
    if exit_pending() {
        return Err(mongo_core::aio::interrupted());
    }
    // a sleep is a wait without sockets (the group stays referenced meanwhile)
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
