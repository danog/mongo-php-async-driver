//! MongoDB\Driver\BulkWriteCommand, BulkWriteCommandResult, Exception\BulkWriteCommandException
//! and phongo_execute_bulkwritecommand (src/MongoDB/BulkWriteCommand.c,
//! BulkWriteCommandResult.c, Exception/BulkWriteCommandException.c, phongo_execute.c) over
//! mongo_core::bulkwrite (mongoc-bulkwrite.c).

use super::ClientRef;
use crate::bson::{encode, throw, E};
use crate::zend::call::{self, Ex};
use crate::zend::{hash, obj, sys, zval};
use mongo_core::bson::iter::{Iter, Value};
use mongo_core::bulkwrite::{BulkWrite, BulkWriteResult, ExecOpts, ModelOpts};
use mongo_core::topology::OpType;
use std::ptr::{addr_of_mut, null_mut};

pub struct BulkWriteCommand {
    pub bw: BulkWrite,
    /// None: PHONGO_BULKWRITECOMMAND_BYPASS_UNSET
    pub bypass: Option<bool>,
    /// {"comment": value}
    pub comment: Option<Vec<u8>>,
    pub let_: Option<Vec<u8>>,
    pub num_ops: i64,
    pub ordered: bool,
    pub verbose: bool,
    /// the session of the last execution (kept so the core never refers to a freed one)
    pub session: sys::zval,
}

impl Default for BulkWriteCommand {
    fn default() -> Self {
        BulkWriteCommand { bw: BulkWrite::new(), bypass: None, comment: None, let_: None, num_ops: 0, ordered: true, verbose: false, session: zval::undef() }
    }
}

static mut H_BWC: sys::zend_object_handlers = unsafe { std::mem::zeroed() };
pub static mut CE: *mut sys::zend_class_entry = null_mut();

unsafe extern "C" fn create(c: *mut sys::zend_class_entry) -> *mut sys::zend_object {
    obj::alloc(c, BulkWriteCommand::default(), addr_of_mut!(H_BWC))
}

/// phongo_bulkwritecommand_get_debug_info
unsafe extern "C" fn get_debug_info(o: *mut sys::zend_object, is_temp: *mut i32) -> *mut sys::HashTable {
    *is_temp = 1;
    let b = obj::data::<BulkWriteCommand>(o);
    let ht = hash::new(6);
    hash::str_update(ht, b"bypassDocumentValidation", b.bypass.map_or_else(zval::null, zval::bool));
    if let Some(c) = &b.comment {
        match super::legacy_value(c) {
            Some(z) => hash::str_update(ht, b"comment", z),
            None => return ht,
        }
    }
    if let Some(l) = &b.let_ {
        match super::to_zval(l, &Default::default()) {
            Some(z) => hash::str_update(ht, b"let", z),
            None => return ht,
        }
    }
    hash::str_update(ht, b"ordered", zval::bool(b.ordered));
    hash::str_update(ht, b"verboseResults", zval::bool(b.verbose));
    hash::str_update(ht, b"session", if zval::is(&b.session, sys::IS_UNDEF) { zval::null() } else { zval::copy(&b.session) });
    ht
}

unsafe extern "C" fn get_gc(o: *mut sys::zend_object, table: *mut *mut sys::zval, n: *mut i32) -> *mut sys::HashTable {
    let b = obj::data::<BulkWriteCommand>(o);
    obj::get_gc_with(o, table, n, [&mut b.session as *mut sys::zval])
}

pub unsafe fn init_MongoDB_Driver_BulkWriteCommand(c: *mut sys::zend_class_entry) {
    CE = c;
    (*c).__bindgen_anon_2.create_object = Some(create);
    let h = obj::handlers::<BulkWriteCommand>(addr_of_mut!(H_BWC));
    h.get_debug_info = Some(get_debug_info);
    h.get_gc = Some(get_gc);
}

#[inline]
unsafe fn me<'a>(ex: Ex) -> &'a mut BulkWriteCommand {
    obj::data::<BulkWriteCommand>(call::this(ex))
}

/// php_array_fetchc_bool
unsafe fn fetch_bool(ht: *mut sys::HashTable, k: &[u8]) -> bool {
    hash::sym_find(ht, k).is_some_and(|z| zval::is_true(z))
}

