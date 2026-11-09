//! Function-call plumbing: arguments of an internal call (ZEND_CALL_ARG, ZEND_NUM_ARGS,
//! getThis), parameter parsing helpers, exceptions, method calls.

use super::{sys, zval};
use std::ffi::{CStr, CString};
use std::ptr::null_mut;

pub type Ex = *mut sys::zend_execute_data;

const FRAME_SLOT: usize = std::mem::size_of::<sys::zend_execute_data>().div_ceil(std::mem::size_of::<sys::zval>());

#[inline]
pub unsafe fn num_args(ex: Ex) -> u32 {
    (*ex).This.u2.num_args
}

/// ZEND_CALL_ARG(ex, n + 1)
#[inline]
pub unsafe fn arg(ex: Ex, n: usize) -> *mut sys::zval {
    (ex as *mut sys::zval).add(FRAME_SLOT + n)
}

#[inline]
pub unsafe fn this(ex: Ex) -> *mut sys::zend_object {
    (*ex).This.value.obj
}

/// zend_parse_parameters_none(): false (an ArgumentCountError is thrown) when args were given.
#[inline]
pub unsafe fn none(ex: Ex) -> bool {
    if num_args(ex) != 0 {
        sys::zend_wrong_parameters_none_error();
        return false;
    }
    true
}

#[inline]
pub unsafe fn has_exception() -> bool {
    !(*super::eg()).exception.is_null()
}

/// zend_throw_exception(ce, msg, 0); the message stops at a NUL as with C strings.
pub unsafe fn throw(ce: *mut sys::zend_class_entry, msg: &str) {
    let b = super::zstr::c_part(msg.as_bytes());
    let c = CString::new(b).unwrap();
    sys::zend_throw_exception(ce, c.as_ptr(), 0);
}

/// zend_throw_error(NULL, ...)
pub unsafe fn throw_error(ce: *mut sys::zend_class_entry, msg: &str) {
    let c = CString::new(super::zstr::c_part(msg.as_bytes())).unwrap();
    sys::zend_throw_error(ce, c"%s".as_ptr(), c.as_ptr());
}

/// zend_call_method(obj, NULL, NULL, name, ...) with up to two arguments; returns the result
/// (UNDEF when the call failed).
pub unsafe fn method(o: *mut sys::zend_object, name: &CStr, args: &mut [sys::zval]) -> sys::zval {
    let mut rv = zval::undef();
    let (a1, a2) = match args.len() {
        0 => (null_mut(), null_mut()),
        1 => (&mut args[0] as *mut _, null_mut()),
        _ => (&mut args[0] as *mut _, &mut args[1] as *mut _),
    };
    sys::zend_call_method(o, null_mut(), null_mut(), name.as_ptr(), name.to_bytes().len(), &mut rv, args.len() as u32, a1, a2);
    rv
}

/// A C string of a Rust string (for variadic engine calls).
pub fn cs(s: &str) -> CString {
    CString::new(super::zstr::c_part(s.as_bytes())).unwrap()
}

/// call_user_function(NULL, object, name, ...): calls the method `name` of `o` with any number
/// of arguments; returns the result (UNDEF when the call failed).
pub unsafe fn method_n(o: *mut sys::zend_object, name: &[u8], args: &mut [sys::zval]) -> sys::zval {
    let mut rv = zval::undef();
    // borrowed: the caller holds a reference to `o`
    let mut obj = zval::borrowed_obj(o);
    let mut func = zval::string(name);
    sys::_call_user_function_impl(&mut *obj, &mut func, &mut rv, args.len() as u32, args.as_mut_ptr(), null_mut());
    rv
}
