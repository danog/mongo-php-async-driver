//! MongoDB\Driver\Session (src/MongoDB/Session.c over mongoc_client_session_t).

use super::readpref::ReadPreference;
use super::ClientRef;
use crate::bson::{throw, E};
use crate::zend::call::{self, Ex};
use crate::zend::{hash, obj, sys, zval};
use mongo_core::session::{ClientSession, TxnOpts, TxnState};
use mongo_core::uri::ReadMode;
use std::cell::RefCell;
use std::ptr::{addr_of_mut, null_mut};
use std::rc::Rc;

#[derive(Default)]
pub struct Session {
    /// None once ended
    pub cs: Option<Rc<RefCell<ClientSession>>>,
    pub client: Option<ClientRef>,
    /// the default transaction read preference (the client's, or the session option's)
    pub default_rp: ReadPreference,
    /// the running transaction's read preference
    pub txn_rp: ReadPreference,
}

impl Drop for Session {
    /// phongo_session_free_object: mongoc_client_session_destroy (a running transaction is
    /// aborted; after a fork the server session is dropped without any I/O).
    fn drop(&mut self) {
        end(self);
    }
}

/// mongoc_client_session_destroy
fn end(s: &mut Session) {
    let Some(cs) = s.cs.take() else { return };
    let Some(c) = s.client.as_ref() else { return };
    // never re-entered from a running operation of the same client: if it is busy, the
    // server session is dropped (not pooled)
    let Ok(mut cl) = c.try_borrow_mut() else { return };
    let Ok(mut b) = cs.try_borrow_mut() else { return };
    cl.end_session(&mut b);
    drop(b);
    drop(cl);
    unsafe { super::apm::flush(c) };
}

static mut H_SESSION: sys::zend_object_handlers = unsafe { std::mem::zeroed() };
pub static mut CE: *mut sys::zend_class_entry = null_mut();

unsafe extern "C" fn create(c: *mut sys::zend_class_entry) -> *mut sys::zend_object {
    obj::alloc(c, Session::default(), addr_of_mut!(H_SESSION))
}

pub unsafe fn init_MongoDB_Driver_Session(c: *mut sys::zend_class_entry) {
    CE = c;
    (*c).__bindgen_anon_2.create_object = Some(create);
    obj::handlers::<Session>(addr_of_mut!(H_SESSION)).get_debug_info = Some(debug_info);
}

/// phongo_session_init
pub unsafe fn new_session(client: ClientRef, cs: ClientSession, default_rp: ReadPreference) -> sys::zval {
    let s = Session { cs: Some(Rc::new(RefCell::new(cs))), client: Some(client), txn_rp: default_rp.clone(), default_rp };
    zval::obj(obj::alloc(CE, s, addr_of_mut!(H_SESSION)))
}

/// The Session state of the object in `z`.
pub unsafe fn of<'a>(z: *mut sys::zval) -> &'a mut Session {
    obj::data::<Session>((*z).value.obj)
}

#[inline]
unsafe fn me<'a>(ex: Ex) -> &'a mut Session {
    obj::data::<Session>(call::this(ex))
}

/// SESSION_CHECK_LIVELINESS
unsafe fn live(ex: Ex, m: &str) -> Option<Rc<RefCell<ClientSession>>> {
    let r = me(ex).cs.clone();
    if r.is_none() {
        throw(E::Logic, format!("Cannot call '{m}', as the session has already been ended."));
    }
    r
}

fn state_str(s: TxnState) -> &'static str {
    match s {
        TxnState::None => "none",
        TxnState::Starting => "starting",
        TxnState::InProgress => "in_progress",
        TxnState::Committed | TxnState::CommittedEmpty => "committed",
        TxnState::Aborted => "aborted",
        // never observable: commit/abort run inside one call
        TxnState::Ending => "none",
    }
}

