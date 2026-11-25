//! Client-side field level encryption and Queryable Encryption: a pure-Rust port of libmongocrypt 1.21.1
//! (Apache-2.0, see NOTICE). No C, no OpenSSL: RustCrypto primitives.

pub mod autoenc;
pub mod broker;
pub mod bson;
pub mod crypto;
pub mod dec128;
pub mod engine;
pub mod fle2;
pub mod key;
pub mod kmip;
pub mod kms;
pub mod opts;
pub mod range;
pub mod text;
#[cfg(feature = "sspl-text-fold")]
pub mod unicode;
