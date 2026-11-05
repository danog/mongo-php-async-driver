//! MongoDB\BSON\Document, PackedArray and Iterator (Document.c, PackedArray.c, Iterator.c).
#![allow(non_snake_case)]

use super::decode::{self, State, TypeMap, TM};
use super::types::{cls, new, setup, this, Cls, Props};
use super::{ce, ce_mut, encode, throw, E};
use crate::zend::call::{self, Ex};
use crate::zend::{hash, obj, sys, zstr, zval};
use mongo_core::bson::iter::{self, Iter};
use mongo_core::bson::json::{self, Mode};
use mongo_core::bson::{fmt, json_parse};
use std::ptr::{addr_of_mut, null_mut};

/// The state of Document and PackedArray: the raw BSON.
#[derive(Default, Clone)]
pub struct Bson {
    pub bson: Vec<u8>,
    props: Props,
}

#[derive(Default, Clone)]
pub struct PackedArray(pub Bson);

cls!(Bson, H_DOCUMENT);
cls!(PackedArray, H_PACKEDARRAY);

unsafe fn fill_data(b: &Bson, ht: *mut sys::HashTable) {
    if !b.bson.is_empty() {
        hash::str_update(ht, b"data", zval::string(fmt::base64_encode(&b.bson).as_bytes()));
    }
}

/// The debug "value": the decoded content with nested documents/arrays as BSON objects.
unsafe fn debug_value(b: &Bson, ht: *mut sys::HashTable, is_array: bool) -> bool {
    if b.bson.is_empty() {
        return true;
    }
    let map = TypeMap { array: TM::Bson, document: TM::Bson, ..Default::default() };
    let st = State { is_visiting_array: is_array, ..State::new(&map) };
    match decode::to_zval(&b.bson, &st) {
        Some(z) => {
            hash::str_update(ht, b"value", z);
            true
        }
        None => false,
    }
}

impl Cls for Bson {
    fn handlers() -> *mut sys::zend_object_handlers {
        Self::h()
    }
    fn props(&mut self) -> &mut Props {
        &mut self.props
    }
    fn dup(&self) -> Self {
        self.clone()
    }
    unsafe fn fill(&mut self, ht: *mut sys::HashTable, debug: bool) -> bool {
        fill_data(self, ht);
        !debug || debug_value(self, ht, false)
    }
}

impl Cls for PackedArray {
    fn handlers() -> *mut sys::zend_object_handlers {
        Self::h()
    }
    fn props(&mut self) -> &mut Props {
        &mut self.0.props
    }
    fn dup(&self) -> Self {
        self.clone()
    }
    unsafe fn fill(&mut self, ht: *mut sys::HashTable, debug: bool) -> bool {
        fill_data(&self.0, ht);
        !debug || debug_value(&self.0, ht, true)
    }
}

pub unsafe fn new_document(d: &[u8]) -> sys::zval {
    new(ce().document, Bson { bson: d.to_vec(), props: Props::default() })
}

pub unsafe fn new_packedarray(d: &[u8]) -> sys::zval {
    new(ce().packedarray, PackedArray(Bson { bson: d.to_vec(), props: Props::default() }))
}

/// bson_compare
fn bson_compare(a: &[u8], b: &[u8]) -> i32 {
    let (x, y) = (&a[4.min(a.len())..], &b[4.min(b.len())..]);
    let n = x.len().min(y.len());
    match x[..n].cmp(&y[..n]) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Greater => 1,
        _ => (x.len() as i64 - y.len() as i64).signum() as i32,
    }
}

unsafe fn bson_of<'a>(z: *mut sys::zval) -> &'a [u8] {
    &obj::data::<Bson>((*z).value.obj).bson
}

unsafe extern "C" fn compare(a: *mut sys::zval, b: *mut sys::zval) -> i32 {
    if let Some(r) = super::types::cmp_fallback(a, b) {
        return r;
    }
    bson_compare(bson_of(a), bson_of(b))
}

unsafe fn from_hash(b: &mut Bson, ht: *mut sys::HashTable, c: *mut sys::zend_class_entry) -> bool {
    if let Some(d) = hash::find(ht, b"data").filter(|&z| zval::is(z, sys::IS_STRING)) {
        let raw = php_base64_decode(zval::str_bytes(d));
        if !iter::is_valid_static(&raw) {
            throw(E::InvalidArgument, format!("{} initialization requires valid BSON", crate::zend::class::name(c)));
            return false;
        }
        b.bson = raw;
        return true;
    }
    throw(E::InvalidArgument, format!("{} initialization requires \"data\" string field", crate::zend::class::name(c)));
    false
}

