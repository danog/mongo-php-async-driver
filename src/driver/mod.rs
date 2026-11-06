//! MongoDB\Driver: the Manager and the classes that talk to servers (phongo_client.c,
//! phongo_execute.c, phongo_error.c and src/MongoDB/*.c of ext-mongodb).

#![allow(non_snake_case)]

pub mod apm;
pub mod apm_sdam;
pub mod bulkwrite;
pub mod bulkwritecommand;
pub mod command;
pub mod cursor;
pub mod manager;
pub mod query;
pub mod readconcern;
pub mod readpref;
pub mod sdam;
pub mod server;
pub mod serverapi;
pub mod session;
pub mod writeconcern;
pub mod uri_options;
pub mod writeresult;

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
    static CLIENTS: RefCell<HashMap<Vec<u8>, ClientRef>> = RefCell::new(HashMap::new());
}

/// The client for `key` (created by `make` when absent). A client inherited across fork() is
/// kept: it resets itself before its next I/O (Client::check_fork), as phongo resets its
/// persistent clients once per pid.
/// The persistent client of a client hash (phongo_find_persistent_client).
pub fn find_persistent_client(key: &[u8]) -> Option<ClientRef> {
    CLIENTS.with(|c| c.borrow().get(key).cloned())
}

/// Registers a persistent client under its hash (phongo_client_register).
pub fn store_persistent_client(key: Vec<u8>, c: ClientRef) {
    CLIENTS.with(|m| m.borrow_mut().insert(key, c));
}

/// MSHUTDOWN (phongo_pclient_destroy_all at GSHUTDOWN): the persistent clients are destroyed.
/// One created by this process ends its pooled server sessions first (mongoc_client_destroy);
/// connections inherited across fork() are forgotten, never closed or written to.
pub fn destroy_persistent_clients() {
    let clients = CLIENTS.with(|c| std::mem::take(&mut *c.borrow_mut()));
    for (_, c) in clients {
        if let Ok(mut cl) = c.try_borrow_mut() {
            cl.check_fork();
            cl.end_pooled_sessions();
            // no subscribers are left to notify
            cl.clear_events();
        }
    }
}

/// phongo's `zval manager` of a Server, Cursor, Session or WriteResult: an owned reference to
/// the Manager object, so the Manager (its subscribers, a disableClientPersistence client)
/// lives as long as the objects created from it. The objects keep their own client reference
/// for I/O: the Manager's state is never read through this (the GC may free a cycle's Manager
/// before the objects that hold it).
#[derive(Default)]
pub struct ManagerRef(sys::zval);

impl ManagerRef {
    /// A reference to the Manager object `m` (none for null).
    pub unsafe fn new(m: *mut sys::zend_object) -> ManagerRef {
        if m.is_null() {
            return ManagerRef::default();
        }
        ManagerRef(zval::copy(&*zval::borrowed_obj(m)))
    }

    /// The Manager object (null: none).
    pub fn obj(&self) -> *mut sys::zend_object {
        if unsafe { zval::is(&self.0, sys::IS_OBJECT) } {
            unsafe { self.0.value.obj }
        } else {
            std::ptr::null_mut()
        }
    }

    /// The zval, for get_gc.
    pub fn gc(&mut self) -> *mut sys::zval {
        &mut self.0
    }
}

/// free_obj of the classes whose destruction may notify subscribers (Manager, Cursor,
/// Session): when the GC frees the object as garbage, subscribers are not called (they may
/// be garbage of the same cycle, about to be freed); the events stay queued.
pub unsafe extern "C" fn free_obj<T>(o: *mut sys::zend_object) {
    if crate::zend::obj::freed_by_gc(o) {
        apm::quiet(|| crate::zend::obj::free::<T>(o));
    } else {
        crate::zend::obj::free::<T>(o);
    }
}

/// Whether `c` is one of the persistent clients.
pub fn is_persistent(c: &ClientRef) -> bool {
    CLIENTS.with(|m| m.borrow().values().any(|x| Rc::ptr_eq(x, c)))
}

pub(crate) unsafe fn lookup(n: &CStr) -> *mut sys::zend_class_entry {
    class::lookup(n)
}