unsafe fn opt_ht(z: *mut sys::zval) -> Option<*mut sys::HashTable> {
    (!z.is_null() && zval::is(z, sys::IS_ARRAY)).then(|| (*z).value.arr)
}

pub unsafe extern "C" fn zim_MongoDB_Driver_BulkWriteCommand___construct(ex: Ex, _rv: *mut sys::zval) {
    let mut options: *mut sys::zval = null_mut();
    if sys::zend_parse_parameters(call::num_args(ex), c"|a!".as_ptr(), &mut options) != 0 {
        return;
    }
    let b = me(ex);
    *b = BulkWriteCommand::default();
    let Some(o) = opt_ht(options) else { return };
    if hash::sym_find(o, b"bypassDocumentValidation").is_some() {
        b.bypass = Some(fetch_bool(o, b"bypassDocumentValidation"));
    }
    if let Some(v) = hash::sym_find(o, b"comment") {
        let Some(d) = encode::to_bson_value_doc(zval::deref(v), b"comment") else { return };
        b.comment = Some(d);
    }
    if let Some(v) = hash::sym_find(o, b"let") {
        let v = zval::deref(v);
        if !zval::is(v, sys::IS_OBJECT) && !zval::is(v, sys::IS_ARRAY) {
            throw(E::InvalidArgument, format!("Expected \"let\" option to be array or object, {} given", super::type_by_const(v)));
            return;
        }
        let Some(d) = encode::to_bson(v, encode::NONE) else { return };
        b.let_ = Some(d);
    }
    if hash::sym_find(o, b"ordered").is_some() {
        b.ordered = fetch_bool(o, b"ordered");
    }
    if hash::sym_find(o, b"verboseResults").is_some() {
        b.verbose = fetch_bool(o, b"verboseResults");
    }
}

pub unsafe extern "C" fn zim_MongoDB_Driver_BulkWriteCommand_count(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        zval::write(rv, zval::long(me(ex).num_ops));
    }
}

/// phongo_bwc_parse_hint: {"hint": <string or document>}
unsafe fn parse_hint(z: *mut sys::zval) -> Option<Vec<u8>> {
    let z = zval::deref(z);
    if !zval::is(z, sys::IS_STRING) && !zval::is(z, sys::IS_OBJECT) && !zval::is(z, sys::IS_ARRAY) {
        throw(E::InvalidArgument, format!("Expected \"hint\" option to be string, array, or object, {} given", super::type_by_const(z)));
        return None;
    }
    let d = encode::to_bson_value_doc(z, b"hint")?;
    let t = Iter::new(&d).and_then(|mut it| it.next().then(|| it.type_code())).unwrap_or(0);
    if t != 2 && t != 3 {
        throw(E::InvalidArgument, format!("Expected \"hint\" option to yield string or document but got \"{}\"", super::uri_options::type_name(t)));
        return None;
    }
    Some(d)
}

/// phongo_bwc_parse_document
unsafe fn parse_document(z: *mut sys::zval, key: &str) -> Option<Vec<u8>> {
    let z = zval::deref(z);
    if !zval::is(z, sys::IS_OBJECT) && !zval::is(z, sys::IS_ARRAY) {
        throw(E::InvalidArgument, format!("Expected \"{key}\" option to be array or object, {} given", super::type_by_const(z)));
        return None;
    }
    encode::to_bson(z, encode::NONE)
}

/// phongo_bwc_parse_array (a PackedArray is allowed; keys must be "0", "1", ...)
unsafe fn parse_array(z: *mut sys::zval, key: &str) -> Option<Vec<u8>> {
    let z = zval::deref(z);
    if !zval::is(z, sys::IS_OBJECT) && !zval::is(z, sys::IS_ARRAY) {
        throw(E::InvalidArgument, format!("Expected \"{key}\" option to be array or object, {} given", super::type_by_const(z)));
        return None;
    }
    let d = encode::to_bson(z, encode::ALLOW_ROOT_ARRAY)?;
    if let Some(mut it) = Iter::new(&d) {
        let mut i = 0usize;
        while it.next() {
            if it.key() != i.to_string().as_bytes() {
                throw(E::InvalidArgument, format!("Expected \"{key}\" option to yield array but got non-sequential keys"));
                return None;
            }
            i += 1;
        }
    }
    Some(d)
}

