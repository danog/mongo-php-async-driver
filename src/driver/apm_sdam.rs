//! The SDAM monitoring event classes (src/MongoDB/Monitoring/{Server,Topology}*Event.c).
//!
//! As phongo: every event sets its declared properties; ServerChangedEvent and
//! TopologyChangedEvent also keep copies of the descriptions (and the topology id), from which
//! their getters build new objects on every call. The other getters read the properties
//! (PHONGO_PROPERTY_GETTER).

use super::apm::{prop, set};
use crate::bson::types::new_objectid;
use crate::zend::call::{self, Ex};
use crate::zend::{obj, sys, zval};
use mongo_core::apm::Event;
use mongo_core::sdam::ServerDescription;
use mongo_core::topology::TopologyDescription;
use std::ffi::CStr;
use std::ptr::{addr_of_mut, null_mut};

#[derive(Default)]
pub struct ServerChanged {
    topology_id: [u8; 12],
    addr: String,
    previous: Option<Box<ServerDescription>>,
    new: Option<Box<ServerDescription>>,
}

#[derive(Default)]
pub struct TopologyChanged {
    topology_id: [u8; 12],
    previous: Option<Box<TopologyDescription>>,
    new: Option<Box<TopologyDescription>>,
}

static mut H_SC: sys::zend_object_handlers = unsafe { std::mem::zeroed() };
static mut H_TC: sys::zend_object_handlers = unsafe { std::mem::zeroed() };
static mut CE_SC: *mut sys::zend_class_entry = null_mut();
static mut CE_TC: *mut sys::zend_class_entry = null_mut();

unsafe extern "C" fn create_sc(c: *mut sys::zend_class_entry) -> *mut sys::zend_object {
    obj::alloc(c, ServerChanged::default(), addr_of_mut!(H_SC))
}

unsafe extern "C" fn create_tc(c: *mut sys::zend_class_entry) -> *mut sys::zend_object {
    obj::alloc(c, TopologyChanged::default(), addr_of_mut!(H_TC))
}

pub unsafe fn init_MongoDB_Driver_Monitoring_ServerChangedEvent(c: *mut sys::zend_class_entry) {
    CE_SC = c;
    (*c).__bindgen_anon_2.create_object = Some(create_sc);
    obj::handlers::<ServerChanged>(addr_of_mut!(H_SC));
}

pub unsafe fn init_MongoDB_Driver_Monitoring_TopologyChangedEvent(c: *mut sys::zend_class_entry) {
    CE_TC = c;
    (*c).__bindgen_anon_2.create_object = Some(create_tc);
    obj::handlers::<TopologyChanged>(addr_of_mut!(H_TC));
}

unsafe fn ce(n: &CStr) -> *mut sys::zend_class_entry {
    super::lookup(n)
}

/// host and port properties from a host_and_port string
unsafe fn host_port(o: *mut sys::zend_object, addr: &str) {
    let (host, port) = super::server::host_port(addr);
    set(o, b"host", zval::string(host.as_bytes()));
    set(o, b"port", zval::long(port));
}

/// The SDAMSubscriber method and the event object for `ev`; None for a command event.
pub unsafe fn event_object(ev: Event) -> Option<(&'static CStr, sys::zval)> {
    Some(match ev {
        Event::ServerChanged { topology_id, addr, previous, new } => {
            let z = zval::obj(obj::alloc(CE_SC, ServerChanged::default(), addr_of_mut!(H_SC)));
            let o = z.value.obj;
            host_port(o, &addr);
            set(o, b"topologyId", new_objectid(&topology_id));
            set(o, b"newDescription", super::sdam::new_serverdescription((*new).clone()));
            set(o, b"previousDescription", super::sdam::new_serverdescription((*previous).clone()));
            *obj::data::<ServerChanged>(o) = ServerChanged { topology_id, addr, previous: Some(previous), new: Some(new) };
            (c"serverChanged", z)
        }
        Event::TopologyChanged { topology_id, previous, new } => {
            let z = zval::obj(obj::alloc(CE_TC, TopologyChanged::default(), addr_of_mut!(H_TC)));
            let o = z.value.obj;
            set(o, b"topologyId", new_objectid(&topology_id));
            set(o, b"newDescription", super::sdam::new_topologydescription((*new).clone()));
            set(o, b"previousDescription", super::sdam::new_topologydescription((*previous).clone()));
            *obj::data::<TopologyChanged>(o) = TopologyChanged { topology_id, previous: Some(previous), new: Some(new) };
            (c"topologyChanged", z)
        }
        ev => return simple_event(ev),
    })
}

