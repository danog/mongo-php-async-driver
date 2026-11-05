//! MongoDB\Driver\ServerApi (src/MongoDB/ServerApi.c over mongoc_server_api_t) and the
//! "serverApi" driver option of the Manager (phongo_manager_set_serverapi_opts).

use crate::bson::{throw, E};
use crate::zend::call::{self, Ex};
use crate::zend::{class, hash, obj, sys, zstr, zval};
use std::ptr::{addr_of_mut, null_mut};

/// mongoc_server_api_t
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Api {
    /// mongoc_server_api_version_to_string (only "1" exists)
    pub version: String,
    pub strict: Option<bool>,
    pub deprecation_errors: Option<bool>,
}

#[derive(Default)]
pub struct ServerApi {
    /// None until initialised (phongo: NULL server_api)
    pub api: Option<Api>,
    /// intern->properties: the get_properties table, kept with the object
    props: crate::bson::types::Props,
}

static mut H_SERVERAPI: sys::zend_object_handlers = unsafe { std::mem::zeroed() };
pub static mut CE: *mut sys::zend_class_entry = null_mut();

unsafe extern "C" fn create(c: *mut sys::zend_class_entry) -> *mut sys::zend_object {
    obj::alloc(c, ServerApi::default(), addr_of_mut!(H_SERVERAPI))
}

/// phongo_serverapi_get_properties_hash: into `ht` (a fresh table, or the object's own one).
unsafe fn fill(api: Option<&Api>, ht: *mut sys::HashTable, include_null: bool) {
    // phongo dereferences a NULL server_api here (an uninitialised object): we leave the table empty
    let Some(a) = api else { return };
    hash::str_update(ht, b"version", zval::string(a.version.as_bytes()));
    for (k, v) in [(&b"strict"[..], a.strict), (b"deprecationErrors", a.deprecation_errors)] {
        if include_null || v.is_some() {
            hash::str_update(ht, k, v.map_or_else(zval::null, zval::bool));
        }
    }
}

unsafe fn temp_props(o: *mut sys::zend_object, include_null: bool) -> *mut sys::HashTable {
    let ht = hash::new(3);
    fill(obj::data::<ServerApi>(o).api.as_ref(), ht, include_null);
    ht
}

unsafe extern "C" fn get_properties(o: *mut sys::zend_object) -> *mut sys::HashTable {
    let s = obj::data::<ServerApi>(o);
    let ht = s.props.get_or_init(3);
    fill(s.api.as_ref(), ht, true);
    ht
}

unsafe extern "C" fn get_debug_info(o: *mut sys::zend_object, is_temp: *mut i32) -> *mut sys::HashTable {
    *is_temp = 1;
    temp_props(o, true)
}

pub unsafe fn init_MongoDB_Driver_ServerApi(c: *mut sys::zend_class_entry) {
    CE = c;
    (*c).__bindgen_anon_2.create_object = Some(create);
    let h = obj::handlers::<ServerApi>(addr_of_mut!(H_SERVERAPI));
    h.get_properties = Some(get_properties);
    h.get_debug_info = Some(get_debug_info);
}

/// phongo_serverapi_create_libmongoc_object (false: an exception was thrown)
unsafe fn create_api(s: &mut ServerApi, version: &[u8], strict: Option<bool>, deprecation_errors: Option<bool>) -> bool {
    // mongoc_server_api_version_from_string compares C strings
    let version = zstr::c_part(version);
    if version != b"1" {
        throw(E::InvalidArgument, format!("Server API version \"{}\" is not supported in this driver version", String::from_utf8_lossy(version)));
        return false;
    }
    if s.api.is_some() {
        throw(E::Logic, "Server API object already initialized. Please file a bug report as this should not happen.");
        return false;
    }
    s.api = Some(Api { version: "1".into(), strict, deprecation_errors });
    true
}