#[derive(Clone, Copy)]
enum Opt {
    ArrayFilters,
    Collation,
    Hint,
    Sort,
    Upsert,
}

/// The model options, parsed in the given order (phongo checks them in a per-method order).
unsafe fn parse_model_opts(options: *mut sys::zval, order: &[Opt]) -> Option<ModelOpts> {
    let mut m = ModelOpts::default();
    let Some(o) = opt_ht(options) else { return Some(m) };
    for opt in order {
        match opt {
            Opt::ArrayFilters => {
                if let Some(v) = hash::sym_find(o, b"arrayFilters") {
                    m.array_filters = Some(parse_array(v, "arrayFilters")?);
                }
            }
            Opt::Collation => {
                if let Some(v) = hash::sym_find(o, b"collation") {
                    m.collation = Some(parse_document(v, "collation")?);
                }
            }
            Opt::Hint => {
                if let Some(v) = hash::sym_find(o, b"hint") {
                    m.hint = Some(parse_hint(v)?);
                }
            }
            Opt::Sort => {
                if let Some(v) = hash::sym_find(o, b"sort") {
                    m.sort = Some(parse_document(v, "sort")?);
                }
            }
            Opt::Upsert => {
                if hash::sym_find(o, b"upsert").is_some() {
                    m.upsert = Some(fetch_bool(o, b"upsert"));
                }
            }
        }
    }
    Some(m)
}

/// The namespace argument: no NUL bytes.
unsafe fn ns_arg(p: *const u8, l: usize) -> Option<String> {
    let ns = std::slice::from_raw_parts(p, l);
    if ns.contains(&0) {
        throw(E::InvalidArgument, "Namespace string should not contain null bytes");
        return None;
    }
    Some(String::from_utf8_lossy(ns).into_owned())
}

/// The outcome of an append: num_ops counts it, a libmongoc error is thrown.
unsafe fn appended(ex: Ex, r: Result<(), mongo_core::error::Error>) -> bool {
    match r {
        Ok(()) => {
            me(ex).num_ops += 1;
            true
        }
        Err(e) => {
            super::throw_error(&e, None);
            false
        }
    }
}

unsafe fn delete(ex: Ex, multi: bool) {
    let (mut p, mut l): (*const u8, usize) = (null_mut(), 0);
    let (mut zfilter, mut options): (*mut sys::zval, *mut sys::zval) = (null_mut(), null_mut());
    if sys::zend_parse_parameters(call::num_args(ex), c"sA|a!".as_ptr(), &mut p, &mut l, &mut zfilter, &mut options) != 0 {
        return;
    }
    let Some(ns) = ns_arg(p, l) else { return };
    let Some(filter) = encode::to_bson(zfilter, encode::NONE) else { return };
    let Some(o) = parse_model_opts(options, &[Opt::Hint, Opt::Collation]) else { return };
    let r = me(ex).bw.append_delete(&ns, &filter, &o, multi);
    appended(ex, r);
}

pub unsafe extern "C" fn zim_MongoDB_Driver_BulkWriteCommand_deleteMany(ex: Ex, _rv: *mut sys::zval) {
    delete(ex, true);
}

pub unsafe extern "C" fn zim_MongoDB_Driver_BulkWriteCommand_deleteOne(ex: Ex, _rv: *mut sys::zval) {
    delete(ex, false);
}

pub unsafe extern "C" fn zim_MongoDB_Driver_BulkWriteCommand_insertOne(ex: Ex, rv: *mut sys::zval) {
    let (mut p, mut l): (*const u8, usize) = (null_mut(), 0);
    let mut zdoc: *mut sys::zval = null_mut();
    if sys::zend_parse_parameters(call::num_args(ex), c"sA".as_ptr(), &mut p, &mut l, &mut zdoc) != 0 {
        return;
    }
    let Some(ns) = ns_arg(p, l) else { return };
    let Some((doc, id)) = encode::to_bson_with_id(zdoc, encode::ADD_ID | encode::RETURN_ID) else { return };
    let Some(id) = id else {
        throw(E::Logic, "phongo_zval_to_bson() did not return an _id. Please file a bug report.");
        return;
    };
    let r = me(ex).bw.append_insert_one(&ns, &doc);
    if !appended(ex, r) {
        return;
    }
    // phongo_bwc_extract_id: decode {_id: ...} with an array root
    let tm = crate::bson::decode::TypeMap { root: crate::bson::decode::TM::Array, ..Default::default() };
    if let Some(z) = super::to_zval(&id, &tm) {
        if zval::is(&z, sys::IS_ARRAY) {
            if let Some(v) = hash::find(z.value.arr, b"_id") {
                zval::write(rv, zval::copy(v));
            }
        }
    }
}

