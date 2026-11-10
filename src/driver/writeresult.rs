//! MongoDB\Driver\WriteResult, WriteError, WriteConcernError and
//! BulkWriteException::getWriteResult (src/MongoDB/WriteResult.c, WriteError.c,
//! WriteConcernError.c, Exception/BulkWriteException.c).

use super::writeconcern::WriteConcern;
use super::ClientRef;
use crate::bson::{throw, E};
use crate::zend::call::{self, Ex};
use crate::zend::{class, hash, obj, sys, zval};
use mongo_core::bson::iter::{Iter, Value};
use std::ptr::{addr_of_mut, null_mut};

pub struct WriteResult {
    pub reply: Vec<u8>,
    pub server_id: u32,
    pub write_concern: Option<WriteConcern>,
    pub client: Option<ClientRef>,
    /// the Manager it came from (phongo_writeresult_t.manager); released last
    pub manager: super::ManagerRef,
}

impl Default for WriteResult {
    fn default() -> Self {
        WriteResult { reply: Vec::new(), server_id: 0, write_concern: None, client: None, manager: super::ManagerRef::default() }
    }
}

/// Its properties and the Manager it holds, for the GC.
unsafe extern "C" fn get_gc(o: *mut sys::zend_object, table: *mut *mut sys::zval, n: *mut i32) -> *mut sys::HashTable {
    let w = obj::data::<WriteResult>(o);
    obj::get_gc_with(o, table, n, [w.manager.gc()])
}

static mut H_WR: sys::zend_object_handlers = unsafe { std::mem::zeroed() };
pub static mut CE: *mut sys::zend_class_entry = null_mut();

unsafe extern "C" fn create(c: *mut sys::zend_class_entry) -> *mut sys::zend_object {
    obj::alloc(c, WriteResult::default(), addr_of_mut!(H_WR))
}

pub unsafe fn init_MongoDB_Driver_WriteResult(c: *mut sys::zend_class_entry) {
    CE = c;
    (*c).__bindgen_anon_2.create_object = Some(create);
    obj::handlers::<WriteResult>(addr_of_mut!(H_WR)).get_gc = Some(get_gc);
}

fn find<'a>(d: &'a [u8], k: &[u8]) -> Option<Value<'a>> {
    let mut it = Iter::new(d)?;
    it.find(k).then(|| it.value())
}

fn int32(d: &[u8], k: &[u8]) -> Option<i64> {
    match find(d, k)? {
        Value::Int32(v) => Some(v as i64),
        _ => None,
    }
}

impl WriteResult {
    fn acknowledged(&self) -> bool {
        self.write_concern.as_ref().is_none_or(WriteConcern::is_acknowledged)
    }
}

/// zend_update_property (which copies `v`; our reference is released here).
unsafe fn set_prop(ce: *mut sys::zend_class_entry, o: *mut sys::zend_object, name: &str, mut v: sys::zval) {
    sys::zend_update_property(ce, o, name.as_ptr() as *const _, name.len(), &mut v);
}

/// phongo_writeerror_init / phongo_writeconcernerror_init: an object of `ce` from `doc`
/// (None: an exception was thrown).
pub unsafe fn error_object(ce: *mut sys::zend_class_entry, doc: &[u8], with_index: bool, index: i32) -> Option<sys::zval> {
    let z = obj::new_zval(ce);
    let o = z.value.obj;
    set_prop(ce, o, "code", zval::long(int32(doc, b"code").unwrap_or(0)));
    let msg = match (find(doc, b"errmsg"), find(doc, b"message")) {
        (Some(Value::Utf8(s)), _) | (_, Some(Value::Utf8(s))) => crate::zend::zstr::c_part(s).to_vec(),
        _ => Vec::new(),
    };
    set_prop(ce, o, "message", zval::string(&msg));
    let info = match (find(doc, b"errInfo"), find(doc, b"details")) {
        (Some(Value::Document(d)), _) | (_, Some(Value::Document(d))) => Some(d.to_vec()),
        _ => None,
    };
    match info {
        Some(d) => match super::to_zval(&d, &Default::default()) {
            Some(i) => set_prop(ce, o, "info", i),
            None => return None,
        },
        None => set_prop(ce, o, "info", zval::null()),
    }
    if with_index {
        let mut index = index as i64;
        if index == 0 {
            index = int32(doc, b"index").unwrap_or(0);
        }
        set_prop(ce, o, "index", zval::long(index));
    }
    Some(z)
}

unsafe fn writeerror_ce() -> *mut sys::zend_class_entry {
    class::lookup(c"mongodb\\driver\\writeerror")
}
unsafe fn writeconcernerror_ce() -> *mut sys::zend_class_entry {
    class::lookup(c"mongodb\\driver\\writeconcernerror")
}

/// The documents of an array field of the reply.
fn docs(field: Option<Value<'_>>) -> Vec<&[u8]> {
    let mut v = Vec::new();
    if let Some(Value::Array(a)) = field {
        if let Some(mut it) = Iter::new(a) {
            while it.next() {
                if let Value::Document(d) = it.value() {
                    v.push(d);
                }
            }
        }
    }
    v
}

