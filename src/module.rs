//! The module entry: registered as `mongodb`, with ext-mongodb's version, dependencies, INI
//! settings and phpinfo() rows (phongo.c, phongo_ini.c).

use crate::zend::{self, sys};
use mongo_core::{EXT_STABILITY, EXT_VERSION, LIBMONGOCRYPT_VERSION, LIBMONGOC_VERSION};
use std::ffi::{c_char, c_int, c_void, CStr};
use std::ptr::{addr_of, addr_of_mut, null, null_mut};

const VERSION: &CStr = c"2.5.6";
const _: () = assert!(VERSION.to_bytes().len() == EXT_VERSION.len());

const MODULE_DEP_REQUIRED: u8 = 1;

const fn dep(name: &'static CStr) -> sys::zend_module_dep {
    sys::zend_module_dep { name: name.as_ptr(), rel: null(), version: null(), type_: MODULE_DEP_REQUIRED }
}

static DEPS: zend::Static<[sys::zend_module_dep; 5]> = zend::Static([
    dep(c"date"),
    dep(c"json"),
    dep(c"spl"),
    dep(c"standard"),
    sys::zend_module_dep { name: null(), rel: null(), version: null(), type_: 0 },
]);

unsafe extern "C" fn on_update_debug(
    _entry: *mut sys::zend_ini_entry,
    new_value: *mut sys::zend_string,
    _a1: *mut c_void,
    _a2: *mut c_void,
    _a3: *mut c_void,
    _stage: c_int,
) -> sys::zend_result {
    crate::driver::log::set_debug(if new_value.is_null() { None } else { Some(zend::zstr::bytes(new_value)) });
    sys::ZEND_RESULT_CODE_SUCCESS
}

const PHP_INI_ALL: u8 = 7;

static INI: zend::Static<[sys::zend_ini_entry_def; 2]> = zend::Static([
    sys::zend_ini_entry_def {
        name: c"mongodb.debug".as_ptr(),
        on_modify: Some(on_update_debug),
        mh_arg1: null_mut(),
        mh_arg2: null_mut(),
        mh_arg3: null_mut(),
        value: c"".as_ptr(),
        displayer: None,
        value_length: 0,
        name_length: 13,
        modifiable: PHP_INI_ALL,
    },
    sys::zend_ini_entry_def {
        name: null(),
        on_modify: None,
        mh_arg1: null_mut(),
        mh_arg2: null_mut(),
        mh_arg3: null_mut(),
        value: null(),
        displayer: None,
        value_length: 0,
        name_length: 0,
        modifiable: 0,
    },
]);

const CONST_PERSISTENT: i32 = 1;

unsafe extern "C" fn minit(_type: c_int, module_number: c_int) -> sys::zend_result {
    crate::driver::log::minit();
    crate::driver::apm::minit();
    sys::zend_register_ini_entries(INI.0.as_ptr(), module_number);
    crate::zend::obj::init_std();
    crate::gen::api::register_all();
    sys::zend_register_string_constant(c"MONGODB_VERSION".as_ptr(), 15, VERSION.as_ptr(), CONST_PERSISTENT, module_number);
    if !crate::driver::aio::minit(module_number) {
        // PHP reports "Unable to start mongodb module" and does not start
        return sys::ZEND_RESULT_CODE_FAILURE;
    }
    sys::zend_register_string_constant(c"MONGODB_STABILITY".as_ptr(), 17, c"stable".as_ptr(), CONST_PERSISTENT, module_number);
    sys::ZEND_RESULT_CODE_SUCCESS
}

unsafe extern "C" fn mshutdown(_type: c_int, module_number: c_int) -> sys::zend_result {
    // (ZTS: the other threads' clients went at their GSHUTDOWN)
    crate::driver::destroy_persistent_clients();
    crate::driver::log::mshutdown();
    crate::driver::aio::mshutdown();
    sys::zend_unregister_ini_entries(module_number);
    sys::ZEND_RESULT_CODE_SUCCESS
}

unsafe extern "C" fn rinit(_type: c_int, _module_number: c_int) -> sys::zend_result {
    crate::driver::aio::rinit();
    sys::ZEND_RESULT_CODE_SUCCESS
}

unsafe extern "C" fn rshutdown(_type: c_int, _module_number: c_int) -> sys::zend_result {
    crate::driver::log::rshutdown();
    crate::driver::apm::rshutdown();
    crate::driver::aio::rshutdown();
    sys::ZEND_RESULT_CODE_SUCCESS
}

