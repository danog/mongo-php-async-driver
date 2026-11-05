//! Compile-time checks of the checked-in bindgen bindings (generated with --no-layout-tests)
//! against the C layouts of PHP 8.4/8.5 on LP64 targets, for every struct and field the
//! extension touches directly. A mismatch fails the build instead of corrupting memory.

use super::sys;
use std::mem::{offset_of, size_of};

const _: () = {
    assert!(size_of::<sys::zval>() == 16);
    assert!(size_of::<sys::zend_refcounted_h>() == 8);
    // zend_string: gc, h, len, val[1]
    assert!(offset_of!(sys::zend_string, h) == 8);
    assert!(offset_of!(sys::zend_string, len) == 16);
    assert!(offset_of!(sys::zend_string, val) == 24);
    // zend_object: gc, handle, ce, handlers, properties, properties_table[1]
    assert!(offset_of!(sys::zend_object, handle) == 8);
    assert!(offset_of!(sys::zend_object, ce) == 16);
    assert!(offset_of!(sys::zend_object, handlers) == 24);
    assert!(offset_of!(sys::zend_object, properties) == 32);
    assert!(offset_of!(sys::zend_object, properties_table) == 40);
    assert!(size_of::<sys::zend_object>() == 56);
    assert!(size_of::<sys::HashTable>() == 56);
    assert!(offset_of!(sys::HashTable, nNumUsed) == 24);
    // zend_object_iterator: std, data, funcs, index
    assert!(offset_of!(sys::zend_object_iterator, data) == 56);
    assert!(offset_of!(sys::zend_object_iterator, funcs) == 72);
    assert!(offset_of!(sys::zend_object_iterator, index) == 80);
    assert!(size_of::<sys::zend_object_iterator>() == 88);
    assert!(size_of::<sys::zend_object_iterator_funcs>() == 8 * 8);
    // php_date_obj: time, std (the object is at the end)
    assert!(offset_of!(sys::php_date_obj, std) == 8);
    assert!(size_of::<sys::php_date_obj>() == 8 + 56);
};