/// replaceOne / updateOne / updateMany: (ns, filter, document, options)
unsafe fn parse_update_args(ex: Ex, update_flags: u32, order: &[Opt]) -> Option<(String, Vec<u8>, Vec<u8>, ModelOpts)> {
    let (mut p, mut l): (*const u8, usize) = (null_mut(), 0);
    let (mut zfilter, mut zdoc, mut options): (*mut sys::zval, *mut sys::zval, *mut sys::zval) = (null_mut(), null_mut(), null_mut());
    if sys::zend_parse_parameters(call::num_args(ex), c"sAA|a!".as_ptr(), &mut p, &mut l, &mut zfilter, &mut zdoc, &mut options) != 0 {
        return None;
    }
    let ns = ns_arg(p, l)?;
    let filter = encode::to_bson(zfilter, encode::NONE)?;
    let doc = encode::to_bson(zdoc, update_flags)?;
    let o = parse_model_opts(options, order)?;
    Some((ns, filter, doc, o))
}

pub unsafe extern "C" fn zim_MongoDB_Driver_BulkWriteCommand_replaceOne(ex: Ex, _rv: *mut sys::zval) {
    let Some((ns, filter, doc, o)) = parse_update_args(ex, encode::NONE, &[Opt::Collation, Opt::Hint, Opt::Sort, Opt::Upsert]) else { return };
    let r = me(ex).bw.append_replace_one(&ns, &filter, &doc, &o);
    appended(ex, r);
}

pub unsafe extern "C" fn zim_MongoDB_Driver_BulkWriteCommand_updateMany(ex: Ex, _rv: *mut sys::zval) {
    // a PackedArray is allowed for update pipelines
    let Some((ns, filter, doc, o)) = parse_update_args(ex, encode::ALLOW_ROOT_ARRAY, &[Opt::ArrayFilters, Opt::Collation, Opt::Hint, Opt::Upsert]) else { return };
    let r = me(ex).bw.append_update(&ns, &filter, &doc, &o, true);
    appended(ex, r);
}

pub unsafe extern "C" fn zim_MongoDB_Driver_BulkWriteCommand_updateOne(ex: Ex, _rv: *mut sys::zval) {
    let Some((ns, filter, doc, o)) = parse_update_args(ex, encode::ALLOW_ROOT_ARRAY, &[Opt::ArrayFilters, Opt::Collation, Opt::Hint, Opt::Sort, Opt::Upsert]) else { return };
    let r = me(ex).bw.append_update(&ns, &filter, &doc, &o, false);
    appended(ex, r);
}

// ---------------------------------------------------------------------------------------------
// BulkWriteCommandResult

#[derive(Default)]
pub struct CommandResult {
    pub is_acknowledged: bool,
    pub inserted: i64,
    pub matched: i64,
    pub modified: i64,
    pub upserted: i64,
    pub deleted: i64,
    pub insert_results: Option<Vec<u8>>,
    pub update_results: Option<Vec<u8>>,
    pub delete_results: Option<Vec<u8>>,
}

static mut H_RES: sys::zend_object_handlers = unsafe { std::mem::zeroed() };
pub static mut RES_CE: *mut sys::zend_class_entry = null_mut();

unsafe extern "C" fn create_result(c: *mut sys::zend_class_entry) -> *mut sys::zend_object {
    obj::alloc(c, CommandResult::default(), addr_of_mut!(H_RES))
}

fn opt_doc(d: &Option<Vec<u8>>) -> sys::zval {
    d.as_ref().map_or_else(zval::null, |d| unsafe { crate::bson::document::new_document(d) })
}

