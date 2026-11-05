//! Minimal Zend Engine layer: the FFI declarations (`sys`) and Rust ports of the inline helpers
//! and macros of the PHP headers that the extension needs (zend_types.h, zend_string.h,
//! zend_hash.h, zend_API.h, ...). NTS builds only: globals are plain symbols.

pub mod sys;

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
