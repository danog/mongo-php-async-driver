//! zval constructors and accessors (ports of the ZVAL_* / Z_* macros of zend_types.h).

use super::sys;
use std::ptr::addr_of;

#[inline]
pub fn undef() -> sys::zval {
    unsafe { std::mem::zeroed() }
}

#[inline]
fn with_type(t: u32) -> sys::zval {
    let mut z = undef();
    z.u1.type_info = t;
    z
}

#[inline]
pub fn null() -> sys::zval {
    with_type(sys::IS_NULL)
}

#[inline]
pub fn bool(b: bool) -> sys::zval {
    with_type(if b { sys::IS_TRUE } else { sys::IS_FALSE })
}

#[inline]
pub fn long(v: i64) -> sys::zval {
    let mut z = with_type(sys::IS_LONG);
    z.value.lval = v;
    z
}

#[inline]
pub fn double(v: f64) -> sys::zval {
    let mut z = with_type(sys::IS_DOUBLE);
    z.value.dval = v;
    z
}

/// An interned (not refcounted) string.
#[inline]
pub fn interned_str(s: *mut sys::zend_string) -> sys::zval {
    let mut z = with_type(sys::IS_INTERNED_STRING_EX);
    z.value.str_ = s;
    z
}

/// ZVAL_STR: takes ownership of `s`.
#[inline]
pub unsafe fn str_(s: *mut sys::zend_string) -> sys::zval {
    let interned = (*s).gc.u.type_info & sys::IS_STR_INTERNED != 0;
    let mut z = with_type(if interned { sys::IS_INTERNED_STRING_EX } else { sys::IS_STRING_EX });
    z.value.str_ = s;
    z
}

/// ZVAL_STRINGL: a new string.
#[inline]
pub unsafe fn string(b: &[u8]) -> sys::zval {
    str_(super::zstr::new(b))
}

#[inline]
pub unsafe fn obj(o: *mut sys::zend_object) -> sys::zval {
    let mut z = with_type(sys::IS_OBJECT_EX);
    z.value.obj = o;
    z
}

#[inline]
pub unsafe fn arr(ht: *mut sys::HashTable) -> sys::zval {
    let mut z = with_type(sys::IS_ARRAY_EX);
    z.value.arr = ht;
    z
}

/// ZVAL_EMPTY_ARRAY: the immutable shared empty array.
#[inline]
pub fn empty_array() -> sys::zval {
    let mut z = with_type(sys::IS_ARRAY);
    z.value.arr = addr_of!(sys::zend_empty_array) as *mut sys::HashTable;
    z
}

/// Z_TYPE: the type byte of the type info.
#[inline]
pub unsafe fn type_of(z: *const sys::zval) -> u8 {
    ((*z).u1.type_info & 0xff) as u8
}

#[inline]
pub unsafe fn is(z: *const sys::zval, t: u32) -> bool {
    type_of(z) as u32 == t
}

/// ZVAL_DEREF
#[inline]
pub unsafe fn deref(mut z: *mut sys::zval) -> *mut sys::zval {
    if is(z, sys::IS_REFERENCE) {
        z = &mut (*(*z).value.ref_).val;
    }
    z
}

#[inline]
pub unsafe fn str_bytes<'a>(z: *const sys::zval) -> &'a [u8] {
    super::zstr::bytes((*z).value.str_)
}

#[inline]
pub unsafe fn obj_ce(z: *const sys::zval) -> *mut sys::zend_class_entry {
    (*(*z).value.obj).ce
}

/// Z_TRY_ADDREF
#[inline]
pub unsafe fn addref(z: *const sys::zval) {
    if (*z).u1.type_info & 0xff00 != 0 {
        (*(*z).value.counted).gc.refcount += 1;
    }
}

/// ZVAL_COPY (value + addref)
#[inline]
pub unsafe fn copy(z: *const sys::zval) -> sys::zval {
    addref(z);
    *z
}

#[inline]
pub unsafe fn dtor(z: *mut sys::zval) {
    sys::zval_ptr_dtor(z);
}

/// zend_zval_type_name
pub unsafe fn type_name(z: *const sys::zval) -> &'static str {
    let p = sys::zend_zval_type_name(z);
    std::ffi::CStr::from_ptr(p).to_str().unwrap_or("?")
}