unsafe extern "C" fn result_debug_info(o: *mut sys::zend_object, is_temp: *mut i32) -> *mut sys::HashTable {
    *is_temp = 1;
    let r = obj::data::<CommandResult>(o);
    let ht = hash::new(12);
    hash::str_update(ht, b"isAcknowledged", zval::bool(r.is_acknowledged));
    hash::str_update(ht, b"insertedCount", zval::long(r.inserted));
    hash::str_update(ht, b"matchedCount", zval::long(r.matched));
    hash::str_update(ht, b"modifiedCount", zval::long(r.modified));
    hash::str_update(ht, b"upsertedCount", zval::long(r.upserted));
    hash::str_update(ht, b"deletedCount", zval::long(r.deleted));
    hash::str_update(ht, b"insertResults", opt_doc(&r.insert_results));
    hash::str_update(ht, b"updateResults", opt_doc(&r.update_results));
    hash::str_update(ht, b"deleteResults", opt_doc(&r.delete_results));
    ht
}

pub unsafe fn init_MongoDB_Driver_BulkWriteCommandResult(c: *mut sys::zend_class_entry) {
    RES_CE = c;
    (*c).__bindgen_anon_2.create_object = Some(create_result);
    obj::handlers::<CommandResult>(addr_of_mut!(H_RES)).get_debug_info = Some(result_debug_info);
}

/// phongo_bulkwritecommandresult_init (None: unacknowledged)
unsafe fn new_result(res: Option<&BulkWriteResult>) -> sys::zval {
    let mut r = CommandResult { is_acknowledged: res.is_some(), ..Default::default() };
    if let Some(res) = res {
        r.inserted = res.inserted_count;
        r.upserted = res.upserted_count;
        r.matched = res.matched_count;
        r.modified = res.modified_count;
        r.deleted = res.deleted_count;
        if let Some([i, u, d]) = res.verbose_results() {
            r.insert_results = Some(i);
            r.update_results = Some(u);
            r.delete_results = Some(d);
        }
    }
    zval::obj(obj::alloc(RES_CE, r, addr_of_mut!(H_RES)))
}

pub unsafe extern "C" fn zim_MongoDB_Driver_BulkWriteCommandResult___construct(_ex: Ex, _rv: *mut sys::zval) {
    throw(E::Runtime, "Accessing private constructor");
}

#[inline]
unsafe fn res<'a>(ex: Ex) -> &'a CommandResult {
    obj::data::<CommandResult>(call::this(ex))
}

macro_rules! getter {
    ($f:ident, $name:literal, |$r:ident| $v:expr) => {
        pub unsafe extern "C" fn $f(ex: Ex, rv: *mut sys::zval) {
            if !call::none(ex) {
                return;
            }
            let $r = res(ex);
            if !$r.is_acknowledged {
                throw(E::Logic, concat!("MongoDB\\Driver\\BulkWriteCommandResult::", $name, "() should not be called for an unacknowledged write result"));
                return;
            }
            zval::write(rv, $v);
        }
    };
}
getter!(zim_MongoDB_Driver_BulkWriteCommandResult_getInsertedCount, "getInsertedCount", |r| zval::long(r.inserted));
getter!(zim_MongoDB_Driver_BulkWriteCommandResult_getMatchedCount, "getMatchedCount", |r| zval::long(r.matched));
getter!(zim_MongoDB_Driver_BulkWriteCommandResult_getModifiedCount, "getModifiedCount", |r| zval::long(r.modified));
getter!(zim_MongoDB_Driver_BulkWriteCommandResult_getDeletedCount, "getDeletedCount", |r| zval::long(r.deleted));
getter!(zim_MongoDB_Driver_BulkWriteCommandResult_getUpsertedCount, "getUpsertedCount", |r| zval::long(r.upserted));
getter!(zim_MongoDB_Driver_BulkWriteCommandResult_getInsertResults, "getInsertResults", |r| opt_doc(&r.insert_results));
getter!(zim_MongoDB_Driver_BulkWriteCommandResult_getUpdateResults, "getUpdateResults", |r| opt_doc(&r.update_results));
getter!(zim_MongoDB_Driver_BulkWriteCommandResult_getDeleteResults, "getDeleteResults", |r| opt_doc(&r.delete_results));

pub unsafe extern "C" fn zim_MongoDB_Driver_BulkWriteCommandResult_isAcknowledged(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        zval::write(rv, zval::bool(res(ex).is_acknowledged));
    }
}

// ---------------------------------------------------------------------------------------------
// BulkWriteCommandException

