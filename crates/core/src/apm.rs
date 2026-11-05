//! Command monitoring (the APM events of mongoc-apm.c, raised by
//! mongoc_cluster_run_command_monitored). Events are queued on the client and handed to the
//! subscribers by the caller once the operation has released the client.

use crate::error::Error;
use std::sync::atomic::{AtomicUsize, Ordering};

/// The number of registered subscribers (events are only built while some exist).
static SUBSCRIBERS: AtomicUsize = AtomicUsize::new(0);

pub fn set_subscriber_count(n: usize) {
    SUBSCRIBERS.store(n, Ordering::Relaxed);
}

pub fn enabled() -> bool {
    SUBSCRIBERS.load(Ordering::Relaxed) > 0
}

/// What every command event carries (mongoc_apm_command_*_t).
#[derive(Clone, Debug)]
pub struct CommandInfo {
    pub command_name: String,
    pub database_name: String,
    pub request_id: i64,
    pub operation_id: i64,
    pub host: String,
    pub port: u16,
    pub server_id: u32,
    /// None for a zero service id
    pub service_id: Option<[u8; 12]>,
    /// -1: none
    pub server_connection_id: i64,
}

#[derive(Clone, Debug)]
pub enum Event {
    CommandStarted { info: CommandInfo, command: Vec<u8> },
    CommandSucceeded { info: CommandInfo, duration_us: i64, reply: Vec<u8> },
    CommandFailed { info: CommandInfo, duration_us: i64, error: Error, reply: Vec<u8> },
}

/// _mongoc_apm_is_sensitive_command_name / _mongoc_apm_is_sensitive_hello_message
pub fn is_sensitive(name: &str, body: &[u8]) -> bool {
    const NAMES: &[&str] = &["authenticate", "saslStart", "saslContinue", "getnonce", "createUser", "updateUser", "copydbgetnonce", "copydbsaslstart", "copydb"];
    if NAMES.iter().any(|n| n.eq_ignore_ascii_case(name)) {
        return true;
    }
    if !(name.eq_ignore_ascii_case("hello") || name.eq_ignore_ascii_case("isMaster")) {
        return false;
    }
    crate::bson::iter::is_empty(body) || crate::bson::iter::Iter::new(body).is_some_and(|mut it| it.find(b"speculativeAuthenticate"))
}
