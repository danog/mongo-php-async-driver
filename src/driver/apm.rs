//! APM (phongo_apm.c, src/MongoDB/Monitoring/*.c): the global and per-Manager subscriber
//! registries, MongoDB\Driver\Monitoring\{add,remove}Subscriber and the command event classes.
//!
//! The core client queues its events while it runs an operation; `flush` hands them to the
//! subscribers once the client is no longer borrowed (subscribers may use the driver), in
//! the order they happened and before the operation's own error is thrown.

use super::ClientRef;
use crate::bson::{throw, E};
use crate::zend::call::{self, Ex};
use crate::zend::{obj, sys, zval};
use mongo_core::apm::{CommandInfo, Event};
use std::cell::RefCell;
use std::ffi::CStr;
use std::ptr::null_mut;

/// A registered subscriber: (object handle, the object zval holding a reference).
pub type Subscribers = Vec<(u32, sys::zval)>;

thread_local! {
    /// MONGODB_G(subscribers)
    static GLOBAL: RefCell<Subscribers> = const { RefCell::new(Vec::new()) };
    /// MONGODB_G(managers): live Manager objects in creation order, with their client
    static MANAGERS: RefCell<Vec<(*mut sys::zend_object, ClientRef)>> = const { RefCell::new(Vec::new()) };
}

unsafe fn ce(n: &CStr) -> *mut sys::zend_class_entry {
    super::lookup(n)
}

fn update_count() {
    let g = GLOBAL.with(|g| g.borrow().len());
    let m: usize = MANAGERS.with(|m| m.borrow().iter().map(|(o, _)| unsafe { super::manager::subscribers_of(*o).len() }).sum());
    mongo_core::apm::set_subscriber_count(g + m);
}

/// Registers a live Manager (phongo_manager_init).
pub fn register_manager(o: *mut sys::zend_object, c: ClientRef) {
    MANAGERS.with(|m| m.borrow_mut().push((o, c)));
}

/// Unregisters a Manager being freed.
pub fn unregister_manager(o: *mut sys::zend_object) {
    let _ = MANAGERS.try_with(|m| m.borrow_mut().retain(|(x, _)| *x != o));
    update_count();
}

/// RSHUTDOWN: the global subscribers are released with the request.
pub unsafe fn rshutdown() {
    let subs = GLOBAL.with(|g| std::mem::take(&mut *g.borrow_mut()));
    drop(subs);
    update_count();
}

/// phongo_apm_check_args_for_add_and_remove + add/remove
pub unsafe fn add(list: &mut Subscribers, z: *mut sys::zval) {
    let h = (*(*z).value.obj).handle;
    if !list.iter().any(|(x, _)| *x == h) {
        list.push((h, zval::copy(z)));
    }
}

pub unsafe fn remove(list: &mut Subscribers, z: *mut sys::zval) {
    let h = (*(*z).value.obj).handle;
    if let Some(i) = list.iter().position(|(x, _)| *x == h) {
        drop(list.remove(i));
    }
}

unsafe fn parse_subscriber(ex: Ex) -> Option<*mut sys::zval> {
    let mut z: *mut sys::zval = null_mut();
    if sys::zend_parse_parameters(call::num_args(ex), c"O".as_ptr(), &mut z, ce(c"mongodb\\driver\\monitoring\\subscriber")) != 0 {
        return None;
    }
    Some(z)
}

pub unsafe extern "C" fn zif_MongoDB_Driver_Monitoring_addSubscriber(ex: Ex, _rv: *mut sys::zval) {
    let Some(z) = parse_subscriber(ex) else { return };
    GLOBAL.with(|g| add(&mut g.borrow_mut(), z));
    update_count();
}

pub unsafe extern "C" fn zif_MongoDB_Driver_Monitoring_removeSubscriber(ex: Ex, _rv: *mut sys::zval) {
    let Some(z) = parse_subscriber(ex) else { return };
    let mut taken = None;
    GLOBAL.with(|g| {
        let mut g = g.borrow_mut();
        let h = (*(*z).value.obj).handle;
        if let Some(i) = g.iter().position(|(x, _)| *x == h) {
            taken = Some(g.remove(i).1);
        }
    });
    // the subscriber's destructor may run here: no registry borrow held
    drop(taken);
    update_count();
}

