//! Class, interface and enum registration (ports of INIT_CLASS_ENTRY and of the register_class_*
//! code that gen_stub.php emits), used by the generated src/gen/api.rs.

use super::sys;
use std::ffi::{c_char, CStr};
use std::ptr::null_mut;

/// A property's default value.
pub enum Default {
    Undef,
    Null,
    EmptyArray,
}

/// A persistent interned string (lives as long as the process).
pub unsafe fn interned(s: &CStr) -> *mut sys::zend_string {
    let f = sys::zend_string_init_interned.expect("zend_string_init_interned");
    f(s.as_ptr(), s.to_bytes().len(), true)
}

/// INIT_CLASS_ENTRY(ce, name, functions).
unsafe fn class_entry(name: &CStr, functions: *const sys::zend_function_entry) -> sys::zend_class_entry {
    let mut ce: sys::zend_class_entry = std::mem::zeroed();
    ce.name = interned(name);
    // the field exists from PHP 8.3 on (objects get their handlers in obj::alloc anyway)
    #[cfg(php83)]
    {
        ce.default_object_handlers = std::ptr::addr_of!(super::obj::STD);
    }
    ce.info.internal.builtin_functions = functions;
    ce
}

pub unsafe fn register_class(name: &CStr, functions: *const sys::zend_function_entry, parent: *mut sys::zend_class_entry, flags: u32) -> *mut sys::zend_class_entry {
    let mut ce = class_entry(name, functions);
    #[cfg(php84)]
    return sys::zend_register_internal_class_with_flags(&mut ce, parent, flags);
    // before 8.4 (zend_register_internal_class_with_flags is new there): register, then flag
    #[cfg(not(php84))]
    {
        let c = sys::zend_register_internal_class_ex(&mut ce, parent);
        (*c).ce_flags |= flags;
        c
    }
}

/// The type-mask bit of a class name given as a C string in internal arginfo
/// (ZEND_TYPE_INIT_CLASS_CONST): _ZEND_TYPE_LITERAL_NAME_BIT from PHP 8.3 on, _ZEND_TYPE_NAME_BIT
/// before (PHP 8.2 converts those C strings at registration).
#[cfg(php83)]
pub const LITERAL_NAME_BIT: u32 = 1 << 23;
#[cfg(not(php83))]
pub const LITERAL_NAME_BIT: u32 = 1 << 24;

/// A method table entry; `frameless_function_infos` and `doc_comment` exist from PHP 8.4 on.
#[allow(clippy::too_many_arguments)]
pub const fn fe(fname: *const c_char, handler: sys::zif_handler, arg_info: *const sys::zend_internal_arg_info, num_args: u32, flags: u64) -> sys::zend_function_entry {
    sys::zend_function_entry {
        fname,
        handler,
        arg_info,
        num_args,
        flags: flags as _,
        #[cfg(php84)]
        frameless_function_infos: std::ptr::null(),
        #[cfg(php84)]
        doc_comment: std::ptr::null(),
    }
}

pub unsafe fn register_interface(name: &CStr, functions: *const sys::zend_function_entry) -> *mut sys::zend_class_entry {
    let mut ce = class_entry(name, functions);
    sys::zend_register_internal_interface(&mut ce)
}

/// A pure (not backed) enum.
pub unsafe fn register_enum(name: &CStr, functions: *const sys::zend_function_entry) -> *mut sys::zend_class_entry {
    sys::zend_register_internal_enum(name.as_ptr(), sys::IS_UNDEF as u8, functions)
}

pub unsafe fn enum_case(ce: *mut sys::zend_class_entry, name: &CStr) {
    sys::zend_enum_add_case_cstr(ce, name.as_ptr(), null_mut());
}

pub unsafe fn implements(ce: *mut sys::zend_class_entry, interfaces: &[*mut sys::zend_class_entry]) {
    for &i in interfaces {
        sys::zend_class_implements(ce, 1, i);
    }
}

/// An internal class by lower-case name (registered earlier: PHP's own, or ours).
pub unsafe fn lookup(lcname: &CStr) -> *mut sys::zend_class_entry {
    let ct = (*super::cg()).class_table;
    let z = sys::zend_hash_str_find(ct, lcname.as_ptr(), lcname.to_bytes().len());
    if z.is_null() {
        panic!("mongodb: class {lcname:?} is not registered");
    }
    (*z).value.ptr as *mut sys::zend_class_entry
}

/// zend_declare_class_constant_ex: the constant takes over `value`.
unsafe fn declare_const(ce: *mut sys::zend_class_entry, name: &CStr, value: sys::zval, flags: u32) {
    let mut value = super::zval::into_raw(value);
    sys::zend_declare_class_constant_ex(ce, interned(name), &mut *value, flags as i32, null_mut());
}

pub unsafe fn const_long(ce: *mut sys::zend_class_entry, name: &CStr, v: i64, flags: u32) {
    declare_const(ce, name, super::zval::long(v), flags);
}

pub unsafe fn const_double(ce: *mut sys::zend_class_entry, name: &CStr, v: f64, flags: u32) {
    declare_const(ce, name, super::zval::double(v), flags);
}

pub unsafe fn const_bool(ce: *mut sys::zend_class_entry, name: &CStr, v: bool, flags: u32) {
    declare_const(ce, name, super::zval::bool(v), flags);
}

pub unsafe fn const_str(ce: *mut sys::zend_class_entry, name: &CStr, v: &CStr, flags: u32) {
    declare_const(ce, name, super::zval::interned_str(interned(v)), flags);
}

const TYPE_NAME_BIT: u32 = 1 << 24;

/// zend_declare_typed_property; `class`: the class part of the type (mask: the rest; 0 and no
/// class = untyped).
pub unsafe fn typed_property(ce: *mut sys::zend_class_entry, name: &CStr, class: Option<&CStr>, mask: u32, flags: u32, default: Default) {
    // the property info takes over the default value
    let mut dv = super::zval::into_raw(match default {
        Default::Undef => super::zval::undef(),
        Default::Null => super::zval::null(),
        Default::EmptyArray => super::zval::empty_array(),
    });
    let ty = match class {
        Some(c) => sys::zend_type { ptr: interned(c) as *mut _, type_mask: TYPE_NAME_BIT | mask },
        None => sys::zend_type { ptr: null_mut(), type_mask: mask },
    };
    sys::zend_declare_typed_property(ce, interned(name), &mut *dv, flags as i32, null_mut(), ty);
}

/// The class name of `ce`, for messages.
pub unsafe fn name(ce: *const sys::zend_class_entry) -> &'static str {
    super::zstr::as_str((*ce).name)
}

#[allow(dead_code)]
fn _assert_c_char(_: *const c_char) {}
