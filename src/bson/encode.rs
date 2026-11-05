//! PHP values -> BSON (phongo_bson_encode.c), writing straight from zvals into the buffer.

use super::decode::FieldPath;
use super::{ce, document, throw, types, E};
use crate::zend::{call, hash, obj, sys, zstr, zval};
use mongo_core::bson::iter::{self, Iter, T_ARRAY, T_DOCUMENT};
use mongo_core::bson::writer::Writer;
use mongo_core::bson::{oid, utf8};

pub const NONE: u32 = 0;
pub const ADD_ID: u32 = 1;
pub const RETURN_ID: u32 = 2;
pub const ALLOW_ROOT_ARRAY: u32 = 4;
const MAX_LENGTH: usize = i32::MAX as usize;

fn length_ok(len: usize, fp: &FieldPath, what: &str) -> bool {
    if len <= MAX_LENGTH {
        return true;
    }
    unsafe { throw(E::UnexpectedValue, format!("Expected {} for field path \"{}\" to be <= {} bytes, {} given", what, fp.as_string(), MAX_LENGTH, len)) };
    false
}

/// phongo_is_array_or_document: true for "array"
unsafe fn is_array(z: *mut sys::zval) -> bool {
    if !zval::is(z, sys::IS_ARRAY) {
        return zval::is(z, sys::IS_OBJECT) && obj::instanceof(zval::obj_ce(z), ce().packedarray);
    }
    hash::is_list((*z).value.arr)
}

/// HASH_OF
unsafe fn hash_of(z: *mut sys::zval) -> *mut sys::HashTable {
    if zval::is(z, sys::IS_ARRAY) {
        (*z).value.arr
    } else if zval::is(z, sys::IS_OBJECT) {
        let o = (*z).value.obj;
        ((*(*o).handlers).get_properties.unwrap())(o)
    } else {
        std::ptr::null_mut()
    }
}

unsafe fn bson_serialize(z: *mut sys::zval) -> Option<sys::zval> {
    let r = call::method((*z).value.obj, c"bsonSerialize", &mut []);
    (!zval::is(&r, sys::IS_UNDEF)).then_some(r)
}

unsafe fn class_name<'a>(c: *const sys::zend_class_entry) -> &'a [u8] {
    zstr::bytes((*c).name)
}

pub struct Enc {
    pub fp: FieldPath,
}