/// php_base64_decode_str (non-strict: invalid characters are skipped)
fn php_base64_decode(s: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let (mut acc, mut n) = (0u32, 0);
    for &c in s {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            _ => continue,
        } as u32;
        acc = acc << 6 | v;
        n += 1;
        if n == 4 {
            out.extend_from_slice(&[(acc >> 16) as u8, (acc >> 8) as u8, acc as u8]);
            acc = 0;
            n = 0;
        }
    }
    match n {
        2 => out.push((acc >> 4) as u8),
        3 => out.extend_from_slice(&[(acc >> 10) as u8, (acc >> 2) as u8]),
        _ => {}
    }
    out
}

/// bson_iter_find_w_len, then phongo_bson_value_to_zval.
unsafe fn get(bson: &[u8], key: &[u8], rv: *mut sys::zval, null_if_missing: bool, array: bool) -> bool {
    let Some(mut it) = Iter::new(bson) else {
        throw(E::Runtime, "Could not initialize BSON iterator");
        return false;
    };
    if !it.find(key) {
        if null_if_missing {
            *rv = zval::null();
            return true;
        }
        if array {
            throw(E::Runtime, format!("Could not find index \"{}\" in BSON array", String::from_utf8_lossy(key)));
        } else {
            throw(E::Runtime, format!("Could not find key \"{}\" in BSON document", super::cstr_lossy(key)));
        }
        return false;
    }
    match decode::value_to_zval(it.value()) {
        Some(z) => {
            *rv = z;
            true
        }
        None => false,
    }
}

fn has(bson: &[u8], key: &[u8]) -> bool {
    Iter::new(bson).is_some_and(|mut it| it.find(key))
}

/// zval_try_get_tmp_string of a string/int key.
unsafe fn key_of(z: *mut sys::zval) -> Vec<u8> {
    if zval::is(z, sys::IS_STRING) {
        zval::str_bytes(z).to_vec()
    } else {
        (*z).value.lval.to_string().into_bytes()
    }
}

unsafe fn get_by_zval(bson: &[u8], key: *mut sys::zval, rv: *mut sys::zval, null_if_missing: bool) -> bool {
    let key = zval::deref(key);
    if !zval::is(key, sys::IS_STRING) && !zval::is(key, sys::IS_LONG) {
        if null_if_missing {
            *rv = zval::null();
            return true;
        }
        throw(E::Runtime, format!("Could not find key of type \"{}\" in BSON document", zval::type_name(key)));
        return false;
    }
    get(bson, &key_of(key), rv, null_if_missing, false)
}

unsafe fn has_by_zval(bson: &[u8], key: *mut sys::zval) -> bool {
    let key = zval::deref(key);
    if !zval::is(key, sys::IS_STRING) && !zval::is(key, sys::IS_LONG) {
        return false;
    }
    has(bson, &key_of(key))
}

const BP_VAR_IS: i32 = 3;

unsafe extern "C" fn read_property(o: *mut sys::zend_object, m: *mut sys::zend_string, t: i32, _c: *mut *mut std::ffi::c_void, rv: *mut sys::zval) -> *mut sys::zval {
    if !get(&obj::data::<Bson>(o).bson, zstr::bytes(m), rv, t == BP_VAR_IS, false) {
        return addr_of_mut!((*addr_of_mut!(sys::executor_globals)).uninitialized_zval);
    }
    rv
}
unsafe extern "C" fn write_property(_o: *mut sys::zend_object, _m: *mut sys::zend_string, v: *mut sys::zval, _c: *mut *mut std::ffi::c_void) -> *mut sys::zval {
    throw(E::Logic, format!("Cannot write to {} property", crate::zend::class::name(ce().document)));
    v
}
unsafe extern "C" fn has_property(o: *mut sys::zend_object, m: *mut sys::zend_string, _h: i32, _c: *mut *mut std::ffi::c_void) -> i32 {
    has(&obj::data::<Bson>(o).bson, zstr::bytes(m)) as i32
}
unsafe extern "C" fn unset_property(_o: *mut sys::zend_object, _m: *mut sys::zend_string, _c: *mut *mut std::ffi::c_void) {
    throw(E::Logic, format!("Cannot unset {} property", crate::zend::class::name(ce().document)));
}
unsafe extern "C" fn read_dimension(o: *mut sys::zend_object, off: *mut sys::zval, t: i32, rv: *mut sys::zval) -> *mut sys::zval {
    if !get_by_zval(&obj::data::<Bson>(o).bson, off, rv, t == BP_VAR_IS) {
        return addr_of_mut!((*addr_of_mut!(sys::executor_globals)).uninitialized_zval);
    }
    rv
}
unsafe extern "C" fn write_dimension(_o: *mut sys::zend_object, _off: *mut sys::zval, _v: *mut sys::zval) {
    throw(E::Logic, format!("Cannot write to {} property", crate::zend::class::name(ce().document)));
}
unsafe extern "C" fn has_dimension(o: *mut sys::zend_object, m: *mut sys::zval, _e: i32) -> i32 {
    has_by_zval(&obj::data::<Bson>(o).bson, m) as i32
}
unsafe extern "C" fn unset_dimension(_o: *mut sys::zend_object, _off: *mut sys::zval) {
    throw(E::Logic, format!("Cannot unset {} property", crate::zend::class::name(ce().document)));
}

