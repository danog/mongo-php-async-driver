//! Port of phongo's BSON layer: decoding (phongo_bson.c), encoding (phongo_bson_encode.c) and
//! the MongoDB\BSON classes (src/BSON/*.c).

#![allow(non_snake_case)]

pub mod decode;
pub mod document;
pub mod encode;
pub mod types;

use crate::zend::{call, class, sys};
use std::ffi::CStr;
use std::ptr::null_mut;

/// The class entries phongo keeps in globals (phongo_*_ce).
pub struct Ces {
    pub type_: *mut sys::zend_class_entry,
    pub serializable: *mut sys::zend_class_entry,
    pub unserializable: *mut sys::zend_class_entry,
    pub persistable: *mut sys::zend_class_entry,
    pub binary: *mut sys::zend_class_entry,
    pub dbpointer: *mut sys::zend_class_entry,
    pub decimal128: *mut sys::zend_class_entry,
    pub document: *mut sys::zend_class_entry,
    pub packedarray: *mut sys::zend_class_entry,
    pub iterator: *mut sys::zend_class_entry,
    pub int64: *mut sys::zend_class_entry,
    pub javascript: *mut sys::zend_class_entry,
    pub maxkey: *mut sys::zend_class_entry,
    pub minkey: *mut sys::zend_class_entry,
    pub objectid: *mut sys::zend_class_entry,
    pub regex: *mut sys::zend_class_entry,
    pub symbol: *mut sys::zend_class_entry,
    pub timestamp: *mut sys::zend_class_entry,
    pub undefined: *mut sys::zend_class_entry,
    pub utcdatetime: *mut sys::zend_class_entry,
    pub vectortype: *mut sys::zend_class_entry,
}

pub static mut CE: Ces = Ces {
    type_: null_mut(),
    serializable: null_mut(),
    unserializable: null_mut(),
    persistable: null_mut(),
    binary: null_mut(),
    dbpointer: null_mut(),
    decimal128: null_mut(),
    document: null_mut(),
    packedarray: null_mut(),
    iterator: null_mut(),
    int64: null_mut(),
    javascript: null_mut(),
    maxkey: null_mut(),
    minkey: null_mut(),
    objectid: null_mut(),
    regex: null_mut(),
    symbol: null_mut(),
    timestamp: null_mut(),
    undefined: null_mut(),
    utcdatetime: null_mut(),
    vectortype: null_mut(),
};

#[inline]
pub fn ce() -> &'static Ces {
    unsafe { &*std::ptr::addr_of!(CE) }
}

#[inline]
pub fn ce_mut() -> &'static mut Ces {
    unsafe { &mut *std::ptr::addr_of_mut!(CE) }
}

/// phongo_error_domain_t
#[derive(Clone, Copy)]
pub enum E {
    InvalidArgument,
    UnexpectedValue,
    Runtime,
    Logic,
}

unsafe fn exc_ce(e: E) -> *mut sys::zend_class_entry {
    let n: &CStr = match e {
        E::InvalidArgument => c"mongodb\\driver\\exception\\invalidargumentexception",
        E::UnexpectedValue => c"mongodb\\driver\\exception\\unexpectedvalueexception",
        E::Runtime => c"mongodb\\driver\\exception\\runtimeexception",
        E::Logic => c"mongodb\\driver\\exception\\logicexception",
    };
    class::lookup(n)
}

/// phongo_throw_exception(domain, "%s", msg)
pub unsafe fn throw(e: E, msg: impl AsRef<str>) {
    call::throw(exc_ce(e), msg.as_ref());
}

/// Lossy text of bytes up to the first NUL (C "%s").
pub fn cstr_lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(crate::zend::zstr::c_part(b)).into_owned()
}

pub unsafe fn init_MongoDB_BSON_Type(ce: *mut sys::zend_class_entry) {
    ce_mut().type_ = ce;
}
pub unsafe fn init_MongoDB_BSON_Serializable(ce: *mut sys::zend_class_entry) {
    ce_mut().serializable = ce;
}
pub unsafe fn init_MongoDB_BSON_Unserializable(ce: *mut sys::zend_class_entry) {
    ce_mut().unserializable = ce;
    (*ce).__bindgen_anon_2.interface_gets_implemented = Some(implement_unserializable);
}

/// phongo_implement_unserializable: enums cannot implement Unserializable (nor Persistable).
unsafe extern "C" fn implement_unserializable(iface: *mut sys::zend_class_entry, class: *mut sys::zend_class_entry) -> i32 {
    if (*class).ce_flags & sys::ZEND_ACC_ENUM != 0 {
        let c = ce();
        let mut err = iface;
        if iface == c.unserializable && !c.persistable.is_null() && crate::zend::obj::instanceof(class, c.persistable) {
            err = c.persistable;
        }
        sys::zend_error_noreturn(
            sys::E_ERROR as i32,
            c"Enum class %s cannot implement interface %s".as_ptr(),
            (*(*class).name).val.as_ptr(),
            (*(*err).name).val.as_ptr(),
        );
    }
    0
}
pub unsafe fn init_MongoDB_BSON_Persistable(ce: *mut sys::zend_class_entry) {
    ce_mut().persistable = ce;
}
pub unsafe fn init_MongoDB_BSON_VectorType(ce: *mut sys::zend_class_entry) {
    ce_mut().vectortype = ce;
}
