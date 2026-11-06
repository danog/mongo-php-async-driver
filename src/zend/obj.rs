//! Objects with Rust state: `Obj<T>` = the state followed by the zend_object (like phongo's
//! `phongo_<name>_t { ...; zend_object std; }`), allocation (zend_object_alloc +
//! zend_object_std_init + object_properties_init), handler tables and the class registry.

use super::sys;
use std::mem::{offset_of, size_of};
use std::ptr::{addr_of_mut, null_mut};

#[repr(C)]
pub struct Obj<T> {
    pub data: T,
    pub std: sys::zend_object,
}

pub const fn offset<T>() -> usize {
    offset_of!(Obj<T>, std)
}

/// zend_object_properties_size
#[inline]
/// (negative, -sizeof(zval), for a class without properties or guards: the last zval of
/// zend_object's properties_table is not allocated then)
unsafe fn props_size(ce: *const sys::zend_class_entry) -> isize {
    let n = (*ce).default_properties_count as isize - if (*ce).ce_flags & sys::ZEND_ACC_USE_GUARDS != 0 { 0 } else { 1 };
    size_of::<sys::zval>() as isize * n
}

/// Allocates and initializes an object of class `ce` carrying `data`.
pub unsafe fn alloc<T>(ce: *mut sys::zend_class_entry, data: T, handlers: *const sys::zend_object_handlers) -> *mut sys::zend_object {
    let p = sys::_emalloc((size_of::<Obj<T>>() as isize + props_size(ce)) as usize) as *mut Obj<T>;
    std::ptr::write(addr_of_mut!((*p).data), data);
    let o = addr_of_mut!((*p).std);
    sys::zend_object_std_init(o, ce);
    sys::object_properties_init(o, ce);
    (*o).handlers = handlers;
    o
}

/// The Rust state of an object created by `alloc::<T>`.
#[inline]
pub unsafe fn data<'a, T>(o: *mut sys::zend_object) -> &'a mut T {
    &mut (*((o as *mut u8).sub(offset::<T>()) as *mut Obj<T>)).data
}

/// The free_obj handler for `Obj<T>`.
pub unsafe extern "C" fn free<T>(o: *mut sys::zend_object) {
    sys::zend_object_std_dtor(o);
    std::ptr::drop_in_place(data::<T>(o) as *mut T);
}

/// Whether the object being freed is garbage freed by the cycle collector (zend_gc_collect_cycles
/// sets GC_NULL as its type just before free_obj; nothing else frees an object typed so).
#[inline]
pub unsafe fn freed_by_gc(o: *mut sys::zend_object) -> bool {
    (*o).gc.u.type_info & 0xf != sys::IS_OBJECT
}

/// Whether `o` is a live object (not freed, nor garbage being freed by the GC).
#[inline]
pub unsafe fn is_live(o: *mut sys::zend_object) -> bool {
    !freed_by_gc(o) && (*o).gc.u.type_info & sys::IS_OBJ_FREE_CALLED == 0
}

/// get_gc for objects holding PHP values outside their properties: the declared properties
/// (or the properties table) as zend_std_get_gc gives them, plus `extra`, through the
/// request's GC buffer (zend_get_gc_buffer_create/add_zval/use).
pub unsafe fn get_gc_with(o: *mut sys::zend_object, table: *mut *mut sys::zval, n: *mut i32, extra: impl IntoIterator<Item = *mut sys::zval>) -> *mut sys::HashTable {
    let (mut t, mut cnt): (*mut sys::zval, i32) = (null_mut(), 0);
    let ht = std_get_gc(o, &mut t, &mut cnt);
    let buf = sys::zend_get_gc_buffer_create();
    let add = |z: *mut sys::zval| {
        // zend_get_gc_buffer_add_zval: refcounted values only
        if (*z).u1.type_info & 0xff00 != 0 {
            if (*buf).cur == (*buf).end {
                sys::zend_get_gc_buffer_grow(buf);
            }
            std::ptr::copy_nonoverlapping(z, (*buf).cur, 1);
            (*buf).cur = (*buf).cur.add(1);
        }
    };
    for i in 0..cnt.max(0) as usize {
        add(t.add(i));
    }
    for z in extra {
        add(z);
    }
    // zend_get_gc_buffer_use
    *table = (*buf).start;
    *n = (*buf).cur.offset_from((*buf).start) as i32;
    ht
}

/// phongo_std_object_handlers: std handlers without clone, with phongo's get_gc.
pub static mut STD: sys::zend_object_handlers = unsafe { std::mem::zeroed() };

unsafe extern "C" fn std_get_gc(o: *mut sys::zend_object, table: *mut *mut sys::zval, n: *mut i32) -> *mut sys::HashTable {
    if !(*(*o).handlers).get_properties.is_some_and(|f| std::ptr::fn_addr_eq(f, sys::zend_std_get_properties as unsafe extern "C" fn(_) -> _)) {
        *table = null_mut();
        *n = 0;
        return (*o).properties;
    }
    sys::zend_std_get_gc(o, table, n)
}

pub unsafe fn init_std() {
    let s = &mut *addr_of_mut!(STD);
    *s = sys::std_object_handlers;
    s.clone_obj = None;
    s.get_gc = Some(std_get_gc);
}

/// A handler table for `Obj<T>` based on phongo's std handlers (offset and free_obj set).
pub unsafe fn handlers<T>(h: *mut sys::zend_object_handlers) -> &'static mut sys::zend_object_handlers {
    *h = *addr_of_mut!(STD);
    (*h).offset = offset::<T>() as i32;
    (*h).free_obj = Some(free::<T>);
    &mut *h
}

/// instanceof_function
#[inline]
pub unsafe fn instanceof(ce: *const sys::zend_class_entry, of: *const sys::zend_class_entry) -> bool {
    ce == of || sys::instanceof_function_slow(ce, of)
}

/// object_init_ex into a fresh zval.
pub unsafe fn new_zval(ce: *mut sys::zend_class_entry) -> sys::zval {
    let mut z = super::zval::undef();
    sys::object_init_ex(&mut z, ce);
    z
}
