//! MongoDB\Driver\ServerDescription and TopologyDescription (src/MongoDB/ServerDescription.c,
//! TopologyDescription.c): snapshots of the core SDAM descriptions, and Server::getServerDescription.

use crate::bson::types::Props;
use crate::bson::{throw, E};
use crate::zend::call::{self, Ex};
use crate::zend::{hash, obj, sys, zval};
use mongo_core::sdam::{ServerDescription, RTT_UNSET};
use mongo_core::topology::{OpType, TopologyDescription};
use std::ptr::{addr_of_mut, null_mut};

/// An empty BSON document (the hello response of a server not heard from yet).
const EMPTY_DOC: [u8; 5] = [5, 0, 0, 0, 0];

#[derive(Default)]
pub struct ServerDesc {
    /// None for an object not made by the driver (phongo: NULL server_description)
    pub sd: Option<ServerDescription>,
    props: Props,
}

#[derive(Default)]
pub struct TopologyDesc {
    pub td: Option<TopologyDescription>,
    props: Props,
}

static mut H_SD: sys::zend_object_handlers = unsafe { std::mem::zeroed() };
static mut H_TD: sys::zend_object_handlers = unsafe { std::mem::zeroed() };
pub static mut CE_SD: *mut sys::zend_class_entry = null_mut();
pub static mut CE_TD: *mut sys::zend_class_entry = null_mut();

// ---------------------------------------------------------------- ServerDescription

unsafe extern "C" fn create_sd(c: *mut sys::zend_class_entry) -> *mut sys::zend_object {
    obj::alloc(c, ServerDesc::default(), addr_of_mut!(H_SD))
}

/// The hello response, decoded with the debug type map (arrays); None after throwing.
unsafe fn hello_zval(sd: &ServerDescription) -> Option<sys::zval> {
    let doc: &[u8] = if sd.hello.is_empty() { &EMPTY_DOC } else { &sd.hello };
    super::to_zval(doc, &super::debug_typemap())
}

/// phongo_serverdescription_get_properties_hash
unsafe fn sd_fill(sd: Option<&ServerDescription>, ht: *mut sys::HashTable) {
    let Some(sd) = sd else { return };
    let (host, port) = super::server::host_port(&sd.addr);
    hash::str_update(ht, b"host", zval::string(host.as_bytes()));
    hash::str_update(ht, b"port", zval::long(port));
    hash::str_update(ht, b"type", zval::string(sd.ty.name().as_bytes()));
    let Some(h) = hello_zval(sd) else { return };
    hash::str_update(ht, b"hello_response", h);
    hash::str_update(ht, b"last_update_time", zval::long(sd.last_update_time_usec));
    hash::str_update(ht, b"round_trip_time", if sd.rtt_ms == RTT_UNSET { zval::null() } else { zval::long(sd.rtt_ms) });
}

unsafe extern "C" fn sd_get_properties(o: *mut sys::zend_object) -> *mut sys::HashTable {
    let d = obj::data::<ServerDesc>(o);
    let ht = d.props.get_or_init(6);
    sd_fill(d.sd.as_ref(), ht);
    ht
}

unsafe extern "C" fn sd_get_debug_info(o: *mut sys::zend_object, is_temp: *mut i32) -> *mut sys::HashTable {
    *is_temp = 1;
    let ht = hash::new(6);
    sd_fill(obj::data::<ServerDesc>(o).sd.as_ref(), ht);
    ht
}

pub unsafe fn init_MongoDB_Driver_ServerDescription(c: *mut sys::zend_class_entry) {
    CE_SD = c;
    (*c).__bindgen_anon_2.create_object = Some(create_sd);
    let h = obj::handlers::<ServerDesc>(addr_of_mut!(H_SD));
    h.get_properties = Some(sd_get_properties);
    h.get_debug_info = Some(sd_get_debug_info);
}

/// phongo_serverdescription_init: a ServerDescription object holding a copy of `sd`.
pub unsafe fn new_serverdescription(sd: ServerDescription) -> sys::zval {
    zval::obj(obj::alloc(CE_SD, ServerDesc { sd: Some(sd), props: Props::default() }, addr_of_mut!(H_SD)))
}

/// The description of `$this` (phongo dereferences NULL for an object not made by the driver;
/// we throw instead).
unsafe fn this_sd<'a>(ex: Ex) -> Option<&'a ServerDescription> {
    let d = obj::data::<ServerDesc>(call::this(ex)).sd.as_ref();
    if d.is_none() {
        throw(E::Logic, "MongoDB\\Driver\\ServerDescription is not initialized");
    }
    d
}

pub unsafe extern "C" fn zim_MongoDB_Driver_ServerDescription___construct(_ex: Ex, _rv: *mut sys::zval) {
    throw(E::Runtime, "Accessing private constructor");
}

pub unsafe extern "C" fn zim_MongoDB_Driver_ServerDescription_getHelloResponse(ex: Ex, rv: *mut sys::zval) {
    if !call::none(ex) {
        return;
    }
    let Some(sd) = this_sd(ex) else { return };
    if let Some(z) = hello_zval(sd) {
        zval::write(rv, z);
    }
}

pub unsafe extern "C" fn zim_MongoDB_Driver_ServerDescription_getHost(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        if let Some(sd) = this_sd(ex) {
            zval::write(rv, zval::string(super::server::host_port(&sd.addr).0.as_bytes()));
        }
    }
}

pub unsafe extern "C" fn zim_MongoDB_Driver_ServerDescription_getLastUpdateTime(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        if let Some(sd) = this_sd(ex) {
            zval::write(rv, zval::long(sd.last_update_time_usec));
        }
    }
}

