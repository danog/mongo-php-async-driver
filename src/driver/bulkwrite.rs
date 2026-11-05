//! MongoDB\Driver\BulkWrite (src/MongoDB/BulkWrite.c over mongoc_bulk_operation_t) and
//! phongo_execute_bulk_write.

use super::writeconcern::WriteConcern;
use crate::bson::types::{cls, Cls, Props};
use crate::bson::{encode, throw, E};
use crate::zend::call::{self, Ex};
use crate::zend::{hash, obj, sys, zval};
use mongo_core::bson::iter::{Iter, Value};
use mongo_core::bson::writer::Writer;
use mongo_core::bulk::{BulkOperation, UpdateOpts};
use std::ptr::{addr_of_mut, null_mut};

pub struct BulkWrite {
    pub bulk: BulkOperation,
    pub ordered: bool,
    /// None: PHONGO_BULKWRITE_BYPASS_UNSET
    pub bypass: Option<bool>,
    pub let_: Option<Vec<u8>>,
    /// {"comment": value}
    pub comment: Option<Vec<u8>>,
    pub num_ops: i64,
    pub executed: bool,
    pub database: Option<String>,
    pub collection: Option<String>,
    pub session: sys::zval,
    pub write_concern: Option<WriteConcern>,
    props: Props,
}

impl Default for BulkWrite {
    fn default() -> Self {
        BulkWrite {
            bulk: BulkOperation::new(true),
            ordered: true,
            bypass: None,
            let_: None,
            comment: None,
            num_ops: 0,
            executed: false,
            database: None,
            collection: None,
            session: zval::undef(),
            write_concern: None,
            props: Props::default(),
        }
    }
}

cls!(BulkWrite, H_BULKWRITE);

impl Cls for BulkWrite {
    fn handlers() -> *mut sys::zend_object_handlers {
        Self::h()
    }
    fn props(&mut self) -> &mut Props {
        &mut self.props
    }
    fn dup(&self) -> Self {
        BulkWrite::default()
    }
    unsafe fn fill(&mut self, ht: *mut sys::HashTable, _debug: bool) -> bool {
        let s = |v: &Option<String>| v.as_ref().map_or_else(zval::null, |s| zval::string(s.as_bytes()));
        hash::str_update(ht, b"database", s(&self.database));
        hash::str_update(ht, b"collection", s(&self.collection));
        hash::str_update(ht, b"ordered", zval::bool(self.ordered));
        hash::str_update(ht, b"bypassDocumentValidation", self.bypass.map_or_else(zval::null, zval::bool));
        if let Some(c) = &self.comment {
            match super::legacy_value(c) {
                Some(z) => hash::str_update(ht, b"comment", z),
                None => return true,
            }
        }
        if let Some(l) = &self.let_ {
            match super::to_zval(l, &Default::default()) {
                Some(z) => hash::str_update(ht, b"let", z),
                None => return true,
            }
        }
        hash::str_update(ht, b"executed", zval::bool(self.executed));
        hash::str_update(ht, b"server_id", zval::long(self.bulk.server_id as i64));
        if zval::is(&self.session, sys::IS_UNDEF) {
            hash::str_update(ht, b"session", zval::null());
        } else {
            hash::str_update(ht, b"session", zval::copy(&self.session));
        }
        match &self.write_concern {
            Some(wc) => hash::str_update(ht, b"write_concern", super::writeconcern::new_writeconcern(wc.clone())),
            None => hash::str_update(ht, b"write_concern", zval::null()),
        }
        true
    }
}

pub static mut CE: *mut sys::zend_class_entry = null_mut();

pub unsafe fn init_MongoDB_Driver_BulkWrite(c: *mut sys::zend_class_entry) {
    CE = c;
    let h = crate::bson::types::setup::<BulkWrite>(c);
    h.get_properties = sys::std_object_handlers.get_properties;
    h.clone_obj = None;
}

#[inline]
unsafe fn me<'a>(ex: Ex) -> &'a mut BulkWrite {
    obj::data::<BulkWrite>(call::this(ex))
}

/// php_array_fetchc_bool
unsafe fn fetch_bool(ht: *mut sys::HashTable, k: &[u8]) -> bool {
    hash::sym_find(ht, k).is_some_and(|z| sys::zend_is_true(z))
}

unsafe fn opt_ht(z: *mut sys::zval) -> Option<*mut sys::HashTable> {
    (!z.is_null() && zval::is(z, sys::IS_ARRAY)).then(|| (*z).value.arr)
}

