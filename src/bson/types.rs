//! The value classes of MongoDB\BSON (src/BSON/*.c of ext-mongodb): Binary, DBPointer,
//! Decimal128, Int64, Javascript, MaxKey, MinKey, ObjectId, Regex, Symbol, Timestamp,
//! UTCDateTime, Undefined; plus the shared object plumbing (property caches, handlers).
#![allow(non_snake_case)]

use super::decode::{self, State, TypeMap};
use super::{ce, ce_mut, encode, throw, E};
use crate::zend::call::{self, Ex};
use crate::zend::{hash, obj, sys, zstr, zval};
use mongo_core::bson::decimal128::Decimal128 as Dec;
use mongo_core::bson::{fmt, oid, writer::Writer};
use std::ptr::{addr_of_mut, null_mut};

/// phongo's `intern->properties` cache.
pub struct Props(*mut sys::HashTable);

impl Default for Props {
    fn default() -> Self {
        Props(null_mut())
    }
}
impl Clone for Props {
    fn clone(&self) -> Self {
        Props(null_mut())
    }
}
impl Drop for Props {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { hash::release(self.0) };
        }
    }
}

/// A class with Rust state.
pub trait Cls: Sized + Default + 'static {
    fn handlers() -> *mut sys::zend_object_handlers;
    fn props(&mut self) -> &mut Props;
    /// Fills the property table (get_properties / get_debug_info / __serialize); false when
    /// an exception was thrown.
    unsafe fn fill(&mut self, ht: *mut sys::HashTable, debug: bool) -> bool;
    fn dup(&self) -> Self;
}

macro_rules! cls {
    ($t:ty, $h:ident) => {
        static mut $h: sys::zend_object_handlers = unsafe { std::mem::zeroed() };
        impl $t {
            #[allow(dead_code)]
            pub fn h() -> *mut sys::zend_object_handlers {
                addr_of_mut!($h)
            }
        }
    };
}
pub(crate) use cls;

pub unsafe extern "C" fn create<T: Cls>(c: *mut sys::zend_class_entry) -> *mut sys::zend_object {
    obj::alloc(c, T::default(), T::handlers())
}

unsafe extern "C" fn clone<T: Cls>(o: *mut sys::zend_object) -> *mut sys::zend_object {
    let n = create::<T>((*o).ce);
    sys::zend_objects_clone_members(n, o);
    *obj::data::<T>(n) = obj::data::<T>(o).dup();
    n
}

unsafe extern "C" fn get_properties<T: Cls>(o: *mut sys::zend_object) -> *mut sys::HashTable {
    let d = obj::data::<T>(o);
    if d.props().0.is_null() {
        d.props().0 = hash::new(2);
    }
    let ht = d.props().0;
    d.fill(ht, false);
    ht
}

unsafe extern "C" fn get_debug_info<T: Cls>(o: *mut sys::zend_object, is_temp: *mut i32) -> *mut sys::HashTable {
    *is_temp = 1;
    let ht = hash::new(2);
    if !obj::data::<T>(o).fill(ht, true) {
        hash::release(ht);
        return null_mut();
    }
    ht
}

/// Registers create_object and the handler table (clone, properties, debug info) of `T`.
pub unsafe fn setup<T: Cls>(c: *mut sys::zend_class_entry) -> &'static mut sys::zend_object_handlers {
    (*c).__bindgen_anon_2.create_object = Some(create::<T>);
    let h = obj::handlers::<T>(T::handlers());
    h.clone_obj = Some(clone::<T>);
    h.get_properties = Some(get_properties::<T>);
    h.get_debug_info = Some(get_debug_info::<T>);
    h
}

/// A new instance of `c` holding `v`.
pub unsafe fn new<T: Cls>(c: *mut sys::zend_class_entry, v: T) -> sys::zval {
    zval::obj(obj::alloc(c, v, T::handlers()))
}

#[inline]
pub unsafe fn this<'a, T>(ex: Ex) -> &'a mut T {
    obj::data::<T>(call::this(ex))
}

/// RETURN_ARR(get_properties_hash(this, true)) for __serialize
unsafe fn serialize<T: Cls>(ex: Ex, rv: *mut sys::zval) {
    if !call::none(ex) {
        return;
    }
    let ht = hash::new(2);
    this::<T>(ex).fill(ht, false);
    *rv = zval::arr(ht);
}

/// ZEND_COMPARE_OBJECTS_FALLBACK: Some(result) when the std comparison applies.
pub unsafe fn cmp_fallback(a: *mut sys::zval, b: *mut sys::zval) -> Option<i32> {
    if !zval::is(a, sys::IS_OBJECT) || !zval::is(b, sys::IS_OBJECT) || (*(*(*a).value.obj).handlers).compare != (*(*(*b).value.obj).handlers).compare {
        return Some(sys::zend_std_compare_objects(a, b));
    }
    None
}

#[inline]
fn ord<T: PartialOrd>(a: T, b: T) -> i32 {
    if a < b {
        -1
    } else if a > b {
        1
    } else {
        0
    }
}

/// strcmp
fn strcmp(a: &[u8], b: &[u8]) -> i32 {
    let (a, b) = (zstr::c_part(a), zstr::c_part(b));
    for i in 0..a.len().min(b.len()) {
        if a[i] != b[i] {
            return a[i] as i32 - b[i] as i32;
        }
    }
    a.len() as i32 - b.len() as i32
}

unsafe fn str_prop(ht: *mut sys::HashTable, k: &[u8], v: &[u8]) {
    hash::str_update(ht, k, zval::string(v));
}

/// Parameter parsing of the common "array $data" signature (__set_state, __unserialize).
unsafe fn arg_array(ex: Ex) -> Option<*mut sys::HashTable> {
    let mut z: *mut sys::zval = null_mut();
    if sys::zend_parse_parameters(call::num_args(ex), c"a".as_ptr(), &mut z) != 0 {
        return None;
    }
    Some((*z).value.arr)
}

/// zend_hash_str_find with a type check
unsafe fn find_t(ht: *mut sys::HashTable, k: &[u8], t: u32) -> Option<*mut sys::zval> {
    hash::find(ht, k).filter(|&z| zval::is(z, t))
}

/// phongo_parse_int64 (bson_ascii_strtoll + whole-string check)
pub fn parse_int64(s: &[u8]) -> Option<i64> {
    let t = std::str::from_utf8(s).ok()?;
    let b = t.trim_start_matches([' ', '\t', '\n', '\r', '\x0b', '\x0c']);
    let (neg, d) = match b.as_bytes().first() {
        Some(b'-') => (true, &b[1..]),
        Some(b'+') => (false, &b[1..]),
        _ => (false, b),
    };
    if d.is_empty() || !d.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let mut v: i128 = 0;
    for c in d.bytes() {
        v = v * 10 + (c - b'0') as i128;
        if v > i64::MAX as i128 + 1 {
            return None;
        }
    }
    let v = if neg { -v } else { v };
    i64::try_from(v).ok()
}

fn cname(c: *const sys::zend_class_entry) -> &'static str {
    unsafe { crate::zend::class::name(c) }
}

/// __set_state: a new instance of the called class, initialized from the array.
unsafe fn set_state<T: Cls>(ex: Ex, rv: *mut sys::zval, c: *mut sys::zend_class_entry, init: unsafe fn(&mut T, *mut sys::HashTable) -> bool) {
    let Some(ht) = arg_array(ex) else { return };
    *rv = new(c, T::default());
    init(obj::data::<T>((*rv).value.obj), ht);
}

unsafe fn unserialize<T: Cls>(ex: Ex, init: unsafe fn(&mut T, *mut sys::HashTable) -> bool) {
    let Some(ht) = arg_array(ex) else { return };
    init(this::<T>(ex), ht);
}

// ---------------------------------------------------------------- Binary

#[derive(Default, Clone)]
pub struct Binary {
    pub data: Vec<u8>,
    pub ty: u8,
    pub init: bool,
    props: Props,
}
cls!(Binary, H_BINARY);

impl Cls for Binary {
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
        if !self.init {
            return true;
        }
        if debug {
            str_prop(ht, b"data", fmt::base64_encode(&self.data).as_bytes());
        } else {
            str_prop(ht, b"data", &self.data);
        }
        hash::str_update(ht, b"type", zval::long(self.ty as i64));
        true
    }
}

#[derive(Clone, Copy, PartialEq)]
enum VT {
    Unknown,
    Int8,
    Float32,
    PackedBit,
}

fn vector_type(d: &[u8]) -> VT {
    if d.len() < 2 {
        return VT::Unknown;
    }
    match (d[0], d[1]) {
        (0x03, 0) => VT::Int8,
        (0x27, 0) if (d.len() - 2) % 4 == 0 => VT::Float32,
        (0x10, p) if p < 8 => {
            if d.len() == 2 {
                if p == 0 {
                    VT::PackedBit
                } else {
                    VT::Unknown
                }
            } else if d[d.len() - 1] & ((1u16 << p) - 1) as u8 == 0 {
                VT::PackedBit
            } else {
                VT::Unknown
            }
        }
        _ => VT::Unknown,
    }
}