pub unsafe fn init_MongoDB_BSON_Document(c: *mut sys::zend_class_entry) {
    ce_mut().document = c;
    let h = setup::<Bson>(c);
    h.compare = Some(compare);
    h.read_property = Some(read_property);
    h.write_property = Some(write_property);
    h.has_property = Some(has_property);
    h.unset_property = Some(unset_property);
    h.read_dimension = Some(read_dimension);
    h.write_dimension = Some(write_dimension);
    h.has_dimension = Some(has_dimension);
    h.unset_dimension = Some(unset_dimension);
}

unsafe fn arg_str<'a>(ex: Ex, spec: &std::ffi::CStr) -> Option<&'a [u8]> {
    let (mut p, mut l): (*const u8, usize) = (null_mut(), 0);
    if sys::zend_parse_parameters(call::num_args(ex), spec.as_ptr(), &mut p, &mut l) != 0 {
        return None;
    }
    Some(std::slice::from_raw_parts(p, l))
}

unsafe fn to_json(ex: Ex, rv: *mut sys::zval, b: &[u8], mode: Mode, array: bool) {
    if !call::none(ex) {
        return;
    }
    match json::to_json(b, mode, array) {
        Some(j) => *rv = zval::string(&j),
        None if array => throw(E::UnexpectedValue, "Could not convert BSON array to a JSON string"),
        None => throw(E::UnexpectedValue, "Could not convert BSON document to a JSON string"),
    }
}

/// toPHP(?array $typeMap)
unsafe fn to_php(ex: Ex, rv: *mut sys::zval, b: &[u8], array: bool) {
    let mut tm: *mut sys::zval = null_mut();
    if sys::zend_parse_parameters(call::num_args(ex), c"|a!".as_ptr(), &mut tm) != 0 {
        return;
    }
    let mut map = TypeMap::default();
    if !decode::typemap(tm, &mut map) {
        return;
    }
    map.int64_as_object = true;
    let st = State { is_visiting_array: array, ..State::new(&map) };
    match decode::to_zval(b, &st) {
        Some(z) => *rv = z,
        None => *rv = zval::null(),
    }
}

pub unsafe extern "C" fn zim_MongoDB_BSON_Document___construct(_ex: Ex, _rv: *mut sys::zval) {}

pub unsafe extern "C" fn zim_MongoDB_BSON_Document_fromBSON(ex: Ex, rv: *mut sys::zval) {
    let Some(s) = arg_zstr(ex) else { return };
    match iter::read_first(s) {
        None => throw(E::UnexpectedValue, "Could not read document from BSON reader"),
        Some((_, false)) => throw(E::UnexpectedValue, "Reading document did not exhaust input buffer"),
        Some((d, true)) => *rv = new_document(d),
    }
}

/// Z_PARAM_STR into bytes
unsafe fn arg_zstr<'a>(ex: Ex) -> Option<&'a [u8]> {
    let mut s: *mut sys::zend_string = null_mut();
    if sys::zend_parse_parameters(call::num_args(ex), c"S".as_ptr(), &mut s) != 0 {
        return None;
    }
    Some(zstr::bytes(s))
}