/// The property-only events (PHONGO_OBJECT_INIT_EX + update_properties).
unsafe fn simple_event(ev: Event) -> Option<(&'static CStr, sys::zval)> {
    let (method, class): (&'static CStr, &CStr) = match &ev {
        Event::ServerOpening { .. } => (c"serverOpening", c"mongodb\\driver\\monitoring\\serveropeningevent"),
        Event::ServerClosed { .. } => (c"serverClosed", c"mongodb\\driver\\monitoring\\serverclosedevent"),
        Event::TopologyOpening { .. } => (c"topologyOpening", c"mongodb\\driver\\monitoring\\topologyopeningevent"),
        Event::TopologyClosed { .. } => (c"topologyClosed", c"mongodb\\driver\\monitoring\\topologyclosedevent"),
        Event::HeartbeatStarted { .. } => (c"serverHeartbeatStarted", c"mongodb\\driver\\monitoring\\serverheartbeatstartedevent"),
        Event::HeartbeatSucceeded { .. } => (c"serverHeartbeatSucceeded", c"mongodb\\driver\\monitoring\\serverheartbeatsucceededevent"),
        Event::HeartbeatFailed { .. } => (c"serverHeartbeatFailed", c"mongodb\\driver\\monitoring\\serverheartbeatfailedevent"),
        _ => return None,
    };
    let z = obj::new_zval(ce(class));
    let o = z.value.obj;
    match ev {
        Event::ServerOpening { topology_id, addr } | Event::ServerClosed { topology_id, addr } => {
            host_port(o, &addr);
            set(o, b"topologyId", new_objectid(&topology_id));
        }
        Event::TopologyOpening { topology_id } | Event::TopologyClosed { topology_id } => set(o, b"topologyId", new_objectid(&topology_id)),
        Event::HeartbeatStarted { addr } => {
            host_port(o, &addr);
            set(o, b"awaited", zval::bool(false));
        }
        Event::HeartbeatSucceeded { addr, duration_us, reply } => {
            host_port(o, &addr);
            set(o, b"awaited", zval::bool(false));
            set(o, b"duration", zval::long(duration_us));
            if let Some(d) = super::to_zval_no_odm(&reply) {
                set(o, b"reply", d);
            }
        }
        Event::HeartbeatFailed { addr, duration_us, error } => {
            host_port(o, &addr);
            set(o, b"awaited", zval::bool(false));
            set(o, b"duration", zval::long(duration_us));
            set(o, b"error", super::apm::error_object(&error));
        }
        _ => unreachable!(),
    }
    Some((method, z))
}

unsafe fn sc<'a>(ex: Ex) -> &'a ServerChanged {
    obj::data::<ServerChanged>(call::this(ex))
}

unsafe fn tc<'a>(ex: Ex) -> &'a TopologyChanged {
    obj::data::<TopologyChanged>(call::this(ex))
}

pub unsafe extern "C" fn zim_MongoDB_Driver_Monitoring_ServerChangedEvent_getHost(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        zval::write(rv, zval::string(super::server::host_port(&sc(ex).addr).0.as_bytes()));
    }
}
pub unsafe extern "C" fn zim_MongoDB_Driver_Monitoring_ServerChangedEvent_getPort(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        zval::write(rv, zval::long(super::server::host_port(&sc(ex).addr).1));
    }
}
pub unsafe extern "C" fn zim_MongoDB_Driver_Monitoring_ServerChangedEvent_getTopologyId(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        zval::write(rv, new_objectid(&sc(ex).topology_id));
    }
}
pub unsafe extern "C" fn zim_MongoDB_Driver_Monitoring_ServerChangedEvent_getNewDescription(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        if let Some(d) = &sc(ex).new {
            zval::write(rv, super::sdam::new_serverdescription((**d).clone()));
        }
    }
}
pub unsafe extern "C" fn zim_MongoDB_Driver_Monitoring_ServerChangedEvent_getPreviousDescription(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        if let Some(d) = &sc(ex).previous {
            zval::write(rv, super::sdam::new_serverdescription((**d).clone()));
        }
    }
}
pub unsafe extern "C" fn zim_MongoDB_Driver_Monitoring_TopologyChangedEvent_getTopologyId(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        zval::write(rv, new_objectid(&tc(ex).topology_id));
    }
}
pub unsafe extern "C" fn zim_MongoDB_Driver_Monitoring_TopologyChangedEvent_getNewDescription(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        if let Some(d) = &tc(ex).new {
            zval::write(rv, super::sdam::new_topologydescription((**d).clone()));
        }
    }
}
pub unsafe extern "C" fn zim_MongoDB_Driver_Monitoring_TopologyChangedEvent_getPreviousDescription(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        if let Some(d) = &tc(ex).previous {
            zval::write(rv, super::sdam::new_topologydescription((**d).clone()));
        }
    }
}