impl Enc {
    /// phongo_bson_append_object
    unsafe fn append_object(&mut self, w: &mut Writer, flags: u32, key: &[u8], z: *mut sys::zval) {
        let c = ce();
        if zval::is(z, sys::IS_OBJECT) {
            let oce = zval::obj_ce(z);
            if obj::instanceof(oce, c.type_) {
                let o = (*z).value.obj;
                if obj::instanceof(oce, c.document) {
                    w.document(key, &obj::data::<document::Bson>(o).bson);
                    return;
                }
                if obj::instanceof(oce, c.packedarray) {
                    w.array(key, &obj::data::<document::Bson>(o).bson);
                    return;
                }
                if obj::instanceof(oce, c.serializable) {
                    let Some(mut data) = bson_serialize(z) else { return };
                    let persist = obj::instanceof(oce, c.persistable);
                    if persist || !is_array(&mut data) {
                        if persist && !length_ok(class_name(oce).len(), &self.fp, "class name") {
                            zval::dtor(&mut data);
                            return;
                        }
                        let p = w.begin(T_DOCUMENT, key);
                        if persist {
                            w.binary(b"__pclass", 0x80, class_name(oce));
                        }
                        self.internal(&mut data, flags, w, None);
                        w.end(p);
                    } else {
                        let p = w.begin(T_ARRAY, key);
                        self.internal(&mut data, flags, w, None);
                        w.end(p);
                    }
                    zval::dtor(&mut data);
                    return;
                }
                if obj::instanceof(oce, c.objectid) {
                    w.oid(key, &obj::data::<types::ObjectId>(o).oid);
                } else if obj::instanceof(oce, c.utcdatetime) {
                    w.date_time(key, obj::data::<types::UTCDateTime>(o).ms);
                } else if obj::instanceof(oce, c.binary) {
                    let b = obj::data::<types::Binary>(o);
                    if length_ok(b.data.len(), &self.fp, "binary data") {
                        w.binary(key, b.ty, &b.data);
                    }
                } else if obj::instanceof(oce, c.decimal128) {
                    w.decimal128(key, &obj::data::<types::Decimal128>(o).dec);
                } else if obj::instanceof(oce, c.int64) {
                    w.int64(key, obj::data::<types::Int64>(o).v);
                } else if obj::instanceof(oce, c.regex) {
                    let r = obj::data::<types::Regex>(o);
                    w.regex(key, &r.pattern, &r.flags);
                } else if obj::instanceof(oce, c.javascript) {
                    let j = obj::data::<types::Javascript>(o);
                    match &j.scope {
                        Some(s) => w.code_with_scope(key, &j.code, s),
                        None => w.code(key, &j.code),
                    }
                } else if obj::instanceof(oce, c.timestamp) {
                    let t = obj::data::<types::Timestamp>(o);
                    w.timestamp(key, t.timestamp, t.increment);
                } else if obj::instanceof(oce, c.maxkey) {
                    w.maxkey(key);
                } else if obj::instanceof(oce, c.minkey) {
                    w.minkey(key);
                } else if obj::instanceof(oce, c.dbpointer) {
                    let d = obj::data::<types::DBPointer>(o);
                    w.dbpointer(key, &d.ref_, &d.id);
                } else if obj::instanceof(oce, c.symbol) {
                    let s = obj::data::<types::Symbol>(o);
                    if length_ok(s.symbol.len(), &self.fp, "symbol") {
                        w.symbol(key, &s.symbol);
                    }
                } else if obj::instanceof(oce, c.undefined) {
                    w.undefined(key);
                } else {
                    throw(E::UnexpectedValue, format!("Unexpected {} instance: {}", crate::zend::class::name(c.type_), crate::zend::class::name(oce)));
                }
                return;
            }
            if (*oce).ce_flags & sys::ZEND_ACC_ENUM != 0 {
                if (*oce).enum_backing_type as u32 == sys::IS_UNDEF {
                    throw(E::UnexpectedValue, format!("Non-backed enum {} cannot be serialized for field path \"{}\"", crate::zend::class::name(oce), self.fp.as_string()));
                    return;
                }
                // zend_enum_fetch_case_value: OBJ_PROP_NUM(obj, 1)
                let o = (*z).value.obj;
                let v = (*o).properties_table.as_mut_ptr().add(1);
                self.append(w, flags, key, v);
                return;
            }
        }
        let p = w.begin(T_DOCUMENT, key);
        self.internal(z, flags, w, None);
        w.end(p);
    }

