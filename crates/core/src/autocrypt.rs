//! The hooks of automatic client-side encryption: libmongoc's `_mongoc_cse_auto_encrypt` (before
//! a command is monitored and sent) and `_mongoc_cse_auto_decrypt` (for every successful reply),
//! called by `mongoc_cluster_run_command_monitored`. The implementation (mongo-crypt's) is
//! installed by the driver; a client without it pays one branch per command.

use crate::client::Client;
use crate::error::Error;

/// MONGOC_REDUCED_MAX_MSG_SIZE_FOR_FLE: the batch size limit of writes of a client with automatic
/// encryption (decryption-only included)
pub const REDUCED_MAX_MSG_SIZE_FOR_FLE: usize = 2_097_152;

pub trait AutoCrypt {
    /// The reply with every ciphertext replaced by its plaintext. `client` is the encrypted client
    /// itself (the key vault client when no other one was given).
    fn decrypt(&mut self, client: &mut Client, db: &str, reply: &[u8]) -> Result<Vec<u8>, Error>;

    /// Whether commands are encrypted (false with bypassAutoEncryption)
    fn encrypts(&self) -> bool {
        false
    }

    /// The command to send instead of `cmd` (None: `cmd` unchanged); `cmd` has its document
    /// sequences as arrays
    fn encrypt(&mut self, client: &mut Client, db: &str, cmd: &[u8]) -> Result<Option<Vec<u8>>, Error> {
        let _ = (client, db, cmd);
        Ok(None)
    }
}
