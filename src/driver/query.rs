//! MongoDB\Driver\Query (src/MongoDB/Query.c) and phongo_execute_query.

use super::readconcern::ReadConcern;
use crate::bson::types::{cls, Cls, Props};
use crate::bson::{encode, throw, E};
use crate::zend::call::{self, Ex};
use crate::zend::{hash, obj, sys, zval};
use mongo_core::bson::writer::Writer;
use std::ptr::{addr_of_mut, null_mut};

#[derive(Default)]
pub struct Query {
    pub filter: Option<Vec<u8>>,
    pub opts: Option<Vec<u8>>,
    pub read_concern: Option<ReadConcern>,
    pub max_await_time_ms: u32,
    props: Props,
}
cls!(Query, H_QUERY);

impl Cls for Query {
    fn handlers() -> *mut sys::zend_object_handlers {
        Self::h()
    }
    fn props(&mut self) -> &mut Props {
        &mut self.props
    }
    fn dup(&self) -> Self {
        Query::default()
    }
    unsafe fn fill(&mut self, ht: *mut sys::HashTable, _debug: bool) -> bool {
        for (k, d) in [(&b"filter"[..], &self.filter), (b"options", &self.opts)] {
            match d {
                Some(b) => match super::to_zval(b, &Default::default()) {
                    Some(z) => hash::str_update(ht, k, z),
                    None => return true,
                },
                None => hash::str_update(ht, k, zval::null()),
            }
        }
        match &self.read_concern {
            Some(rc) => hash::str_update(ht, b"readConcern", super::readconcern::new_readconcern(rc.clone())),
            None => hash::str_update(ht, b"readConcern", zval::null()),
        }
        true
    }
}

pub static mut CE: *mut sys::zend_class_entry = null_mut();

pub unsafe fn init_MongoDB_Driver_Query(c: *mut sys::zend_class_entry) {
    CE = c;
    let h = crate::bson::types::setup::<Query>(c);
    h.get_properties = sys::std_object_handlers.get_properties;
    h.clone_obj = None;
}

/// phongo_query_init(&query, NULL, NULL): an empty Query (ClientEncryption::getKeys)
pub unsafe fn new_empty() -> sys::zval {
    let z = crate::zend::obj::new_zval(CE);
    let q = crate::zend::obj::data::<Query>(z.value.obj);
    q.filter = Some(Writer::new().finish());
    q.opts = Some(Writer::new().finish());
    z
}

fn has_empty_keys(d: &[u8]) -> bool {
    mongo_core::bulk::validate_empty_keys(d).is_err()
}

