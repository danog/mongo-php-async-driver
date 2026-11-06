//! Command and SDAM monitoring (the APM events of mongoc-apm.c: command events raised by
//! mongoc_cluster_run_command_monitored, SDAM events by mongoc-topology-description-apm.c and
//! the topology scanner). Events are queued on the topology description and handed to the
//! subscribers by the caller once the operation has released the client.

use crate::error::Error;
use crate::sdam::ServerDescription;
use crate::topology::TopologyDescription;
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
    /// `addr`: the server's host_and_port
    ServerOpening { topology_id: [u8; 12], addr: String },
    ServerClosed { topology_id: [u8; 12], addr: String },
    ServerChanged { topology_id: [u8; 12], addr: String, previous: Box<ServerDescription>, new: Box<ServerDescription> },
    TopologyOpening { topology_id: [u8; 12] },
    TopologyChanged { topology_id: [u8; 12], previous: Box<TopologyDescription>, new: Box<TopologyDescription> },
    TopologyClosed { topology_id: [u8; 12] },
    /// the single-threaded scanner's heartbeats are never awaited
    HeartbeatStarted { addr: String },
    HeartbeatSucceeded { addr: String, duration_us: i64, reply: Vec<u8> },
    HeartbeatFailed { addr: String, duration_us: i64, error: Error },
}

/// An event and the time it was created: nanoseconds on the clock of PHP's hrtime(true).
#[derive(Clone, Debug)]
pub struct Timed {
    pub at: u64,
    pub event: Event,
}

/// The queue of events not yet handed to the subscribers. A copy of a description starts
/// with an empty queue (the descriptions carried by events never carry events).
#[derive(Debug, Default)]
pub struct Queue(pub Vec<Timed>);

impl Clone for Queue {
    fn clone(&self) -> Self {
        Queue(Vec::new())
    }
}

impl Queue {
    /// Queues `ev` (built by `f`) while some subscriber exists, stamped with the time.
    pub fn push(&mut self, f: impl FnOnce() -> Event) {
        if enabled() {
            let at = hrtime();
            self.0.push(Timed { at, event: f() });
        }
    }

    /// Queues `event` (the caller checked `enabled()`), stamped with the time.
    pub fn push_now(&mut self, event: Event) {
        self.0.push(Timed { at: hrtime(), event });
    }
}

/// The clock of PHP's hrtime(true) (zend_hrtime), chosen once by the extension for the PHP
/// version it is built for; the default is the platform's zend_hrtime of PHP 8.4.
static CLOCK: std::sync::OnceLock<fn() -> u64> = std::sync::OnceLock::new();

/// Sets the clock (once, at module startup).
pub fn set_clock(f: fn() -> u64) {
    let _ = CLOCK.set(f);
}

/// Now, in nanoseconds, on the clock of hrtime(true).
pub fn hrtime() -> u64 {
    CLOCK.get().map_or_else(clock_default, |f| f())
}

/// zend_hrtime on Linux and other POSIX systems: CLOCK_MONOTONIC.
#[cfg(not(target_vendor = "apple"))]
pub fn clock_default() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: a valid timespec out-parameter
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) } == 0 {
        (ts.tv_sec as u64).wrapping_mul(1_000_000_000).wrapping_add(ts.tv_nsec as u64)
    } else {
        0
    }
}

/// zend_hrtime of PHP 8.4 on macOS: mach_absolute_time() scaled by the timebase.
#[cfg(target_vendor = "apple")]
#[allow(deprecated)] // libc's mach bindings point to the mach2 crate; they are correct
pub fn clock_default() -> u64 {
    static TB: std::sync::OnceLock<(u64, u64)> = std::sync::OnceLock::new();
    let &(numer, denom) = TB.get_or_init(|| {
        let mut tb = libc::mach_timebase_info { numer: 0, denom: 0 };
        // SAFETY: a valid out-parameter
        unsafe { libc::mach_timebase_info(&mut tb) };
        if tb.denom == 0 { (1, 1) } else { (u64::from(tb.numer), u64::from(tb.denom)) }
    });
    // SAFETY: no arguments
    let t = unsafe { libc::mach_absolute_time() };
    t.wrapping_mul(numer) / denom
}

/// zend_hrtime of PHP 8.5 on macOS: clock_gettime_nsec_np(CLOCK_UPTIME_RAW).
#[cfg(target_vendor = "apple")]
pub fn clock_uptime_raw() -> u64 {
    extern "C" {
        fn clock_gettime_nsec_np(clock_id: libc::clockid_t) -> u64;
    }
    // SAFETY: a valid clock id
    unsafe { clock_gettime_nsec_np(libc::CLOCK_UPTIME_RAW) }
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