/// The PHONGO_BSON_MAX_LENGTH (INT32_MAX) check of the BSON type constructors.
unsafe fn len_ok(what: &str, len: usize) -> bool {
    if len <= i32::MAX as usize {
        return true;
    }
    throw(E::InvalidArgument, format!("Expected {} length to be <= {} bytes, {} given", what, i32::MAX, len));
    false
}

impl Binary {
    unsafe fn set(&mut self, data: &[u8], ty: i64) -> bool {
        if !(0..=255).contains(&ty) {
            throw(E::InvalidArgument, format!("Expected type to be an unsigned 8-bit integer, {} given", ty));
            return false;
        }
        if !len_ok("data", data.len()) {
            return false;
        }
        if (ty == 3 || ty == 4) && data.len() != 16 {
            throw(E::InvalidArgument, format!("Expected UUID length to be 16 bytes, {} given", data.len()));
            return false;
        }
        if ty == 9 && vector_type(data) == VT::Unknown {
            throw(E::InvalidArgument, "Binary vector data is invalid");
            return false;
        }
        self.data = data.to_vec();
        self.ty = ty as u8;
        self.init = true;
        true
    }
    unsafe fn from_hash(&mut self, ht: *mut sys::HashTable) -> bool {
        if let (Some(d), Some(t)) = (find_t(ht, b"data", sys::IS_STRING), find_t(ht, b"type", sys::IS_LONG)) {
            return self.set(zval::str_bytes(d), (*t).value.lval);
        }
        throw(E::InvalidArgument, format!("{} initialization requires \"data\" string and \"type\" integer fields", cname(ce().binary)));
        false
    }
    /// phongo_binary_get_vector_as_array
    unsafe fn vector_array(&self) -> Option<sys::zval> {
        let d = &self.data;
        let ht = hash::new(0);
        match vector_type(d) {
            VT::Int8 => d[2..].iter().for_each(|&b| hash::push(ht, zval::long(b as i8 as i64))),
            VT::Float32 => d[2..].chunks(4).for_each(|c| hash::push(ht, zval::double(f32::from_le_bytes(c.try_into().unwrap()) as f64))),
            VT::PackedBit => {
                let n = (d.len() - 2) * 8 - d[1] as usize;
                for i in 0..n {
                    let bit = (d[2 + i / 8] >> (7 - i % 8)) & 1;
                    hash::push(ht, zval::long(bit as i64));
                }
            }
            VT::Unknown => {
                hash::release(ht);
                throw(E::UnexpectedValue, "Binary vector data is invalid");
                return None;
            }
        }
        Some(zval::arr(ht))
    }
}

unsafe fn vector_case(t: VT) -> *mut sys::zend_object {
    let name: &std::ffi::CStr = match t {
        VT::Int8 => c"Int8",
        VT::Float32 => c"Float32",
        _ => c"PackedBit",
    };
    sys::zend_enum_get_case_cstr(ce().vectortype, name.as_ptr())
}

unsafe extern "C" fn binary_debug(o: *mut sys::zend_object, is_temp: *mut i32) -> *mut sys::HashTable {
    *is_temp = 1;
    let b = obj::data::<Binary>(o);
    let ht = hash::new(2);
    b.fill(ht, true);
    if b.ty == 9 {
        let Some(v) = b.vector_array() else { return ht };
        hash::str_update(ht, b"vector", v);
        let c = vector_case(vector_type(&b.data));
        (*c).gc.refcount += 1;
        hash::str_update(ht, b"vectorType", zval::obj(c));
    }
    ht
}

unsafe extern "C" fn binary_compare(a: *mut sys::zval, b: *mut sys::zval) -> i32 {
    if let Some(r) = cmp_fallback(a, b) {
        return r;
    }
    let (x, y) = (obj::data::<Binary>((*a).value.obj), obj::data::<Binary>((*b).value.obj));
    if x.data.len() != y.data.len() {
        return ord(x.data.len(), y.data.len());
    }
    if x.ty != y.ty {
        return ord(x.ty, y.ty);
    }
    sys::zend_binary_strcmp(x.data.as_ptr() as *const _, x.data.len(), y.data.as_ptr() as *const _, y.data.len())
}

pub unsafe fn init_MongoDB_BSON_Binary(c: *mut sys::zend_class_entry) {
    ce_mut().binary = c;
    let h = setup::<Binary>(c);
    h.get_debug_info = Some(binary_debug);
    h.compare = Some(binary_compare);
}

pub unsafe fn new_binary(data: &[u8], ty: u8) -> sys::zval {
    new(ce().binary, Binary { data: data.to_vec(), ty, init: true, props: Props::default() })
}

pub unsafe extern "C" fn zim_MongoDB_BSON_Binary___construct(ex: Ex, _rv: *mut sys::zval) {
    let (mut p, mut l, mut t): (*const u8, usize, i64) = (null_mut(), 0, 0);
    if sys::zend_parse_parameters(call::num_args(ex), c"s|l".as_ptr(), &mut p, &mut l, &mut t) != 0 {
        return;
    }
    this::<Binary>(ex).set(std::slice::from_raw_parts(p, l), t);
}
pub unsafe extern "C" fn zim_MongoDB_BSON_Binary___set_state(ex: Ex, rv: *mut sys::zval) {
    set_state::<Binary>(ex, rv, ce().binary, Binary::from_hash);
}
pub unsafe extern "C" fn zim_MongoDB_BSON_Binary___toString(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        *rv = zval::string(&this::<Binary>(ex).data);
    }
}
pub unsafe extern "C" fn zim_MongoDB_BSON_Binary_getData(ex: Ex, rv: *mut sys::zval) {
    zim_MongoDB_BSON_Binary___toString(ex, rv)
}
pub unsafe extern "C" fn zim_MongoDB_BSON_Binary_getType(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        *rv = zval::long(this::<Binary>(ex).ty as i64);
    }
}
pub unsafe extern "C" fn zim_MongoDB_BSON_Binary_jsonSerialize(ex: Ex, rv: *mut sys::zval) {
    if !call::none(ex) {
        return;
    }
    let b = this::<Binary>(ex);
    let ht = hash::new(2);
    str_prop(ht, b"$binary", fmt::base64_encode(&b.data).as_bytes());
    str_prop(ht, b"$type", format!("{:02x}", b.ty).as_bytes());
    *rv = zval::arr(ht);
}
pub unsafe extern "C" fn zim_MongoDB_BSON_Binary___serialize(ex: Ex, rv: *mut sys::zval) {
    serialize::<Binary>(ex, rv)
}
pub unsafe extern "C" fn zim_MongoDB_BSON_Binary___unserialize(ex: Ex, _rv: *mut sys::zval) {
    unserialize::<Binary>(ex, Binary::from_hash)
}
pub unsafe extern "C" fn zim_MongoDB_BSON_Binary_fromVector(ex: Ex, rv: *mut sys::zval) {
    *rv = new(ce().binary, Binary::default());
    let (mut ht, mut t): (*mut sys::HashTable, *mut sys::zval) = (null_mut(), null_mut());
    if sys::zend_parse_parameters(call::num_args(ex), c"hO".as_ptr(), &mut ht, &mut t, ce().vectortype) != 0 {
        return;
    }
    let case_name = zval::str_bytes((*(*t).value.obj).properties_table.as_ptr());
    let b = obj::data::<Binary>((*rv).value.obj);
    if !hash::is_list(ht) {
        throw(E::InvalidArgument, "Expected vector to be a list");
        return;
    }
    let mut data = Vec::with_capacity(2 + hash::count(ht) as usize * 4);
    let mut i = 0usize;
    let mut ok = true;
    match case_name {
        b"Float32" => {
            data.extend_from_slice(&[0x27, 0]);
            hash::each(ht, false, |_, v| {
                // phongo checks Z_TYPE_P without dereferencing (a reference is rejected)
                let f = match zval::type_of(v) as u32 {
                    sys::IS_DOUBLE => (*v).value.dval,
                    _ => {
                        throw(E::InvalidArgument, format!("Expected vector[{}] to be a float, {} given", i, zval::type_name(v)));
                        ok = false;
                        return false;
                    }
                };
                data.extend_from_slice(&(f as f32).to_le_bytes());
                i += 1;
                true
            });
        }
        b"Int8" => {
            data.extend_from_slice(&[0x03, 0]);
            hash::each(ht, false, |_, v| {
                if !zval::is(v, sys::IS_LONG) {
                    throw(E::InvalidArgument, format!("Expected vector[{}] to be an integer, {} given", i, zval::type_name(v)));
                    ok = false;
                    return false;
                }
                let n = (*v).value.lval;
                if !(-128..=127).contains(&n) {
                    throw(E::InvalidArgument, format!("Expected vector[{}] to be a signed 8-bit integer, {} given", i, n));
                    ok = false;
                    return false;
                }
                data.push(n as i8 as u8);
                i += 1;
                true
            });
        }
        b"PackedBit" => {
            let n = hash::count(ht) as usize;
            let bytes = n.div_ceil(8);
            data.extend_from_slice(&[0x10, ((8 - n % 8) % 8) as u8]);
            data.resize(2 + bytes, 0);
            hash::each(ht, false, |_, v| {
                let bit = match zval::type_of(v) as u32 {
                    sys::IS_TRUE => true,
                    sys::IS_FALSE => false,
                    sys::IS_LONG => match (*v).value.lval {
                        0 => false,
                        1 => true,
                        n => {
                            throw(E::InvalidArgument, format!("Expected vector[{}] to be 0 or 1, {} given", i, n));
                            ok = false;
                            return false;
                        }
                    },
                    _ => {
                        throw(E::InvalidArgument, format!("Expected vector[{}] to be 0, 1, or a boolean, {} given", i, zval::type_name(v)));
                        ok = false;
                        return false;
                    }
                };
                if bit {
                    data[2 + i / 8] |= 0x80 >> (i % 8);
                }
                i += 1;
                true
            });
        }
        _ => {
            throw(E::Logic, format!("Unsupported binary vector type: {}", String::from_utf8_lossy(case_name)));
            return;
        }
    }
    if ok {
        b.data = data;
        b.ty = 9;
        b.init = true;
    }
}
pub unsafe extern "C" fn zim_MongoDB_BSON_Binary_getVectorType(ex: Ex, rv: *mut sys::zval) {
    if !call::none(ex) {
        return;
    }
    let b = this::<Binary>(ex);
    if b.ty != 9 {
        throw(E::Logic, format!("Expected Binary of type vector (9) but it is {}", b.ty));
        return;
    }
    let t = vector_type(&b.data);
    if t == VT::Unknown {
        throw(E::UnexpectedValue, "Binary vector data is invalid");
        return;
    }
    let c = vector_case(t);
    (*c).gc.refcount += 1;
    *rv = zval::obj(c);
}
pub unsafe extern "C" fn zim_MongoDB_BSON_Binary_toArray(ex: Ex, rv: *mut sys::zval) {
    if !call::none(ex) {
        return;
    }
    let b = this::<Binary>(ex);
    if b.ty != 9 {
        throw(E::Logic, format!("Expected Binary of type vector (9) but it is {}", b.ty));
        return;
    }
    if let Some(v) = b.vector_array() {
        *rv = v;
    }
}

