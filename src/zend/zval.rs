//! zval constructors and accessors (ports of the ZVAL_* / Z_* macros of zend_types.h).
//!
//! Ownership follows ext-php-rs: `sys::zval` is ext-php-rs's `Zval`, whose `Drop` releases the
//! value. A `sys::zval` held by value is therefore always *owned* (one reference). The rules:
//! - borrowed engine zvals are only handled through `*mut`/`*const sys::zval` (or `&Zval`); an owned
//!   copy of one is made with [`copy`] (addref), never by bit-copying;
//! - out-parameters (`return_value`, handler results, uninitialised slots) are filled with
//!   [`write`] (ZVAL_COPY_VALUE: the old bytes are not released);
//! - a value handed over to the engine (hash insert, property default, ...) goes through
//!   [`into_raw`], so it is not released here as well;
//! - a non-owning view of an engine value (no addref) is a `ManuallyDrop` ([`borrowed_obj`]).

use super::sys;
use std::mem::ManuallyDrop;
use std::ptr::addr_of;

/// ext-php-rs's owned zval (`Drop` = zval_ptr_dtor).
pub type Zval = sys::zval;

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

/// zend_is_true (a bool from PHP 8.4 on, an int before).
#[inline]
#[allow(clippy::unnecessary_cast)]
pub unsafe fn is_true(z: *mut sys::zval) -> bool {
    sys::zend_is_true(z) as i32 != 0
}

/// The value's name in zend_parse_parameters' type errors: zend_zval_value_name from PHP 8.3
/// on ("true", a class name), zend_zval_type_name before ("bool"), as PHP's own messages.
#[inline]
pub unsafe fn value_name(z: *mut sys::zval) -> *const std::ffi::c_char {
    #[cfg(php83)]
    return sys::zend_zval_value_name(z);
    #[cfg(not(php83))]
    return sys::zend_zval_type_name(z);
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

/// ZVAL_COPY: an owned copy (value + addref) of a borrowed zval.
#[inline]
pub unsafe fn copy(z: *const sys::zval) -> sys::zval {
    addref(z);
    std::ptr::read(z)
}

/// ZVAL_COPY_VALUE into an out-parameter or an empty slot: `dst` is uninitialised or holds
/// nothing that needs releasing (the old bytes are not dropped). Ownership of `v` moves to `dst`.
#[inline]
pub unsafe fn write(dst: *mut sys::zval, v: sys::zval) {
    std::ptr::write(dst, v);
}

/// Hands `v` over to the engine (which now owns its reference): the result is never dropped here.
#[inline]
pub fn into_raw(v: sys::zval) -> ManuallyDrop<sys::zval> {
    ManuallyDrop::new(v)
}

/// A non-owning view (no addref) of an engine object: never released.
#[inline]
pub unsafe fn borrowed_obj(o: *mut sys::zend_object) -> ManuallyDrop<sys::zval> {
    ManuallyDrop::new(obj(o))
}

/// zval_ptr_dtor on an engine-owned slot, which is left UNDEF (so nothing can release it again).
#[inline]
pub unsafe fn release(z: *mut sys::zval) {
    sys::zval_ptr_dtor(z);
    (*z).u1.type_info = sys::IS_UNDEF;
}

/// zend_zval_type_name
pub unsafe fn type_name(z: *const sys::zval) -> &'static str {
    let p = sys::zend_zval_type_name(z);
    std::ffi::CStr::from_ptr(p).to_str().unwrap_or("?")
}