/// From the reply's writeConcernErrors.
unsafe fn write_concern_error(field: Option<Value>) -> Option<sys::zval> {
    match docs(field).first() {
        Some(d) => error_object(writeconcernerror_ce(), d, false, 0),
        None => Some(zval::null()),
    }
}

/// From the reply's writeErrors.
unsafe fn write_errors(field: Option<Value>) -> Option<sys::zval> {
    let ht = hash::new(0);
    let z = zval::arr(ht);
    for d in docs(field) {
        match error_object(writeerror_ce(), d, true, 0) {
            Some(e) => hash::push(ht, e),
            None => return None,
        }
    }
    Some(z)
}

/// From the reply's errorReplies.
unsafe fn error_replies(field: Option<Value>) -> sys::zval {
    let ht = hash::new(0);
    for d in docs(field) {
        if let Some(z) = super::to_zval(d, &Default::default()) {
            hash::push(ht, z);
        }
    }
    zval::arr(ht)
}

/// From the reply's upserted.
unsafe fn upserted_ids(field: Option<Value>) -> sys::zval {
    let ht = hash::new(0);
    for d in docs(field) {
        let tm = crate::bson::decode::TypeMap { root: crate::bson::decode::TM::Array, ..Default::default() };
        let mut st = crate::bson::decode::State::new(&tm);
        st.skip_odm = true;
        if let Some(z) = crate::bson::decode::to_zval(d, &st) {
            if zval::is(&z, sys::IS_ARRAY) {
                let id = hash::find(z.value.arr, b"_id");
                let idx = hash::find(z.value.arr, b"index").map_or(0, |i| sys::zval_get_long_func(i, false));
                if let Some(id) = id {
                    hash::index_update(ht, idx as u64, zval::copy(id));
                }
            }
        }
    }
    zval::arr(ht)
}

unsafe fn server_zval(w: &WriteResult) -> sys::zval {
    match &w.client {
        Some(c) => super::server::new_server(c.clone(), w.server_id, w.manager.obj()),
        None => zval::null(),
    }
}

/// phongo_writeresult_init
pub unsafe fn new_writeresult(reply: Vec<u8>, client: ClientRef, server_id: u32, wc: WriteConcern, manager: *mut sys::zend_object) -> sys::zval {
    let o = obj::alloc(CE, WriteResult { reply, server_id, write_concern: Some(wc), client: Some(client), manager: super::ManagerRef::new(manager) }, addr_of_mut!(H_WR));
    let w = obj::data::<WriteResult>(o);
    let acked = w.acknowledged();
    // one pass over the reply for all the fields read here
    let [n_ins, n_mat, n_mod, n_rem, n_ups, upserted, write_errs, wc_errs, err_replies] = mongo_core::bson::iter::find_many(
        &w.reply,
        [b"nInserted", b"nMatched", b"nModified", b"nRemoved", b"nUpserted", b"upserted", b"writeErrors", b"writeConcernErrors", b"errorReplies"],
    );
    for (prop, field) in [("insertedCount", n_ins), ("matchedCount", n_mat), ("modifiedCount", n_mod), ("deletedCount", n_rem), ("upsertedCount", n_ups)] {
        let v = match field {
            Some(Value::Int32(n)) if acked => zval::long(n as i64),
            _ => zval::null(),
        };
        set_prop(CE, o, prop, v);
    }
    set_prop(CE, o, "upsertedIds", upserted_ids(upserted));
    if let Some(e) = write_errors(write_errs) {
        set_prop(CE, o, "writeErrors", e);
    }
    if let Some(e) = write_concern_error(wc_errs) {
        set_prop(CE, o, "writeConcernError", e);
    }
    let wcz = match &w.write_concern {
        Some(wc) => super::writeconcern::new_writeconcern(wc.clone()),
        None => zval::null(),
    };
    set_prop(CE, o, "writeConcern", wcz);
    set_prop(CE, o, "errorReplies", error_replies(err_replies));
    set_prop(CE, o, "server", server_zval(w));
    zval::obj(o)
}

#[inline]
unsafe fn me<'a>(ex: Ex) -> &'a WriteResult {
    obj::data::<WriteResult>(call::this(ex))
}

pub unsafe extern "C" fn zim_MongoDB_Driver_WriteResult___construct(_ex: Ex, _rv: *mut sys::zval) {
    throw(E::Runtime, "Accessing private constructor");
}

