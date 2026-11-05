//! zend_string: accessors (ZSTR_VAL / ZSTR_LEN) and the inline helpers of zend_string.h.

use super::sys;
use std::mem::offset_of;

/// The bytes of a zend_string.
#[inline]
pub unsafe fn bytes<'a>(s: *const sys::zend_string) -> &'a [u8] {
    std::slice::from_raw_parts((*s).val.as_ptr() as *const u8, (*s).len)
}

/// The zend_string as &str (lossy: invalid UTF-8 yields "?").
#[inline]
pub unsafe fn as_str<'a>(s: *const sys::zend_string) -> &'a str {
    std::str::from_utf8(bytes(s)).unwrap_or("?")
}

/// zend_string_init(s, len, 0): a new request-allocated string (refcount 1).
#[inline]
pub unsafe fn new(b: &[u8]) -> *mut sys::zend_string {
    let hdr = offset_of!(sys::zend_string, val);
    let size = (hdr + b.len() + 1 + 7) & !7;
    let s = sys::_emalloc(size) as *mut sys::zend_string;
    (*s).gc.refcount = 1;
    (*s).gc.u.type_info = sys::GC_STRING;
    (*s).h = 0;
    (*s).len = b.len();
    let p = (*s).val.as_mut_ptr() as *mut u8;
    std::ptr::copy_nonoverlapping(b.as_ptr(), p, b.len());
    *p.add(b.len()) = 0;
    s
}

/// zend_string_release.
#[inline]
pub unsafe fn release(s: *mut sys::zend_string) {
    if (*s).gc.u.type_info & sys::IS_STR_INTERNED == 0 {
        (*s).gc.refcount -= 1;
        if (*s).gc.refcount == 0 {
            if (*s).gc.u.type_info & sys::IS_STR_PERSISTENT != 0 {
                libc::free(s as *mut _);
            } else {
                sys::_efree(s as *mut _);
            }
        }
    }
}

/// zend_string_copy (addref unless interned).
#[inline]
pub unsafe fn copy(s: *mut sys::zend_string) -> *mut sys::zend_string {
    if (*s).gc.u.type_info & sys::IS_STR_INTERNED == 0 {
        (*s).gc.refcount += 1;
    }
    s
}

/// The bytes up to the first NUL, as a C "%s" conversion would print them.
#[inline]
pub fn c_part(b: &[u8]) -> &[u8] {
    &b[..b.iter().position(|&c| c == 0).unwrap_or(b.len())]
}
