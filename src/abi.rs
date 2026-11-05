//! The only coupling to ext-mongodb internals: its BSON encoder (PHP value -> BSON, as
//! MongoDB\BSON\Document::fromPHP) and decoder (BSON -> PHP array, as a Cursor with
//! typeMap root/document/array = array). Both are exported symbols of mongodb.so, resolved with
//! dlsym (PHP loads extensions RTLD_GLOBAL).
//!
//! Two layouts, picked by the loaded ext-mongodb version (LP64):
//! - 2.1.x: `php_phongo_bson_state`, `php_phongo_bson_data_to_zval_ex(ptr, int, state)`,
//!   `php_phongo_zval_to_bson`;
//! - 2.5.x: `phongo_bson_state` (144 bytes, type map nested, skip_odm/nesting_depth),
//!   `phongo_bson_data_to_zval_ex(ptr, size_t, state)`, `phongo_zval_to_bson`.
//! Check the mirror against each version before adding it to `SUPPORTED`.

use crate::error::{Error, R};
use ext_php_rs::types::Zval;
use std::ffi::{c_char, c_int, c_void};
use std::sync::OnceLock;

/// ext-mongodb versions whose php_phongo_bson_state layout matches `BsonState`.
const SUPPORTED: &[&str] = &["2.1.", "2.5."];

#[repr(C)]
struct TypemapElement {
    ty: c_int,
    ce: *mut c_void,
}

/// ext-mongodb 2.5.x phongo_bson_state.
#[repr(C)]
struct FieldPaths25 {
    map: *mut c_void,
    allocated_size: usize,
    size: usize,
}

#[repr(C)]
struct Typemap25 {
    document: TypemapElement,
    array: TypemapElement,
    root: TypemapElement,
    int64_as_object: bool,
    field_paths: FieldPaths25,
}

#[repr(C)]
struct BsonState25 {
    zchild: Zval,
    map: Typemap25,
    odm_ce: *mut c_void,
    is_visiting_array: bool,
    skip_odm: bool,
    field_path: *mut c_void,
    nesting_depth: usize,
    field_type: TypemapElement,
}

const _: () = assert!(std::mem::size_of::<BsonState25>() == 144);
const _: () = assert!(std::mem::offset_of!(BsonState25, odm_ce) == 96);
const _: () = assert!(std::mem::offset_of!(BsonState25, field_type) == 128);

/// ext-mongodb 2.1.x php_phongo_bson_state.
#[repr(C)]
struct BsonState {
    zchild: Zval,
    document: TypemapElement,
    array: TypemapElement,
    root: TypemapElement,
    int64_as_object: bool,
    fp_map: *mut c_void,
    fp_allocated: usize,
    fp_size: usize,
    odm_ce: *mut c_void,
    is_visiting_array: bool,
    field_path: *mut c_void,
    field_type: TypemapElement,
}

/// libbson's bson_t: 128 bytes, 128-aligned (BSON_EXTRA_ALIGN).
#[repr(C, align(128))]
struct BsonT {
    flags: u32,
    len: u32,
    padding: [u8; 120],
}

const PHONGO_TYPEMAP_NATIVE_ARRAY: c_int = 1;

struct Api {
    v25: bool,
    /// (ptr, len as c_int for 2.1 / size_t for 2.5, state)
    decode: *mut c_void,
    encode: unsafe extern "C" fn(*mut Zval, c_int, *mut BsonT, *mut *mut BsonT),
    bson_init: unsafe extern "C" fn(*mut BsonT),
    bson_destroy: unsafe extern "C" fn(*mut BsonT),
    bson_get_data: unsafe extern "C" fn(*const BsonT) -> *const u8,
}

extern "C" {
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    fn zend_get_module_version(name: *const c_char) -> *const c_char;
}