pub unsafe extern "C" fn zim_MongoDB_BSON_Document_fromJSON(ex: Ex, rv: *mut sys::zval) {
    let Some(j) = arg_zstr(ex) else { return };
    match json_parse::from_json(j) {
        Ok(b) => *rv = new(ce().document, Bson { bson: b, props: Props::default() }),
        Err(e) => throw(E::UnexpectedValue, e),
    }
}
pub unsafe extern "C" fn zim_MongoDB_BSON_Document_fromPHP(ex: Ex, rv: *mut sys::zval) {
    let mut z: *mut sys::zval = null_mut();
    if sys::zend_parse_parameters(call::num_args(ex), c"A".as_ptr(), &mut z) != 0 {
        return;
    }
    let mut o = new(ce().document, Bson::default());
    match encode::to_bson(z, encode::ALLOW_ROOT_ARRAY) {
        Some(b) => {
            obj::data::<Bson>(o.value.obj).bson = b;
            *rv = o;
        }
        None => zval::dtor(&mut o),
    }
}
pub unsafe extern "C" fn zim_MongoDB_BSON_Document_get(ex: Ex, rv: *mut sys::zval) {
    let Some(k) = arg_str(ex, c"s") else { return };
    get(&this::<Bson>(ex).bson, k, rv, false, false);
}
pub unsafe extern "C" fn zim_MongoDB_BSON_Document_getIterator(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        iterator_init(rv, &mut (*ex).This);
    }
}
pub unsafe extern "C" fn zim_MongoDB_BSON_Document_has(ex: Ex, rv: *mut sys::zval) {
    let Some(k) = arg_str(ex, c"s") else { return };
    *rv = zval::bool(has(&this::<Bson>(ex).bson, k));
}
pub unsafe extern "C" fn zim_MongoDB_BSON_Document_toCanonicalExtendedJSON(ex: Ex, rv: *mut sys::zval) {
    to_json(ex, rv, &this::<Bson>(ex).bson, Mode::Canonical, false)
}
pub unsafe extern "C" fn zim_MongoDB_BSON_Document_toRelaxedExtendedJSON(ex: Ex, rv: *mut sys::zval) {
    to_json(ex, rv, &this::<Bson>(ex).bson, Mode::Relaxed, false)
}
pub unsafe extern "C" fn zim_MongoDB_BSON_Document_toPHP(ex: Ex, rv: *mut sys::zval) {
    to_php(ex, rv, &this::<Bson>(ex).bson, false)
}
unsafe fn one_zval(ex: Ex) -> Option<*mut sys::zval> {
    let mut z: *mut sys::zval = null_mut();
    if sys::zend_parse_parameters(call::num_args(ex), c"z".as_ptr(), &mut z) != 0 {
        return None;
    }
    Some(z)
}
pub unsafe extern "C" fn zim_MongoDB_BSON_Document_offsetExists(ex: Ex, rv: *mut sys::zval) {
    let Some(k) = one_zval(ex) else { return };
    *rv = zval::bool(has_by_zval(&this::<Bson>(ex).bson, k));
}
pub unsafe extern "C" fn zim_MongoDB_BSON_Document_offsetGet(ex: Ex, rv: *mut sys::zval) {
    let Some(k) = one_zval(ex) else { return };
    get_by_zval(&this::<Bson>(ex).bson, k, rv, false);
}
pub unsafe extern "C" fn zim_MongoDB_BSON_Document_offsetSet(_ex: Ex, _rv: *mut sys::zval) {
    throw(E::Logic, format!("Cannot write to {} property", crate::zend::class::name(ce().document)));
}
pub unsafe extern "C" fn zim_MongoDB_BSON_Document_offsetUnset(_ex: Ex, _rv: *mut sys::zval) {
    throw(E::Logic, format!("Cannot unset {} property", crate::zend::class::name(ce().document)));
}
pub unsafe extern "C" fn zim_MongoDB_BSON_Document___toString(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        *rv = zval::string(&this::<Bson>(ex).bson);
    }
}
unsafe fn arg_array(ex: Ex) -> Option<*mut sys::HashTable> {
    let mut z: *mut sys::zval = null_mut();
    if sys::zend_parse_parameters(call::num_args(ex), c"a".as_ptr(), &mut z) != 0 {
        return None;
    }
    Some((*z).value.arr)
}
pub unsafe extern "C" fn zim_MongoDB_BSON_Document___set_state(ex: Ex, rv: *mut sys::zval) {
    let Some(ht) = arg_array(ex) else { return };
    *rv = new(ce().document, Bson::default());
    from_hash(obj::data::<Bson>((*rv).value.obj), ht, ce().document);
}
pub unsafe extern "C" fn zim_MongoDB_BSON_Document___serialize(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        let ht = hash::new(1);
        fill_data(this::<Bson>(ex), ht);
        *rv = zval::arr(ht);
    }
}
pub unsafe extern "C" fn zim_MongoDB_BSON_Document___unserialize(ex: Ex, _rv: *mut sys::zval) {
    let Some(ht) = arg_array(ex) else { return };
    from_hash(this::<Bson>(ex), ht, ce().document);
}

// ---------------------------------------------------------------- PackedArray

unsafe fn pa<'a>(ex: Ex) -> &'a mut Bson {
    &mut this::<PackedArray>(ex).0
}

