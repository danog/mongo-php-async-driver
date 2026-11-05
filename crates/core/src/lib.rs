//! The PHP-independent parts of the driver: everything here is plain Rust, unit-tested with
//! `cargo test -p mongo-core` (including the ports of the specification test runners).

/// The ext-mongodb version this extension is a drop-in replacement for.
pub const EXT_VERSION: &str = "2.5.6";
/// The ext-mongodb stability string.
pub const EXT_STABILITY: &str = "stable";
/// The libbson/libmongoc version whose behaviour the ported code follows (bundled in ext-mongodb 2.5.6).
pub const LIBMONGOC_VERSION: &str = "2.5.8";
/// The libmongocrypt version whose behaviour the ported code follows (bundled in ext-mongodb 2.5.6).
pub const LIBMONGOCRYPT_VERSION: &str = "1.21.1";