/// phongo_exception_from_mongoc_domain
pub(crate) unsafe fn exception_ce(e: &Error) -> *mut sys::zend_class_entry {
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

/// Decodes `doc` with the default type map and no ODM (PHONGO_BSON_INIT_NO_ODM_STATE).
pub unsafe fn to_zval_no_odm(doc: &[u8]) -> Option<sys::zval> {
    let tm = crate::bson::decode::TypeMap::default();
    let mut st = crate::bson::decode::State::new(&tm);
    st.skip_odm = true;
    crate::bson::decode::to_zval(doc, &st)
}

pub unsafe extern "C" fn zim_MongoDB_Driver_Exception_CommandException_getResultDocument(ex: call::Ex, rv: *mut sys::zval) {
    if !call::none(ex) {
        return;
    }
    let o = call::this(ex);
    let mut tmp = zval::undef();
    let z = sys::zend_read_property((*o).ce, o, c"resultDocument".as_ptr(), 14, true, &mut tmp);
    zval::write(rv, zval::copy(zval::deref(z)));
}

/// zend_get_type_by_const(Z_TYPE_P(z))
pub unsafe fn type_by_const(z: *const sys::zval) -> String {
    let p = sys::zend_get_type_by_const(zval::type_of(z) as i32);
    if p.is_null() {
        return "unknown".into();
    }
    CStr::from_ptr(p).to_string_lossy().into_owned()
}

/// phongo_bson_value_to_zval_legacy: the value of the single element of `doc` (documents and
/// arrays as stdClass / array).
pub unsafe fn legacy_value(doc: &[u8]) -> Option<sys::zval> {
    let tm = crate::bson::decode::TypeMap { root: crate::bson::decode::TM::Array, ..Default::default() };
    let z = to_zval(doc, &tm)?;
    let mut r = None;
    if zval::is(&z, sys::IS_ARRAY) {
        crate::zend::hash::each(z.value.arr, true, |_, v| {
            r = Some(zval::copy(v));
            false
        });
    }
    r
}

/// phongo_parse_session: the "session" option (a MongoDB\Driver\Session of this client).
pub unsafe fn parse_session(options: *mut sys::zval, client: &ClientRef) -> Result<Option<*mut sys::zval>, ()> {
    if options.is_null() {
        return Ok(None);
    }
    if !zval::is(options, sys::IS_ARRAY) {
        crate::bson::throw(crate::bson::E::InvalidArgument, format!("Expected options to be array, {} given", zval::type_name(options)));
        return Err(());
    }
    let Some(z) = crate::zend::hash::sym_find((*options).value.arr, b"session") else { return Ok(None) };
    let z = zval::deref(z);
    let ce = lookup(c"mongodb\\driver\\session");
    if !zval::is(z, sys::IS_OBJECT) || !crate::zend::obj::instanceof((*(*z).value.obj).ce, ce) {
        crate::bson::throw(crate::bson::E::InvalidArgument, format!("Expected \"session\" option to be MongoDB\\Driver\\Session, {} given", zval::type_name(z)));
        return Err(());
    }
    if !session_belongs_to(z, client) {
        crate::bson::throw(crate::bson::E::InvalidArgument, "Cannot use Session started from a different Manager");
        return Err(());
    }
    Ok(Some(z))
}

/// Whether the Session object `z` was started from `client`.
unsafe fn session_belongs_to(z: *mut sys::zval, client: &ClientRef) -> bool {
    session::of(z).client.as_ref().is_some_and(|c| Rc::ptr_eq(c, client))
}

/// The server a Session is pinned to (0: none).
pub unsafe fn session_server_id(z: *mut sys::zval) -> u32 {
    session::of(z).cs.as_ref().map_or(0, |c| c.borrow().pinned_server_id)
}

/// The client session of the Session object `z` (an ended one: a fresh, empty state).
pub unsafe fn session_rc(z: *mut sys::zval) -> Rc<RefCell<mongo_core::session::ClientSession>> {
    session::of(z).cs.clone().unwrap_or_default()
}

/// Runs `f` with the client session of the Session object `z`.
pub unsafe fn with_session<R>(z: *mut sys::zval, f: impl FnOnce(&mut mongo_core::session::ClientSession) -> R) -> R {
    let cs = session::of(z).cs.clone().unwrap_or_default();
    let mut b = cs.borrow_mut();
    f(&mut b)
}

/// phongo_parse_write_concern: the "writeConcern" option.
pub unsafe fn parse_write_concern(options: *mut sys::zval) -> Result<Option<writeconcern::WriteConcern>, ()> {
    if options.is_null() {
        return Ok(None);
    }
    if !zval::is(options, sys::IS_ARRAY) {
        crate::bson::throw(crate::bson::E::InvalidArgument, format!("Expected options to be array, {} given", zval::type_name(options)));
        return Err(());
    }
    let Some(z) = crate::zend::hash::sym_find((*options).value.arr, b"writeConcern") else { return Ok(None) };
    let z = zval::deref(z);
    if !zval::is(z, sys::IS_OBJECT) || !crate::zend::obj::instanceof((*(*z).value.obj).ce, writeconcern::CE) {
        crate::bson::throw(crate::bson::E::InvalidArgument, format!("Expected \"writeConcern\" option to be MongoDB\\Driver\\WriteConcern, {} given", zval::type_name(z)));
        return Err(());
    }
    Ok(Some(crate::zend::obj::data::<writeconcern::WriteConcern>((*z).value.obj).clone()))
}

/// phongo_split_namespace (after phongo_validate_namespace)
pub unsafe fn split_namespace(ns: &[u8]) -> Option<(String, String)> {
    if ns.contains(&0) {
        crate::bson::throw(crate::bson::E::InvalidArgument, "Invalid namespace provided: namespaces may not contain a null byte");
        return None;
    }
    let s = String::from_utf8_lossy(ns);
    match s.split_once('.') {
        Some((d, c)) => Some((d.to_owned(), c.to_owned())),
        None => {
            crate::bson::throw(crate::bson::E::InvalidArgument, format!("Invalid namespace provided: {s}"));
            None
        }
    }
}
