//! MongoDB\Driver\Server (src/MongoDB/Server.c)

use super::ClientRef;
use crate::bson::{throw, E};
use crate::zend::call::{self, Ex};
use crate::zend::{obj, sys, zval};
use mongo_core::bson::iter::{Iter, Value};
use mongo_core::sdam::{ServerDescription, ServerType, RTT_UNSET};
use std::ptr::{addr_of_mut, null_mut};

#[derive(Default)]
pub struct Server {
    pub client: Option<ClientRef>,
    pub server_id: u32,
    /// the Manager it came from (phongo_server_t.manager); released last
    pub manager: super::ManagerRef,
}

static mut H_SERVER: sys::zend_object_handlers = unsafe { std::mem::zeroed() };
pub static mut CE: *mut sys::zend_class_entry = null_mut();

unsafe extern "C" fn create(c: *mut sys::zend_class_entry) -> *mut sys::zend_object {
    obj::alloc(c, Server::default(), addr_of_mut!(H_SERVER))
}

/// The current description of a Server object's server (mongoc_client_get_server_description).
fn description(s: &Server) -> Option<ServerDescription> {
    s.client.as_ref().and_then(|c| c.try_borrow().ok().and_then(|cl| cl.td.server(s.server_id).cloned()))
}

/// strcasecmp (C locale)
fn strcasecmp(a: &[u8], b: &[u8]) -> i32 {
    for i in 0.. {
        let (x, y) = (a.get(i).map_or(0, u8::to_ascii_lowercase), b.get(i).map_or(0, u8::to_ascii_lowercase));
        if x != y || x == 0 {
            return x as i32 - y as i32;
        }
    }
    unreachable!()
}

/// phongo_server_compare_objects: by host_and_port, case-insensitively; a RuntimeException
/// when a description is missing.
unsafe extern "C" fn compare(a: *mut sys::zval, b: *mut sys::zval) -> i32 {
    // ZEND_COMPARE_OBJECTS_FALLBACK
    if !zval::is(a, sys::IS_OBJECT) || !zval::is(b, sys::IS_OBJECT) || (*(*a).value.obj).handlers != (*(*b).value.obj).handlers {
        return sys::zend_std_compare_objects(a, b);
    }
    let (x, y) = (obj::data::<Server>((*a).value.obj), obj::data::<Server>((*b).value.obj));
    match (description(x), description(y)) {
        (Some(sx), Some(sy)) => strcasecmp(sx.addr.as_bytes(), sy.addr.as_bytes()),
        _ => {
            throw(E::Runtime, "Failed to get server description(s)");
            0
        }
    }
}

/// The hello response phongo reports: a load balancer's comes from the handshake (the
/// backing server). None after throwing.
fn reported_hello(s: &Server, sd: &ServerDescription) -> Option<Vec<u8>> {
    if sd.ty != ServerType::LoadBalancer {
        return Some(sd.hello.clone());
    }
    let c = s.client.as_ref()?;
    let Ok(mut cl) = c.try_borrow_mut() else {
        unsafe { throw(E::Runtime, "Failed to get handshake server description: the client is in use") };
        return None;
    };
    // mongoc_client_get_handshake_description: connects first if needed
    let r = cl.stream_for_server(s.server_id, true).map(|_| ());
    match (r, cl.handshake_description(s.server_id)) {
        (Ok(()), Some(h)) => Some(h.hello.clone()),
        (Err(e), _) => {
            drop(cl);
            unsafe { throw(E::Runtime, format!("Failed to get handshake server description: {}", e.message)) };
            None
        }
        (Ok(()), None) => {
            drop(cl);
            unsafe { throw(E::Runtime, "Failed to get handshake server description: ") };
            None
        }
    }
}

