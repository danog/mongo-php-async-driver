//! mongodb.so: a pure-Rust, drop-in replacement for ext-mongodb.
//!
//! - `zend`: the Zend Engine layer on top of ext-php-rs (its FFI bindings and owned `Zval`), plus
//!   Rust ports of the engine's inline helpers and macros the extension needs.
//! - `module`: the module entry, lifecycle hooks, INI settings and phpinfo().
#![allow(clippy::missing_safety_doc)]

pub mod gen;
pub mod module;
pub mod zend;
pub mod bson;
pub mod driver;
