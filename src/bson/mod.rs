//! Port of phongo's BSON layer: decoding (phongo_bson.c), encoding (phongo_bson_encode.c) and
//! the MongoDB\BSON classes (src/BSON/*.c).

#![allow(non_snake_case)]

pub mod decode;
pub mod document;
pub mod encode;
pub mod types;

use crate::zend::{call, class, sys, ClassEntry};
use std::ffi::CStr;

/// The class entries phongo keeps in globals (phongo_*_ce).
pub struct Ces {
    pub type_: ClassEntry,
    pub serializable: ClassEntry,
    pub unserializable: ClassEntry,
    pub persistable: ClassEntry,
    pub binary: ClassEntry,
    pub dbpointer: ClassEntry,
    pub decimal128: ClassEntry,
    pub document: ClassEntry,
    pub packedarray: ClassEntry,
    pub iterator: ClassEntry,
    pub int64: ClassEntry,
    pub javascript: ClassEntry,
    pub maxkey: ClassEntry,
    pub minkey: ClassEntry,
    pub objectid: ClassEntry,
    pub regex: ClassEntry,
    pub symbol: ClassEntry,
    pub timestamp: ClassEntry,
    pub undefined: ClassEntry,
    pub utcdatetime: ClassEntry,
    pub vectortype: ClassEntry,
}

pub static CE: Ces = Ces {
    type_: ClassEntry::new(),
    serializable: ClassEntry::new(),
    unserializable: ClassEntry::new(),
    persistable: ClassEntry::new(),
    binary: ClassEntry::new(),
    dbpointer: ClassEntry::new(),
    decimal128: ClassEntry::new(),
    document: ClassEntry::new(),
    packedarray: ClassEntry::new(),
    iterator: ClassEntry::new(),
    int64: ClassEntry::new(),
    javascript: ClassEntry::new(),
    maxkey: ClassEntry::new(),
    minkey: ClassEntry::new(),
    objectid: ClassEntry::new(),
    regex: ClassEntry::new(),
    symbol: ClassEntry::new(),
    timestamp: ClassEntry::new(),
    undefined: ClassEntry::new(),
    utcdatetime: ClassEntry::new(),
    vectortype: ClassEntry::new(),
};

/// The class entries (set at MINIT).
#[inline]
pub fn ce() -> &'static Ces {
    &CE
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
    CE.type_.set(ce);
}
pub unsafe fn init_MongoDB_BSON_Serializable(ce: *mut sys::zend_class_entry) {
    CE.serializable.set(ce);
}
pub unsafe fn init_MongoDB_BSON_Unserializable(ce: *mut sys::zend_class_entry) {
    CE.unserializable.set(ce);
    (*ce).__bindgen_anon_2.interface_gets_implemented = Some(implement_unserializable);
}

/// phongo_implement_unserializable: enums cannot implement Unserializable (nor Persistable).
unsafe extern "C" fn implement_unserializable(iface: *mut sys::zend_class_entry, class: *mut sys::zend_class_entry) -> i32 {
    if (*class).ce_flags & sys::ZEND_ACC_ENUM != 0 {
        let c = ce();
        let mut err = iface;
        if iface == c.unserializable.get() && !c.persistable.get().is_null() && crate::zend::obj::instanceof(class, c.persistable.get()) {
            err = c.persistable.get();
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
    CE.persistable.set(ce);
}
pub unsafe fn init_MongoDB_BSON_VectorType(ce: *mut sys::zend_class_entry) {
    CE.vectortype.set(ce);
}