unsafe extern "C" fn minfo(module: *mut sys::zend_module_entry) {
    sys::php_info_print_table_start();
    zend::php_info_print_table_header(2, c"MongoDB support".as_ptr(), c"enabled".as_ptr());
    zend::info_row(c"MongoDB extension version", EXT_VERSION);
    zend::info_row(c"MongoDB extension stability", EXT_STABILITY);
    zend::info_row(c"libbson bundled version", LIBMONGOC_VERSION);
    zend::info_row(c"libmongoc bundled version", LIBMONGOC_VERSION);
    zend::info_row(c"libmongoc SSL", "enabled");
    zend::info_row(c"libmongoc SSL library", "rustls");
    zend::info_row(c"libmongoc crypto", "enabled");
    zend::info_row(c"libmongoc crypto library", "RustCrypto");
    zend::info_row(c"libmongoc crypto system profile", "disabled");
    zend::info_row(c"libmongoc SASL", "enabled");
    zend::info_row(c"libmongoc SRV", "enabled");
    zend::info_row(c"libmongoc compression", "enabled");
    zend::info_row(c"libmongoc compression snappy", "enabled");
    zend::info_row(c"libmongoc compression zlib", "enabled");
    zend::info_row(c"libmongoc compression zstd", "enabled");
    zend::info_row(c"libmongocrypt bundled version", LIBMONGOCRYPT_VERSION);
    zend::info_row(c"libmongocrypt crypto", "enabled");
    zend::info_row(c"libmongocrypt crypto library", "RustCrypto");
    zend::info_row(c"crypt_shared library version", "unknown");
    sys::php_info_print_table_end();
    sys::display_ini_entries(module);
}

static mut MODULE: sys::zend_module_entry = sys::zend_module_entry {
    size: std::mem::size_of::<sys::zend_module_entry>() as u16,
    zend_api: const_parse(env!("PHP_MODULE_API")),
    zend_debug: 0,
    zts: cfg!(php_zts) as u8,
    ini_entry: null(),
    deps: addr_of!(DEPS.0) as *const sys::zend_module_dep,
    name: c"mongodb".as_ptr(),
    functions: null(),
    module_startup_func: Some(minit),
    module_shutdown_func: Some(mshutdown),
    request_startup_func: Some(rinit),
    request_shutdown_func: Some(rshutdown),
    info_func: Some(minfo),
    version: VERSION.as_ptr(),
    #[cfg(not(php_zts))]
    globals_size: 0,
    #[cfg(not(php_zts))]
    globals_ptr: null_mut(),
    #[cfg(not(php_zts))]
    globals_ctor: None,
    #[cfg(not(php_zts))]
    globals_dtor: None,
    #[cfg(php_zts)]
    globals_size: std::mem::size_of::<usize>(),
    #[cfg(php_zts)]
    globals_id_ptr: addr_of_mut!(GLOBALS_ID),
    #[cfg(php_zts)]
    globals_ctor: Some(gctor),
    #[cfg(php_zts)]
    globals_dtor: Some(gdtor),
    post_deactivate_func: None,
    module_started: 0,
    type_: 0,
    handle: null_mut(),
    module_number: 0,
    build_id: concat!(env!("PHP_BUILD_ID"), "\0").as_ptr() as *const c_char,
};

/// ZTS: the TSRM id of the module's per-thread globals, which hold nothing but the thread they
/// were made for: their destructor (GSHUTDOWN) destroys that thread's persistent clients
/// (phongo_pclient_destroy_all at GSHUTDOWN), as each thread has its own (thread_local).
#[cfg(php_zts)]
static mut GLOBALS_ID: c_int = 0;

/// A per-thread address: the identity of the calling thread while it lives.
#[cfg(php_zts)]
fn thread_mark() -> usize {
    thread_local! {
        static MARK: u8 = const { 0 };
    }
    MARK.with(|m| m as *const u8 as usize)
}

/// GINIT: the globals of a new thread (run on that thread).
#[cfg(php_zts)]
unsafe extern "C" fn gctor(g: *mut c_void) {
    *(g as *mut usize) = thread_mark();
}

/// GSHUTDOWN: the thread's end (ts_free_thread, on the thread), or module shutdown (ts_free_id,
/// on the main thread, for every thread still registered). Only the owning thread can reach its
/// clients; another thread's are dropped with that thread's storage (their sockets closed,
/// nothing sent: as phongo when a client's thread is gone).
#[cfg(php_zts)]
unsafe extern "C" fn gdtor(g: *mut c_void) {
    if *(g as *const usize) == thread_mark() {
        crate::driver::destroy_persistent_clients();
    }
}

const fn const_parse(s: &str) -> u32 {
    let b = s.as_bytes();
    let mut i = 0;
    let mut n = 0u32;
    while i < b.len() {
        n = n * 10 + (b[i] - b'0') as u32;
        i += 1;
    }
    n
}

/// The entry point PHP looks up when loading the extension.
#[no_mangle]
pub extern "C" fn get_module() -> *mut sys::zend_module_entry {
    unsafe { (*addr_of_mut!(MODULE)).functions = crate::gen::api::functions() };
    addr_of_mut!(MODULE)
}