/// php_mongodb_session_parse_transaction_options: (options, read preference); Err when an
/// exception was thrown.
pub unsafe fn parse_txn_options(options: *mut sys::zval) -> Result<(Option<TxnOpts>, Option<ReadPreference>), ()> {
    let ht = (*options).value.arr;
    let mut opts: Option<TxnOpts> = None;
    let mut rp = None;
    if let Some(z) = hash::find(ht, b"maxCommitTimeMS") {
        let v = sys::zval_get_long_func(zval::deref(z), false);
        if v < 0 {
            throw(E::InvalidArgument, format!("Expected \"maxCommitTimeMS\" option to be >= 0, {v} given"));
            return Err(());
        }
        if v > u32::MAX as i64 {
            throw(E::InvalidArgument, format!("Expected \"maxCommitTimeMS\" option to be <= {}, {v} given", u32::MAX));
            return Err(());
        }
        opts.get_or_insert_with(Default::default).max_commit_time_ms = v;
    }
    let typed = |key: &[u8], ce: *mut sys::zend_class_entry, name: &str| -> Result<Option<*mut sys::zval>, ()> {
        let Some(z) = hash::find(ht, key) else { return Ok(None) };
        let z = zval::deref(z);
        if !zval::is(z, sys::IS_OBJECT) || !obj::instanceof((*(*z).value.obj).ce, ce) {
            throw(E::InvalidArgument, format!("Expected \"{}\" option to be {name}, {} given", String::from_utf8_lossy(key), zval::type_name(z)));
            return Err(());
        }
        Ok(Some(z))
    };
    if let Some(z) = typed(b"readConcern", super::readconcern::CE, "MongoDB\\Driver\\ReadConcern")? {
        opts.get_or_insert_with(Default::default).read_concern = Some(obj::data::<super::readconcern::ReadConcern>((*z).value.obj).clone());
    }
    if let Some(z) = typed(b"readPreference", super::readpref::CE, "MongoDB\\Driver\\ReadPreference")? {
        let r = obj::data::<ReadPreference>((*z).value.obj).clone();
        opts.get_or_insert_with(Default::default).read_primary = Some(r.mode == ReadMode::Primary);
        rp = Some(r);
    }
    if let Some(z) = typed(b"writeConcern", super::writeconcern::CE, "MongoDB\\Driver\\WriteConcern")? {
        opts.get_or_insert_with(Default::default).write_concern = Some(obj::data::<super::writeconcern::WriteConcern>((*z).value.obj).clone());
    }
    Ok((opts, rp))
}

pub unsafe extern "C" fn zim_MongoDB_Driver_Session___construct(_ex: Ex, _rv: *mut sys::zval) {
    throw(E::Runtime, "Accessing private constructor");
}

pub unsafe extern "C" fn zim_MongoDB_Driver_Session_advanceClusterTime(ex: Ex, _rv: *mut sys::zval) {
    let Some(cs) = live(ex, "advanceClusterTime") else { return };
    let mut z: *mut sys::zval = null_mut();
    if sys::zend_parse_parameters(call::num_args(ex), c"A".as_ptr(), &mut z) != 0 {
        return;
    }
    let Some(doc) = crate::bson::encode::to_bson(z, 0) else { return };
    if call::has_exception() {
        return;
    }
    cs.borrow_mut().advance_cluster_time(&doc);
}

pub unsafe extern "C" fn zim_MongoDB_Driver_Session_advanceOperationTime(ex: Ex, _rv: *mut sys::zval) {
    let Some(cs) = live(ex, "advanceOperationTime") else { return };
    let mut z: *mut sys::zval = null_mut();
    if sys::zend_parse_parameters(call::num_args(ex), c"O".as_ptr(), &mut z, crate::bson::decode::fetch_class(b"mongodb\\bson\\timestampinterface")) != 0 {
        return;
    }
    let o = (*z).value.obj;
    let mut t = call::method(o, c"getTimestamp", &mut []);
    if call::has_exception() || zval::is(&t, sys::IS_UNDEF) {
        zval::dtor(&mut t);
        return;
    }
    let mut i = call::method(o, c"getIncrement", &mut []);
    if call::has_exception() || zval::is(&i, sys::IS_UNDEF) {
        zval::dtor(&mut t);
        zval::dtor(&mut i);
        return;
    }
    // Z_LVAL of whatever the methods returned (the interface declares int)
    let (tv, iv) = (t.value.lval as u32, i.value.lval as u32);
    zval::dtor(&mut t);
    zval::dtor(&mut i);
    cs.borrow_mut().advance_operation_time(tv, iv);
}