macro_rules! count {
    ($f:ident, $name:literal, $field:literal) => {
        pub unsafe extern "C" fn $f(ex: Ex, rv: *mut sys::zval) {
            if !call::none(ex) {
                return;
            }
            let w = me(ex);
            if !w.acknowledged() {
                throw(E::Logic, concat!("MongoDB\\Driver\\WriteResult::", $name, "() should not be called for an unacknowledged write result"));
            }
            zval::write(rv, zval::long(int32(&w.reply, $field).unwrap_or(0)));
        }
    };
}
count!(zim_MongoDB_Driver_WriteResult_getInsertedCount, "getInsertedCount", b"nInserted");
count!(zim_MongoDB_Driver_WriteResult_getMatchedCount, "getMatchedCount", b"nMatched");
count!(zim_MongoDB_Driver_WriteResult_getModifiedCount, "getModifiedCount", b"nModified");
count!(zim_MongoDB_Driver_WriteResult_getDeletedCount, "getDeletedCount", b"nRemoved");
count!(zim_MongoDB_Driver_WriteResult_getUpsertedCount, "getUpsertedCount", b"nUpserted");

pub unsafe extern "C" fn zim_MongoDB_Driver_WriteResult_getServer(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        zval::write(rv, server_zval(me(ex)));
    }
}
pub unsafe extern "C" fn zim_MongoDB_Driver_WriteResult_getUpsertedIds(ex: Ex, rv: *mut sys::zval) {
    if !call::none(ex) {
        return;
    }
    let w = me(ex);
    if !w.acknowledged() {
        throw(E::Logic, "MongoDB\\Driver\\WriteResult::getUpsertedIds() should not be called for an unacknowledged write result");
    }
    zval::write(rv, upserted_ids(find(&w.reply, b"upserted")));
}
pub unsafe extern "C" fn zim_MongoDB_Driver_WriteResult_getWriteConcernError(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        zval::write(rv, write_concern_error(find(&me(ex).reply, b"writeConcernErrors")).unwrap_or_else(zval::null));
    }
}
pub unsafe extern "C" fn zim_MongoDB_Driver_WriteResult_getWriteErrors(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        if let Some(z) = write_errors(find(&me(ex).reply, b"writeErrors")) {
            zval::write(rv, z);
        }
    }
}
pub unsafe extern "C" fn zim_MongoDB_Driver_WriteResult_getErrorReplies(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        zval::write(rv, error_replies(find(&me(ex).reply, b"errorReplies")));
    }
}
pub unsafe extern "C" fn zim_MongoDB_Driver_WriteResult_isAcknowledged(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        zval::write(rv, zval::bool(me(ex).acknowledged()));
    }
}

/// PHONGO_PROPERTY_GETTER
unsafe fn prop_getter(ex: Ex, rv: *mut sys::zval, name: &str) {
    if !call::none(ex) {
        return;
    }
    let o = call::this(ex);
    let mut tmp = zval::undef();
    let z = sys::zend_read_property((*o).ce, o, name.as_ptr() as *const _, name.len(), true, &mut tmp);
    zval::write(rv, zval::copy(zval::deref(z)));
}

macro_rules! getter {
    ($f:ident, $p:literal) => {
        pub unsafe extern "C" fn $f(ex: Ex, rv: *mut sys::zval) {
            prop_getter(ex, rv, $p);
        }
    };
}
getter!(zim_MongoDB_Driver_WriteError_getCode, "code");
getter!(zim_MongoDB_Driver_WriteError_getIndex, "index");
getter!(zim_MongoDB_Driver_WriteError_getMessage, "message");
getter!(zim_MongoDB_Driver_WriteError_getInfo, "info");
getter!(zim_MongoDB_Driver_WriteConcernError_getCode, "code");
getter!(zim_MongoDB_Driver_WriteConcernError_getMessage, "message");
getter!(zim_MongoDB_Driver_WriteConcernError_getInfo, "info");
getter!(zim_MongoDB_Driver_Exception_BulkWriteException_getWriteResult, "writeResult");

pub unsafe extern "C" fn zim_MongoDB_Driver_WriteError___construct(_ex: Ex, _rv: *mut sys::zval) {
    throw(E::Runtime, "Accessing private constructor");
}
pub unsafe extern "C" fn zim_MongoDB_Driver_WriteConcernError___construct(_ex: Ex, _rv: *mut sys::zval) {
    throw(E::Runtime, "Accessing private constructor");
}

/// RuntimeException::hasErrorLabel
pub unsafe extern "C" fn zim_MongoDB_Driver_Exception_RuntimeException_hasErrorLabel(ex: Ex, rv: *mut sys::zval) {
    let (mut p, mut l): (*const u8, usize) = (null_mut(), 0);
    if sys::zend_parse_parameters(call::num_args(ex), c"s".as_ptr(), &mut p, &mut l) != 0 {
        return;
    }
    let label = crate::zend::zstr::c_part(std::slice::from_raw_parts(p, l));
    let o = call::this(ex);
    let mut tmp = zval::undef();
    let z = sys::zend_read_property(class::lookup(c"mongodb\\driver\\exception\\runtimeexception"), o, c"errorLabels".as_ptr(), 11, true, &mut tmp);
    let mut found = false;
    if zval::is(z, sys::IS_ARRAY) {
        hash::each((*z).value.arr, true, |_, v| {
            if zval::is(v, sys::IS_STRING) && crate::zend::zstr::c_part(zval::str_bytes(v)) == label {
                found = true;
                return false;
            }
            true
        });
    }
    zval::write(rv, zval::bool(found));
}