    /// phongo_bson_append
    pub unsafe fn append(&mut self, w: &mut Writer, flags: u32, key: &[u8], mut z: *mut sys::zval) {
        if key.len() > MAX_LENGTH {
            throw(E::UnexpectedValue, format!("Expected key length to be <= {} bytes, {} given", MAX_LENGTH, key.len()));
            return;
        }
        self.fp.write(key);
        loop {
            match zval::type_of(z) as u32 {
                sys::IS_NULL => w.null(key),
                sys::IS_TRUE => w.bool(key, true),
                sys::IS_FALSE => w.bool(key, false),
                sys::IS_LONG => w.int(key, (*z).value.lval),
                sys::IS_DOUBLE => w.double(key, (*z).value.dval),
                sys::IS_STRING => {
                    let s = zval::str_bytes(z);
                    if !length_ok(s.len(), &self.fp, "string") {
                        return;
                    }
                    if utf8::validate(s, true) {
                        w.utf8(key, s);
                    } else {
                        throw(E::UnexpectedValue, format!("Detected invalid UTF-8 for field path \"{}\": {}", self.fp.as_string(), String::from_utf8_lossy(zstr::c_part(s))));
                    }
                }
                sys::IS_ARRAY if is_array(z) => {
                    let ht = (*z).value.arr;
                    if !hash::protect_begin(ht) {
                        throw(E::UnexpectedValue, format!("Detected recursion for field path \"{}\"", self.fp.as_string()));
                        return;
                    }
                    if !self.fp.push(None) {
                        throw(E::UnexpectedValue, "Nesting level too deep");
                        hash::protect_end(ht);
                        return;
                    }
                    let p = w.begin(T_ARRAY, key);
                    self.internal(z, flags, w, None);
                    self.fp.pop();
                    w.end(p);
                    hash::protect_end(ht);
                }
                sys::IS_ARRAY | sys::IS_OBJECT => {
                    let ht = hash_of(z);
                    if !hash::protect_begin(ht) {
                        throw(E::UnexpectedValue, format!("Detected recursion for field path \"{}\"", self.fp.as_string()));
                        return;
                    }
                    if !self.fp.push(None) {
                        throw(E::UnexpectedValue, "Nesting level too deep");
                        hash::protect_end(ht);
                        return;
                    }
                    self.append_object(w, flags, key, z);
                    self.fp.pop();
                    hash::protect_end(ht);
                }
                sys::IS_REFERENCE => {
                    z = zval::deref(z);
                    continue;
                }
                t => {
                    let tn = std::ffi::CStr::from_ptr(sys::zend_get_type_by_const(t as i32)).to_string_lossy().into_owned();
                    throw(E::UnexpectedValue, format!("Detected unsupported PHP type for field path \"{}\": {} ({})", self.fp.as_string(), t, tn));
                }
            }
            return;
        }
    }

    /// phongo_zval_to_bson_internal. `id_out`: receives the _id element (RETURN_ID).
    pub unsafe fn internal(&mut self, data: *mut sys::zval, mut flags: u32, w: &mut Writer, id_out: Option<&mut Option<Vec<u8>>>) {
        let c = ce();
        let mut obj_data = zval::undef();
        let mut from_props = false;
        let mut skip_odm = false;
        let ht: *mut sys::HashTable;
        match zval::type_of(data) as u32 {
            sys::IS_OBJECT => {
                let oce = zval::obj_ce(data);
                let o = (*data).value.obj;
                if obj::instanceof(oce, c.document) {
                    copy_elements(&obj::data::<document::Bson>(o).bson, w);
                    if flags & ADD_ID != 0 {
                        let d = w.buf_as_doc();
                        let mut it = Iter::new(&d).unwrap();
                        if it.find(b"_id") {
                            flags &= !ADD_ID;
                        }
                    }
                    return self.done(flags, w, id_out, &mut obj_data);
                }
                if obj::instanceof(oce, c.packedarray) {
                    if self.fp.size == 0 && flags & ALLOW_ROOT_ARRAY == 0 {
                        throw(E::UnexpectedValue, format!("{} cannot be serialized as a root document", crate::zend::class::name(oce)));
                        return;
                    }
                    copy_elements(&obj::data::<document::Bson>(o).bson, w);
                    return self.done(flags, w, id_out, &mut obj_data);
                }
                if obj::instanceof(oce, c.serializable) {
                    let Some(d) = bson_serialize(data) else { return };
                    obj_data = d;
                    if obj::instanceof(oce, c.persistable) {
                        if !length_ok(class_name(oce).len(), &self.fp, "class name") {
                            zval::dtor(&mut obj_data);
                            return;
                        }
                        w.binary(b"__pclass", 0x80, class_name(oce));
                        skip_odm = true;
                    }
                    if zval::is(&obj_data, sys::IS_OBJECT) && (obj::instanceof(zval::obj_ce(&obj_data), c.document) || obj::instanceof(zval::obj_ce(&obj_data), c.packedarray)) {
                        self.internal(&mut obj_data, flags, w, id_out);
                        zval::dtor(&mut obj_data);
                        return;
                    }
                    ht = hash_of(&mut obj_data);
                } else if (*oce).ce_flags & sys::ZEND_ACC_ENUM != 0 {
                    throw(E::UnexpectedValue, format!("Enum {} cannot be serialized as a root element", crate::zend::class::name(oce)));
                    return;
                } else if obj::instanceof(oce, c.type_) {
                    throw(E::UnexpectedValue, format!("{} instance {} cannot be serialized as a root element", crate::zend::class::name(c.type_), crate::zend::class::name(oce)));
                    return;
                } else {
                    ht = ((*(*o).handlers).get_properties.unwrap())(o);
                    from_props = true;
                }
            }
            sys::IS_ARRAY => ht = (*data).value.arr,
            _ => return,
        }
        if !ht.is_null() {
            let mut kbuf = String::new();
            let mut stop = false;
            hash::each(ht, true, |k, v| {
                let key: &[u8] = match k {
                    hash::Key::Str(s) => {
                        let b = zstr::bytes(s);
                        if from_props && !b.is_empty() && b[0] == 0 {
                            return true;
                        }
                        if b.contains(&0) {
                            throw(E::UnexpectedValue, format!("BSON keys cannot contain null bytes. Unexpected null byte after \"{}\".", String::from_utf8_lossy(zstr::c_part(b))));
                            stop = true;
                            return false;
                        }
                        if skip_odm && b == b"__pclass" {
                            return true;
                        }
                        if flags & ADD_ID != 0 && b == b"_id" {
                            flags &= !ADD_ID;
                        }
                        b
                    }
                    hash::Key::Index(_) => hash::key_bytes(k, &mut kbuf),
                };
                let key = key.to_vec();
                self.append(w, flags & !ADD_ID, &key, v);
                if call::has_exception() {
                    stop = true;
                    return false;
                }
                true
            });
            if stop {
                if !zval::is(&obj_data, sys::IS_UNDEF) {
                    zval::dtor(&mut obj_data);
                }
                return;
            }
        }
        self.done(flags, w, id_out, &mut obj_data)
    }