// ---------------------------------------------------------------- DBPointer

#[derive(Default, Clone)]
pub struct DBPointer {
    pub ref_: Vec<u8>,
    pub id: [u8; 12],
    pub init: bool,
    props: Props,
}
cls!(DBPointer, H_DBPOINTER);

impl Cls for DBPointer {
    fn handlers() -> *mut sys::zend_object_handlers {
        Self::h()
    }
    fn props(&mut self) -> &mut Props {
        &mut self.props
    }
    fn dup(&self) -> Self {
        self.clone()
    }
    unsafe fn fill(&mut self, ht: *mut sys::HashTable, _debug: bool) -> bool {
        if self.init {
            str_prop(ht, b"ref", zstr::c_part(&self.ref_));
            str_prop(ht, b"id", &oid::to_hex(&self.id));
        }
        true
    }
}

impl DBPointer {
    unsafe fn set(&mut self, r: &[u8], id: &[u8]) -> bool {
        if !len_ok("ref", r.len()) {
            return false;
        }
        if r.contains(&0) {
            throw(E::InvalidArgument, "Ref cannot contain null bytes");
            return false;
        }
        let Some(o) = oid::from_hex(id) else {
            throw(E::InvalidArgument, format!("Error parsing ObjectId string: {}", super::cstr_lossy(id)));
            return false;
        };
        self.ref_ = r.to_vec();
        self.id = o;
        self.init = true;
        true
    }
    unsafe fn from_hash(&mut self, ht: *mut sys::HashTable) -> bool {
        if let (Some(r), Some(i)) = (find_t(ht, b"ref", sys::IS_STRING), find_t(ht, b"id", sys::IS_STRING)) {
            return self.set(zval::str_bytes(r), zval::str_bytes(i));
        }
        throw(E::InvalidArgument, format!("{} initialization requires \"ref\" and \"id\" string fields", cname(ce().dbpointer)));
        false
    }
}

unsafe extern "C" fn dbpointer_compare(a: *mut sys::zval, b: *mut sys::zval) -> i32 {
    if let Some(r) = cmp_fallback(a, b) {
        return r;
    }
    let (x, y) = (obj::data::<DBPointer>((*a).value.obj), obj::data::<DBPointer>((*b).value.obj));
    let r = strcmp(&x.ref_, &y.ref_);
    if r != 0 {
        return r;
    }
    strcmp(&oid::to_hex(&x.id), &oid::to_hex(&y.id))
}

pub unsafe fn init_MongoDB_BSON_DBPointer(c: *mut sys::zend_class_entry) {
    ce_mut().dbpointer = c;
    setup::<DBPointer>(c).compare = Some(dbpointer_compare);
}

pub unsafe fn new_dbpointer(coll: &[u8], id: &[u8; 12]) -> sys::zval {
    new(ce().dbpointer, DBPointer { ref_: coll.to_vec(), id: *id, init: true, props: Props::default() })
}

pub unsafe extern "C" fn zim_MongoDB_BSON_DBPointer___construct(_ex: Ex, _rv: *mut sys::zval) {}
pub unsafe extern "C" fn zim_MongoDB_BSON_DBPointer___toString(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        let d = this::<DBPointer>(ex);
        let mut s = b"[".to_vec();
        s.extend_from_slice(zstr::c_part(&d.ref_));
        s.push(b'/');
        s.extend_from_slice(&oid::to_hex(&d.id));
        s.push(b']');
        *rv = zval::string(&s);
    }
}
pub unsafe extern "C" fn zim_MongoDB_BSON_DBPointer___set_state(ex: Ex, rv: *mut sys::zval) {
    set_state::<DBPointer>(ex, rv, ce().dbpointer, DBPointer::from_hash);
}
pub unsafe extern "C" fn zim_MongoDB_BSON_DBPointer_jsonSerialize(ex: Ex, rv: *mut sys::zval) {
    if !call::none(ex) {
        return;
    }
    let d = this::<DBPointer>(ex);
    let inner = hash::new(2);
    hash::sym_update(inner, b"$ref", zval::string(&d.ref_));
    let o = hash::new(1);
    hash::sym_update(o, b"$oid", zval::string(&oid::to_hex(&d.id)));
    hash::sym_update(inner, b"$id", zval::arr(o));
    let ht = hash::new(1);
    hash::sym_update(ht, b"$dbPointer", zval::arr(inner));
    *rv = zval::arr(ht);
}
pub unsafe extern "C" fn zim_MongoDB_BSON_DBPointer___serialize(ex: Ex, rv: *mut sys::zval) {
    serialize::<DBPointer>(ex, rv)
}
pub unsafe extern "C" fn zim_MongoDB_BSON_DBPointer___unserialize(ex: Ex, _rv: *mut sys::zval) {
    unserialize::<DBPointer>(ex, DBPointer::from_hash)
}

// ---------------------------------------------------------------- Decimal128

#[derive(Clone)]
pub struct Decimal128 {
    pub dec: Dec,
    pub init: bool,
    props: Props,
}
impl Default for Decimal128 {
    fn default() -> Self {
        Decimal128 { dec: Dec::NAN, init: false, props: Props::default() }
    }
}
cls!(Decimal128, H_DECIMAL128);

impl Cls for Decimal128 {
    fn handlers() -> *mut sys::zend_object_handlers {
        Self::h()
    }
    fn props(&mut self) -> &mut Props {
        &mut self.props
    }
    fn dup(&self) -> Self {
        self.clone()
    }
    unsafe fn fill(&mut self, ht: *mut sys::HashTable, _debug: bool) -> bool {
        if self.init {
            str_prop(ht, b"dec", self.dec.to_string().as_bytes());
        }
        true
    }
}

impl Decimal128 {
    unsafe fn set(&mut self, s: &[u8]) -> bool {
        match Dec::parse(zstr::c_part(s)) {
            Some(d) => {
                self.dec = d;
                self.init = true;
                true
            }
            None => {
                throw(E::InvalidArgument, format!("Error parsing Decimal128 string: {}", super::cstr_lossy(s)));
                false
            }
        }
    }
    unsafe fn from_hash(&mut self, ht: *mut sys::HashTable) -> bool {
        if let Some(d) = find_t(ht, b"dec", sys::IS_STRING) {
            return self.set(zval::str_bytes(d));
        }
        throw(E::InvalidArgument, format!("{} initialization requires \"dec\" string field", cname(ce().decimal128)));
        false
    }
}

pub unsafe fn init_MongoDB_BSON_Decimal128(c: *mut sys::zend_class_entry) {
    ce_mut().decimal128 = c;
    setup::<Decimal128>(c);
}

pub unsafe fn new_decimal128(d: Dec) -> sys::zval {
    new(ce().decimal128, Decimal128 { dec: d, init: true, props: Props::default() })
}

