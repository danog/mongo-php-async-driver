//! Zend Engine layer: the FFI declarations (`sys` = ext-php-rs's bindings) and Rust ports of the
//! inline helpers and macros of the PHP headers that the extension needs (zend_types.h,
//! zend_string.h, zend_hash.h, zend_API.h, ...). zvals follow ext-php-rs's ownership (see
//! `zval`). NTS builds only: globals are plain symbols.

pub mod call;
pub mod class;
pub mod hash;
mod layout;
pub mod obj;
pub mod sys;
pub mod zstr;
pub mod zval;

use std::ffi::{c_char, CStr};

/// A `*const c_char` for a static C string literal.
#[inline]
pub const fn cstr(s: &'static CStr) -> *const c_char {
    s.as_ptr()
}

extern "C" {
    // variadic: not emitted usefully by bindgen
    pub fn php_info_print_table_header(num_cols: i32, ...);
    pub fn php_info_print_table_row(num_cols: i32, ...);
}

/// `php_info_print_table_row(2, k, v)` with Rust strings.
pub fn info_row(k: &CStr, v: &str) {
    let v = std::ffi::CString::new(v).unwrap();
    unsafe { php_info_print_table_row(2, k.as_ptr(), v.as_ptr()) };
}

/// Static engine tables (arrays of structs holding raw pointers): only read by PHP, on its thread.
#[repr(transparent)]
pub struct Static<T>(pub T);
unsafe impl<T> Sync for Static<T> {}

/// Stub for an ext-mongodb method or function not ported yet: throws Error.
pub unsafe fn not_implemented(handler: &CStr) {
    sys::zend_throw_error(std::ptr::null_mut(), c"%s is not implemented yet by this mongodb extension".as_ptr(), handler.as_ptr());
}
