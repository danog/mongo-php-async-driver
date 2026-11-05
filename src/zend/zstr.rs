//! zend_string accessors (ZSTR_VAL / ZSTR_LEN).

use super::sys;

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