pub unsafe extern "C" fn zim_MongoDB_Driver_Manager_addSubscriber(ex: Ex, _rv: *mut sys::zval) {
    let Some(z) = parse_subscriber(ex) else { return };
    add(super::manager::subscribers_of(call::this(ex)), z);
    update_count();
}

pub unsafe extern "C" fn zim_MongoDB_Driver_Manager_removeSubscriber(ex: Ex, _rv: *mut sys::zval) {
    let Some(z) = parse_subscriber(ex) else { return };
    remove(super::manager::subscribers_of(call::this(ex)), z);
    update_count();
}

/// phongo_apm_get_subscribers_to_notify: global ones, then those of every Manager of the
/// client; instances of `of` only, each once.
unsafe fn subscribers_for(c: &ClientRef, of: *mut sys::zend_class_entry) -> Vec<sys::zval> {
    let mut seen: Vec<u32> = Vec::new();
    let mut out = Vec::new();
    let mut take = |list: &Subscribers| {
        for (h, z) in list {
            if zval::is(z, sys::IS_OBJECT) && obj::instanceof((*z.value.obj).ce, of) && !seen.contains(h) {
                seen.push(*h);
                out.push(zval::copy(z));
            }
        }
    };
    GLOBAL.with(|g| take(&g.borrow()));
    let managers: Vec<*mut sys::zend_object> = MANAGERS.with(|m| m.borrow().iter().filter(|(_, mc)| std::rc::Rc::ptr_eq(mc, c)).map(|(o, _)| *o).collect());
    for o in managers {
        take(super::manager::subscribers_of(o));
    }
    out
}

/// Dispatches the client's queued events.
pub unsafe fn flush(c: &ClientRef) {
    let events = match c.try_borrow_mut() {
        Ok(mut cl) if !cl.events.is_empty() => std::mem::take(&mut cl.events),
        _ => return,
    };
    for ev in events {
        dispatch(c, ev);
    }
}

unsafe fn dispatch(c: &ClientRef, ev: Event) {
    let sub_ce = ce(c"mongodb\\driver\\monitoring\\commandsubscriber");
    let subs = subscribers_for(c, sub_ce);
    if !subs.is_empty() {
        let (method, z): (&CStr, sys::zval) = match ev {
            Event::CommandStarted { info, command } => (c"commandStarted", started_event(&info, &command)),
            Event::CommandSucceeded { info, duration_us, reply } => (c"commandSucceeded", succeeded_event(&info, duration_us, &reply)),
            Event::CommandFailed { info, duration_us, error, reply } => (c"commandFailed", failed_event(&info, duration_us, &error, &reply)),
        };
        for s in &subs {
            if call::has_exception() {
                break;
            }
            let mut arg = [zval::copy(&z)];
            drop(call::method(s.value.obj, method, &mut arg));
        }
    }
    // the event and the subscriber references are released here
}

/// zend_update_property (which copies `v`; our reference is released here).
unsafe fn set(o: *mut sys::zend_object, name: &[u8], mut v: sys::zval) {
    sys::zend_update_property((*o).ce, o, name.as_ptr() as *const _, name.len(), &mut v);
}

/// The fields every command event has (host, port, names, ids).
unsafe fn common(o: *mut sys::zend_object, info: &CommandInfo) {
    set(o, b"host", zval::string(info.host.as_bytes()));
    set(o, b"port", zval::long(info.port as i64));
    set(o, b"commandName", zval::string(info.command_name.as_bytes()));
    set(o, b"databaseName", zval::string(info.database_name.as_bytes()));
}

