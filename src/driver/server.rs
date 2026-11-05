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
}

static mut H_SERVER: sys::zend_object_handlers = unsafe { std::mem::zeroed() };
pub static mut CE: *mut sys::zend_class_entry = null_mut();

unsafe extern "C" fn create(c: *mut sys::zend_class_entry) -> *mut sys::zend_object {
    obj::alloc(c, Server::default(), addr_of_mut!(H_SERVER))
}

/// Server objects compare equal when they name the same server of the same client.
unsafe extern "C" fn compare(a: *mut sys::zval, b: *mut sys::zval) -> i32 {
    if !zval::is(a, sys::IS_OBJECT) || !zval::is(b, sys::IS_OBJECT) || (*(*a).value.obj).handlers != (*(*b).value.obj).handlers {
        return sys::zend_std_compare_objects(a, b);
    }
    let (x, y) = (obj::data::<Server>((*a).value.obj), obj::data::<Server>((*b).value.obj));
    let (Some(cx), Some(cy)) = (&x.client, &y.client) else { return 1 };
    let (sx, sy) = (cx.borrow().td.server(x.server_id).map(|s| s.addr.clone()), cy.borrow().td.server(y.server_id).map(|s| s.addr.clone()));
    if sx.is_some() && sx == sy { 0 } else { 1 }
}

pub unsafe fn init_MongoDB_Driver_Server(c: *mut sys::zend_class_entry) {
    CE = c;
    (*c).__bindgen_anon_2.create_object = Some(create);
    let h = obj::handlers::<Server>(addr_of_mut!(H_SERVER));
    h.compare = Some(compare);
}

pub unsafe fn new_server(client: ClientRef, server_id: u32) -> sys::zval {
    zval::obj(obj::alloc(CE, Server { client: Some(client), server_id }, addr_of_mut!(H_SERVER)))
}

#[inline]
unsafe fn me<'a>(ex: Ex) -> &'a mut Server {
    obj::data::<Server>(call::this(ex))
}

/// The current description of the server (a copy), or a RuntimeException.
unsafe fn sd(ex: Ex) -> Option<ServerDescription> {
    let s = me(ex);
    let d = s.client.as_ref().and_then(|c| c.borrow().td.server(s.server_id).cloned());
    if d.is_none() {
        throw(E::Runtime, "Failed to get server description");
    }
    d
}

fn host_port(addr: &str) -> (String, i64) {
    match addr.rsplit_once(':') {
        Some((h, p)) => (h.trim_start_matches('[').trim_end_matches(']').to_owned(), p.parse().unwrap_or(27017)),
        None => (addr.to_owned(), 27017),
    }
}

fn hello_flag(sd: &ServerDescription, k: &[u8]) -> bool {
    Iter::new(&sd.hello).is_some_and(|mut it| it.find(k) && matches!(it.value(), Value::Bool(true)))
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
            if let Some(z) = super::to_zval(&d.hello, &super::debug_typemap()) {
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
            if let Some(z) = super::to_zval(&d.tags, &super::debug_typemap()) {
                zval::write(rv, z);
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
            super::manager::execute_command_args(ex, rv, c, $ty, Some(s.server_id));
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
    super::manager::execute_bulk_write(ex, rv, c, Some(s.server_id));
}

pub unsafe extern "C" fn zim_MongoDB_Driver_Server_executeQuery(ex: Ex, rv: *mut sys::zval) {
    let s = me(ex);
    let Some(c) = s.client.clone() else { return };
    super::manager::execute_query(ex, rv, c, Some(s.server_id));
}