pub unsafe extern "C" fn zim_MongoDB_BSON_Decimal128___construct(ex: Ex, _rv: *mut sys::zval) {
    let (mut p, mut l): (*const u8, usize) = (null_mut(), 0);
    if sys::zend_parse_parameters(call::num_args(ex), c"s".as_ptr(), &mut p, &mut l) != 0 {
        return;
    }
    this::<Decimal128>(ex).set(std::slice::from_raw_parts(p, l));
}
pub unsafe extern "C" fn zim_MongoDB_BSON_Decimal128___set_state(ex: Ex, rv: *mut sys::zval) {
    set_state::<Decimal128>(ex, rv, ce().decimal128, Decimal128::from_hash);
}
pub unsafe extern "C" fn zim_MongoDB_BSON_Decimal128___toString(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        *rv = zval::string(this::<Decimal128>(ex).dec.to_string().as_bytes());
    }
}
pub unsafe extern "C" fn zim_MongoDB_BSON_Decimal128_jsonSerialize(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        let ht = hash::new(1);
        str_prop(ht, b"$numberDecimal", this::<Decimal128>(ex).dec.to_string().as_bytes());
        *rv = zval::arr(ht);
    }
}
pub unsafe extern "C" fn zim_MongoDB_BSON_Decimal128___serialize(ex: Ex, rv: *mut sys::zval) {
    serialize::<Decimal128>(ex, rv)
}
pub unsafe extern "C" fn zim_MongoDB_BSON_Decimal128___unserialize(ex: Ex, _rv: *mut sys::zval) {
    unserialize::<Decimal128>(ex, Decimal128::from_hash)
}

// ---------------------------------------------------------------- Int64

#[derive(Default, Clone)]
pub struct Int64 {
    pub v: i64,
    pub init: bool,
    props: Props,
}
cls!(Int64, H_INT64);

impl Cls for Int64 {
    fn handlers() -> *mut sys::zend_object_handlers {
        Self::h()
    }
    fn props(&mut self) -> &mut Props {
        &mut self.props
    }
    fn dup(&self) -> Self {
        self.clone()
    }
    unsafe fn fill(&mut self, ht: *mut sys::HashTable, _debug: bool) -> bool {
        if self.init {
            str_prop(ht, b"integer", self.v.to_string().as_bytes());
        }
        true
    }
}

impl Int64 {
    unsafe fn set_str(&mut self, s: &[u8]) -> bool {
        match parse_int64(s) {
            Some(v) => {
                self.v = v;
                self.init = true;
                true
            }
            None => {
                throw(E::InvalidArgument, format!("Error parsing \"{}\" as 64-bit integer for {} initialization", super::cstr_lossy(s), cname(ce().int64)));
                false
            }
        }
    }
    unsafe fn from_hash(&mut self, ht: *mut sys::HashTable) -> bool {
        if let Some(d) = find_t(ht, b"integer", sys::IS_STRING) {
            return self.set_str(zval::str_bytes(d));
        }
        throw(E::InvalidArgument, format!("{} initialization requires \"integer\" string field", cname(ce().int64)));
        false
    }
}

unsafe fn is_int64(z: *mut sys::zval) -> bool {
    zval::is(z, sys::IS_OBJECT) && zval::obj_ce(z) == ce().int64
}
unsafe fn is_num(z: *mut sys::zval) -> bool {
    zval::is(z, sys::IS_LONG) || zval::is(z, sys::IS_DOUBLE)
}
unsafe fn int64_of(z: *mut sys::zval) -> i64 {
    obj::data::<Int64>((*z).value.obj).v
}
unsafe fn cmp_int64_num(o: *mut sys::zval, v: *mut sys::zval) -> i32 {
    let i = int64_of(o);
    if zval::is(v, sys::IS_LONG) {
        ord(i, (*v).value.lval)
    } else {
        let d = (*v).value.dval;
        if (i as f64) != d {
            if (i as f64) < d {
                -1
            } else {
                1
            }
        } else {
            0
        }
    }
}

unsafe extern "C" fn int64_compare(a: *mut sys::zval, b: *mut sys::zval) -> i32 {
    if is_int64(a) && is_int64(b) {
        return ord(int64_of(a), int64_of(b));
    }
    if is_int64(a) && is_num(b) {
        return cmp_int64_num(a, b);
    }
    if is_num(a) && is_int64(b) {
        return -cmp_int64_num(b, a);
    }
    cmp_fallback(a, b).unwrap_or(0)
}

const IS_NUMBER: i32 = 19; // _IS_NUMBER (zend_types.h, 8.4 and 8.5)
const IS_BOOL: i32 = 18;

unsafe extern "C" fn int64_cast(o: *mut sys::zend_object, rv: *mut sys::zval, t: i32) -> sys::zend_result {
    let v = obj::data::<Int64>(o).v;
    match t as u32 {
        sys::IS_DOUBLE => *rv = zval::double(v as f64),
        sys::IS_LONG => *rv = zval::long(v),
        _ if t == IS_NUMBER => *rv = zval::long(v),
        _ if t == IS_BOOL => *rv = zval::bool(v != 0),
        _ => return sys::zend_std_cast_object_tostring(o, rv, t),
    }
    0
}

fn pow_i64(base: i64, exp: i64) -> i64 {
    if exp == 0 {
        return 1;
    }
    if exp % 2 != 0 {
        return base.wrapping_mul(pow_i64(base.wrapping_mul(base), (exp - 1) / 2));
    }
    pow_i64(base.wrapping_mul(base), exp / 2)
}

unsafe fn int64_operand(z: *mut sys::zval) -> Option<i64> {
    if zval::is(z, sys::IS_LONG) {
        Some((*z).value.lval)
    } else if is_int64(z) {
        Some(int64_of(z))
    } else {
        None
    }
}

unsafe fn int64_op_ex(op: u8, result: *mut sys::zval, a: *mut sys::zval, b: *mut sys::zval) -> sys::zend_result {
    const FAIL: sys::zend_result = -1;
    let Some(v1) = int64_operand(a) else { return FAIL };
    let res = |v: i64| *result = new_int64(v);
    if op as u32 == sys::ZEND_BW_NOT {
        res(!v1);
        return 0;
    }
    let Some(v2) = int64_operand(b) else { return FAIL };
    const SIGN: i64 = i64::MIN;
    match op as u32 {
        sys::ZEND_ADD => {
            let r = v1.wrapping_add(v2);
            if (v1 & SIGN) != (r & SIGN) && (v1 & SIGN) == (v2 & SIGN) {
                *result = zval::double(v1 as f64 + v2 as f64);
            } else {
                res(r);
            }
        }
        sys::ZEND_SUB => {
            let r = v1.wrapping_sub(v2);
            if (v1 & SIGN) != (r & SIGN) && (v1 & SIGN) != (v2 & SIGN) {
                *result = zval::double(v1 as f64 - v2 as f64);
            } else {
                res(r);
            }
        }
        sys::ZEND_MUL => match v1.checked_mul(v2) {
            Some(r) => res(r),
            None => *result = zval::double(v1 as f64 * v2 as f64),
        },
        sys::ZEND_DIV => {
            if v2 == 0 {
                call::throw(sys::zend_ce_division_by_zero_error, "Division by zero");
                return FAIL;
            }
            if (v1 == i64::MIN && v2 == -1) || v1 % v2 != 0 {
                *result = zval::double(v1 as f64 / v2 as f64);
            } else {
                res(v1 / v2);
            }
        }
        sys::ZEND_MOD => {
            if v2 == 0 {
                call::throw(sys::zend_ce_division_by_zero_error, "Division by zero");
                return FAIL;
            }
            res(v1.wrapping_rem(v2));
        }
        sys::ZEND_SL => {
            if v2 < 0 {
                call::throw(sys::zend_ce_arithmetic_error, "Bit shift by negative number");
                return FAIL;
            }
            res(if v2 >= 64 { 0 } else { ((v1 as u64) << v2) as i64 });
        }
        sys::ZEND_SR => {
            if v2 < 0 {
                call::throw(sys::zend_ce_arithmetic_error, "Bit shift by negative number");
                return FAIL;
            }
            res(if v2 >= 64 { if v1 < 0 { -1 } else { 0 } } else { v1 >> v2 });
        }
        sys::ZEND_POW => {
            if v2 < 0 {
                return FAIL;
            }
            if v1 == 0 {
                res(0);
                return 0;
            }
            let p = pow_i64(v1, v2);
            if p == 0 {
                return FAIL;
            }
            res(p);
        }
        sys::ZEND_BW_AND => res(v1 & v2),
        sys::ZEND_BW_OR => res(v1 | v2),
        sys::ZEND_BW_XOR => res(v1 ^ v2),
        _ => return FAIL,
    }
    0
}

unsafe extern "C" fn int64_do_operation(op: u8, result: *mut sys::zval, mut a: *mut sys::zval, b: *mut sys::zval) -> sys::zend_result {
    let mut copy = zval::undef();
    let copied = result == a;
    if copied {
        copy = *a;
        a = &mut copy;
    }
    let r = int64_op_ex(op, result, a, b);
    if r == 0 && copied {
        zval::dtor(&mut copy);
    }
    r
}

pub unsafe fn init_MongoDB_BSON_Int64(c: *mut sys::zend_class_entry) {
    ce_mut().int64 = c;
    let h = setup::<Int64>(c);
    h.compare = Some(int64_compare);
    h.cast_object = Some(int64_cast);
    h.do_operation = Some(int64_do_operation);
}

pub unsafe fn new_int64(v: i64) -> sys::zval {
    new(ce().int64, Int64 { v, init: true, props: Props::default() })
}