/// phongo_packedarray_get: seek_iter_to_index advances `index + 1` times (by position, not by key).
unsafe fn pa_get(b: &[u8], index: i64, rv: *mut sys::zval, null_if_missing: bool) -> bool {
    let Some(mut it) = Iter::new(b) else {
        throw(E::Runtime, "Could not initialize BSON iterator");
        return false;
    };
    if index < 0 {
        // never advanced: libbson reads a value at offset 0 (see Iter::unpositioned_value)
        return match Iter::unpositioned_value(b) {
            Ok(v) => match decode::value_to_zval(v) {
                Some(z) => {
                    *rv = z;
                    true
                }
                None => false,
            },
            Err(t) => {
                throw(E::UnexpectedValue, format!("Detected unsupported BSON type {}", t));
                false
            }
        };
    }
    if !seek_index(&mut it, index) {
        if null_if_missing {
            *rv = zval::null();
            return true;
        }
        // "%d" of a zend_long: phongo's printf reads an int
        throw(E::Runtime, format!("Could not find index \"{}\" in BSON array", index as i32));
        return false;
    }
    match decode::value_to_zval(it.value()) {
        Some(z) => {
            *rv = z;
            true
        }
        None => false,
    }
}

/// seek_iter_to_index
fn seek_index(it: &mut Iter, index: i64) -> bool {
    let mut i = 0;
    while i <= index {
        if !it.next() {
            return false;
        }
        i += 1;
    }
    true
}

/// phongo_packedarray_has
fn pa_has(b: &[u8], index: i64) -> bool {
    Iter::new(b).is_some_and(|mut it| seek_index(&mut it, index))
}

unsafe fn pa_get_by_zval(b: &[u8], key: *mut sys::zval, rv: *mut sys::zval, null_if_missing: bool) -> bool {
    let key = zval::deref(key);
    if !zval::is(key, sys::IS_LONG) {
        if null_if_missing {
            *rv = zval::null();
            return true;
        }
        throw(E::Runtime, format!("Could not find index of type \"{}\" in BSON array", zval::type_name(key)));
        return false;
    }
    pa_get(b, (*key).value.lval, rv, null_if_missing)
}

unsafe fn pa_has_by_zval(b: &[u8], key: *mut sys::zval) -> bool {
    let key = zval::deref(key);
    zval::is(key, sys::IS_LONG) && pa_has(b, (*key).value.lval)
}

unsafe extern "C" fn pa_read_dimension(o: *mut sys::zend_object, off: *mut sys::zval, t: i32, rv: *mut sys::zval) -> *mut sys::zval {
    if !pa_get_by_zval(&obj::data::<PackedArray>(o).0.bson, off, rv, t == BP_VAR_IS) {
        return addr_of_mut!((*addr_of_mut!(sys::executor_globals)).uninitialized_zval);
    }
    rv
}
unsafe extern "C" fn pa_write_dimension(_o: *mut sys::zend_object, _off: *mut sys::zval, _v: *mut sys::zval) {
    throw(E::Logic, format!("Cannot write to {} offset", crate::zend::class::name(ce().packedarray)));
}
unsafe extern "C" fn pa_has_dimension(o: *mut sys::zend_object, m: *mut sys::zval, _e: i32) -> i32 {
    pa_has_by_zval(&obj::data::<PackedArray>(o).0.bson, m) as i32
}
unsafe extern "C" fn pa_unset_dimension(_o: *mut sys::zend_object, _off: *mut sys::zval) {
    throw(E::Logic, format!("Cannot unset {} offset", crate::zend::class::name(ce().packedarray)));
}
unsafe extern "C" fn pa_compare(a: *mut sys::zval, b: *mut sys::zval) -> i32 {
    if let Some(r) = super::types::cmp_fallback(a, b) {
        return r;
    }
    bson_compare(&obj::data::<PackedArray>((*a).value.obj).0.bson, &obj::data::<PackedArray>((*b).value.obj).0.bson)
}

pub unsafe fn init_MongoDB_BSON_PackedArray(c: *mut sys::zend_class_entry) {
    ce_mut().packedarray = c;
    let h = setup::<PackedArray>(c);
    h.compare = Some(pa_compare);
    h.read_dimension = Some(pa_read_dimension);
    h.write_dimension = Some(pa_write_dimension);
    h.has_dimension = Some(pa_has_dimension);
    h.unset_dimension = Some(pa_unset_dimension);
}