// PHONGO_PROPERTY_GETTER methods of the property-only events
pub unsafe extern "C" fn zim_MongoDB_Driver_Monitoring_ServerOpeningEvent_getHost(ex: Ex, rv: *mut sys::zval) {
    prop(ex, rv, c"host")
}
pub unsafe extern "C" fn zim_MongoDB_Driver_Monitoring_ServerOpeningEvent_getPort(ex: Ex, rv: *mut sys::zval) {
    prop(ex, rv, c"port")
}
pub unsafe extern "C" fn zim_MongoDB_Driver_Monitoring_ServerOpeningEvent_getTopologyId(ex: Ex, rv: *mut sys::zval) {
    prop(ex, rv, c"topologyId")
}
pub unsafe extern "C" fn zim_MongoDB_Driver_Monitoring_ServerClosedEvent_getHost(ex: Ex, rv: *mut sys::zval) {
    prop(ex, rv, c"host")
}
pub unsafe extern "C" fn zim_MongoDB_Driver_Monitoring_ServerClosedEvent_getPort(ex: Ex, rv: *mut sys::zval) {
    prop(ex, rv, c"port")
}
pub unsafe extern "C" fn zim_MongoDB_Driver_Monitoring_ServerClosedEvent_getTopologyId(ex: Ex, rv: *mut sys::zval) {
    prop(ex, rv, c"topologyId")
}
pub unsafe extern "C" fn zim_MongoDB_Driver_Monitoring_TopologyOpeningEvent_getTopologyId(ex: Ex, rv: *mut sys::zval) {
    prop(ex, rv, c"topologyId")
}
pub unsafe extern "C" fn zim_MongoDB_Driver_Monitoring_TopologyClosedEvent_getTopologyId(ex: Ex, rv: *mut sys::zval) {
    prop(ex, rv, c"topologyId")
}
pub unsafe extern "C" fn zim_MongoDB_Driver_Monitoring_ServerHeartbeatStartedEvent_getHost(ex: Ex, rv: *mut sys::zval) {
    prop(ex, rv, c"host")
}
pub unsafe extern "C" fn zim_MongoDB_Driver_Monitoring_ServerHeartbeatStartedEvent_getPort(ex: Ex, rv: *mut sys::zval) {
    prop(ex, rv, c"port")
}
pub unsafe extern "C" fn zim_MongoDB_Driver_Monitoring_ServerHeartbeatStartedEvent_isAwaited(ex: Ex, rv: *mut sys::zval) {
    prop(ex, rv, c"awaited")
}
pub unsafe extern "C" fn zim_MongoDB_Driver_Monitoring_ServerHeartbeatSucceededEvent_getHost(ex: Ex, rv: *mut sys::zval) {
    prop(ex, rv, c"host")
}
pub unsafe extern "C" fn zim_MongoDB_Driver_Monitoring_ServerHeartbeatSucceededEvent_getPort(ex: Ex, rv: *mut sys::zval) {
    prop(ex, rv, c"port")
}
pub unsafe extern "C" fn zim_MongoDB_Driver_Monitoring_ServerHeartbeatSucceededEvent_isAwaited(ex: Ex, rv: *mut sys::zval) {
    prop(ex, rv, c"awaited")
}
pub unsafe extern "C" fn zim_MongoDB_Driver_Monitoring_ServerHeartbeatSucceededEvent_getDurationMicros(ex: Ex, rv: *mut sys::zval) {
    prop(ex, rv, c"duration")
}
pub unsafe extern "C" fn zim_MongoDB_Driver_Monitoring_ServerHeartbeatSucceededEvent_getReply(ex: Ex, rv: *mut sys::zval) {
    prop(ex, rv, c"reply")
}
pub unsafe extern "C" fn zim_MongoDB_Driver_Monitoring_ServerHeartbeatFailedEvent_getHost(ex: Ex, rv: *mut sys::zval) {
    prop(ex, rv, c"host")
}
pub unsafe extern "C" fn zim_MongoDB_Driver_Monitoring_ServerHeartbeatFailedEvent_getPort(ex: Ex, rv: *mut sys::zval) {
    prop(ex, rv, c"port")
}
pub unsafe extern "C" fn zim_MongoDB_Driver_Monitoring_ServerHeartbeatFailedEvent_isAwaited(ex: Ex, rv: *mut sys::zval) {
    prop(ex, rv, c"awaited")
}
pub unsafe extern "C" fn zim_MongoDB_Driver_Monitoring_ServerHeartbeatFailedEvent_getDurationMicros(ex: Ex, rv: *mut sys::zval) {
    prop(ex, rv, c"duration")
}
pub unsafe extern "C" fn zim_MongoDB_Driver_Monitoring_ServerHeartbeatFailedEvent_getError(ex: Ex, rv: *mut sys::zval) {
    prop(ex, rv, c"error")
}