pub unsafe extern "C" fn zim_MongoDB_BSON_Int64___construct(ex: Ex, _rv: *mut sys::zval) {
    let mut z: *mut sys::zval = null_mut();
    if sys::zend_parse_parameters(call::num_args(ex), c"z".as_ptr(), &mut z) != 0 {
        return;
    }
    let me = this::<Int64>(ex);
    match zval::type_of(z) as u32 {
        sys::IS_STRING => {
            me.set_str(zval::str_bytes(z));
        }
        sys::IS_LONG => {
            me.v = (*z).value.lval;
            me.init = true;
        }
        _ => throw(E::InvalidArgument, format!("Expected value to be integer or string, {} given", zval::type_name(z))),
    }
}
pub unsafe extern "C" fn zim_MongoDB_BSON_Int64___toString(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        *rv = zval::string(this::<Int64>(ex).v.to_string().as_bytes());
    }
}
pub unsafe extern "C" fn zim_MongoDB_BSON_Int64___set_state(ex: Ex, rv: *mut sys::zval) {
    set_state::<Int64>(ex, rv, ce().int64, Int64::from_hash);
}
pub unsafe extern "C" fn zim_MongoDB_BSON_Int64_jsonSerialize(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        let ht = hash::new(1);
        str_prop(ht, b"$numberLong", this::<Int64>(ex).v.to_string().as_bytes());
        *rv = zval::arr(ht);
    }
}
pub unsafe extern "C" fn zim_MongoDB_BSON_Int64___serialize(ex: Ex, rv: *mut sys::zval) {
    serialize::<Int64>(ex, rv)
}
pub unsafe extern "C" fn zim_MongoDB_BSON_Int64___unserialize(ex: Ex, _rv: *mut sys::zval) {
    unserialize::<Int64>(ex, Int64::from_hash)
}

// ---------------------------------------------------------------- Javascript

#[derive(Default, Clone)]
pub struct Javascript {
    pub code: Vec<u8>,
    pub scope: Option<Vec<u8>>,
    pub init: bool,
    props: Props,
}
cls!(Javascript, H_JAVASCRIPT);

unsafe fn decode_scope(scope: &[u8]) -> Option<sys::zval> {
    let map = TypeMap::default();
    decode::to_zval(scope, &State::new(&map))
}

impl Cls for Javascript {
    fn handlers() -> *mut sys::zend_object_handlers {
        Self::h()
    }
    fn props(&mut self) -> &mut Props {
        &mut self.props
    }
    fn dup(&self) -> Self {
        self.clone()
    }
    unsafe fn fill(&mut self, ht: *mut sys::HashTable, _debug: bool) -> bool {
        if !self.init {
            return true;
        }
        str_prop(ht, b"code", zstr::c_part(&self.code));
        match &self.scope {
            Some(s) => match decode_scope(s) {
                Some(z) => hash::str_update(ht, b"scope", z),
                None => return false,
            },
            None => hash::str_update(ht, b"scope", zval::null()),
        }
        true
    }
}

impl Javascript {
    unsafe fn set(&mut self, code: &[u8], scope: *mut sys::zval) -> bool {
        if !scope.is_null() && !zval::is(scope, sys::IS_OBJECT) && !zval::is(scope, sys::IS_ARRAY) && !zval::is(scope, sys::IS_NULL) {
            let tn = std::ffi::CStr::from_ptr(sys::zend_get_type_by_const(zval::type_of(scope) as i32)).to_string_lossy().into_owned();
            throw(E::InvalidArgument, format!("Expected scope to be array or object, {} given", tn));
            return false;
        }
        if !len_ok("code", code.len()) {
            return false;
        }
        if code.contains(&0) {
            throw(E::InvalidArgument, "Code cannot contain null bytes");
            return false;
        }
        let mut sc = None;
        if !scope.is_null() && (zval::is(scope, sys::IS_OBJECT) || zval::is(scope, sys::IS_ARRAY)) {
            match encode::to_bson(scope, encode::NONE) {
                Some(b) => sc = Some(b),
                None => return false,
            }
        }
        self.code = code.to_vec();
        self.scope = sc;
        self.init = true;
        true
    }
    unsafe fn from_hash(&mut self, ht: *mut sys::HashTable) -> bool {
        if let Some(c) = find_t(ht, b"code", sys::IS_STRING) {
            let s = hash::find(ht, b"scope").unwrap_or(null_mut());
            return self.set(zval::str_bytes(c), s);
        }
        throw(E::InvalidArgument, format!("{} initialization requires \"code\" string field", cname(ce().javascript)));
        false
    }
}

unsafe extern "C" fn javascript_compare(a: *mut sys::zval, b: *mut sys::zval) -> i32 {
    if let Some(r) = cmp_fallback(a, b) {
        return r;
    }
    strcmp(&obj::data::<Javascript>((*a).value.obj).code, &obj::data::<Javascript>((*b).value.obj).code)
}

pub unsafe fn init_MongoDB_BSON_Javascript(c: *mut sys::zend_class_entry) {
    ce_mut().javascript = c;
    setup::<Javascript>(c).compare = Some(javascript_compare);
}

/// phongo_javascript_new: validates the scope by decoding it (at `depth`).
pub unsafe fn new_javascript(code: &[u8], scope: Option<&[u8]>, depth: usize) -> Option<sys::zval> {
    if let Some(s) = scope {
        let map = TypeMap::default();
        let st = State { nesting_depth: depth, ..State::new(&map) };
        let mut z = decode::to_zval(s, &st)?;
        zval::dtor(&mut z);
    }
    Some(new(ce().javascript, Javascript { code: code.to_vec(), scope: scope.map(|s| s.to_vec()), init: true, props: Props::default() }))
}

pub unsafe extern "C" fn zim_MongoDB_BSON_Javascript___construct(ex: Ex, _rv: *mut sys::zval) {
    let (mut p, mut l, mut s): (*const u8, usize, *mut sys::zval) = (null_mut(), 0, null_mut());
    if sys::zend_parse_parameters(call::num_args(ex), c"s|A!".as_ptr(), &mut p, &mut l, &mut s) != 0 {
        return;
    }
    this::<Javascript>(ex).set(std::slice::from_raw_parts(p, l), s);
}
pub unsafe extern "C" fn zim_MongoDB_BSON_Javascript___set_state(ex: Ex, rv: *mut sys::zval) {
    set_state::<Javascript>(ex, rv, ce().javascript, Javascript::from_hash);
}
pub unsafe extern "C" fn zim_MongoDB_BSON_Javascript___toString(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        *rv = zval::string(&this::<Javascript>(ex).code);
    }
}
pub unsafe extern "C" fn zim_MongoDB_BSON_Javascript_getCode(ex: Ex, rv: *mut sys::zval) {
    zim_MongoDB_BSON_Javascript___toString(ex, rv)
}
pub unsafe extern "C" fn zim_MongoDB_BSON_Javascript_getScope(ex: Ex, rv: *mut sys::zval) {
    if !call::none(ex) {
        return;
    }
    match &this::<Javascript>(ex).scope {
        Some(s) => {
            if let Some(z) = decode_scope(s) {
                *rv = z;
            }
        }
        None => *rv = zval::null(),
    }
}
pub unsafe extern "C" fn zim_MongoDB_BSON_Javascript_jsonSerialize(ex: Ex, rv: *mut sys::zval) {
    if !call::none(ex) {
        return;
    }
    let j = this::<Javascript>(ex);
    let ht = hash::new(2);
    str_prop(ht, b"$code", &j.code);
    *rv = zval::arr(ht);
    if let Some(s) = &j.scope {
        if let Some(z) = decode_scope(s) {
            hash::sym_update(ht, b"$scope", z);
        }
    }
}
pub unsafe extern "C" fn zim_MongoDB_BSON_Javascript___serialize(ex: Ex, rv: *mut sys::zval) {
    if !call::none(ex) {
        return;
    }
    let ht = hash::new(2);
    if !this::<Javascript>(ex).fill(ht, false) {
        hash::release(ht);
        return;
    }
    *rv = zval::arr(ht);
}
pub unsafe extern "C" fn zim_MongoDB_BSON_Javascript___unserialize(ex: Ex, _rv: *mut sys::zval) {
    unserialize::<Javascript>(ex, Javascript::from_hash)
}

// ---------------------------------------------------------------- MaxKey, MinKey, Undefined

#[derive(Default, Clone)]
pub struct Unit {
    props: Props,
}

macro_rules! unit_class {
    ($t:ident, $h:ident, $field:ident, $init:ident, $ss:ident, $js:ident, $ser:ident, $unser:ident, $jkey:expr, $jval:expr) => {
        #[derive(Default, Clone)]
        pub struct $t(Unit);
        cls!($t, $h);
        impl Cls for $t {
            fn handlers() -> *mut sys::zend_object_handlers {
                Self::h()
            }
            fn props(&mut self) -> &mut Props {
                &mut self.0.props
            }
            fn dup(&self) -> Self {
                self.clone()
            }
            unsafe fn fill(&mut self, _ht: *mut sys::HashTable, _debug: bool) -> bool {
                true
            }
        }
        pub unsafe fn $init(c: *mut sys::zend_class_entry) {
            ce_mut().$field = c;
            setup::<$t>(c);
        }
        pub unsafe extern "C" fn $ss(ex: Ex, rv: *mut sys::zval) {
            if arg_array(ex).is_some() {
                *rv = obj::new_zval(ce().$field);
            }
        }
        pub unsafe extern "C" fn $js(ex: Ex, rv: *mut sys::zval) {
            if call::none(ex) {
                let ht = hash::new(1);
                hash::sym_update(ht, $jkey, $jval);
                *rv = zval::arr(ht);
            }
        }
        pub unsafe extern "C" fn $ser(ex: Ex, rv: *mut sys::zval) {
            if call::none(ex) {
                *rv = zval::arr(hash::new(0));
            }
        }
        pub unsafe extern "C" fn $unser(ex: Ex, _rv: *mut sys::zval) {
            arg_array(ex);
        }
    };
}

