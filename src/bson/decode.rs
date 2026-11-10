//! BSON -> PHP values (phongo_bson.c): typemaps, field paths, ODM (__pclass) handling, nesting
//! limits and error messages as in ext-mongodb. Values are built straight into zvals while
//! libbson's visitor semantics (validation, error offsets) are kept.

use super::{ce, throw, types, E};
use crate::zend::{call, hash, obj, sys, zstr, zval};
use mongo_core::bson::iter::{self, Iter, Value, Visitor};

pub const MAX_DECODE_NESTING: usize = 500;
pub const MAX_NESTING: usize = 100;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TM {
    None,
    Array,
    Object,
    Class(*mut sys::zend_class_entry),
    Bson,
}

#[derive(Clone)]
pub struct TypeMap {
    pub document: TM,
    pub array: TM,
    pub root: TM,
    pub int64_as_object: bool,
    pub field_paths: Vec<(Vec<Vec<u8>>, TM)>,
}

impl Default for TypeMap {
    fn default() -> Self {
        TypeMap { document: TM::None, array: TM::None, root: TM::None, int64_as_object: false, field_paths: vec![] }
    }
}

/// phongo_field_path (non-owning): element pointers into the BSON being decoded/encoded.
#[derive(Default)]
pub struct FieldPath {
    pub elems: Vec<Option<(*const u8, usize)>>,
    pub size: usize,
    pub base_depth: usize,
}

impl FieldPath {
    fn ensure(&mut self, level: usize) {
        if level >= self.elems.len() {
            self.elems.resize(level + 1, None);
        }
    }
    pub fn write(&mut self, e: &[u8]) {
        self.ensure(self.size);
        self.elems[self.size] = Some((e.as_ptr(), e.len()));
    }
    /// push(element or none); true while within BSON_MAX_NESTING_LEVEL
    pub fn push(&mut self, e: Option<&[u8]>) -> bool {
        if let Some(e) = e {
            self.write(e);
        }
        self.ensure(self.size);
        self.size += 1;
        self.size <= MAX_NESTING
    }
    pub fn pop(&mut self) {
        if self.size == 0 {
            return;
        }
        self.ensure(self.size);
        self.elems[self.size] = None;
        self.size -= 1;
        self.elems[self.size] = None;
    }
    pub fn as_string(&self) -> String {
        let mut out = Vec::new();
        for i in 0..=self.size {
            if i >= self.elems.len() {
                break;
            }
            if let Some((p, l)) = self.elems[i] {
                let b = unsafe { std::slice::from_raw_parts(p, l) };
                out.extend_from_slice(zstr::c_part(b));
                out.push(b'.');
            }
        }
        out.pop();
        String::from_utf8_lossy(&out).into_owned()
    }
    fn key(&self, i: usize) -> &[u8] {
        match self.elems.get(i).copied().flatten() {
            Some((p, l)) => unsafe { std::slice::from_raw_parts(p, l) },
            None => b"",
        }
    }
}

/// phongo_bson_state, minus the result zval (passed around explicitly).
pub struct State<'m> {
    pub map: &'m TypeMap,
    pub skip_odm: bool,
    pub is_visiting_array: bool,
    pub nesting_depth: usize,
}

impl<'m> State<'m> {
    pub fn new(map: &'m TypeMap) -> Self {
        State { map, skip_odm: false, is_visiting_array: false, nesting_depth: 0 }
    }
}

struct V<'s, 'm> {
    ht: *mut sys::HashTable,
    st: &'s State<'m>,
    fp: &'s mut FieldPath,
    odm: Option<*mut sys::zend_class_entry>,
    is_array: bool,
}

#[inline]
unsafe fn add(ht: *mut sys::HashTable, is_array: bool, key: &[u8], v: sys::zval) {
    if is_array {
        hash::push(ht, v);
    } else {
        hash::sym_update(ht, key, v);
    }
}

fn nesting_exception(fp: &FieldPath) {
    unsafe { throw(E::UnexpectedValue, format!("Nesting level too deep for field path \"{}\"", fp.as_string())) };
}

fn instantiatable(c: *const sys::zend_class_entry) -> bool {
    let f = unsafe { (*c).ce_flags };
    f & (sys::ZEND_ACC_EXPLICIT_ABSTRACT_CLASS | sys::ZEND_ACC_IMPLICIT_ABSTRACT_CLASS | sys::ZEND_ACC_INTERFACE | sys::ZEND_ACC_TRAIT | sys::ZEND_ACC_ENUM) == 0
}

/// zend_fetch_class(name, ZEND_FETCH_CLASS_AUTO | ZEND_FETCH_CLASS_SILENT)
pub unsafe fn fetch_class(name: &[u8]) -> *mut sys::zend_class_entry {
    let s = zstr::new(name);
    let c = sys::zend_fetch_class(s, sys::ZEND_FETCH_CLASS_AUTO | sys::ZEND_FETCH_CLASS_SILENT);
    zstr::release(s);
    c
}

