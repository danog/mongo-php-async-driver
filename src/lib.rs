//! mongodb.so: a pure-Rust, drop-in replacement for ext-mongodb.
//!
//! - `zend`: minimal bindings to the Zend Engine (pre-generated FFI declarations plus Rust ports of
//!   the engine's inline helpers and macros); no C is compiled.
//! - `module`: the module entry, lifecycle hooks, INI settings and phpinfo().
#![allow(clippy::missing_safety_doc)]

pub mod gen;
pub mod module;
pub mod zend;
pub mod bson;
pub mod driver;