pub unsafe extern "C" fn zim_MongoDB_Driver_Session_getClusterTime(ex: Ex, rv: *mut sys::zval) {
    let Some(cs) = live(ex, "getClusterTime") else { return };
    if !call::none(ex) {
        return;
    }
    let t = cs.borrow().cluster_time.clone();
    match t {
        None => *rv = zval::null(),
        Some(t) => {
            if let Some(z) = super::to_zval_no_odm(&t) {
                *rv = z;
            }
        }
    }
}

pub unsafe extern "C" fn zim_MongoDB_Driver_Session_getLogicalSessionId(ex: Ex, rv: *mut sys::zval) {
    let Some(cs) = live(ex, "getLogicalSessionId") else { return };
    if !call::none(ex) {
        return;
    }
    let l = cs.borrow().lsid().to_vec();
    if let Some(z) = super::to_zval_no_odm(&l) {
        *rv = z;
    }
}

pub unsafe extern "C" fn zim_MongoDB_Driver_Session_getOperationTime(ex: Ex, rv: *mut sys::zval) {
    let Some(cs) = live(ex, "getOperationTime") else { return };
    if !call::none(ex) {
        return;
    }
    let (t, i) = cs.borrow().operation_time;
    *rv = if (t, i) == (0, 0) { zval::null() } else { crate::bson::types::new_timestamp(i, t) };
}

pub unsafe extern "C" fn zim_MongoDB_Driver_Session_getServer(ex: Ex, rv: *mut sys::zval) {
    let Some(cs) = live(ex, "getServer") else { return };
    if !call::none(ex) {
        return;
    }
    let sid = cs.borrow().pinned_server_id;
    *rv = match (sid, me(ex).client.clone()) {
        (0, _) | (_, None) => zval::null(),
        (s, Some(c)) => super::server::new_server(c, s),
    };
}

/// phongo_transaction_options_to_zval
unsafe fn txn_options_zval(s: &Session) -> sys::zval {
    let Some(cs) = s.cs.as_ref() else { return zval::null() };
    let cs = cs.borrow();
    if !cs.in_txn() {
        return zval::null();
    }
    let ht = hash::new(4);
    if cs.txn.max_commit_time_ms != 0 {
        hash::str_update(ht, b"maxCommitTimeMS", zval::long(cs.txn.max_commit_time_ms));
    }
    if let Some(r) = cs.txn.read_concern.as_ref().filter(|r| !r.is_default()) {
        hash::str_update(ht, b"readConcern", super::readconcern::new_readconcern(r.clone()));
    }
    if cs.txn.read_primary.is_some() {
        hash::str_update(ht, b"readPreference", super::readpref::new_readpreference(s.txn_rp.clone()));
    }
    if let Some(w) = cs.txn.write_concern.as_ref().filter(|w| !w.is_default) {
        hash::str_update(ht, b"writeConcern", super::writeconcern::new_writeconcern(w.clone()));
    }
    zval::arr(ht)
}

pub unsafe extern "C" fn zim_MongoDB_Driver_Session_getTransactionOptions(ex: Ex, rv: *mut sys::zval) {
    if live(ex, "getTransactionOptions").is_none() || !call::none(ex) {
        return;
    }
    *rv = txn_options_zval(me(ex));
}

pub unsafe extern "C" fn zim_MongoDB_Driver_Session_getTransactionState(ex: Ex, rv: *mut sys::zval) {
    let Some(cs) = live(ex, "getTransactionState") else { return };
    if !call::none(ex) {
        return;
    }
    let st = cs.borrow().txn_state;
    *rv = zval::string(state_str(st).as_bytes());
}

pub unsafe extern "C" fn zim_MongoDB_Driver_Session_startTransaction(ex: Ex, _rv: *mut sys::zval) {
    let Some(cs) = live(ex, "startTransaction") else { return };
    let mut options: *mut sys::zval = null_mut();
    if sys::zend_parse_parameters(call::num_args(ex), c"|a!".as_ptr(), &mut options) != 0 {
        return;
    }
    let (opts, rp) = if options.is_null() {
        (None, None)
    } else {
        match parse_txn_options(options) {
            Ok(v) => v,
            Err(()) => return,
        }
    };
    let s = me(ex);
    let Some(client) = s.client.clone() else { return };
    let r = {
        let mut cl = client.borrow_mut();
        let mut b = cs.borrow_mut();
        cl.start_transaction(&mut b, opts.as_ref())
    };
    super::apm::flush(&client);
    if cs.borrow().txn_state == TxnState::Starting {
        s.txn_rp = rp.unwrap_or_else(|| s.default_rp.clone());
    }
    if let Err(e) = r {
        super::throw_error(&e, None);
    }
}