fn api() -> R<&'static Api> {
    struct SyncApi(Result<Api, String>);
    unsafe impl Sync for SyncApi {}
    unsafe impl Send for SyncApi {}
    static API: OnceLock<SyncApi> = OnceLock::new();
    API.get_or_init(|| SyncApi((|| unsafe {
        let v = zend_get_module_version(c"mongodb".as_ptr());
        if v.is_null() {
            return Err("ext-mongodb is not loaded".into());
        }
        let v = std::ffi::CStr::from_ptr(v).to_string_lossy();
        if !SUPPORTED.iter().any(|p| v.starts_with(p)) {
            return Err(format!("mongo-php-async-driver: unsupported ext-mongodb {v} (php_phongo_bson_state layout unchecked)"));
        }
        let v25 = v.starts_with("2.5.");
        let sym = |n: &std::ffi::CStr| {
            let p = dlsym(std::ptr::null_mut(), n.as_ptr());
            if p.is_null() {
                Err(format!("ext-mongodb symbol {n:?} not found"))
            } else {
                Ok(p)
            }
        };
        Ok(Api {
            v25,
            decode: sym(if v25 { c"phongo_bson_data_to_zval_ex" } else { c"php_phongo_bson_data_to_zval_ex" })?,
            encode: std::mem::transmute(sym(if v25 { c"phongo_zval_to_bson" } else { c"php_phongo_zval_to_bson" })?),
            bson_init: std::mem::transmute(sym(c"bson_init")?),
            bson_destroy: std::mem::transmute(sym(c"bson_destroy")?),
            bson_get_data: std::mem::transmute(sym(c"bson_get_data")?),
        })
    })()))
    .0
    .as_ref()
    .map_err(|e| Error::Invalid(e.clone()))
}

/// Checks ext-mongodb once (call before any decode/encode).
pub fn check() -> R<()> {
    api().map(|_| ())
}

/// Decodes one BSON document into a PHP array. PHP thread only.
#[inline]
pub fn decode(bytes: &[u8]) -> R<Zval> {
    let api = api()?;
    let (ok, z) = if api.v25 {
        let mut st: BsonState25 = unsafe { std::mem::zeroed() };
        st.map.document.ty = PHONGO_TYPEMAP_NATIVE_ARRAY;
        st.map.array.ty = PHONGO_TYPEMAP_NATIVE_ARRAY;
        st.map.root.ty = PHONGO_TYPEMAP_NATIVE_ARRAY;
        let f: unsafe extern "C" fn(*const u8, usize, *mut BsonState25) -> bool = unsafe { std::mem::transmute(api.decode) };
        let ok = unsafe { f(bytes.as_ptr(), bytes.len(), &mut st) };
        (ok, std::mem::replace(&mut st.zchild, Zval::new()))
    } else {
        let mut st: BsonState = unsafe { std::mem::zeroed() };
        st.document.ty = PHONGO_TYPEMAP_NATIVE_ARRAY;
        st.array.ty = PHONGO_TYPEMAP_NATIVE_ARRAY;
        st.root.ty = PHONGO_TYPEMAP_NATIVE_ARRAY;
        let f: unsafe extern "C" fn(*const u8, c_int, *mut BsonState) -> bool = unsafe { std::mem::transmute(api.decode) };
        let ok = unsafe { f(bytes.as_ptr(), bytes.len() as c_int, &mut st) };
        (ok, std::mem::replace(&mut st.zchild, Zval::new()))
    };
    if !ok {
        return Err(Error::bson("BSON decoding failed"));
    }
    Ok(z)
}

/// Appends the BSON encoding of a PHP array/object (Document::fromPHP semantics) to `out`.
/// Returns false if ext-mongodb threw (the exception is pending).
pub fn encode_into(z: &Zval, out: &mut Vec<u8>) -> R<bool> {
    let api = api()?;
    unsafe {
        let mut b: BsonT = std::mem::zeroed();
        (api.bson_init)(&mut b);
        (api.encode)(z as *const Zval as *mut Zval, 0, &mut b, std::ptr::null_mut());
        let ok = !ext_php_rs::zend::ExecutorGlobals::has_exception();
        if ok {
            out.extend_from_slice(std::slice::from_raw_parts((api.bson_get_data)(&b), b.len as usize));
        }
        (api.bson_destroy)(&mut b);
        Ok(ok)
    }
}
