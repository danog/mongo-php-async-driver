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
