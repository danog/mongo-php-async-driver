//! MongoDB\Driver\ReadConcern (src/MongoDB/ReadConcern.c over mongoc_read_concern_t).

use crate::bson::{throw, E};
use crate::zend::call::{self, Ex};
use crate::zend::{class, hash, obj, sys, zval};
use std::ptr::{addr_of_mut, null_mut};

pub use mongo_core::concern::ReadConcern;

static mut H_RC: sys::zend_object_handlers = unsafe { std::mem::zeroed() };
pub static mut CE: *mut sys::zend_class_entry = null_mut();

unsafe extern "C" fn create(c: *mut sys::zend_class_entry) -> *mut sys::zend_object {
    obj::alloc(c, ReadConcern::default(), addr_of_mut!(H_RC))
}

unsafe extern "C" fn clone(o: *mut sys::zend_object) -> *mut sys::zend_object {
    let n = create((*o).ce);
    sys::zend_objects_clone_members(n, o);
    *obj::data::<ReadConcern>(n) = obj::data::<ReadConcern>(o).clone();
    n
}

pub unsafe fn init_MongoDB_Driver_ReadConcern(c: *mut sys::zend_class_entry) {
    CE = c;
    (*c).__bindgen_anon_2.create_object = Some(create);
    obj::handlers::<ReadConcern>(addr_of_mut!(H_RC)).clone_obj = Some(clone);
}

unsafe fn update_properties(o: *mut sys::zend_object) {
    match obj::data::<ReadConcern>(o).level.clone() {
        Some(l) => {
            let mut z = zval::string(&l);
            sys::zend_update_property(CE, o, c"level".as_ptr(), 5, &mut z);
            zval::dtor(&mut z);
        }
        None => sys::zend_update_property_null(CE, o, c"level".as_ptr(), 5),
    }
}

/// phongo_readconcern_init
pub unsafe fn new_readconcern(rc: ReadConcern) -> sys::zval {
    let o = obj::alloc(CE, rc, addr_of_mut!(H_RC));
    update_properties(o);
    zval::obj(o)
}

pub unsafe extern "C" fn zim_MongoDB_Driver_ReadConcern___construct(ex: Ex, _rv: *mut sys::zval) {
    let (mut p, mut l): (*const u8, usize) = (null_mut(), 0);
    if sys::zend_parse_parameters(call::num_args(ex), c"|s!".as_ptr(), &mut p, &mut l) != 0 {
        return;
    }
    let o = call::this(ex);
    let rc = obj::data::<ReadConcern>(o);
    *rc = ReadConcern::default();
    if !p.is_null() {
        rc.level = Some(crate::zend::zstr::c_part(std::slice::from_raw_parts(p, l)).to_vec());
    }
    update_properties(o);
}

unsafe fn init_from_hash(o: *mut sys::zend_object, props: *mut sys::HashTable) -> bool {
    let rc = obj::data::<ReadConcern>(o);
    *rc = ReadConcern::default();
    if let Some(l) = hash::find(props, b"level") {
        if zval::is(l, sys::IS_STRING) {
            rc.level = Some(crate::zend::zstr::c_part(zval::str_bytes(l)).to_vec());
        } else if !zval::is(l, sys::IS_NULL) {
            throw(E::InvalidArgument, format!("{} initialization requires \"level\" string field", class::name(CE)));
            return false;
        }
    }
    update_properties(o);
    true
}

pub unsafe extern "C" fn zim_MongoDB_Driver_ReadConcern___set_state(ex: Ex, rv: *mut sys::zval) {
    let mut a: *mut sys::zval = null_mut();
    if sys::zend_parse_parameters(call::num_args(ex), c"a".as_ptr(), &mut a) != 0 {
        return;
    }
    let z = obj::new_zval(CE);
    init_from_hash(z.value.obj, (*a).value.arr);
    *rv = z;
}

pub unsafe extern "C" fn zim_MongoDB_Driver_ReadConcern___unserialize(ex: Ex, _rv: *mut sys::zval) {
    let mut a: *mut sys::zval = null_mut();
    if sys::zend_parse_parameters(call::num_args(ex), c"a".as_ptr(), &mut a) != 0 {
        return;
    }
    init_from_hash(call::this(ex), (*a).value.arr);
}

pub unsafe extern "C" fn zim_MongoDB_Driver_ReadConcern_getLevel(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        *rv = obj::data::<ReadConcern>(call::this(ex)).level.as_ref().map_or_else(zval::null, |l| zval::string(l));
    }
}

pub unsafe extern "C" fn zim_MongoDB_Driver_ReadConcern_isDefault(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        *rv = zval::bool(obj::data::<ReadConcern>(call::this(ex)).is_default());
    }
}

pub unsafe extern "C" fn zim_MongoDB_Driver_ReadConcern_bsonSerialize(ex: Ex, rv: *mut sys::zval) {
    if !call::none(ex) {
        return;
    }
    let o = call::this(ex);
    let ht = hash::new(1);
    let props = ((*(*o).handlers).get_properties.unwrap())(o);
    let mut buf = String::new();
    hash::each(props, true, |k, v| {
        if let hash::Key::Str(_) = k {
            if !zval::is(v, sys::IS_NULL) {
                hash::str_update(ht, hash::key_bytes(k, &mut buf), zval::copy(v));
            }
        }
        true
    });
    let mut z = zval::arr(ht);
    sys::convert_to_object(&mut z);
    *rv = z;
}
