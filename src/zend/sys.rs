//! Raw Zend Engine FFI: ext-php-rs's bindgen bindings (`ext_php_rs::ffi`, extended through
//! EXT_PHP_RS_ALLOWED_BINDINGS in .cargo/config.toml), plus the few ext/date and ext/spl
//! declarations its wrapper header does not include.
#![allow(non_upper_case_globals, non_camel_case_types, non_snake_case, dead_code, clippy::all)]

pub use ext_php_rs::ffi::*;

pub type timelib_sll = ::core::ffi::c_longlong;

/// timelib_rel_time (ext/date/lib/timelib.h; embedded in timelib_time)
#[repr(C)]
#[derive(Copy, Clone)]
pub struct timelib_rel_time {
    pub y: timelib_sll,
    pub m: timelib_sll,
    pub d: timelib_sll,
    pub h: timelib_sll,
    pub i: timelib_sll,
    pub s: timelib_sll,
    pub us: timelib_sll,
    pub weekday: ::core::ffi::c_int,
    pub weekday_behavior: ::core::ffi::c_int,
    pub first_last_day_of: ::core::ffi::c_int,
    pub invert: ::core::ffi::c_int,
    pub days: timelib_sll,
    pub special_type: ::core::ffi::c_uint,
    pub special_amount: timelib_sll,
    pub have_weekday_relative: ::core::ffi::c_uint,
    pub have_special_relative: ::core::ffi::c_uint,
}

/// timelib_time (ext/date/lib/timelib.h)
#[repr(C)]
#[derive(Copy, Clone)]
pub struct timelib_time {
    pub y: timelib_sll,
    pub m: timelib_sll,
    pub d: timelib_sll,
    pub h: timelib_sll,
    pub i: timelib_sll,
    pub s: timelib_sll,
    pub us: timelib_sll,
    pub z: ::core::ffi::c_int,
    pub tz_abbr: *mut ::core::ffi::c_char,
    pub tz_info: *mut ::core::ffi::c_void,
    pub dst: ::core::ffi::c_int,
    pub relative: timelib_rel_time,
    pub sse: timelib_sll,
    pub have_time: ::core::ffi::c_uint,
    pub have_date: ::core::ffi::c_uint,
    pub have_zone: ::core::ffi::c_uint,
    pub have_relative: ::core::ffi::c_uint,
    pub have_weeknr_day: ::core::ffi::c_uint,
    pub sse_uptodate: ::core::ffi::c_uint,
    pub tim_uptodate: ::core::ffi::c_uint,
    pub is_localtime: ::core::ffi::c_uint,
    pub zone_type: ::core::ffi::c_uint,
}

// timelib.h layout (x86_64/aarch64, LP64): checked against the bindgen output of PHP 8.4/8.5
const _: () = {
    assert!(::core::mem::size_of::<timelib_rel_time>() == 104);
    assert!(::core::mem::offset_of!(timelib_time, us) == 48);
    assert!(::core::mem::offset_of!(timelib_time, relative) == 88);
    assert!(::core::mem::offset_of!(timelib_time, sse) == 192);
    assert!(::core::mem::size_of::<timelib_time>() == 240);
};

/// php_date_obj (ext/date/php_date.h)
#[repr(C)]
pub struct php_date_obj {
    pub time: *mut timelib_time,
    pub std: zend_object,
}

pub type spl_iterator_apply_func_t = ::core::option::Option<unsafe extern "C" fn(iter: *mut zend_object_iterator, puser: *mut ::core::ffi::c_void) -> ::core::ffi::c_int>;

unsafe extern "C" {
    pub fn php_date_get_date_ce() -> *mut zend_class_entry;
    pub fn php_date_get_immutable_ce() -> *mut zend_class_entry;
    pub fn php_date_get_interface_ce() -> *mut zend_class_entry;
    pub fn php_date_initialize(dateobj: *mut php_date_obj, time_str: *const ::core::ffi::c_char, time_str_len: usize, format: *const ::core::ffi::c_char, timezone_object: *mut zval, flags: ::core::ffi::c_int) -> bool;
    pub fn spl_iterator_apply(obj: *mut zval, apply_func: spl_iterator_apply_func_t, puser: *mut ::core::ffi::c_void) -> zend_result;
}
