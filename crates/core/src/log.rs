//! libmongoc's log API (mongoc-log.c): levels, per-file domains and the one process-wide handler.
//!
//! The extension installs the handler (phongo_log_handler: the `mongodb.debug` stream and the
//! LogSubscriber instances) and says which levels anyone listens to, so a message is only
//! formatted when it will be used. Without a handler (unit tests, the CLI tools) nothing is logged.
//!
//! libmongoc's TRACE lines (function entry/exit and hex dumps of every wire message) are not
//! produced: they name libmongoc's C functions, and the dumps contain credentials and documents.
//! Only explicit TRACE messages (`MongoDB\Driver\Monitoring\mongoc_log()`) reach the stream.

use std::sync::atomic::{AtomicPtr, AtomicU8, Ordering};

/// mongoc_log_level_t
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Level {
    Error = 0,
    Critical = 1,
    Warning = 2,
    Message = 3,
    Info = 4,
    Debug = 5,
    Trace = 6,
}

impl Level {
    pub fn from_i64(l: i64) -> Option<Level> {
        Some(match l {
            0 => Level::Error,
            1 => Level::Critical,
            2 => Level::Warning,
            3 => Level::Message,
            4 => Level::Info,
            5 => Level::Debug,
            6 => Level::Trace,
            _ => return None,
        })
    }

    /// mongoc_log_level_str
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Error => "ERROR",
            Level::Critical => "CRITICAL",
            Level::Warning => "WARNING",
            Level::Message => "MESSAGE",
            Level::Info => "INFO",
            Level::Debug => "DEBUG",
            Level::Trace => "TRACE",
        }
    }
}

/// The handler: (level, domain, message).
pub type Handler = fn(Level, &str, &str);

static HANDLER: AtomicPtr<()> = AtomicPtr::new(std::ptr::null_mut());
/// The most verbose level anyone listens to, plus one (0: nobody listens).
static MAX: AtomicU8 = AtomicU8::new(0);

/// Installs the handler (once, at module startup).
pub fn set_handler(h: Handler) {
    HANDLER.store(h as *mut (), Ordering::Relaxed);
}

/// Sets the most verbose level that is listened to (None: logging is off).
pub fn set_max_level(l: Option<Level>) {
    MAX.store(l.map_or(0, |l| l as u8 + 1), Ordering::Relaxed);
}

pub fn enabled(l: Level) -> bool {
    (l as u8) < MAX.load(Ordering::Relaxed)
}

/// mongoc_log: `msg` is only built when the level is listened to.
pub fn log(l: Level, domain: &str, msg: impl FnOnce() -> String) {
    if !enabled(l) {
        return;
    }
    let p = HANDLER.load(Ordering::Relaxed);
    if p.is_null() {
        return;
    }
    // SAFETY: only `set_handler` stores into HANDLER, always a `Handler`
    let h: Handler = unsafe { std::mem::transmute::<*mut (), Handler>(p) };
    h(l, domain, &msg());
}

pub fn error(domain: &str, msg: impl FnOnce() -> String) {
    log(Level::Error, domain, msg)
}

pub fn warning(domain: &str, msg: impl FnOnce() -> String) {
    log(Level::Warning, domain, msg)
}

pub fn info(domain: &str, msg: impl FnOnce() -> String) {
    log(Level::Info, domain, msg)
}

pub fn debug(domain: &str, msg: impl FnOnce() -> String) {
    log(Level::Debug, domain, msg)
}

/// The default MONGOC_LOG_DOMAIN and those of the libmongoc files whose messages are ported.
pub const MONGOC: &str = "mongoc";
pub const CLIENT: &str = "client";
pub const CLUSTER: &str = "cluster";
pub const TOPOLOGY_SCANNER: &str = "topology_scanner";

/// Names a value that holds secrets (phongo's client hash: the URI's SHA-256 and the option
/// values) in log lines: HMAC-SHA-256 with a random per-process key, in hex. Equal values give
/// equal fingerprints within a process; nothing about the value can be recovered or guessed
/// offline from it.
pub fn fingerprint(data: &[u8]) -> String {
    use hmac::{Hmac, KeyInit, Mac};
    static KEY: std::sync::OnceLock<Option<[u8; 32]>> = std::sync::OnceLock::new();
    let key = KEY.get_or_init(|| {
        let mut k = [0u8; 32];
        getrandom::fill(&mut k).ok().map(|()| k)
    });
    // without a key nothing is shown
    let Some(key) = key else { return "******".to_owned() };
    let mut m = <Hmac<sha2::Sha256> as KeyInit>::new_from_slice(key).expect("HMAC takes any key size");
    m.update(data);
    m.finalize().into_bytes().iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn fingerprint_is_keyed_and_stable() {
        let a = super::fingerprint(b"a:4:{s:3:\"pid\";i:1;}");
        assert_eq!(a, super::fingerprint(b"a:4:{s:3:\"pid\";i:1;}"));
        assert_ne!(a, super::fingerprint(b"a:4:{s:3:\"pid\";i:2;}"));
        assert_eq!(a.len(), 64);
        // not the plain SHA-256 of the value
        let plain: String = crate::auth::scram::sha256(b"a:4:{s:3:\"pid\";i:1;}").iter().map(|b| format!("{b:02x}")).collect();
        assert_ne!(a, plain);
    }
}