pub unsafe extern "C" fn zim_MongoDB_BSON_PackedArray___construct(_ex: Ex, _rv: *mut sys::zval) {}
pub unsafe extern "C" fn zim_MongoDB_BSON_PackedArray_fromJSON(ex: Ex, rv: *mut sys::zval) {
    let Some(j) = arg_zstr(ex) else { return };
    let b = match json_parse::from_json(j) {
        Ok(b) => b,
        Err(e) => {
            throw(E::UnexpectedValue, e);
            return;
        }
    };
    let Some(mut it) = Iter::new(&b) else {
        throw(E::UnexpectedValue, "Received invalid JSON array");
        return;
    };
    let mut expected = 0u32;
    while it.next() {
        let k = it.key();
        if k != expected.to_string().as_bytes() {
            throw(E::UnexpectedValue, format!("Received invalid JSON array: expected key {}, but found \"{}\"", expected, super::cstr_lossy(k)));
            return;
        }
        expected += 1;
    }
    *rv = new(ce().packedarray, PackedArray(Bson { bson: b, props: Props::default() }));
}
pub unsafe extern "C" fn zim_MongoDB_BSON_PackedArray_fromPHP(ex: Ex, rv: *mut sys::zval) {
    let mut z: *mut sys::zval = null_mut();
    if sys::zend_parse_parameters(call::num_args(ex), c"a".as_ptr(), &mut z) != 0 {
        return;
    }
    if !hash::is_list((*z).value.arr) {
        throw(E::InvalidArgument, "Expected value to be a list, but given array is not");
        return;
    }
    let mut o = new(ce().packedarray, PackedArray::default());
    match encode::to_bson(z, encode::NONE) {
        Some(b) => {
            obj::data::<PackedArray>(o.value.obj).0.bson = b;
            *rv = o;
        }
        None => zval::dtor(&mut o),
    }
}
unsafe fn arg_long(ex: Ex) -> Option<i64> {
    let mut l: i64 = 0;
    if sys::zend_parse_parameters(call::num_args(ex), c"l".as_ptr(), &mut l) != 0 {
        return None;
    }
    Some(l)
}
pub unsafe extern "C" fn zim_MongoDB_BSON_PackedArray_get(ex: Ex, rv: *mut sys::zval) {
    let Some(i) = arg_long(ex) else { return };
    pa_get(&pa(ex).bson, i, rv, false);
}
pub unsafe extern "C" fn zim_MongoDB_BSON_PackedArray_getIterator(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        iterator_init(rv, &mut (*ex).This);
    }
}
pub unsafe extern "C" fn zim_MongoDB_BSON_PackedArray_has(ex: Ex, rv: *mut sys::zval) {
    let Some(i) = arg_long(ex) else { return };
    *rv = zval::bool(pa_has(&pa(ex).bson, i));
}
pub unsafe extern "C" fn zim_MongoDB_BSON_PackedArray_toCanonicalExtendedJSON(ex: Ex, rv: *mut sys::zval) {
    to_json(ex, rv, &pa(ex).bson, Mode::Canonical, true)
}
pub unsafe extern "C" fn zim_MongoDB_BSON_PackedArray_toRelaxedExtendedJSON(ex: Ex, rv: *mut sys::zval) {
    to_json(ex, rv, &pa(ex).bson, Mode::Relaxed, true)
}
pub unsafe extern "C" fn zim_MongoDB_BSON_PackedArray_toPHP(ex: Ex, rv: *mut sys::zval) {
    to_php(ex, rv, &pa(ex).bson, true)
}
pub unsafe extern "C" fn zim_MongoDB_BSON_PackedArray_offsetExists(ex: Ex, rv: *mut sys::zval) {
    let Some(k) = one_zval(ex) else { return };
    *rv = zval::bool(pa_has_by_zval(&pa(ex).bson, k));
}
pub unsafe extern "C" fn zim_MongoDB_BSON_PackedArray_offsetGet(ex: Ex, rv: *mut sys::zval) {
    let Some(k) = one_zval(ex) else { return };
    pa_get_by_zval(&pa(ex).bson, k, rv, false);
}
pub unsafe extern "C" fn zim_MongoDB_BSON_PackedArray_offsetSet(_ex: Ex, _rv: *mut sys::zval) {
    throw(E::Logic, format!("Cannot write to {} offset", crate::zend::class::name(ce().packedarray)));
}
pub unsafe extern "C" fn zim_MongoDB_BSON_PackedArray_offsetUnset(_ex: Ex, _rv: *mut sys::zval) {
    throw(E::Logic, format!("Cannot unset {} offset", crate::zend::class::name(ce().packedarray)));
}
pub unsafe extern "C" fn zim_MongoDB_BSON_PackedArray___toString(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        *rv = zval::string(&pa(ex).bson);
    }
}
pub unsafe extern "C" fn zim_MongoDB_BSON_PackedArray___set_state(ex: Ex, rv: *mut sys::zval) {
    let Some(ht) = arg_array(ex) else { return };
    *rv = new(ce().packedarray, PackedArray::default());
    from_hash(&mut obj::data::<PackedArray>((*rv).value.obj).0, ht, ce().packedarray);
}
pub unsafe extern "C" fn zim_MongoDB_BSON_PackedArray___serialize(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        let ht = hash::new(1);
        fill_data(pa(ex), ht);
        *rv = zval::arr(ht);
    }
}
pub unsafe extern "C" fn zim_MongoDB_BSON_PackedArray___unserialize(ex: Ex, _rv: *mut sys::zval) {
    let Some(ht) = arg_array(ex) else { return };
    from_hash(pa(ex), ht, ce().packedarray);
}