pub unsafe extern "C" fn zim_MongoDB_Driver_BulkWrite___construct(ex: Ex, _rv: *mut sys::zval) {
    let mut options: *mut sys::zval = null_mut();
    if sys::zend_parse_parameters(call::num_args(ex), c"|a!".as_ptr(), &mut options) != 0 {
        return;
    }
    let b = me(ex);
    let opts = opt_ht(options);
    let mut ordered = true;
    if let Some(o) = opts {
        if hash::sym_find(o, b"ordered").is_some() {
            ordered = fetch_bool(o, b"ordered");
        }
    }
    b.bulk = BulkOperation::new(ordered);
    b.ordered = ordered;
    b.bypass = None;
    b.let_ = None;
    b.num_ops = 0;
    b.executed = false;
    let Some(o) = opts else { return };
    if hash::sym_find(o, b"bypassDocumentValidation").is_some() {
        let v = fetch_bool(o, b"bypassDocumentValidation");
        b.bulk.flags.bypass_document_validation = v;
        b.bypass = Some(v);
    }
    if let Some(v) = hash::sym_find(o, b"let") {
        let v = zval::deref(v);
        if !zval::is(v, sys::IS_OBJECT) && !zval::is(v, sys::IS_ARRAY) {
            throw(E::InvalidArgument, format!("Expected \"let\" option to be array or object, {} given", super::type_by_const(v)));
            return;
        }
        let Some(d) = encode::to_bson(v, encode::NONE) else { return };
        b.let_ = Some(d.clone());
        b.bulk.let_ = Some(d);
    }
    if let Some(v) = hash::sym_find(o, b"comment") {
        let Some(d) = encode::to_bson_value_doc(v, b"comment") else { return };
        b.comment = Some(d.clone());
        b.bulk.comment = Some(d);
    }
}

pub unsafe extern "C" fn zim_MongoDB_Driver_BulkWrite_insert(ex: Ex, rv: *mut sys::zval) {
    let mut doc: *mut sys::zval = null_mut();
    if sys::zend_parse_parameters(call::num_args(ex), c"A".as_ptr(), &mut doc) != 0 {
        return;
    }
    let Some((bdoc, id)) = encode::to_bson_with_id(doc, encode::ADD_ID | encode::RETURN_ID) else { return };
    let Some(id) = id else {
        throw(E::Logic, "phongo_zval_to_bson() did not return an _id. Please file a bug report.");
        return;
    };
    let b = me(ex);
    if let Err(e) = b.bulk.insert(&bdoc) {
        super::throw_error(&e, None);
        return;
    }
    b.num_ops += 1;
    // phongo_bulkwrite_extract_id: decode {_id: ...} with an array root
    let tm = crate::bson::decode::TypeMap { root: crate::bson::decode::TM::Array, ..Default::default() };
    if let Some(z) = super::to_zval(&id, &tm) {
        if zval::is(&z, sys::IS_ARRAY) {
            if let Some(v) = hash::find(z.value.arr, b"_id") {
                zval::write(rv, zval::copy(v));
            }
        }
    }
}

/// phongo_bulkwrite_opts_append_document: the option as a document element.
unsafe fn opt_document(w: &mut Writer, o: *mut sys::HashTable, key: &str, flags: u32) -> Result<(), ()> {
    let Some(v) = hash::sym_find(o, key.as_bytes()) else { return Ok(()) };
    let v = zval::deref(v);
    if !zval::is(v, sys::IS_OBJECT) && !zval::is(v, sys::IS_ARRAY) {
        throw(E::InvalidArgument, format!("Expected \"{key}\" option to be array or object, {} given", super::type_by_const(v)));
        return Err(());
    }
    let Some(d) = encode::to_bson(v, flags) else { return Err(()) };
    if flags & encode::ALLOW_ROOT_ARRAY != 0 {
        // phongo_bulkwrite_bson_array_has_valid_keys
        if let Some(mut it) = Iter::new(&d) {
            let mut i = 0usize;
            while it.next() {
                if it.key() != i.to_string().as_bytes() {
                    throw(E::InvalidArgument, format!("\"{key}\" option has invalid keys for a BSON array"));
                    return Err(());
                }
                i += 1;
            }
        }
        w.array(key.as_bytes(), &d);
    } else {
        w.document(key.as_bytes(), &d);
    }
    Ok(())
}