unit_class!(MaxKey, H_MAXKEY, maxkey, init_MongoDB_BSON_MaxKey, zim_MongoDB_BSON_MaxKey___set_state, zim_MongoDB_BSON_MaxKey_jsonSerialize, zim_MongoDB_BSON_MaxKey___serialize, zim_MongoDB_BSON_MaxKey___unserialize, b"$maxKey", zval::long(1));
unit_class!(MinKey, H_MINKEY, minkey, init_MongoDB_BSON_MinKey, zim_MongoDB_BSON_MinKey___set_state, zim_MongoDB_BSON_MinKey_jsonSerialize, zim_MongoDB_BSON_MinKey___serialize, zim_MongoDB_BSON_MinKey___unserialize, b"$minKey", zval::long(1));
unit_class!(Undefined, H_UNDEFINED, undefined, init_MongoDB_BSON_Undefined, zim_MongoDB_BSON_Undefined___set_state, zim_MongoDB_BSON_Undefined_jsonSerialize, zim_MongoDB_BSON_Undefined___serialize, zim_MongoDB_BSON_Undefined___unserialize, b"$undefined", zval::bool(true));

pub unsafe extern "C" fn zim_MongoDB_BSON_Undefined___construct(_ex: Ex, _rv: *mut sys::zval) {}
pub unsafe extern "C" fn zim_MongoDB_BSON_Undefined___toString(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        *rv = zval::string(b"");
    }
}

// ---------------------------------------------------------------- ObjectId

#[derive(Default, Clone)]
pub struct ObjectId {
    pub oid: [u8; 12],
    pub init: bool,
    props: Props,
}
cls!(ObjectId, H_OBJECTID);

impl Cls for ObjectId {
    fn handlers() -> *mut sys::zend_object_handlers {
        Self::h()
    }
    fn props(&mut self) -> &mut Props {
        &mut self.props
    }
    fn dup(&self) -> Self {
        self.clone()
    }
    unsafe fn fill(&mut self, ht: *mut sys::HashTable, _debug: bool) -> bool {
        if self.init {
            str_prop(ht, b"oid", &oid::to_hex(&self.oid));
        }
        true
    }
}

impl ObjectId {
    unsafe fn set_hex(&mut self, s: &[u8]) -> bool {
        match oid::from_hex(s) {
            Some(o) => {
                self.oid = o;
                self.init = true;
                true
            }
            None => {
                throw(E::InvalidArgument, format!("Error parsing ObjectId string: {}", super::cstr_lossy(s)));
                false
            }
        }
    }
    unsafe fn from_hash(&mut self, ht: *mut sys::HashTable) -> bool {
        if let Some(z) = find_t(ht, b"oid", sys::IS_STRING) {
            return self.set_hex(zval::str_bytes(z));
        }
        throw(E::InvalidArgument, format!("{} initialization requires \"oid\" string field", cname(ce().objectid)));
        false
    }
}

unsafe extern "C" fn objectid_compare(a: *mut sys::zval, b: *mut sys::zval) -> i32 {
    if let Some(r) = cmp_fallback(a, b) {
        return r;
    }
    strcmp(&oid::to_hex(&obj::data::<ObjectId>((*a).value.obj).oid), &oid::to_hex(&obj::data::<ObjectId>((*b).value.obj).oid))
}

pub unsafe fn init_MongoDB_BSON_ObjectId(c: *mut sys::zend_class_entry) {
    ce_mut().objectid = c;
    setup::<ObjectId>(c).compare = Some(objectid_compare);
}

pub unsafe fn new_objectid(o: &[u8; 12]) -> sys::zval {
    new(ce().objectid, ObjectId { oid: *o, init: true, props: Props::default() })
}

pub unsafe extern "C" fn zim_MongoDB_BSON_ObjectId___construct(ex: Ex, _rv: *mut sys::zval) {
    let (mut p, mut l): (*const u8, usize) = (null_mut(), 0);
    if sys::zend_parse_parameters(call::num_args(ex), c"|s!".as_ptr(), &mut p, &mut l) != 0 {
        return;
    }
    let me = this::<ObjectId>(ex);
    if p.is_null() {
        me.oid = oid::generate();
        me.init = true;
    } else {
        me.set_hex(std::slice::from_raw_parts(p, l));
    }
}
pub unsafe extern "C" fn zim_MongoDB_BSON_ObjectId_getTimestamp(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        let o = this::<ObjectId>(ex).oid;
        *rv = zval::long(u32::from_be_bytes([o[0], o[1], o[2], o[3]]) as i64);
    }
}
pub unsafe extern "C" fn zim_MongoDB_BSON_ObjectId___set_state(ex: Ex, rv: *mut sys::zval) {
    set_state::<ObjectId>(ex, rv, ce().objectid, ObjectId::from_hash);
}
pub unsafe extern "C" fn zim_MongoDB_BSON_ObjectId___toString(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        *rv = zval::string(&oid::to_hex(&this::<ObjectId>(ex).oid));
    }
}
pub unsafe extern "C" fn zim_MongoDB_BSON_ObjectId_jsonSerialize(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        let ht = hash::new(1);
        str_prop(ht, b"$oid", &oid::to_hex(&this::<ObjectId>(ex).oid));
        *rv = zval::arr(ht);
    }
}
pub unsafe extern "C" fn zim_MongoDB_BSON_ObjectId___serialize(ex: Ex, rv: *mut sys::zval) {
    serialize::<ObjectId>(ex, rv)
}
pub unsafe extern "C" fn zim_MongoDB_BSON_ObjectId___unserialize(ex: Ex, _rv: *mut sys::zval) {
    unserialize::<ObjectId>(ex, ObjectId::from_hash)
}

// ---------------------------------------------------------------- Regex

#[derive(Default, Clone)]
pub struct Regex {
    pub pattern: Vec<u8>,
    pub flags: Vec<u8>,
    pub init: bool,
    props: Props,
}
cls!(Regex, H_REGEX);

impl Cls for Regex {
    fn handlers() -> *mut sys::zend_object_handlers {
        Self::h()
    }
    fn props(&mut self) -> &mut Props {
        &mut self.props
    }
    fn dup(&self) -> Self {
        self.clone()
    }
    unsafe fn fill(&mut self, ht: *mut sys::HashTable, _debug: bool) -> bool {
        if self.init {
            str_prop(ht, b"pattern", &self.pattern);
            str_prop(ht, b"flags", &self.flags);
        }
        true
    }
}

impl Regex {
    unsafe fn set(&mut self, p: &[u8], f: &[u8]) -> bool {
        if !len_ok("pattern", p.len()) || !len_ok("flags", f.len()) {
            return false;
        }
        if p.contains(&0) {
            throw(E::InvalidArgument, "Pattern cannot contain null bytes");
            return false;
        }
        if f.contains(&0) {
            throw(E::InvalidArgument, "Flags cannot contain null bytes");
            return false;
        }
        self.pattern = p.to_vec();
        let mut fl = f.to_vec();
        fl.sort_unstable();
        self.flags = fl;
        self.init = true;
        true
    }
    unsafe fn from_hash(&mut self, ht: *mut sys::HashTable) -> bool {
        if let (Some(p), Some(f)) = (find_t(ht, b"pattern", sys::IS_STRING), find_t(ht, b"flags", sys::IS_STRING)) {
            return self.set(zval::str_bytes(p), zval::str_bytes(f));
        }
        throw(E::InvalidArgument, format!("{} initialization requires \"pattern\" and \"flags\" string fields", cname(ce().regex)));
        false
    }
}

unsafe extern "C" fn regex_compare(a: *mut sys::zval, b: *mut sys::zval) -> i32 {
    if let Some(r) = cmp_fallback(a, b) {
        return r;
    }
    let (x, y) = (obj::data::<Regex>((*a).value.obj), obj::data::<Regex>((*b).value.obj));
    let r = strcmp(&x.pattern, &y.pattern);
    if r != 0 {
        return r;
    }
    strcmp(&x.flags, &y.flags)
}

pub unsafe fn init_MongoDB_BSON_Regex(c: *mut sys::zend_class_entry) {
    ce_mut().regex = c;
    setup::<Regex>(c).compare = Some(regex_compare);
}

/// phongo_regex_new: from BSON (the flags are already sorted by libbson).
pub unsafe fn new_regex(p: &[u8], f: &[u8]) -> sys::zval {
    let mut fl = f.to_vec();
    fl.sort_unstable();
    new(ce().regex, Regex { pattern: p.to_vec(), flags: fl, init: true, props: Props::default() })
}