// ---------------------------------------------------------------- Iterator

pub struct Iterator {
    bson: sys::zval,
    current: sys::zval,
    /// iterates the BSON owned by `bson` (immutable while referenced)
    it: Option<Iter<'static>>,
    is_array: bool,
    valid: bool,
    key: i64,
    props: Props,
}

impl Default for Iterator {
    fn default() -> Self {
        Iterator { bson: zval::undef(), current: zval::undef(), it: None, is_array: false, valid: false, key: 0, props: Props::default() }
    }
}

impl Drop for Iterator {
    fn drop(&mut self) {
        self.it = None;
        unsafe {
            if !zval::is(&self.current, sys::IS_UNDEF) {
                zval::dtor(&mut self.current);
            }
            if !zval::is(&self.bson, sys::IS_UNDEF) {
                zval::dtor(&mut self.bson);
            }
        }
    }
}

cls!(Iterator, H_ITERATOR);

impl Cls for Iterator {
    fn handlers() -> *mut sys::zend_object_handlers {
        Self::h()
    }
    fn props(&mut self) -> &mut Props {
        &mut self.props
    }
    /// phongo_iterator_clone_object: a fresh iterator over the same BSON, at its first element.
    fn dup(&self) -> Self {
        let mut n = Iterator::default();
        n.is_array = self.is_array;
        unsafe {
            n.bson = zval::copy(&self.bson);
            if let Some(mut it) = Iter::new(n.data()) {
                n.valid = it.next();
                n.it = Some(it);
            }
        }
        n
    }
    unsafe fn fill(&mut self, ht: *mut sys::HashTable, _debug: bool) -> bool {
        hash::str_update(ht, b"bson", zval::copy(&self.bson));
        true
    }
}

impl Iterator {
    unsafe fn data(&self) -> &'static [u8] {
        let o = self.bson.value.obj;
        let b: &[u8] = if self.is_array { &obj::data::<PackedArray>(o).0.bson } else { &obj::data::<Bson>(o).bson };
        std::slice::from_raw_parts(b.as_ptr(), b.len())
    }
    fn free_current(&mut self) {
        if unsafe { !zval::is(&self.current, sys::IS_UNDEF) } {
            unsafe { zval::dtor(&mut self.current) };
            self.current = zval::undef();
        }
    }
}

pub unsafe fn init_MongoDB_BSON_Iterator(c: *mut sys::zend_class_entry) {
    ce_mut().iterator = c;
    setup::<Iterator>(c);
    (*c).get_iterator = Some(get_iterator);
}

/// phongo_iterator_init
unsafe fn iterator_init(rv: *mut sys::zval, zbson: *mut sys::zval) {
    let c = ce();
    let oce = zval::obj_ce(zbson);
    let is_array = obj::instanceof(oce, c.packedarray);
    if !is_array && !obj::instanceof(oce, c.document) {
        throw(E::Logic, format!("Could not create iterator for {} instance", zval::type_name(zbson)));
        return;
    }
    let mut o = new(c.iterator, Iterator::default());
    let me = obj::data::<Iterator>(o.value.obj);
    me.bson = zval::copy(zbson);
    me.is_array = is_array;
    let Some(mut it) = Iter::new(me.data()) else {
        throw(E::UnexpectedValue, format!("Could not create iterator for {} instance", zval::type_name(zbson)));
        zval::dtor(&mut o);
        return;
    };
    me.valid = it.next();
    me.it = Some(it);
    *rv = o;
}