unsafe fn exception_ce() -> *mut sys::zend_class_entry {
    super::lookup(c"mongodb\\driver\\exception\\bulkwritecommandexception")
}

pub unsafe fn init_MongoDB_Driver_Exception_BulkWriteCommandException(_c: *mut sys::zend_class_entry) {}

macro_rules! prop_getter {
    ($f:ident, $prop:literal) => {
        pub unsafe extern "C" fn $f(ex: Ex, rv: *mut sys::zval) {
            if !call::none(ex) {
                return;
            }
            let mut tmp = zval::undef();
            let p = sys::zend_read_property(exception_ce(), call::this(ex), $prop.as_ptr() as *const _, $prop.len(), false, &mut tmp);
            zval::write(rv, zval::copy(p));
        }
    };
}
prop_getter!(zim_MongoDB_Driver_Exception_BulkWriteCommandException_getErrorReply, "errorReply");
prop_getter!(zim_MongoDB_Driver_Exception_BulkWriteCommandException_getPartialResult, "partialResult");
prop_getter!(zim_MongoDB_Driver_Exception_BulkWriteCommandException_getWriteErrors, "writeErrors");
prop_getter!(zim_MongoDB_Driver_Exception_BulkWriteCommandException_getWriteConcernErrors, "writeConcernErrors");

/// zend_update_property (copies `v`; ours is released here)
unsafe fn set_prop(o: *mut sys::zend_object, name: &str, mut v: sys::zval) {
    sys::zend_update_property(exception_ce(), o, name.as_ptr() as *const _, name.len(), &mut v);
}

/// phongo_bulkwritecommandexception_init_props
unsafe fn init_props(o: *mut sys::zend_object, exc: &mongo_core::bulkwrite::BulkWriteException, result: Option<&sys::zval>) {
    if !exc.error_reply.is_empty() && !mongo_core::bson::iter::is_empty(&exc.error_reply) {
        set_prop(o, "errorReply", crate::bson::document::new_document(&exc.error_reply));
    }
    if let Some(r) = result {
        set_prop(o, "partialResult", zval::copy(r));
    }
    let wce_ce = super::lookup(c"mongodb\\driver\\writeconcernerror");
    let wces = hash::new(0);
    let mut ok = true;
    for_each_doc(&exc.write_concern_errors(), |_, d| {
        match super::writeresult::error_object(wce_ce, d, false, 0) {
            Some(z) => hash::push(wces, z),
            None => ok = false,
        }
        ok
    });
    let mut wces = zval::arr(wces);
    if ok {
        set_prop(o, "writeConcernErrors", std::mem::replace(&mut wces, zval::undef()));
    }
    drop(wces);
    let we_ce = super::lookup(c"mongodb\\driver\\writeerror");
    let wes = hash::new(0);
    let mut ok = true;
    for_each_doc(&exc.write_errors(), |key, d| {
        // ZEND_STRTOUL of the key
        let index = std::str::from_utf8(key).ok().and_then(|k| k.parse::<u64>().ok()).unwrap_or(0);
        match super::writeresult::error_object(we_ce, d, true, index as i32) {
            Some(z) => hash::index_update(wes, index, z),
            None => ok = false,
        }
        ok
    });
    let mut wes = zval::arr(wes);
    if ok {
        set_prop(o, "writeErrors", std::mem::replace(&mut wes, zval::undef()));
    }
    drop(wes);
}