pub unsafe extern "C" fn zim_MongoDB_BSON_Regex___construct(ex: Ex, _rv: *mut sys::zval) {
    let (mut p, mut l, mut f, mut fl): (*const u8, usize, *const u8, usize) = (null_mut(), 0, null_mut(), 0);
    if sys::zend_parse_parameters(call::num_args(ex), c"s|s".as_ptr(), &mut p, &mut l, &mut f, &mut fl) != 0 {
        return;
    }
    let flags = if f.is_null() { &[][..] } else { std::slice::from_raw_parts(f, fl) };
    this::<Regex>(ex).set(std::slice::from_raw_parts(p, l), flags);
}
pub unsafe extern "C" fn zim_MongoDB_BSON_Regex_getPattern(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        *rv = zval::string(&this::<Regex>(ex).pattern);
    }
}
pub unsafe extern "C" fn zim_MongoDB_BSON_Regex_getFlags(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        *rv = zval::string(&this::<Regex>(ex).flags);
    }
}
pub unsafe extern "C" fn zim_MongoDB_BSON_Regex___set_state(ex: Ex, rv: *mut sys::zval) {
    set_state::<Regex>(ex, rv, ce().regex, Regex::from_hash);
}
pub unsafe extern "C" fn zim_MongoDB_BSON_Regex___toString(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        let r = this::<Regex>(ex);
        let mut s = b"/".to_vec();
        s.extend_from_slice(&r.pattern);
        s.push(b'/');
        s.extend_from_slice(&r.flags);
        *rv = zval::string(&s);
    }
}
pub unsafe extern "C" fn zim_MongoDB_BSON_Regex_jsonSerialize(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        let r = this::<Regex>(ex);
        let ht = hash::new(2);
        hash::sym_update(ht, b"$regex", zval::string(&r.pattern));
        hash::sym_update(ht, b"$options", zval::string(&r.flags));
        *rv = zval::arr(ht);
    }
}
pub unsafe extern "C" fn zim_MongoDB_BSON_Regex___serialize(ex: Ex, rv: *mut sys::zval) {
    serialize::<Regex>(ex, rv)
}
pub unsafe extern "C" fn zim_MongoDB_BSON_Regex___unserialize(ex: Ex, _rv: *mut sys::zval) {
    unserialize::<Regex>(ex, Regex::from_hash)
}

// ---------------------------------------------------------------- Symbol

#[derive(Default, Clone)]
pub struct Symbol {
    pub symbol: Vec<u8>,
    pub init: bool,
    props: Props,
}
cls!(Symbol, H_SYMBOL);

impl Cls for Symbol {
    fn handlers() -> *mut sys::zend_object_handlers {
        Self::h()
    }
    fn props(&mut self) -> &mut Props {
        &mut self.props
    }
    fn dup(&self) -> Self {
        self.clone()
    }
    unsafe fn fill(&mut self, ht: *mut sys::HashTable, _debug: bool) -> bool {
        if self.init {
            str_prop(ht, b"symbol", zstr::c_part(&self.symbol));
        }
        true
    }
}

impl Symbol {
    unsafe fn from_hash(&mut self, ht: *mut sys::HashTable) -> bool {
        if let Some(s) = find_t(ht, b"symbol", sys::IS_STRING) {
            let b = zval::str_bytes(s);
            if !len_ok("symbol", b.len()) {
                return false;
            }
            if b.contains(&0) {
                throw(E::InvalidArgument, "Symbol cannot contain null bytes");
                return false;
            }
            self.symbol = b.to_vec();
            self.init = true;
            return true;
        }
        throw(E::InvalidArgument, format!("{} initialization requires \"symbol\" string field", cname(ce().symbol)));
        false
    }
}

unsafe extern "C" fn symbol_compare(a: *mut sys::zval, b: *mut sys::zval) -> i32 {
    if let Some(r) = cmp_fallback(a, b) {
        return r;
    }
    strcmp(&obj::data::<Symbol>((*a).value.obj).symbol, &obj::data::<Symbol>((*b).value.obj).symbol)
}

pub unsafe fn init_MongoDB_BSON_Symbol(c: *mut sys::zend_class_entry) {
    ce_mut().symbol = c;
    setup::<Symbol>(c).compare = Some(symbol_compare);
}

pub unsafe fn new_symbol(s: &[u8]) -> sys::zval {
    new(ce().symbol, Symbol { symbol: s.to_vec(), init: true, props: Props::default() })
}

pub unsafe extern "C" fn zim_MongoDB_BSON_Symbol___construct(_ex: Ex, _rv: *mut sys::zval) {}
pub unsafe extern "C" fn zim_MongoDB_BSON_Symbol___toString(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        *rv = zval::string(&this::<Symbol>(ex).symbol);
    }
}
pub unsafe extern "C" fn zim_MongoDB_BSON_Symbol___set_state(ex: Ex, rv: *mut sys::zval) {
    set_state::<Symbol>(ex, rv, ce().symbol, Symbol::from_hash);
}
pub unsafe extern "C" fn zim_MongoDB_BSON_Symbol_jsonSerialize(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        let ht = hash::new(1);
        hash::sym_update(ht, b"$symbol", zval::string(&this::<Symbol>(ex).symbol));
        *rv = zval::arr(ht);
    }
}
pub unsafe extern "C" fn zim_MongoDB_BSON_Symbol___serialize(ex: Ex, rv: *mut sys::zval) {
    serialize::<Symbol>(ex, rv)
}
pub unsafe extern "C" fn zim_MongoDB_BSON_Symbol___unserialize(ex: Ex, _rv: *mut sys::zval) {
    unserialize::<Symbol>(ex, Symbol::from_hash)
}

// ---------------------------------------------------------------- Timestamp

#[derive(Default, Clone)]
pub struct Timestamp {
    pub increment: u32,
    pub timestamp: u32,
    pub init: bool,
    props: Props,
}
cls!(Timestamp, H_TIMESTAMP);

impl Cls for Timestamp {
    fn handlers() -> *mut sys::zend_object_handlers {
        Self::h()
    }
    fn props(&mut self) -> &mut Props {
        &mut self.props
    }
    fn dup(&self) -> Self {
        self.clone()
    }
    unsafe fn fill(&mut self, ht: *mut sys::HashTable, _debug: bool) -> bool {
        if self.init {
            str_prop(ht, b"increment", self.increment.to_string().as_bytes());
            str_prop(ht, b"timestamp", self.timestamp.to_string().as_bytes());
        }
        true
    }
}

impl Timestamp {
    unsafe fn set(&mut self, inc: i64, ts: i64) -> bool {
        if inc < 0 || inc > u32::MAX as i64 {
            throw(E::InvalidArgument, format!("Expected increment to be an unsigned 32-bit integer, {} given", inc));
            return false;
        }
        if ts < 0 || ts > u32::MAX as i64 {
            throw(E::InvalidArgument, format!("Expected timestamp to be an unsigned 32-bit integer, {} given", ts));
            return false;
        }
        self.increment = inc as u32;
        self.timestamp = ts as u32;
        self.init = true;
        true
    }
    unsafe fn set_str(&mut self, inc: &[u8], ts: &[u8]) -> bool {
        let Some(i) = parse_int64(inc) else {
            throw(E::InvalidArgument, format!("Error parsing \"{}\" as 64-bit integer increment for {} initialization", super::cstr_lossy(inc), cname(ce().timestamp)));
            return false;
        };
        let Some(t) = parse_int64(ts) else {
            throw(E::InvalidArgument, format!("Error parsing \"{}\" as 64-bit integer timestamp for {} initialization", super::cstr_lossy(ts), cname(ce().timestamp)));
            return false;
        };
        self.set(i, t)
    }
    unsafe fn from_hash(&mut self, ht: *mut sys::HashTable) -> bool {
        if let (Some(i), Some(t)) = (find_t(ht, b"increment", sys::IS_LONG), find_t(ht, b"timestamp", sys::IS_LONG)) {
            return self.set((*i).value.lval, (*t).value.lval);
        }
        if let (Some(i), Some(t)) = (find_t(ht, b"increment", sys::IS_STRING), find_t(ht, b"timestamp", sys::IS_STRING)) {
            return self.set_str(zval::str_bytes(i), zval::str_bytes(t));
        }
        throw(E::InvalidArgument, format!("{} initialization requires \"increment\" and \"timestamp\" integer or numeric string fields", cname(ce().timestamp)));
        false
    }
}

unsafe extern "C" fn timestamp_compare(a: *mut sys::zval, b: *mut sys::zval) -> i32 {
    if let Some(r) = cmp_fallback(a, b) {
        return r;
    }
    let (x, y) = (obj::data::<Timestamp>((*a).value.obj), obj::data::<Timestamp>((*b).value.obj));
    if x.timestamp != y.timestamp {
        return ord(x.timestamp, y.timestamp);
    }
    ord(x.increment, y.increment)
}

pub unsafe fn init_MongoDB_BSON_Timestamp(c: *mut sys::zend_class_entry) {
    ce_mut().timestamp = c;
    setup::<Timestamp>(c).compare = Some(timestamp_compare);
}

pub unsafe fn new_timestamp(inc: u32, ts: u32) -> sys::zval {
    new(ce().timestamp, Timestamp { increment: inc, timestamp: ts, init: true, props: Props::default() })
}