/// phongo_serverapi_init_from_hash
unsafe fn init_from_hash(s: &mut ServerApi, props: *mut sys::HashTable) -> bool {
    let name = class::name(CE);
    let version = match hash::find(props, b"version") {
        Some(v) if zval::is(v, sys::IS_STRING) => zval::str_bytes(v).to_vec(),
        _ => {
            throw(E::InvalidArgument, format!("{name} initialization requires \"version\" field to be string"));
            return false;
        }
    };
    let flag = |key: &[u8], label: &str| -> Result<Option<bool>, ()> {
        match hash::find(props, key) {
            None => Ok(None),
            Some(z) if zval::is(z, sys::IS_NULL) => Ok(None),
            Some(z) if zval::is(z, sys::IS_TRUE) => Ok(Some(true)),
            Some(z) if zval::is(z, sys::IS_FALSE) => Ok(Some(false)),
            Some(_) => {
                throw(E::InvalidArgument, format!("{name} initialization requires \"{label}\" field to be bool or null"));
                Err(())
            }
        }
    };
    let Ok(strict) = flag(b"strict", "strict") else { return false };
    let Ok(deprecation_errors) = flag(b"deprecationErrors", "deprecationErrors") else { return false };
    create_api(s, &version, strict, deprecation_errors)
}

pub unsafe extern "C" fn zim_MongoDB_Driver_ServerApi___construct(ex: Ex, _rv: *mut sys::zval) {
    let mut version: *mut sys::zend_string = null_mut();
    let (mut strict, mut strict_null, mut dep, mut dep_null) = (false, true, false, true);
    if sys::zend_parse_parameters(call::num_args(ex), c"S|b!b!".as_ptr(), &mut version, &mut strict, &mut strict_null, &mut dep, &mut dep_null) != 0 {
        return;
    }
    let s = obj::data::<ServerApi>(call::this(ex));
    create_api(s, zstr::bytes(version), (!strict_null).then_some(strict), (!dep_null).then_some(dep));
}

pub unsafe extern "C" fn zim_MongoDB_Driver_ServerApi___set_state(ex: Ex, rv: *mut sys::zval) {
    let mut a: *mut sys::zval = null_mut();
    if sys::zend_parse_parameters(call::num_args(ex), c"a".as_ptr(), &mut a) != 0 {
        return;
    }
    let z = obj::new_zval(CE);
    init_from_hash(obj::data::<ServerApi>(z.value.obj), (*a).value.arr);
    zval::write(rv, z);
}

pub unsafe extern "C" fn zim_MongoDB_Driver_ServerApi_bsonSerialize(ex: Ex, rv: *mut sys::zval) {
    if !call::none(ex) {
        return;
    }
    let mut z = zval::arr(temp_props(call::this(ex), false));
    sys::convert_to_object(&mut z);
    zval::write(rv, z);
}

pub unsafe extern "C" fn zim_MongoDB_Driver_ServerApi___serialize(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        zval::write(rv, zval::arr(temp_props(call::this(ex), true)));
    }
}

pub unsafe extern "C" fn zim_MongoDB_Driver_ServerApi___unserialize(ex: Ex, _rv: *mut sys::zval) {
    let mut a: *mut sys::zval = null_mut();
    if sys::zend_parse_parameters(call::num_args(ex), c"a".as_ptr(), &mut a) != 0 {
        return;
    }
    init_from_hash(obj::data::<ServerApi>(call::this(ex)), (*a).value.arr);
}

/// The "serverApi" driver option: Ok(None) when absent, Err(the InvalidArgumentException message).
pub unsafe fn from_driver_options(driver_options: *mut sys::zval) -> Result<Option<Api>, String> {
    if driver_options.is_null() {
        return Ok(None);
    }
    let Some(z) = hash::find((*driver_options).value.arr, b"serverApi") else { return Ok(None) };
    let z = zval::deref(z);
    if !zval::is(z, sys::IS_OBJECT) || !obj::instanceof(zval::obj_ce(z), CE) {
        return Err(format!("Expected \"serverApi\" driver option to be {}, {} given", class::name(CE), zval::type_name(z)));
    }
    Ok(obj::data::<ServerApi>((*z).value.obj).api.clone())
}