unsafe fn ids(o: *mut sys::zend_object, info: &CommandInfo) {
    set(o, b"operationId", zval::string(info.operation_id.to_string().as_bytes()));
    set(o, b"requestId", zval::string(info.request_id.to_string().as_bytes()));
    set(o, b"serviceId", info.service_id.as_ref().map_or_else(zval::null, |s| crate::bson::types::new_objectid(s)));
    set(o, b"serverConnectionId", if info.server_connection_id == -1 { zval::null() } else { zval::long(info.server_connection_id) });
}

unsafe fn doc(d: &[u8]) -> sys::zval {
    super::to_zval_no_odm(d).unwrap_or_else(zval::null)
}

/// phongo_commandstartedevent_init
unsafe fn started_event(info: &CommandInfo, command: &[u8]) -> sys::zval {
    let z = obj::new_zval(ce(c"mongodb\\driver\\monitoring\\commandstartedevent"));
    let o = z.value.obj;
    common(o, info);
    if let Some(d) = super::to_zval_no_odm(command) {
        set(o, b"command", d);
    }
    ids(o, info);
    z
}

/// phongo_commandsucceededevent_init
unsafe fn succeeded_event(info: &CommandInfo, duration_us: i64, reply: &[u8]) -> sys::zval {
    let z = obj::new_zval(ce(c"mongodb\\driver\\monitoring\\commandsucceededevent"));
    let o = z.value.obj;
    common(o, info);
    set(o, b"duration", zval::long(duration_us));
    if let Some(d) = super::to_zval_no_odm(reply) {
        set(o, b"reply", d);
    }
    ids(o, info);
    z
}

/// phongo_commandfailedevent_init
unsafe fn failed_event(info: &CommandInfo, duration_us: i64, error: &mongo_core::error::Error, reply: &[u8]) -> sys::zval {
    let z = obj::new_zval(ce(c"mongodb\\driver\\monitoring\\commandfailedevent"));
    let o = z.value.obj;
    common(o, info);
    set(o, b"duration", zval::long(duration_us));
    let ze = obj::new_zval(super::exception_ce(error));
    let eo = ze.value.obj;
    let exc = sys::zend_ce_exception;
    let mut m = zval::string(error.message.as_bytes());
    sys::zend_update_property(exc, eo, c"message".as_ptr(), 7, &mut m);
    let mut code = zval::long(error.code as i64);
    sys::zend_update_property(exc, eo, c"code".as_ptr(), 4, &mut code);
    set(o, b"error", ze);
    let _ = doc;
    if let Some(d) = super::to_zval_no_odm(reply) {
        set(o, b"reply", d);
    }
    ids(o, info);
    z
}

/// PHONGO_PROPERTY_GETTER
unsafe fn prop(ex: Ex, rv: *mut sys::zval, name: &CStr) {
    if !call::none(ex) {
        return;
    }
    let o = call::this(ex);
    let mut tmp = zval::undef();
    let z = sys::zend_read_property((*o).ce, o, name.as_ptr(), name.to_bytes().len(), false, &mut tmp);
    zval::write(rv, zval::copy(zval::deref(z)));
}

unsafe fn private_ctor() {
    throw(E::Runtime, "Accessing private constructor");
}