    unsafe fn done(&mut self, flags: u32, w: &mut Writer, id_out: Option<&mut Option<Vec<u8>>>, obj_data: &mut sys::zval) {
        if flags & ADD_ID != 0 {
            w.oid(b"_id", &oid::generate());
        }
        if flags & RETURN_ID != 0 {
            if let Some(out) = id_out {
                let d = w.buf_as_doc();
                let mut it = Iter::new(&d).unwrap();
                if !it.find(b"_id") {
                    throw(E::UnexpectedValue, "Error copying \"_id\" field from encoded document");
                } else {
                    let mut iw = Writer::new();
                    iw.element_from(None, &it);
                    *out = Some(iw.finish());
                }
            }
        }
        if !zval::is(obj_data, sys::IS_UNDEF) {
            zval::dtor(obj_data);
        }
    }
}

/// phongo_bson_copy_to_noinit
fn copy_elements(src: &[u8], w: &mut Writer) {
    if let Some(mut it) = Iter::new(src) {
        while it.next() {
            w.element_from(None, &it);
        }
    }
}

/// phongo_zval_to_bson: the encoded document (None if an exception was thrown).
pub unsafe fn to_bson(data: *mut sys::zval, flags: u32) -> Option<Vec<u8>> {
    let mut w = Writer::new();
    let mut e = Enc { fp: FieldPath::default() };
    e.internal(data, flags, &mut w, None);
    if call::has_exception() {
        return None;
    }
    Some(w.finish())
}

/// phongo_zval_to_bson with RETURN_ID: (document, the {_id: ...} document).
pub unsafe fn to_bson_with_id(data: *mut sys::zval, flags: u32) -> Option<(Vec<u8>, Option<Vec<u8>>)> {
    let mut w = Writer::new();
    let mut e = Enc { fp: FieldPath::default() };
    let mut id = None;
    e.internal(data, flags, &mut w, Some(&mut id));
    if call::has_exception() {
        return None;
    }
    Some((w.finish(), id))
}

#[allow(dead_code)]
fn _unused(_: &Iter) -> bool {
    iter::is_empty(&[])
}