pub unsafe extern "C" fn zim_MongoDB_Driver_Session_commitTransaction(ex: Ex, _rv: *mut sys::zval) {
    let Some(cs) = live(ex, "commitTransaction") else { return };
    if !call::none(ex) {
        return;
    }
    let Some(client) = me(ex).client.clone() else { return };
    let r = {
        let mut cl = client.borrow_mut();
        let mut b = cs.borrow_mut();
        cl.commit_transaction(&mut b)
    };
    super::apm::flush(&client);
    if let Err(f) = r {
        super::throw_error(&f.error, Some(&f.reply));
    }
}

pub unsafe extern "C" fn zim_MongoDB_Driver_Session_abortTransaction(ex: Ex, _rv: *mut sys::zval) {
    let Some(cs) = live(ex, "abortTransaction") else { return };
    if !call::none(ex) {
        return;
    }
    let Some(client) = me(ex).client.clone() else { return };
    let r = {
        let mut cl = client.borrow_mut();
        let mut b = cs.borrow_mut();
        cl.abort_transaction(&mut b)
    };
    super::apm::flush(&client);
    if let Err(e) = r {
        super::throw_error(&e, None);
    }
}

pub unsafe extern "C" fn zim_MongoDB_Driver_Session_endSession(ex: Ex, _rv: *mut sys::zval) {
    if !call::none(ex) {
        return;
    }
    end(me(ex));
}

pub unsafe extern "C" fn zim_MongoDB_Driver_Session_isDirty(ex: Ex, rv: *mut sys::zval) {
    let Some(cs) = live(ex, "isDirty") else { return };
    if call::none(ex) {
        *rv = zval::bool(cs.borrow().dirty());
    }
}

pub unsafe extern "C" fn zim_MongoDB_Driver_Session_isInTransaction(ex: Ex, rv: *mut sys::zval) {
    let Some(cs) = live(ex, "isInTransaction") else { return };
    if call::none(ex) {
        *rv = zval::bool(cs.borrow().in_txn());
    }
}

/// phongo_session_get_debug_info
unsafe extern "C" fn debug_info(o: *mut sys::zend_object, is_temp: *mut i32) -> *mut sys::HashTable {
    *is_temp = 1;
    let s = obj::data::<Session>(o);
    let ht = hash::new(0);
    let Some(rc) = s.cs.clone() else {
        hash::str_update(ht, b"ended", zval::bool(true));
        return ht;
    };
    let cs = rc.borrow();
    let tm = super::debug_typemap();
    match super::to_zval(cs.lsid(), &tm) {
        Some(z) => hash::str_update(ht, b"logicalSessionId", z),
        None => return ht,
    }
    match &cs.cluster_time {
        Some(t) => match super::to_zval(t, &tm) {
            Some(z) => hash::str_update(ht, b"clusterTime", z),
            None => return ht,
        },
        None => hash::str_update(ht, b"clusterTime", zval::null()),
    }
    hash::str_update(ht, b"causalConsistency", zval::bool(cs.causal_consistency));
    hash::str_update(ht, b"snapshot", zval::bool(cs.snapshot));
    let (t, i) = cs.operation_time;
    hash::str_update(ht, b"operationTime", if t != 0 && i != 0 { crate::bson::types::new_timestamp(i, t) } else { zval::null() });
    let server = match (cs.pinned_server_id, s.client.clone()) {
        (0, _) | (_, None) => zval::null(),
        (sid, Some(c)) => super::server::new_server(c, sid),
    };
    hash::str_update(ht, b"server", server);
    hash::str_update(ht, b"dirty", zval::bool(cs.dirty()));
    hash::str_update(ht, b"inTransaction", zval::bool(cs.in_txn()));
    hash::str_update(ht, b"transactionState", zval::string(state_str(cs.txn_state).as_bytes()));
    drop(cs);
    hash::str_update(ht, b"transactionOptions", txn_options_zval(s));
    ht
}
