//! libmongoc's log API (mongoc-log.c): levels, per-file domains and the one process-wide handler.
//!
//! The extension installs the handler (phongo_log_handler: the `mongodb.debug` stream and the
//! LogSubscriber instances) and says which levels anyone listens to, so a message is only
//! formatted when it will be used. Without a handler (unit tests, the CLI tools) nothing is logged.
//!
//! libmongoc's TRACE lines (function entry/exit and hex dumps of every wire message) are not
//! produced: they name libmongoc's C functions, and the dumps contain credentials and documents.
//! Only explicit TRACE messages (`MongoDB\Driver\Monitoring\mongoc_log()`) reach the stream.

use std::sync::OnceLock;

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

static HANDLER: OnceLock<Handler> = OnceLock::new();
thread_local! {
    /// The most verbose level anyone on this thread listens to, plus one (0: nobody listens;
    /// a thread-safe PHP's threads each have their own listeners).
    static MAX: std::cell::Cell<u8> = const { std::cell::Cell::new(0) };
}

/// Installs the handler (once, at module startup).
pub fn set_handler(h: Handler) {
    // the extension installs the same handler at every startup: a later call changes nothing
    let _ = HANDLER.set(h);
}

/// Sets the most verbose level that is listened to (None: logging is off).
pub fn set_max_level(l: Option<Level>) {
    MAX.with(|m| m.set(l.map_or(0, |l| l as u8 + 1)));
}

pub fn enabled(l: Level) -> bool {
    (l as u8) < MAX.with(|m| m.get())
}

/// mongoc_log: `msg` is only built when the level is listened to.
pub fn log(l: Level, domain: &str, msg: impl FnOnce() -> String) {
    if !enabled(l) {
        return;
    }
    let Some(h) = HANDLER.get() else { return };
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