unsafe fn init(q: &mut Query, filter: *mut sys::zval, options: *mut sys::zval) {
    q.max_await_time_ms = 0;
    let Some(f) = encode::to_bson(filter, encode::NONE) else { return };
    if has_empty_keys(&f) {
        q.filter = Some(f);
        throw(E::InvalidArgument, "Cannot use empty keys in filter document");
        return;
    }
    q.filter = Some(f);
    let mut w = Writer::with_capacity(128);
    let done = |q: &mut Query, w: Writer| q.opts = Some(w.finish());
    if options.is_null() || !zval::is(options, sys::IS_ARRAY) {
        done(q, w);
        return;
    }
    let o = (*options).value.arr;
    macro_rules! bool_opt {
        ($k:literal) => {
            if let Some(v) = hash::sym_find(o, $k.as_bytes()) {
                w.bool($k.as_bytes(), zval::is_true(v));
            }
        };
    }
    macro_rules! i64_opt {
        ($k:literal) => {
            if let Some(v) = hash::sym_find(o, $k.as_bytes()) {
                w.int64($k.as_bytes(), sys::zval_get_long_func(v, false));
            }
        };
    }
    macro_rules! doc_opt {
        ($k:literal) => {
            if let Some(v) = hash::sym_find(o, $k.as_bytes()) {
                if !doc_opt(&mut w, $k, v) {
                    done(q, w);
                    return;
                }
            }
        };
    }
    bool_opt!("allowDiskUse");
    bool_opt!("allowPartialResults");
    bool_opt!("awaitData");
    i64_opt!("batchSize");
    doc_opt!("collation");
    if let Some(v) = hash::sym_find(o, b"comment") {
        let Some(d) = encode::to_bson_value_doc(v, b"comment") else {
            done(q, w);
            return;
        };
        w.concat(&d);
    }
    bool_opt!("exhaust");
    doc_opt!("let");
    i64_opt!("limit");
    doc_opt!("max");
    i64_opt!("maxTimeMS");
    doc_opt!("min");
    bool_opt!("noCursorTimeout");
    doc_opt!("projection");
    bool_opt!("returnKey");
    bool_opt!("showRecordId");
    bool_opt!("singleBatch");
    i64_opt!("skip");
    doc_opt!("sort");
    bool_opt!("tailable");
    // hint: string or document
    if let Some(v) = hash::sym_find(o, b"hint") {
        let v = zval::deref(v);
        if zval::is(v, sys::IS_STRING) {
            let hint = zval::str_bytes(v);
            if hint.len() > i32::MAX as usize {
                throw(E::InvalidArgument, format!("Expected \"hint\" option to be <= {} bytes, {} given", i32::MAX, hint.len()));
                done(q, w);
                return;
            }
            w.utf8(b"hint", hint);
        } else if zval::is(v, sys::IS_OBJECT) || zval::is(v, sys::IS_ARRAY) {
            if !doc_opt(&mut w, "hint", v) {
                done(q, w);
                return;
            }
        } else {
            throw(E::InvalidArgument, format!("Expected \"hint\" option to be string, array, or object, {} given", super::type_by_const(v)));
            done(q, w);
            return;
        }
    }
    done(q, w);
    if let Some(v) = hash::sym_find(o, b"readConcern") {
        let v = zval::deref(v);
        if !zval::is(v, sys::IS_OBJECT) || !obj::instanceof((*(*v).value.obj).ce, super::readconcern::CE) {
            throw(E::InvalidArgument, format!("Expected \"readConcern\" option to be MongoDB\\Driver\\ReadConcern, {} given", zval::type_name(v)));
            return;
        }
        q.read_concern = Some(obj::data::<ReadConcern>((*v).value.obj).clone());
    }
    if let Some(v) = hash::sym_find(o, b"maxAwaitTimeMS") {
        let m = sys::zval_get_long_func(v, false);
        if m < 0 {
            throw(E::InvalidArgument, format!("Expected \"maxAwaitTimeMS\" option to be >= 0, {m} given"));
            return;
        }
        if m > u32::MAX as i64 {
            throw(E::InvalidArgument, format!("Expected \"maxAwaitTimeMS\" option to be <= {}, {m} given", u32::MAX));
            return;
        }
        q.max_await_time_ms = m as u32;
    }
}

/// phongo_query_opts_append_document
unsafe fn doc_opt(w: &mut Writer, k: &str, v: *mut sys::zval) -> bool {
    let v = zval::deref(v);
    if !zval::is(v, sys::IS_OBJECT) && !zval::is(v, sys::IS_ARRAY) {
        throw(E::InvalidArgument, format!("Expected \"{k}\" option to be array or object, {} given", zval::type_name(v)));
        return false;
    }
    let Some(d) = encode::to_bson(v, encode::NONE) else { return false };
    if has_empty_keys(&d) {
        throw(E::InvalidArgument, format!("Cannot use empty keys in \"{k}\" option"));
        return false;
    }
    w.document(k.as_bytes(), &d);
    true
}

pub unsafe extern "C" fn zim_MongoDB_Driver_Query___construct(ex: Ex, _rv: *mut sys::zval) {
    let (mut filter, mut options): (*mut sys::zval, *mut sys::zval) = (null_mut(), null_mut());
    if sys::zend_parse_parameters(call::num_args(ex), c"A|a!".as_ptr(), &mut filter, &mut options) != 0 {
        return;
    }
    init(obj::data::<Query>(call::this(ex)), filter, options);
}

#[allow(dead_code)]
fn _h() -> *mut sys::zend_object_handlers {
    addr_of_mut!(H_QUERY)
}