pub unsafe extern "C" fn zim_MongoDB_Driver_ServerDescription_getPort(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        if let Some(sd) = this_sd(ex) {
            zval::write(rv, zval::long(super::server::host_port(&sd.addr).1));
        }
    }
}

pub unsafe extern "C" fn zim_MongoDB_Driver_ServerDescription_getRoundTripTime(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        if let Some(sd) = this_sd(ex) {
            zval::write(rv, if sd.rtt_ms == RTT_UNSET { zval::null() } else { zval::long(sd.rtt_ms) });
        }
    }
}

pub unsafe extern "C" fn zim_MongoDB_Driver_ServerDescription_getType(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        if let Some(sd) = this_sd(ex) {
            zval::write(rv, zval::string(sd.ty.name().as_bytes()));
        }
    }
}

pub unsafe extern "C" fn zim_MongoDB_Driver_Server_getServerDescription(ex: Ex, rv: *mut sys::zval) {
    if !call::none(ex) {
        return;
    }
    if let Some(sd) = super::server::sd(ex) {
        zval::write(rv, new_serverdescription(sd));
    }
}

// ---------------------------------------------------------------- TopologyDescription

unsafe extern "C" fn create_td(c: *mut sys::zend_class_entry) -> *mut sys::zend_object {
    obj::alloc(c, TopologyDesc::default(), addr_of_mut!(H_TD))
}

/// getServers(): the known servers' descriptions (mongoc_topology_description_get_servers).
unsafe fn servers_zval(td: &TopologyDescription) -> sys::zval {
    let known: Vec<&ServerDescription> = td.known_servers().collect();
    let ht = hash::new(known.len() as u32);
    for sd in known {
        hash::push(ht, new_serverdescription(sd.clone()));
    }
    zval::arr(ht)
}

/// phongo_topologydescription_get_properties_hash
unsafe fn td_fill(td: Option<&TopologyDescription>, ht: *mut sys::HashTable) {
    let Some(td) = td else { return };
    hash::str_update(ht, b"servers", servers_zval(td));
    hash::str_update(ht, b"type", zval::string(td.ty.name().as_bytes()));
}

unsafe extern "C" fn td_get_properties(o: *mut sys::zend_object) -> *mut sys::HashTable {
    let d = obj::data::<TopologyDesc>(o);
    let ht = d.props.get_or_init(2);
    td_fill(d.td.as_ref(), ht);
    ht
}

unsafe extern "C" fn td_get_debug_info(o: *mut sys::zend_object, is_temp: *mut i32) -> *mut sys::HashTable {
    *is_temp = 1;
    let ht = hash::new(2);
    td_fill(obj::data::<TopologyDesc>(o).td.as_ref(), ht);
    ht
}

pub unsafe fn init_MongoDB_Driver_TopologyDescription(c: *mut sys::zend_class_entry) {
    CE_TD = c;
    (*c).__bindgen_anon_2.create_object = Some(create_td);
    let h = obj::handlers::<TopologyDesc>(addr_of_mut!(H_TD));
    h.get_properties = Some(td_get_properties);
    h.get_debug_info = Some(td_get_debug_info);
}

/// phongo_topologydescription_init: a TopologyDescription object holding a copy of `td`.
pub unsafe fn new_topologydescription(td: TopologyDescription) -> sys::zval {
    zval::obj(obj::alloc(CE_TD, TopologyDesc { td: Some(td), props: Props::default() }, addr_of_mut!(H_TD)))
}

unsafe fn this_td<'a>(ex: Ex) -> Option<&'a TopologyDescription> {
    let d = obj::data::<TopologyDesc>(call::this(ex)).td.as_ref();
    if d.is_none() {
        throw(E::Logic, "MongoDB\\Driver\\TopologyDescription is not initialized");
    }
    d
}

pub unsafe extern "C" fn zim_MongoDB_Driver_TopologyDescription___construct(_ex: Ex, _rv: *mut sys::zval) {
    throw(E::Runtime, "Accessing private constructor");
}

pub unsafe extern "C" fn zim_MongoDB_Driver_TopologyDescription_getServers(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        if let Some(td) = this_td(ex) {
            zval::write(rv, servers_zval(td));
        }
    }
}

pub unsafe extern "C" fn zim_MongoDB_Driver_TopologyDescription_getType(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        if let Some(td) = this_td(ex) {
            zval::write(rv, zval::string(td.ty.name().as_bytes()));
        }
    }
}

/// mongoc_topology_description_has_readable_server / _has_writable_server: a compatible
/// topology with a server selectable for `op`.
fn selectable(td: &TopologyDescription, op: OpType, rp: Option<&mongo_core::topology::ReadPrefs>) -> bool {
    if td.compatible(rp).is_err() {
        return false;
    }
    // the local threshold does not matter for "is there one"
    td.clone().select(op, rp, &[], 0).0.is_some()
}

pub unsafe extern "C" fn zim_MongoDB_Driver_TopologyDescription_hasReadableServer(ex: Ex, rv: *mut sys::zval) {
    let mut z: *mut sys::zval = null_mut();
    if sys::zend_parse_parameters(call::num_args(ex), c"|O".as_ptr(), &mut z, super::readpref::CE) != 0 {
        return;
    }
    let Some(td) = this_td(ex) else { return };
    let rp = (!z.is_null()).then(|| obj::data::<super::readpref::ReadPreference>((*z).value.obj).prefs());
    zval::write(rv, zval::bool(selectable(td, OpType::Read, rp.as_ref())));
}

pub unsafe extern "C" fn zim_MongoDB_Driver_TopologyDescription_hasWritableServer(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        if let Some(td) = this_td(ex) {
            zval::write(rv, zval::bool(selectable(td, OpType::Write, None)));
        }
    }
}
