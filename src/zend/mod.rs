//! Zend Engine layer: the FFI declarations (`sys` = ext-php-rs's bindings) and Rust ports of the
//! inline helpers and macros of the PHP headers that the extension needs (zend_types.h,
//! zend_string.h, zend_hash.h, zend_API.h, ...). zvals follow ext-php-rs's ownership (see
//! `zval`). The engine's per-thread globals (EG, CG) are reached through [`eg`] and [`cg`]:
//! plain symbols on NTS builds, the calling thread's TSRM storage on ZTS builds.

pub mod call;
pub mod class;
pub mod hash;
mod layout;
pub mod obj;
pub mod sys;
pub mod zstr;
pub mod zval;

use std::ffi::{c_char, CStr};

/// The executor globals (EG) of the calling thread.
#[inline(always)]
pub fn eg() -> *mut sys::zend_executor_globals {
    // the address of the engine's globals (nothing is read here)
    #[cfg(not(php_zts))]
    return std::ptr::addr_of_mut!(sys::executor_globals);
    // the thread's TSRM block plus the globals' offset (TSRMG_FAST_BULK, in ext-php-rs's wrapper.c)
    #[cfg(php_zts)]
    return unsafe { sys::ext_php_rs_executor_globals() };
}

/// The compiler globals (CG) of the calling thread.
#[inline(always)]
pub fn cg() -> *mut sys::zend_compiler_globals {
    #[cfg(not(php_zts))]
    return std::ptr::addr_of_mut!(sys::compiler_globals);
    #[cfg(php_zts)]
    return unsafe { sys::ext_php_rs_compiler_globals() };
}

/// A `*const c_char` for a static C string literal.
#[inline]
pub const fn cstr(s: &'static CStr) -> *const c_char {
    s.as_ptr()
}

unsafe extern "C" {
    // variadic: not emitted usefully by bindgen
    pub fn php_info_print_table_header(num_cols: i32, ...);
    pub fn php_info_print_table_row(num_cols: i32, ...);
}

/// `php_info_print_table_row(2, k, v)` with Rust strings.
pub fn info_row(k: &CStr, v: &str) {
    let v = std::ffi::CString::new(v).unwrap();
    unsafe { php_info_print_table_row(2, k.as_ptr(), v.as_ptr()) };
}

/// The `zif_handler` of an `extern "C"` handler `$f`. PHP calls internal function handlers with
/// ZEND_FASTCALL, i.e. __vectorcall on Windows: there a vectorcall shim calls `$f`; elsewhere the
/// handler is `$f` itself.
#[cfg(windows)]
#[macro_export]
macro_rules! zh {
    ($f:path) => {{
        unsafe extern "vectorcall" fn shim(ex: *mut $crate::zend::sys::zend_execute_data, rv: *mut $crate::zend::sys::zval) {
            unsafe { $f(ex, rv) }
        }
        shim
    }};
}

/// See the Windows definition.
#[cfg(not(windows))]
#[macro_export]
macro_rules! zh {
    ($f:path) => {
        $f
    };
}

/// Static engine tables (arrays of structs holding raw pointers): only read by PHP, on its thread.
#[repr(transparent)]
pub struct Static<T>(pub T);
unsafe impl<T> Sync for Static<T> {}

/// Engine data PHP is handed a pointer to and that is written in place: the handler tables
/// (filled at MINIT), the module entry, the ZTS globals id (written by the engine).
#[repr(transparent)]
pub struct EngineCell<T>(std::cell::UnsafeCell<T>);

// SAFETY: written only during module startup (by the extension or by the engine), before any
// request runs on any thread; only read afterwards
unsafe impl<T> Sync for EngineCell<T> {}

impl<T> EngineCell<T> {
    pub const fn new(v: T) -> Self {
        EngineCell(std::cell::UnsafeCell::new(v))
    }

    /// The address PHP is given (and MINIT writes through).
    #[inline]
    pub const fn get(&self) -> *mut T {
        self.0.get()
    }
}

impl EngineCell<sys::zend_object_handlers> {
    /// An empty handler table, filled at MINIT (`obj::handlers`).
    pub const fn handlers() -> Self {
        // SAFETY: integers and Option<fn> pointers: all zero is valid (the offset 0, no handlers)
        EngineCell::new(unsafe { std::mem::zeroed() })
    }
}

/// A class entry, registered at MINIT and read from then on (a relaxed load: a plain load,
/// as from a `static mut`; the request threads of a ZTS build start after MINIT).
pub struct ClassEntry(std::sync::atomic::AtomicPtr<sys::zend_class_entry>);

impl ClassEntry {
    #[allow(clippy::new_without_default, reason = "used in statics: const")]
    pub const fn new() -> Self {
        ClassEntry(std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()))
    }

    #[inline]
    pub fn get(&self) -> *mut sys::zend_class_entry {
        self.0.load(std::sync::atomic::Ordering::Relaxed)
    }

    #[inline]
    pub fn set(&self, c: *mut sys::zend_class_entry) {
        self.0.store(c, std::sync::atomic::Ordering::Relaxed)
    }
}

/// Stub for an ext-mongodb method or function not ported yet: throws Error.
pub unsafe fn not_implemented(handler: &CStr) {
    sys::zend_throw_error(std::ptr::null_mut(), c"%s is not implemented yet by this mongodb extension".as_ptr(), handler.as_ptr());
}
