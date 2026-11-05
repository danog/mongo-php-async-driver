//! MongoDB\Driver: the Manager and the classes that talk to servers (phongo_client.c,
//! phongo_execute.c, phongo_error.c and src/MongoDB/*.c of ext-mongodb).

#![allow(non_snake_case)]

pub mod command;
pub mod cursor;
pub mod manager;
pub mod server;

use crate::zend::{call, class, sys, zval};
use mongo_core::client::Client;
use mongo_core::error::{code, domain, Error};
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::CStr;
use std::rc::Rc;

pub type ClientRef = Rc<RefCell<Client>>;

thread_local! {
    /// phongo's persistent clients: one client per URI (+ options) per process.
    static CLIENTS: RefCell<HashMap<String, (u32, ClientRef)>> = RefCell::new(HashMap::new());
}

/// The client for `key` (created by `make` when absent or created by another process).
pub fn persistent_client(key: &str, make: impl FnOnce() -> Client) -> ClientRef {
    let pid = std::process::id();
    CLIENTS.with(|c| {
        let mut m = c.borrow_mut();
        if let Some((p, cl)) = m.get(key) {
            if *p == pid {
                return cl.clone();
            }
            // created before a fork: its sockets belong to the parent; leak them
            if let Some((_, old)) = m.remove(key) {
                std::mem::forget(old);
            }
        }
        let cl = Rc::new(RefCell::new(make()));
        m.insert(key.to_owned(), (pid, cl.clone()));
        cl
    })
}

unsafe fn lookup(n: &CStr) -> *mut sys::zend_class_entry {
    class::lookup(n)
}

/// phongo_exception_from_mongoc_domain
unsafe fn exception_ce(e: &Error) -> *mut sys::zend_class_entry {
    let n: &CStr = match (e.domain, e.code) {
        (domain::CLIENT, code::CLIENT_AUTHENTICATE) => c"mongodb\\driver\\exception\\authenticationexception",
        (domain::CLIENT, code::CLIENT_INVALID_ENCRYPTION_ARG) => c"mongodb\\driver\\exception\\invalidargumentexception",
        (domain::COMMAND, code::COMMAND_INVALID_ARG) => c"mongodb\\driver\\exception\\invalidargumentexception",
        (domain::SERVER, 50) => c"mongodb\\driver\\exception\\executiontimeoutexception",
        (domain::SERVER, _) => c"mongodb\\driver\\exception\\serverexception",
        (domain::SERVER_SELECTION, code::SERVER_SELECTION_FAILURE) => c"mongodb\\driver\\exception\\connectiontimeoutexception",
        (domain::STREAM, code::STREAM_SOCKET) => c"mongodb\\driver\\exception\\connectiontimeoutexception",
        (domain::STREAM, _) => c"mongodb\\driver\\exception\\connectionexception",
        (domain::WRITE_CONCERN, _) => c"mongodb\\driver\\exception\\serverexception",
        (domain::PROTOCOL, code::PROTOCOL_BAD_WIRE_VERSION) => c"mongodb\\driver\\exception\\connectionexception",
        (domain::CLIENT_SIDE_ENCRYPTION, _) => c"mongodb\\driver\\exception\\encryptionexception",
        _ => c"mongodb\\driver\\exception\\runtimeexception",
    };
    lookup(n)
}

unsafe fn throw_code(ce: *mut sys::zend_class_entry, msg: &str, code: i64) -> *mut sys::zend_object {
    let c = call::cs(msg);
    sys::zend_throw_exception(ce, c.as_ptr(), code)
}

unsafe fn update_prop(o: *mut sys::zend_object, name: &[u8], v: *mut sys::zval) {
    sys::zend_update_property((*o).ce, o, name.as_ptr() as *const _, name.len(), v);
}

/// phongo_throw_exception_from_bson_error_t_and_reply
pub unsafe fn throw_error(e: &Error, reply: Option<&[u8]>) {
    let ex;
    if let Some(r) = reply.filter(|_| (e.domain == domain::SERVER && e.code != 50) || e.domain == domain::WRITE_CONCERN) {
        ex = throw_code(lookup(c"mongodb\\driver\\exception\\commandexception"), &e.message, e.code as i64);
        let tm = crate::bson::decode::TypeMap::default();
        let st = crate::bson::decode::State::new(&tm);
        if let Some(mut z) = crate::bson::decode::to_zval(r, &st) {
            update_prop(ex, b"resultDocument", &mut z);
            zval::dtor(&mut z);
        }
    } else {
        ex = throw_code(exception_ce(e), &e.message, e.code as i64);
    }
    if let Some(r) = reply {
        add_error_labels(ex, r);
    }
}

/// phongo_exception_add_error_labels
unsafe fn add_error_labels(ex: *mut sys::zend_object, reply: &[u8]) {
    use mongo_core::bson::iter::{Iter, Value};
    if ex.is_null() || !crate::zend::obj::instanceof((*ex).ce, lookup(c"mongodb\\driver\\exception\\runtimeexception")) {
        return;
    }
    let labels = crate::zend::hash::new(0);
    let mut n = 0;
    let mut add = |arr: &[u8]| {
        if let Some(mut it) = Iter::new(arr) {
            while it.next() {
                if let Value::Utf8(s) = it.value() {
                    crate::zend::hash::push(labels, zval::string(s));
                    n += 1;
                }
            }
        }
    };
    let find = |d: &'_ [u8], k: &[u8]| -> Option<Vec<u8>> {
        let mut it = Iter::new(d)?;
        if !it.find(k) {
            return None;
        }
        match it.value() {
            Value::Array(a) | Value::Document(a) => Some(a.to_vec()),
            _ => None,
        }
    };
    if let Some(a) = find(reply, b"errorLabels") {
        add(&a);
    }
    if let Some(w) = find(reply, b"writeConcernError") {
        if let Some(a) = find(&w, b"errorLabels") {
            add(&a);
        }
    }
    if let Some(ws) = find(reply, b"writeConcernErrors") {
        if let Some(mut it) = Iter::new(&ws) {
            while it.next() {
                if let Value::Document(w) = it.value() {
                    if let Some(a) = find(w, b"errorLabels") {
                        add(&a);
                    }
                }
            }
        }
    }
    let mut z = zval::arr(labels);
    if n > 0 {
        update_prop(ex, b"errorLabels", &mut z);
    }
    zval::dtor(&mut z);
}

/// PHONGO_BSON_INIT_DEBUG_STATE's type map: arrays everywhere.
pub fn debug_typemap() -> crate::bson::decode::TypeMap {
    use crate::bson::decode::TM;
    crate::bson::decode::TypeMap { document: TM::Array, array: TM::Array, root: TM::Array, ..Default::default() }
}

/// Decodes `doc` with `tm` (None: an exception was thrown).
pub unsafe fn to_zval(doc: &[u8], tm: &crate::bson::decode::TypeMap) -> Option<sys::zval> {
    let st = crate::bson::decode::State::new(tm);
    crate::bson::decode::to_zval(doc, &st)
}

pub unsafe extern "C" fn zim_MongoDB_Driver_Exception_CommandException_getResultDocument(ex: call::Ex, rv: *mut sys::zval) {
    if !call::none(ex) {
        return;
    }
    let o = call::this(ex);
    let mut tmp = zval::undef();
    let z = sys::zend_read_property((*o).ce, o, c"resultDocument".as_ptr(), 14, true, &mut tmp);
    *rv = zval::copy(zval::deref(z));
}