impl<'a> Visitor<'a> for V<'_, '_> {
    fn visit(&mut self, _it: &Iter<'a>, key: &'a [u8], v: Value<'a>) -> bool {
        unsafe { self.value(key, v) }
    }
    /// phongo_bson_visit_corrupt (a cold path: corrupt data only)
    fn corrupt(&mut self, _it: &Iter<'a>) {
        mongo_core::log::warning("PHONGO", || "Corrupt BSON data detected!".to_owned());
    }
    fn unsupported(&mut self, _it: &Iter<'a>, key: &'a [u8], t: u8) {
        self.fp.write(key);
        unsafe {
            throw(E::UnexpectedValue, format!("Detected unknown BSON type 0x{:02x} for field path \"{}\". Are you using the latest driver?", t, self.fp.as_string()));
        }
    }
}

impl V<'_, '_> {
    /// Returns true to stop (exception thrown).
    unsafe fn value(&mut self, key: &[u8], v: Value) -> bool {
        let (ht, ia) = (self.ht, self.is_array);
        let z = match v {
            Value::Double(d) => zval::double(d),
            Value::Utf8(s) => zval::string(s),
            Value::Binary(st, data) => {
                if !self.st.skip_odm && st == 0x80 && key == b"__pclass" {
                    let c = fetch_class(data);
                    if !c.is_null() && instantiatable(c) && obj::instanceof(c, ce().persistable) {
                        self.odm = Some(c);
                    }
                }
                types::new_binary(data, st)
            }
            Value::Undefined => obj::new_zval(ce().undefined),
            Value::Oid(o) => types::new_objectid(o),
            Value::Bool(b) => zval::bool(b),
            Value::DateTime(ms) => types::new_utcdatetime(ms),
            Value::Decimal128(d) => types::new_decimal128(d),
            Value::Null => zval::null(),
            Value::Regex(p, o) => types::new_regex(p, o),
            Value::Symbol(s) => types::new_symbol(s),
            Value::Code(c) => types::new_javascript(c, None, 0).unwrap(),
            Value::DbPointer(c, o) => types::new_dbpointer(c, o),
            Value::CodeWScope(c, scope) => {
                self.fp.write(key);
                let depth = self.fp.base_depth + self.fp.size + 1;
                if depth > MAX_DECODE_NESTING {
                    nesting_exception(self.fp);
                    return true;
                }
                match types::new_javascript(c, Some(scope), depth) {
                    Some(z) => {
                        add(ht, ia, key, z);
                        return false;
                    }
                    None => return true,
                }
            }
            Value::Int32(i) => zval::long(i as i64),
            Value::Timestamp(t, i) => types::new_timestamp(i, t),
            Value::Int64(i) => {
                self.fp.write(key);
                let z = if self.st.map.int64_as_object { types::new_int64(i) } else { zval::long(i) };
                add(ht, ia, key, z);
                return false;
            }
            Value::MaxKey => obj::new_zval(ce().maxkey),
            Value::MinKey => obj::new_zval(ce().minkey),
            Value::Document(d) => return self.container(key, d, false),
            Value::Array(d) => return self.container(key, d, true),
        };
        add(ht, ia, key, z);
        self.fp.write(key);
        false
    }

    /// phongo_bson_visit_document / phongo_bson_visit_array
    unsafe fn container(&mut self, key: &[u8], d: &[u8], is_arr: bool) -> bool {
        self.fp.push(Some(key));
        let st = self.st;
        let default = if is_arr { st.map.array } else { st.map.document };
        let mut ftype = default;
        for (path, t) in &st.map.field_paths {
            if path.len() == self.fp.size && path.iter().enumerate().all(|(i, e)| e.as_slice() == b"$" || e.as_slice() == self.fp.key(i)) {
                ftype = *t;
                break;
            }
        }
        let mut child = zval::undef();
        let mut odm = None;
        if ftype != TM::Bson {
            if self.fp.base_depth + self.fp.size > MAX_DECODE_NESTING {
                nesting_exception(self.fp);
                return true;
            }
            let Some(mut it) = Iter::new(d) else { return false };
            let ht = hash::new(0);
            child = zval::arr(ht);
            let cst = State { map: st.map, skip_odm: st.skip_odm, is_visiting_array: is_arr, nesting_depth: 0 };
            let mut v = V { ht, st: &cst, fp: &mut *self.fp, odm: None, is_array: is_arr };
            if iter::visit_all(&mut it, &mut v) || it.err_off != 0 {
                return true;
            }
            odm = v.odm;
        }
        if !is_arr && odm.is_some() && ftype == TM::None {
            ftype = TM::Class(std::ptr::null_mut());
        }
        match ftype {
            TM::Bson => child = if is_arr { super::document::new_packedarray(d) } else { super::document::new_document(d) },
            TM::Class(c) => {
                let c = if !is_arr { odm.unwrap_or(c) } else { c };
                let o = obj::new_zval(c);
                let mut args = [child];
                drop(call::method(o.value.obj, c"bsonUnserialize", &mut args));
                drop(args);
                if call::has_exception() {
                    drop(o);
                    self.fp.pop();
                    return true;
                }
                child = o;
            }
            TM::Array => {}
            TM::None if is_arr => {}
            _ => sys::convert_to_object(&mut child),
        }
        add(self.ht, self.is_array, key, child);
        self.fp.pop();
        false
    }
}

