//! The PHP-independent parts of the driver: everything here is plain Rust, unit-tested with
//! `cargo test -p mongo-core` (including the ports of the specification test runners).

/// The ext-mongodb version this extension is a drop-in replacement for.
pub const EXT_VERSION: &str = "2.5.8";
/// The ext-mongodb stability string.
pub const EXT_STABILITY: &str = "stable";
/// The libbson/libmongoc version whose behaviour the ported code follows (bundled in ext-mongodb 2.5.8).
pub const LIBMONGOC_VERSION: &str = "2.5.9";
/// The libmongocrypt version whose behaviour the ported code follows (bundled in ext-mongodb 2.5.8).
pub const LIBMONGOCRYPT_VERSION: &str = "1.21.1";

pub mod aio;
pub mod apm;
pub mod auth;
pub mod autocrypt;
pub mod bson;
pub mod bulk;
pub mod bulkwrite;
pub mod client;
pub mod cloud_creds;
pub mod cmd;
pub mod cmdparts;
pub mod compress;
pub mod concern;
pub mod dns;
pub mod error;
pub mod handshake;
pub mod http;
pub mod kms_tls;
pub mod listcoll;
pub mod log;
pub mod net;
pub mod ocsp;
pub mod os;
pub mod scan;
pub mod sdam;
pub mod session;
pub mod srv;
pub mod tls;
pub mod tls_key;
pub mod topology;
pub mod uri;
pub mod wire;