/// phongo_bulkwrite_opt_hint: {"hint": string|document} or nothing.
unsafe fn opt_hint(o: Option<*mut sys::HashTable>) -> Result<Option<Vec<u8>>, ()> {
    let Some(o) = o else { return Ok(None) };
    let Some(v) = hash::sym_find(o, b"hint") else { return Ok(None) };
    let v = zval::deref(v);
    let mut w = Writer::with_capacity(64);
    if zval::is(v, sys::IS_STRING) {
        w.utf8(b"hint", zval::str_bytes(v));
    } else if zval::is(v, sys::IS_OBJECT) || zval::is(v, sys::IS_ARRAY) {
        opt_document(&mut w, o, "hint", encode::NONE)?;
    } else {
        throw(E::InvalidArgument, format!("Expected \"hint\" option to be string, array, or object, {} given", super::type_by_const(v)));
        return Err(());
    }
    Ok(Some(w.finish()))
}

/// The value of element `k` of a one-element document as a standalone document.
fn first_doc(d: &[u8]) -> Option<Vec<u8>> {
    let mut it = Iter::new(d)?;
    if !it.next() {
        return None;
    }
    match it.value() {
        Value::Document(x) | Value::Array(x) => Some(x.to_vec()),
        _ => None,
    }
}

unsafe fn one_opt(o: Option<*mut sys::HashTable>, key: &str, flags: u32) -> Result<Option<Vec<u8>>, ()> {
    let Some(o) = o else { return Ok(None) };
    let mut w = Writer::with_capacity(64);
    opt_document(&mut w, o, key, flags)?;
    let d = w.finish();
    Ok(first_doc(&d))
}

pub unsafe extern "C" fn zim_MongoDB_Driver_BulkWrite_update(ex: Ex, _rv: *mut sys::zval) {
    let (mut q, mut u, mut options): (*mut sys::zval, *mut sys::zval, *mut sys::zval) = (null_mut(), null_mut(), null_mut());
    if sys::zend_parse_parameters(call::num_args(ex), c"AA|a!".as_ptr(), &mut q, &mut u, &mut options) != 0 {
        return;
    }
    let Some(bq) = encode::to_bson(q, encode::NONE) else { return };
    let Some(bu) = encode::to_bson(u, encode::ALLOW_ROOT_ARRAY) else { return };
    let o = opt_ht(options);
    let multi = o.is_some_and(|o| fetch_bool(o, b"multi"));
    let upsert = o.is_some_and(|o| fetch_bool(o, b"upsert"));
    let mut uo = UpdateOpts { multi, upsert, ..Default::default() };
    let Ok(af) = one_opt(o, "arrayFilters", encode::ALLOW_ROOT_ARRAY) else { return };
    // the option was given (an empty array is "given" too)
    uo.array_filters = af;
    let Ok(c) = one_opt(o, "collation", encode::NONE) else { return };
    uo.collation = c;
    let Ok(s) = one_opt(o, "sort", encode::NONE) else { return };
    uo.sort = s;
    let Ok(h) = opt_hint(o) else { return };
    uo.hint = h;
    let b = me(ex);
    let r = if has_operators(&bu) || mongo_core::bulk::is_pipeline(&bu) {
        b.bulk.update(&bq, &bu, &uo, multi)
    } else {
        if multi {
            throw(E::InvalidArgument, "Replacement document conflicts with true \"multi\" option");
            return;
        }
        b.bulk.replace_one(&bq, &bu, &uo)
    };
    if let Err(e) = r {
        super::throw_error(&e, None);
        return;
    }
    b.num_ops += 1;
}

/// phongo_bulkwrite_update_has_operators: a key containing '$'
fn has_operators(d: &[u8]) -> bool {
    let Some(mut it) = Iter::new(d) else { return false };
    while it.next() {
        if it.key().contains(&b'$') {
            return true;
        }
    }
    false
}

pub unsafe extern "C" fn zim_MongoDB_Driver_BulkWrite_delete(ex: Ex, _rv: *mut sys::zval) {
    let (mut q, mut options): (*mut sys::zval, *mut sys::zval) = (null_mut(), null_mut());
    if sys::zend_parse_parameters(call::num_args(ex), c"A|a!".as_ptr(), &mut q, &mut options) != 0 {
        return;
    }
    let Some(bq) = encode::to_bson(q, encode::NONE) else { return };
    let o = opt_ht(options);
    let limit = o.is_some_and(|o| fetch_bool(o, b"limit"));
    let Ok(c) = one_opt(o, "collation", encode::NONE) else { return };
    let Ok(h) = opt_hint(o) else { return };
    let b = me(ex);
    if let Err(e) = b.bulk.remove(&bq, limit as i32, c, h) {
        super::throw_error(&e, None);
        return;
    }
    b.num_ops += 1;
}

pub unsafe extern "C" fn zim_MongoDB_Driver_BulkWrite_count(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        zval::write(rv, zval::long(me(ex).num_ops));
    }
}

#[allow(dead_code)]
fn _h() -> *mut sys::zend_object_handlers {
    addr_of_mut!(H_BULKWRITE)
}