/// phongo_server_get_debug_info
unsafe extern "C" fn get_debug_info(o: *mut sys::zend_object, is_temp: *mut i32) -> *mut sys::HashTable {
    use crate::zend::hash;
    *is_temp = 1;
    let s = obj::data::<Server>(o);
    let Some(sd) = description(s) else {
        throw(E::Runtime, "Failed to get server description");
        return null_mut();
    };
    let (host, port) = host_port(&sd.addr);
    let ht = hash::new(8);
    hash::str_update(ht, b"host", zval::string(host.as_bytes()));
    hash::str_update(ht, b"port", zval::long(port));
    hash::str_update(ht, b"type", zval::long(sd.ty as i64));
    hash::str_update(ht, b"is_primary", zval::bool(sd.ty == ServerType::RsPrimary));
    hash::str_update(ht, b"is_secondary", zval::bool(sd.ty == ServerType::RsSecondary));
    hash::str_update(ht, b"is_arbiter", zval::bool(sd.ty == ServerType::RsArbiter));
    hash::str_update(ht, b"is_hidden", zval::bool(hello_flag(&sd, b"hidden")));
    hash::str_update(ht, b"is_passive", zval::bool(hello_flag(&sd, b"passive")));
    if let Some(tags) = hello_tags(&sd) {
        match super::to_zval(tags, &super::debug_typemap()) {
            Some(z) => hash::str_update(ht, b"tags", z),
            None => return release(ht),
        }
    }
    let Some(hello) = reported_hello(s, &sd) else { return release(ht) };
    let doc: &[u8] = if hello.is_empty() { &[5, 0, 0, 0, 0] } else { &hello };
    match super::to_zval(doc, &super::debug_typemap()) {
        Some(z) => hash::str_update(ht, b"last_hello_response", z),
        None => return release(ht),
    }
    hash::str_update(ht, b"round_trip_time", if sd.rtt_ms == RTT_UNSET { zval::null() } else { zval::long(sd.rtt_ms) });
    ht
}

/// Frees a debug-info table being abandoned after an exception.
unsafe fn release(ht: *mut sys::HashTable) -> *mut sys::HashTable {
    sys::zend_array_destroy(ht);
    null_mut()
}

/// The "tags" document of the hello response, if it is one.
fn hello_tags(sd: &ServerDescription) -> Option<&[u8]> {
    let mut it = Iter::new(&sd.hello)?;
    if !it.find(b"tags") {
        return None;
    }
    match it.value() {
        Value::Document(d) => Some(d),
        _ => None,
    }
}

pub unsafe fn init_MongoDB_Driver_Server(c: *mut sys::zend_class_entry) {
    CE = c;
    (*c).__bindgen_anon_2.create_object = Some(create);
    let h = obj::handlers::<Server>(addr_of_mut!(H_SERVER));
    h.compare = Some(compare);
    h.get_debug_info = Some(get_debug_info);
    h.get_gc = Some(get_gc);
}

unsafe extern "C" fn get_gc(o: *mut sys::zend_object, table: *mut *mut sys::zval, n: *mut i32) -> *mut sys::HashTable {
    let s = obj::data::<Server>(o);
    obj::get_gc_with(o, table, n, [s.manager.gc()])
}

/// phongo_server_init: a Server of the Manager object `manager`.
pub unsafe fn new_server(client: ClientRef, server_id: u32, manager: *mut sys::zend_object) -> sys::zval {
    zval::obj(obj::alloc(CE, Server { client: Some(client), server_id, manager: super::ManagerRef::new(manager) }, addr_of_mut!(H_SERVER)))
}

#[inline]
unsafe fn me<'a>(ex: Ex) -> &'a mut Server {
    obj::data::<Server>(call::this(ex))
}

/// The current description of the server (a copy), or a RuntimeException.
pub(crate) unsafe fn sd(ex: Ex) -> Option<ServerDescription> {
    let d = description(me(ex));
    if d.is_none() {
        throw(E::Runtime, "Failed to get server description");
    }
    d
}

pub(crate) fn host_port(addr: &str) -> (String, i64) {
    match addr.rsplit_once(':') {
        Some((h, p)) => (h.trim_start_matches('[').trim_end_matches(']').to_owned(), p.parse().unwrap_or(27017)),
        None => (addr.to_owned(), 27017),
    }
}

/// bson_iter_init_find_case(hello, k) && bson_iter_as_bool
fn hello_flag(sd: &ServerDescription, k: &[u8]) -> bool {
    Iter::new(&sd.hello).is_some_and(|mut it| it.find_case(k) && it.value().as_bool())
}

pub unsafe extern "C" fn zim_MongoDB_Driver_Server___construct(_ex: Ex, _rv: *mut sys::zval) {
    throw(E::Runtime, "Accessing private constructor");
}

