//! The hook of automatic client-side encryption: libmongoc's `_mongoc_cse_auto_decrypt`, called by
//! `mongoc_cluster_run_command_monitored` for every successful reply of a client with automatic
//! encryption. The implementation (mongo-crypt's) is installed by the driver; a client without it
//! pays one branch per command.

use crate::client::Client;
use crate::error::Error;

pub trait AutoCrypt {
    /// The reply with every ciphertext replaced by its plaintext. `client` is the encrypted client
    /// itself (the key vault client when no other one was given).
    fn decrypt(&mut self, client: &mut Client, db: &str, reply: &[u8]) -> Result<Vec<u8>, Error>;
}
