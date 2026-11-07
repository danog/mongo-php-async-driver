//! mongodb.so: a pure-Rust, drop-in replacement for ext-mongodb.
//!
//! - `zend`: the Zend Engine layer on top of ext-php-rs (its FFI bindings and owned `Zval`), plus
//!   Rust ports of the engine's inline helpers and macros the extension needs.
//! - `module`: the module entry, lifecycle hooks, INI settings and phpinfo().
#![allow(clippy::missing_safety_doc)]
// PHP's ZEND_FASTCALL functions (internal function handlers among them) are __vectorcall on
// Windows; ext-php-rs needs the same (nightly-only) ABI there
#![cfg_attr(windows, feature(abi_vectorcall))]

pub mod gen;
pub mod module;
pub mod zend;
pub mod bson;
pub mod driver;