/// phongo_bson_to_zval_ex: decodes `doc` (already a valid static BSON) per `st`.
/// On error an exception is thrown and None returned.
pub unsafe fn to_zval(doc: &[u8], st: &State) -> Option<sys::zval> {
    let mut fp = FieldPath { base_depth: st.nesting_depth, ..Default::default() };
    to_zval_fp(doc, st, &mut fp)
}

unsafe fn to_zval_fp(doc: &[u8], st: &State, fp: &mut FieldPath) -> Option<sys::zval> {
    if st.map.root == TM::Bson {
        return Some(if st.is_visiting_array { super::document::new_packedarray(doc) } else { super::document::new_document(doc) });
    }
    let Some(mut it) = Iter::new(doc) else {
        throw(E::UnexpectedValue, "Could not initialize BSON iterator");
        return None;
    };
    let ht = hash::new(0);
    let mut z = zval::arr(ht);
    let mut v = V { ht, st, fp, odm: None, is_array: st.is_visiting_array };
    if iter::visit_all(&mut it, &mut v) || it.err_off != 0 {
        if !call::has_exception() {
            throw(E::UnexpectedValue, format!("Detected corrupt BSON data for field path '{}' at offset {}", v.fp.as_string(), it.err_off));
        }
        return None;
    }
    let odm = v.odm;
    let mut root = st.map.root;
    if root == TM::None {
        if st.is_visiting_array {
            root = TM::Array;
        } else if odm.is_some() {
            root = TM::Class(std::ptr::null_mut());
        }
    }
    match root {
        TM::Array => {}
        TM::Class(c) => {
            let c = odm.unwrap_or(c);
            let o = obj::new_zval(c);
            let mut args = [z];
            drop(call::method(o.value.obj, c"bsonUnserialize", &mut args));
            drop(args);
            if call::has_exception() {
                return None;
            }
            z = o;
        }
        _ => sys::convert_to_object(&mut z),
    }
    Some(z)
}

/// phongo_bson_data_to_zval_ex: read exactly one document from `data`.
pub unsafe fn data_to_zval(data: &[u8], st: &State) -> Option<sys::zval> {
    let Some((doc, exhausted)) = iter::read_first(data) else {
        throw(E::UnexpectedValue, "Could not read document from BSON reader");
        return None;
    };
    let mut r = to_zval(doc, st);
    if !exhausted {
        drop(r.take());
        if !call::has_exception() {
            throw(E::UnexpectedValue, "Reading document did not exhaust input buffer");
        }
        r = None;
    }
    r
}

/// phongo_bson_value_to_zval: one value; documents/arrays as Document/PackedArray.
pub unsafe fn value_to_zval(v: Value) -> Option<sys::zval> {
    Some(match v {
        Value::Int32(i) => zval::long(i as i64),
        Value::Int64(i) => types::new_int64(i),
        Value::Double(d) => zval::double(d),
        Value::Bool(b) => zval::bool(b),
        Value::Null => zval::null(),
        Value::Utf8(s) => zval::string(s),
        Value::Binary(st, d) => types::new_binary(d, st),
        Value::Oid(o) => types::new_objectid(o),
        Value::DateTime(ms) => types::new_utcdatetime(ms),
        Value::Regex(p, o) => types::new_regex(p, o),
        Value::DbPointer(c, o) => types::new_dbpointer(c, o),
        Value::Code(c) => types::new_javascript(c, None, 0)?,
        Value::CodeWScope(c, s) => {
            if !iter::is_valid_static(s) {
                throw(E::UnexpectedValue, "Invalid BSON received for BSON_TYPE_CODEWSCOPE");
                return None;
            }
            types::new_javascript(c, Some(s), 1)?
        }
        Value::Symbol(s) => types::new_symbol(s),
        Value::Timestamp(t, i) => types::new_timestamp(i, t),
        Value::Decimal128(d) => types::new_decimal128(d),
        Value::MaxKey => obj::new_zval(ce().maxkey),
        Value::MinKey => obj::new_zval(ce().minkey),
        Value::Undefined => obj::new_zval(ce().undefined),
        Value::Array(d) => {
            if !iter::is_valid_static(d) {
                throw(E::UnexpectedValue, "Invalid BSON received for BSON_TYPE_ARRAY");
                return None;
            }
            super::document::new_packedarray(d)
        }
        Value::Document(d) => {
            if !iter::is_valid_static(d) {
                throw(E::UnexpectedValue, "Invalid BSON received for BSON_TYPE_DOCUMENT");
                return None;
            }
            super::document::new_document(d)
        }
    })
}