// the command event getters (PHONGO_PROPERTY_GETTER)
pub unsafe extern "C" fn zim_MongoDB_Driver_Monitoring_CommandFailedEvent_getCommandName(ex: Ex, rv: *mut sys::zval) {
    prop(ex, rv, c"commandName")
}
pub unsafe extern "C" fn zim_MongoDB_Driver_Monitoring_CommandFailedEvent_getDatabaseName(ex: Ex, rv: *mut sys::zval) {
    prop(ex, rv, c"databaseName")
}
pub unsafe extern "C" fn zim_MongoDB_Driver_Monitoring_CommandFailedEvent_getDurationMicros(ex: Ex, rv: *mut sys::zval) {
    prop(ex, rv, c"duration")
}
pub unsafe extern "C" fn zim_MongoDB_Driver_Monitoring_CommandFailedEvent_getError(ex: Ex, rv: *mut sys::zval) {
    prop(ex, rv, c"error")
}
pub unsafe extern "C" fn zim_MongoDB_Driver_Monitoring_CommandFailedEvent_getHost(ex: Ex, rv: *mut sys::zval) {
    prop(ex, rv, c"host")
}
pub unsafe extern "C" fn zim_MongoDB_Driver_Monitoring_CommandFailedEvent_getOperationId(ex: Ex, rv: *mut sys::zval) {
    prop(ex, rv, c"operationId")
}
pub unsafe extern "C" fn zim_MongoDB_Driver_Monitoring_CommandFailedEvent_getPort(ex: Ex, rv: *mut sys::zval) {
    prop(ex, rv, c"port")
}
pub unsafe extern "C" fn zim_MongoDB_Driver_Monitoring_CommandFailedEvent_getReply(ex: Ex, rv: *mut sys::zval) {
    prop(ex, rv, c"reply")
}
pub unsafe extern "C" fn zim_MongoDB_Driver_Monitoring_CommandFailedEvent_getRequestId(ex: Ex, rv: *mut sys::zval) {
    prop(ex, rv, c"requestId")
}
pub unsafe extern "C" fn zim_MongoDB_Driver_Monitoring_CommandFailedEvent_getServerConnectionId(ex: Ex, rv: *mut sys::zval) {
    prop(ex, rv, c"serverConnectionId")
}
pub unsafe extern "C" fn zim_MongoDB_Driver_Monitoring_CommandFailedEvent_getServiceId(ex: Ex, rv: *mut sys::zval) {
    prop(ex, rv, c"serviceId")
}
pub unsafe extern "C" fn zim_MongoDB_Driver_Monitoring_CommandStartedEvent_getCommand(ex: Ex, rv: *mut sys::zval) {
    prop(ex, rv, c"command")
}
pub unsafe extern "C" fn zim_MongoDB_Driver_Monitoring_CommandStartedEvent_getCommandName(ex: Ex, rv: *mut sys::zval) {
    prop(ex, rv, c"commandName")
}
pub unsafe extern "C" fn zim_MongoDB_Driver_Monitoring_CommandStartedEvent_getDatabaseName(ex: Ex, rv: *mut sys::zval) {
    prop(ex, rv, c"databaseName")
}
pub unsafe extern "C" fn zim_MongoDB_Driver_Monitoring_CommandStartedEvent_getHost(ex: Ex, rv: *mut sys::zval) {
    prop(ex, rv, c"host")
}
pub unsafe extern "C" fn zim_MongoDB_Driver_Monitoring_CommandStartedEvent_getOperationId(ex: Ex, rv: *mut sys::zval) {
    prop(ex, rv, c"operationId")
}
pub unsafe extern "C" fn zim_MongoDB_Driver_Monitoring_CommandStartedEvent_getPort(ex: Ex, rv: *mut sys::zval) {
    prop(ex, rv, c"port")
}
pub unsafe extern "C" fn zim_MongoDB_Driver_Monitoring_CommandStartedEvent_getRequestId(ex: Ex, rv: *mut sys::zval) {
    prop(ex, rv, c"requestId")
}
pub unsafe extern "C" fn zim_MongoDB_Driver_Monitoring_CommandStartedEvent_getServerConnectionId(ex: Ex, rv: *mut sys::zval) {
    prop(ex, rv, c"serverConnectionId")
}
pub unsafe extern "C" fn zim_MongoDB_Driver_Monitoring_CommandStartedEvent_getServiceId(ex: Ex, rv: *mut sys::zval) {
    prop(ex, rv, c"serviceId")
}
pub unsafe extern "C" fn zim_MongoDB_Driver_Monitoring_CommandSucceededEvent_getCommandName(ex: Ex, rv: *mut sys::zval) {
    prop(ex, rv, c"commandName")
}
pub unsafe extern "C" fn zim_MongoDB_Driver_Monitoring_CommandSucceededEvent_getDatabaseName(ex: Ex, rv: *mut sys::zval) {
    prop(ex, rv, c"databaseName")
}
pub unsafe extern "C" fn zim_MongoDB_Driver_Monitoring_CommandSucceededEvent_getDurationMicros(ex: Ex, rv: *mut sys::zval) {
    prop(ex, rv, c"duration")
}
pub unsafe extern "C" fn zim_MongoDB_Driver_Monitoring_CommandSucceededEvent_getHost(ex: Ex, rv: *mut sys::zval) {
    prop(ex, rv, c"host")
}
pub unsafe extern "C" fn zim_MongoDB_Driver_Monitoring_CommandSucceededEvent_getOperationId(ex: Ex, rv: *mut sys::zval) {
    prop(ex, rv, c"operationId")
}
pub unsafe extern "C" fn zim_MongoDB_Driver_Monitoring_CommandSucceededEvent_getPort(ex: Ex, rv: *mut sys::zval) {
    prop(ex, rv, c"port")
}
pub unsafe extern "C" fn zim_MongoDB_Driver_Monitoring_CommandSucceededEvent_getReply(ex: Ex, rv: *mut sys::zval) {
    prop(ex, rv, c"reply")
}
pub unsafe extern "C" fn zim_MongoDB_Driver_Monitoring_CommandSucceededEvent_getRequestId(ex: Ex, rv: *mut sys::zval) {
    prop(ex, rv, c"requestId")
}
pub unsafe extern "C" fn zim_MongoDB_Driver_Monitoring_CommandSucceededEvent_getServerConnectionId(ex: Ex, rv: *mut sys::zval) {
    prop(ex, rv, c"serverConnectionId")
}
pub unsafe extern "C" fn zim_MongoDB_Driver_Monitoring_CommandSucceededEvent_getServiceId(ex: Ex, rv: *mut sys::zval) {
    prop(ex, rv, c"serviceId")
}
pub unsafe extern "C" fn zim_MongoDB_Driver_Monitoring_CommandFailedEvent___construct(_ex: Ex, _rv: *mut sys::zval) {
    private_ctor()
}
pub unsafe extern "C" fn zim_MongoDB_Driver_Monitoring_CommandStartedEvent___construct(_ex: Ex, _rv: *mut sys::zval) {
    private_ctor()
}
pub unsafe extern "C" fn zim_MongoDB_Driver_Monitoring_CommandSucceededEvent___construct(_ex: Ex, _rv: *mut sys::zval) {
    private_ctor()
}
pub unsafe extern "C" fn zim_MongoDB_Driver_Monitoring_ServerChangedEvent___construct(_ex: Ex, _rv: *mut sys::zval) {
    private_ctor()
}
pub unsafe extern "C" fn zim_MongoDB_Driver_Monitoring_ServerClosedEvent___construct(_ex: Ex, _rv: *mut sys::zval) {
    private_ctor()
}
pub unsafe extern "C" fn zim_MongoDB_Driver_Monitoring_ServerHeartbeatFailedEvent___construct(_ex: Ex, _rv: *mut sys::zval) {
    private_ctor()
}
pub unsafe extern "C" fn zim_MongoDB_Driver_Monitoring_ServerHeartbeatStartedEvent___construct(_ex: Ex, _rv: *mut sys::zval) {
    private_ctor()
}
pub unsafe extern "C" fn zim_MongoDB_Driver_Monitoring_ServerHeartbeatSucceededEvent___construct(_ex: Ex, _rv: *mut sys::zval) {
    private_ctor()
}
pub unsafe extern "C" fn zim_MongoDB_Driver_Monitoring_ServerOpeningEvent___construct(_ex: Ex, _rv: *mut sys::zval) {
    private_ctor()
}
pub unsafe extern "C" fn zim_MongoDB_Driver_Monitoring_TopologyChangedEvent___construct(_ex: Ex, _rv: *mut sys::zval) {
    private_ctor()
}
pub unsafe extern "C" fn zim_MongoDB_Driver_Monitoring_TopologyClosedEvent___construct(_ex: Ex, _rv: *mut sys::zval) {
    private_ctor()
}
pub unsafe extern "C" fn zim_MongoDB_Driver_Monitoring_TopologyOpeningEvent___construct(_ex: Ex, _rv: *mut sys::zval) {
    private_ctor()
}