pub unsafe extern "C" fn zim_MongoDB_Driver_Server_getHost(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        if let Some(d) = sd(ex) {
            zval::write(rv, zval::string(host_port(&d.addr).0.as_bytes()));
        }
    }
}
pub unsafe extern "C" fn zim_MongoDB_Driver_Server_getPort(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        if let Some(d) = sd(ex) {
            zval::write(rv, zval::long(host_port(&d.addr).1));
        }
    }
}
pub unsafe extern "C" fn zim_MongoDB_Driver_Server_getInfo(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        if let Some(d) = sd(ex) {
            let Some(hello) = reported_hello(me(ex), &d) else { return };
            let doc: &[u8] = if hello.is_empty() { &[5, 0, 0, 0, 0] } else { &hello };
            if let Some(z) = super::to_zval(doc, &super::debug_typemap()) {
                zval::write(rv, z);
            }
        }
    }
}
pub unsafe extern "C" fn zim_MongoDB_Driver_Server_getLatency(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        if let Some(d) = sd(ex) {
            zval::write(rv, if d.rtt_ms == RTT_UNSET { zval::null() } else { zval::long(d.rtt_ms) });
        }
    }
}
pub unsafe extern "C" fn zim_MongoDB_Driver_Server_getTags(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        if let Some(d) = sd(ex) {
            match hello_tags(&d) {
                Some(t) => {
                    if let Some(z) = super::to_zval(t, &super::debug_typemap()) {
                        zval::write(rv, z);
                    }
                }
                None => zval::write(rv, zval::arr(crate::zend::hash::new(0))),
            }
        }
    }
}
pub unsafe extern "C" fn zim_MongoDB_Driver_Server_getType(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        if let Some(d) = sd(ex) {
            zval::write(rv, zval::long(d.ty as i64));
        }
    }
}
macro_rules! is_type {
    ($f:ident, $t:expr) => {
        pub unsafe extern "C" fn $f(ex: Ex, rv: *mut sys::zval) {
            if call::none(ex) {
                if let Some(d) = sd(ex) {
                    zval::write(rv, zval::bool(d.ty == $t));
                }
            }
        }
    };
}
is_type!(zim_MongoDB_Driver_Server_isPrimary, ServerType::RsPrimary);
is_type!(zim_MongoDB_Driver_Server_isSecondary, ServerType::RsSecondary);
is_type!(zim_MongoDB_Driver_Server_isArbiter, ServerType::RsArbiter);
pub unsafe extern "C" fn zim_MongoDB_Driver_Server_isHidden(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        if let Some(d) = sd(ex) {
            zval::write(rv, zval::bool(hello_flag(&d, b"hidden")));
        }
    }
}
pub unsafe extern "C" fn zim_MongoDB_Driver_Server_isPassive(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        if let Some(d) = sd(ex) {
            zval::write(rv, zval::bool(hello_flag(&d, b"passive")));
        }
    }
}

macro_rules! exec {
    ($f:ident, $ty:expr) => {
        pub unsafe extern "C" fn $f(ex: Ex, rv: *mut sys::zval) {
            let s = me(ex);
            let Some(c) = s.client.clone() else { return };
            super::manager::execute_command_args(ex, rv, c, $ty, Some(s.server_id), s.manager.obj());
        }
    };
}
exec!(zim_MongoDB_Driver_Server_executeCommand, super::manager::CmdType::Raw);
exec!(zim_MongoDB_Driver_Server_executeReadCommand, super::manager::CmdType::Read);
exec!(zim_MongoDB_Driver_Server_executeWriteCommand, super::manager::CmdType::Write);
exec!(zim_MongoDB_Driver_Server_executeReadWriteCommand, super::manager::CmdType::ReadWrite);

pub unsafe extern "C" fn zim_MongoDB_Driver_Server_executeBulkWrite(ex: Ex, rv: *mut sys::zval) {
    let s = me(ex);
    let Some(c) = s.client.clone() else { return };
    super::manager::execute_bulk_write(ex, rv, c, Some(s.server_id), s.manager.obj());
}

pub unsafe extern "C" fn zim_MongoDB_Driver_Server_executeQuery(ex: Ex, rv: *mut sys::zval) {
    let s = me(ex);
    let Some(c) = s.client.clone() else { return };
    super::manager::execute_query(ex, rv, c, Some(s.server_id), s.manager.obj());
}