/// php_array_fetch_string(options, name): the string value (converted from scalars) or None.
unsafe fn fetch_string(ht: *const sys::HashTable, name: &[u8]) -> Option<Vec<u8>> {
    let z = zval::deref(hash::find(ht, name)?);
    match zval::type_of(z) as u32 {
        sys::IS_STRING => Some(zval::str_bytes(z).to_vec()),
        sys::IS_LONG | sys::IS_DOUBLE | sys::IS_TRUE => {
            let s = sys::zval_get_string_func(z);
            let r = zstr::bytes(s).to_vec();
            zstr::release(s);
            Some(r)
        }
        _ => None,
    }
}

/// phongo_bson_state_parse_type
unsafe fn parse_type(ht: *const sys::HashTable, name: &[u8], el: &mut TM) -> bool {
    let Some(t) = fetch_string(ht, name) else { return true };
    if t.is_empty() {
        return true;
    }
    let lc = t.to_ascii_lowercase();
    *el = match lc.as_slice() {
        b"array" => TM::Array,
        b"bson" => TM::Bson,
        b"stdclass" | b"object" => TM::Object,
        _ => {
            let name = super::cstr_lossy(&t);
            let c = fetch_class(&t);
            if c.is_null() {
                throw(E::InvalidArgument, format!("Class {} does not exist", name));
                return false;
            } else if !instantiatable(c) {
                let kind = std::ffi::CStr::from_ptr(sys::zend_get_object_type_case(c, true)).to_string_lossy().into_owned();
                throw(E::InvalidArgument, format!("{} {} is not instantiatable", kind, name));
                return false;
            } else if !obj::instanceof(c, ce().unserializable) {
                throw(E::InvalidArgument, format!("Class {} does not implement {}", name, crate::zend::class::name(ce().unserializable)));
                return false;
            }
            TM::Class(c)
        }
    };
    true
}

/// phongo_bson_typemap_to_state
pub unsafe fn typemap(z: *mut sys::zval, map: &mut TypeMap) -> bool {
    if z.is_null() {
        return true;
    }
    let ht = (*z).value.arr;
    if !parse_type(ht, b"array", &mut map.array) || !parse_type(ht, b"document", &mut map.document) || !parse_type(ht, b"root", &mut map.root) {
        return false;
    }
    let Some(fpz) = hash::find(ht, b"fieldPaths") else { return true };
    let fpz = zval::deref(fpz);
    let fht = match zval::type_of(fpz) as u32 {
        sys::IS_ARRAY => (*fpz).value.arr,
        sys::IS_OBJECT => sys::zend_std_get_properties((*fpz).value.obj),
        _ => {
            throw(E::InvalidArgument, "The 'fieldPaths' element is not an array");
            return false;
        }
    };
    let mut ok = true;
    // parse_type may run an autoloader, which may change or free the table: it is held (see
    // hash::hold), the walk sees it as it was when it started
    let held = hash::hold(fht);
    hash::each(fht, false, |k, _| {
        let hash::Key::Str(s) = k else {
            throw(E::InvalidArgument, "The 'fieldPaths' element is not an associative array");
            ok = false;
            return false;
        };
        let key = zstr::c_part(zstr::bytes(s)).to_vec();
        if key.is_empty() {
            throw(E::InvalidArgument, "The 'fieldPaths' element may not be an empty string");
            ok = false;
            return false;
        }
        let mut el = TM::None;
        if !parse_type(fht, zstr::bytes(s), &mut el) {
            ok = false;
            return false;
        }
        if key[0] == b'.' {
            throw(E::InvalidArgument, "A 'fieldPaths' key may not start with a '.'");
            ok = false;
            return false;
        }
        if key[key.len() - 1] == b'.' {
            throw(E::InvalidArgument, "A 'fieldPaths' key may not end with a '.'");
            ok = false;
            return false;
        }
        let segs: Vec<Vec<u8>> = key.split(|&c| c == b'.').map(|s| s.to_vec()).collect();
        if segs.iter().any(|s| s.is_empty()) {
            throw(E::InvalidArgument, "A 'fieldPaths' key may not have an empty segment");
            ok = false;
            return false;
        }
        map.field_paths.push((segs, el));
        true
    });
    hash::unhold(fht, held);
    ok
}