/// The document elements of `doc` (others are skipped); `f` returns false to stop.
fn for_each_doc(doc: &[u8], mut f: impl FnMut(&[u8], &[u8]) -> bool) {
    let Some(mut it) = Iter::new(doc) else { return };
    while it.next() {
        if let Value::Document(d) = it.value() {
            if !f(it.key(), d) {
                return;
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// executeBulkWriteCommand

/// Manager::executeBulkWriteCommand and Server::executeBulkWriteCommand (the Server passes its
/// id; the Manager selects a writable server, or the session's pinned one, first).
pub unsafe fn execute(ex: Ex, rv: *mut sys::zval, client: ClientRef, server_id: Option<u32>) {
    let mut zbwc: *mut sys::zval = null_mut();
    let mut options: *mut sys::zval = null_mut();
    if sys::zend_parse_parameters(call::num_args(ex), c"O|z!".as_ptr(), &mut zbwc, CE, &mut options) != 0 {
        return;
    }
    let Ok(session) = super::parse_session(options, &client) else { return };
    let server_id = match server_id {
        Some(s) => s,
        None => {
            // phongo_manager_select_server(for_writes)
            let Some(pinned) = super::session_server_id(session) else { return };
            if pinned > 0 {
                pinned
            } else {
                let selected = client.checkout().select_server_id(OpType::Write, None, &[]);
                super::apm::flush(&client);
                match selected {
                    Ok((s, _)) => s,
                    Err(e) => {
                        super::throw_error(&e, None);
                        return;
                    }
                }
            }
        }
    };
    execute_bulkwritecommand(rv, &client, obj::data::<BulkWriteCommand>((*zbwc).value.obj), options, server_id);
}

/// phongo_execute_bulkwritecommand
unsafe fn execute_bulkwritecommand(rv: *mut sys::zval, client: &ClientRef, bwc: &mut BulkWriteCommand, options: *mut sys::zval, server_id: u32) {
    let Ok(session) = super::parse_session(options, client) else { return };
    let Ok(wc_opt) = super::parse_write_concern(options) else { return };
    let acked = wc_opt.as_ref().map_or_else(|| super::writeconcern::WriteConcern::from_uri(&client.shared().uri).is_acknowledged(), |w| w.is_acknowledged());
    if session.is_some() && !acked {
        throw(E::InvalidArgument, "Cannot combine \"session\" option with an unacknowledged write concern");
        return;
    }
    let opts = ExecOpts {
        ordered: Some(bwc.ordered),
        bypass_document_validation: bwc.bypass,
        let_: bwc.let_.clone(),
        write_concern: wc_opt,
        verbose_results: Some(bwc.verbose),
        comment: bwc.comment.clone(),
        server_id,
    };
    if let Some(z) = session {
        bwc.session = zval::copy(z);
    }
    let ret = {
        let mut cl = client.checkout();
        match session {
            Some(z) => super::with_session(z, |s| bwc.bw.execute(&mut cl, &opts, Some(s))),
            None => Some(bwc.bw.execute(&mut cl, &opts, None)),
        }
    };
    let Some(ret) = ret else { return };
    super::apm::flush(client);
    zval::write(rv, new_result(ret.res.as_ref()));
    let Some(exc) = ret.exc else { return };
    use mongo_core::error::domain;
    let has_top_level_error = exc.error.code != 0;
    let empty = [5u8, 0, 0, 0, 0];
    let error_reply: &[u8] = if exc.error_reply.is_empty() { &empty } else { &exc.error_reply };
    if has_top_level_error && exc.error.domain != domain::SERVER && exc.error.domain != domain::WRITE_CONCERN {
        super::throw_error(&exc.error, Some(error_reply));
    }
    let pending = (*crate::zend::eg()).exception;
    // an InvalidArgumentException alone when there is neither a partial result nor an error reply
    if !pending.is_null() && (*pending).ce == super::lookup(c"mongodb\\driver\\exception\\invalidargumentexception") && ret.res.is_none() && mongo_core::bson::iter::is_empty(error_reply) {
        return;
    }
    let ex_obj = if !pending.is_null() {
        let name = crate::zend::class::name((*pending).ce);
        super::throw_code(exception_ce(), &format!("Bulk write failed due to previous {name}: {}", exc.error.message), 0)
    } else {
        let msg = if has_top_level_error { exc.error.message.as_str() } else { "Bulk write failed" };
        super::throw_code(exception_ce(), msg, exc.error.php_code())
    };
    init_props(ex_obj, &exc, ret.res.is_some().then_some(&*rv));
    super::add_error_labels(ex_obj, error_reply);
}

pub unsafe extern "C" fn zim_MongoDB_Driver_Manager_executeBulkWriteCommand(ex: Ex, rv: *mut sys::zval) {
    let Some(c) = super::manager::client_of(ex) else { return };
    execute(ex, rv, c, None);
}

pub unsafe extern "C" fn zim_MongoDB_Driver_Server_executeBulkWriteCommand(ex: Ex, rv: *mut sys::zval) {
    let s = obj::data::<super::server::Server>(call::this(ex));
    let Some(c) = s.client.clone() else { return };
    execute(ex, rv, c, Some(s.server_id));
}