pub unsafe extern "C" fn zim_MongoDB_BSON_Iterator___construct(_ex: Ex, _rv: *mut sys::zval) {}
impl Iterator {
    /// phongo_iterator_get_current: null after throwing.
    unsafe fn get_current(&mut self) -> *mut sys::zval {
        if !self.valid {
            throw(E::Logic, "Cannot call current() on an exhausted iterator");
            return null_mut();
        }
        if zval::is(&self.current, sys::IS_UNDEF) {
            match decode::value_to_zval(self.it.as_ref().unwrap().value()) {
                Some(z) => self.current = z,
                None => return null_mut(),
            }
        }
        &mut self.current
    }
    /// phongo_iterator_key: false after throwing.
    unsafe fn get_key(&self, rv: *mut sys::zval) -> bool {
        if !self.valid {
            throw(E::Logic, "Cannot call key() on an exhausted iterator");
            return false;
        }
        if self.is_array {
            *rv = zval::long(self.key);
            return true;
        }
        let it = self.it.as_ref().unwrap();
        let k = it.key();
        if !mongo_core::bson::utf8::validate(k, false) {
            throw(E::UnexpectedValue, format!("Detected corrupt BSON data at offset {}", it.off));
            return false;
        }
        *rv = zval::string(k);
        true
    }
    /// phongo_iterator_next
    fn advance(&mut self) {
        self.valid = self.it.as_mut().unwrap().next();
        self.key += 1;
        self.free_current();
    }
    /// phongo_iterator_rewind
    unsafe fn rewind(&mut self) {
        if self.key == 0 {
            return;
        }
        self.free_current();
        let mut it = Iter::new(self.data()).unwrap();
        self.key = 0;
        self.valid = it.next();
        self.it = Some(it);
    }
}

pub unsafe extern "C" fn zim_MongoDB_BSON_Iterator_current(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        let c = this::<Iterator>(ex).get_current();
        if !c.is_null() {
            *rv = zval::copy(c);
        }
    }
}
pub unsafe extern "C" fn zim_MongoDB_BSON_Iterator_key(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        this::<Iterator>(ex).get_key(rv);
    }
}
pub unsafe extern "C" fn zim_MongoDB_BSON_Iterator_next(ex: Ex, _rv: *mut sys::zval) {
    if call::none(ex) {
        this::<Iterator>(ex).advance();
    }
}
pub unsafe extern "C" fn zim_MongoDB_BSON_Iterator_valid(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        *rv = zval::bool(this::<Iterator>(ex).valid);
    }
}
pub unsafe extern "C" fn zim_MongoDB_BSON_Iterator_rewind(ex: Ex, _rv: *mut sys::zval) {
    if call::none(ex) {
        this::<Iterator>(ex).rewind();
    }
}

// zend_object_iterator for foreach (phongo_iterator_it_funcs)
unsafe fn it_intern<'a>(iter: *mut sys::zend_object_iterator) -> &'a mut Iterator {
    obj::data::<Iterator>((*iter).data.value.obj)
}
unsafe extern "C" fn it_dtor(iter: *mut sys::zend_object_iterator) {
    zval::dtor(&mut (*iter).data);
}
unsafe extern "C" fn it_valid(iter: *mut sys::zend_object_iterator) -> sys::zend_result {
    if it_intern(iter).valid {
        0
    } else {
        -1
    }
}
unsafe extern "C" fn it_get_current_data(iter: *mut sys::zend_object_iterator) -> *mut sys::zval {
    it_intern(iter).get_current()
}
unsafe extern "C" fn it_get_current_key(iter: *mut sys::zend_object_iterator, key: *mut sys::zval) {
    it_intern(iter).get_key(key);
}
unsafe extern "C" fn it_move_forward(iter: *mut sys::zend_object_iterator) {
    it_intern(iter).advance();
}
unsafe extern "C" fn it_rewind(iter: *mut sys::zend_object_iterator) {
    it_intern(iter).rewind();
}
unsafe extern "C" fn it_get_gc(iter: *mut sys::zend_object_iterator, table: *mut *mut sys::zval, n: *mut i32) -> *mut sys::HashTable {
    *n = 1;
    *table = &mut (*iter).data;
    null_mut()
}
static IT_FUNCS: sys::zend_object_iterator_funcs = sys::zend_object_iterator_funcs {
    dtor: Some(it_dtor),
    valid: Some(it_valid),
    get_current_data: Some(it_get_current_data),
    get_current_key: Some(it_get_current_key),
    move_forward: Some(it_move_forward),
    rewind: Some(it_rewind),
    invalidate_current: None,
    get_gc: Some(it_get_gc),
};
/// phongo_iterator_get_iterator
unsafe extern "C" fn get_iterator(_ce: *mut sys::zend_class_entry, object: *mut sys::zval, by_ref: i32) -> *mut sys::zend_object_iterator {
    if by_ref != 0 {
        throw(E::Logic, "An iterator cannot be used with foreach by reference");
        return null_mut();
    }
    let iter = sys::_emalloc(std::mem::size_of::<sys::zend_object_iterator>()) as *mut sys::zend_object_iterator;
    sys::zend_iterator_init(iter);
    (*iter).data = zval::copy(object);
    (*iter).funcs = &IT_FUNCS;
    iter
}