pub unsafe extern "C" fn zim_MongoDB_BSON_Timestamp___construct(ex: Ex, _rv: *mut sys::zval) {
    let (mut i, mut t): (*mut sys::zval, *mut sys::zval) = (null_mut(), null_mut());
    if sys::zend_parse_parameters(call::num_args(ex), c"zz".as_ptr(), &mut i, &mut t) != 0 {
        return;
    }
    let me = this::<Timestamp>(ex);
    let conv = |z: *mut sys::zval, what: &str| -> Option<i64> {
        match zval::type_of(z) as u32 {
            sys::IS_LONG => Some((*z).value.lval),
            sys::IS_STRING => match parse_int64(zval::str_bytes(z)) {
                Some(v) => Some(v),
                None => {
                    throw(E::InvalidArgument, format!("Error parsing \"{}\" as 64-bit integer {} for {} initialization", super::cstr_lossy(zval::str_bytes(z)), what, cname(ce().timestamp)));
                    None
                }
            },
            _ => {
                throw(E::InvalidArgument, format!("Expected {} to be an unsigned 32-bit integer or string, {} given", what, zval::type_name(z)));
                None
            }
        }
    };
    let Some(iv) = conv(i, "increment") else { return };
    let Some(tv) = conv(t, "timestamp") else { return };
    me.set(iv, tv);
}
pub unsafe extern "C" fn zim_MongoDB_BSON_Timestamp_getIncrement(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        *rv = zval::long(this::<Timestamp>(ex).increment as i64);
    }
}
pub unsafe extern "C" fn zim_MongoDB_BSON_Timestamp_getTimestamp(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        *rv = zval::long(this::<Timestamp>(ex).timestamp as i64);
    }
}
pub unsafe extern "C" fn zim_MongoDB_BSON_Timestamp___set_state(ex: Ex, rv: *mut sys::zval) {
    set_state::<Timestamp>(ex, rv, ce().timestamp, Timestamp::from_hash);
}
pub unsafe extern "C" fn zim_MongoDB_BSON_Timestamp___toString(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        let t = this::<Timestamp>(ex);
        *rv = zval::string(format!("[{}:{}]", t.increment, t.timestamp).as_bytes());
    }
}
pub unsafe extern "C" fn zim_MongoDB_BSON_Timestamp_jsonSerialize(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        let t = this::<Timestamp>(ex);
        let inner = hash::new(2);
        hash::sym_update(inner, b"t", zval::long(t.timestamp as i64));
        hash::sym_update(inner, b"i", zval::long(t.increment as i64));
        let ht = hash::new(1);
        hash::sym_update(ht, b"$timestamp", zval::arr(inner));
        *rv = zval::arr(ht);
    }
}
pub unsafe extern "C" fn zim_MongoDB_BSON_Timestamp___serialize(ex: Ex, rv: *mut sys::zval) {
    serialize::<Timestamp>(ex, rv)
}
pub unsafe extern "C" fn zim_MongoDB_BSON_Timestamp___unserialize(ex: Ex, _rv: *mut sys::zval) {
    unserialize::<Timestamp>(ex, Timestamp::from_hash)
}

// ---------------------------------------------------------------- UTCDateTime

#[derive(Default, Clone)]
pub struct UTCDateTime {
    pub ms: i64,
    pub init: bool,
    props: Props,
}
cls!(UTCDateTime, H_UTCDATETIME);

impl Cls for UTCDateTime {
    fn handlers() -> *mut sys::zend_object_handlers {
        Self::h()
    }
    fn props(&mut self) -> &mut Props {
        &mut self.props
    }
    fn dup(&self) -> Self {
        self.clone()
    }
    unsafe fn fill(&mut self, ht: *mut sys::HashTable, _debug: bool) -> bool {
        if self.init {
            str_prop(ht, b"milliseconds", self.ms.to_string().as_bytes());
        }
        true
    }
}

impl UTCDateTime {
    fn set(&mut self, ms: i64) -> bool {
        self.ms = ms;
        self.init = true;
        true
    }
    unsafe fn from_hash(&mut self, ht: *mut sys::HashTable) -> bool {
        if let Some(m) = find_t(ht, b"milliseconds", sys::IS_LONG) {
            return self.set((*m).value.lval);
        }
        if let Some(m) = find_t(ht, b"milliseconds", sys::IS_STRING) {
            let s = zval::str_bytes(m);
            return match parse_int64(s) {
                Some(v) => self.set(v),
                None => {
                    throw(E::InvalidArgument, format!("Error parsing \"{}\" as 64-bit integer for {} initialization", super::cstr_lossy(s), cname(ce().utcdatetime)));
                    false
                }
            };
        }
        throw(E::InvalidArgument, format!("{} initialization requires \"milliseconds\" integer or numeric string field", cname(ce().utcdatetime)));
        false
    }
}

unsafe extern "C" fn utcdatetime_compare(a: *mut sys::zval, b: *mut sys::zval) -> i32 {
    if let Some(r) = cmp_fallback(a, b) {
        return r;
    }
    ord(obj::data::<UTCDateTime>((*a).value.obj).ms, obj::data::<UTCDateTime>((*b).value.obj).ms)
}

pub unsafe fn init_MongoDB_BSON_UTCDateTime(c: *mut sys::zend_class_entry) {
    ce_mut().utcdatetime = c;
    setup::<UTCDateTime>(c).compare = Some(utcdatetime_compare);
}

pub unsafe fn new_utcdatetime(ms: i64) -> sys::zval {
    new(ce().utcdatetime, UTCDateTime { ms, init: true, props: Props::default() })
}

unsafe fn date_obj(o: *mut sys::zend_object) -> *mut sys::php_date_obj {
    (o as *mut u8).sub(std::mem::offset_of!(sys::php_date_obj, std)) as *mut sys::php_date_obj
}

pub unsafe extern "C" fn zim_MongoDB_BSON_UTCDateTime___construct(ex: Ex, _rv: *mut sys::zval) {
    let mut z: *mut sys::zval = null_mut();
    if sys::zend_parse_parameters(call::num_args(ex), c"|z!".as_ptr(), &mut z) != 0 {
        return;
    }
    let me = this::<UTCDateTime>(ex);
    if z.is_null() {
        let d = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
        me.set(d.as_secs() as i64 * 1000 + d.subsec_micros() as i64 / 1000);
        return;
    }
    match zval::type_of(z) as u32 {
        sys::IS_OBJECT => {
            let o = (*z).value.obj;
            if obj::instanceof((*o).ce, sys::php_date_get_interface_ce()) {
                let t = (*date_obj(o)).time;
                me.set((*t).sse * 1000 + (*t).us / 1000);
            } else if obj::instanceof((*o).ce, ce().int64) {
                me.set(obj::data::<Int64>(o).v);
            } else {
                throw(E::InvalidArgument, format!("Expected instance of {} or {}, {} given", cname(sys::php_date_get_interface_ce()), cname(ce().int64), cname((*o).ce)));
            }
        }
        sys::IS_LONG => {
            me.set((*z).value.lval);
        }
        _ => throw(E::InvalidArgument, format!("Expected integer or object, {} given", zval::type_name(z))),
    }
}
pub unsafe extern "C" fn zim_MongoDB_BSON_UTCDateTime___set_state(ex: Ex, rv: *mut sys::zval) {
    set_state::<UTCDateTime>(ex, rv, ce().utcdatetime, UTCDateTime::from_hash);
}
pub unsafe extern "C" fn zim_MongoDB_BSON_UTCDateTime___toString(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        *rv = zval::string(this::<UTCDateTime>(ex).ms.to_string().as_bytes());
    }
}
unsafe fn to_php_date(ex: Ex, rv: *mut sys::zval, c: *mut sys::zend_class_entry) {
    if !call::none(ex) {
        return;
    }
    let ms = this::<UTCDateTime>(ex).ms;
    sys::object_init_ex(rv, c);
    let d = date_obj((*rv).value.obj);
    let mut sec = ms / 1000;
    let mut usec = (ms.abs() % 1000) * 1000;
    if ms < 0 && usec != 0 {
        sec -= 1;
        usec = 1_000_000 - usec;
    }
    let s = format!("@{}", sec);
    sys::php_date_initialize(d, s.as_ptr() as *const _, s.len(), null_mut(), null_mut(), 0);
    (*(*d).time).us = usec;
}
pub unsafe extern "C" fn zim_MongoDB_BSON_UTCDateTime_toDateTime(ex: Ex, rv: *mut sys::zval) {
    to_php_date(ex, rv, sys::php_date_get_date_ce())
}
pub unsafe extern "C" fn zim_MongoDB_BSON_UTCDateTime_toDateTimeImmutable(ex: Ex, rv: *mut sys::zval) {
    to_php_date(ex, rv, sys::php_date_get_immutable_ce())
}
pub unsafe extern "C" fn zim_MongoDB_BSON_UTCDateTime_jsonSerialize(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        let inner = hash::new(1);
        str_prop(inner, b"$numberLong", this::<UTCDateTime>(ex).ms.to_string().as_bytes());
        let ht = hash::new(1);
        hash::sym_update(ht, b"$date", zval::arr(inner));
        *rv = zval::arr(ht);
    }
}
pub unsafe extern "C" fn zim_MongoDB_BSON_UTCDateTime___serialize(ex: Ex, rv: *mut sys::zval) {
    serialize::<UTCDateTime>(ex, rv)
}
pub unsafe extern "C" fn zim_MongoDB_BSON_UTCDateTime___unserialize(ex: Ex, _rv: *mut sys::zval) {
    unserialize::<UTCDateTime>(ex, UTCDateTime::from_hash)
}

#[allow(dead_code)]
fn _w(_: Writer) {}
