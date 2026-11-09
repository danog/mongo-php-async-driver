//! Logging (phongo_log.c, phongo_ini.c's OnUpdateDebug, src/MongoDB/Monitoring/functions.c):
//! the `mongodb.debug` stream, the LogSubscriber registry and `Monitoring\mongoc_log()`.
//!
//! Messages are written to the stream at once. The LogSubscriber instances get them through a
//! queue that is drained whenever no client is borrowed (the end of every driver entry point
//! that may log, next to the APM flush): a subscriber may call into the driver, which the
//! reference allows while libmongoc is inside an operation.

use crate::bson::{throw, E};
use crate::zend::call::{self, Ex};
use crate::zend::{obj, sys, zstr, zval};
use mongo_core::log::Level;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::ffi::{c_char, c_int};
use std::io::Write;
use std::ptr::null_mut;

extern "C" {
    // main/php_open_temporary_file.h (PHPAPI)
    fn php_open_temporary_fd(dir: *const c_char, pfx: *const c_char, opened_path_p: *mut *mut sys::zend_string) -> c_int;
}

enum Stream {
    Stderr,
    Stdout,
    File(std::fs::File),
}

/// Messages queued for the subscribers at most (a flush point is reached after every
/// operation; the bound only matters if one operation logs without end).
const MAX_PENDING: usize = 10_000;

thread_local! {
    /// MONGODB_G(debug_fd)
    static STREAM: RefCell<Option<Stream>> = const { RefCell::new(None) };
    /// MONGODB_G(loggers): (object handle, the object), in registration order
    /// (handle, the object, whether its log() takes the trailing hrtime argument)
    static LOGGERS: RefCell<Vec<(u32, sys::zval, bool)>> = const { RefCell::new(Vec::new()) };
    /// (level, domain, message, hrtime(true) when the message was produced)
    static PENDING: RefCell<VecDeque<(Level, Vec<u8>, Vec<u8>, u64)>> = const { RefCell::new(VecDeque::new()) };
    /// PENDING is not empty (the per-operation flush is one flag test otherwise)
    static HAS_PENDING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// MINIT: installs the handler (logging stays off until a stream or a subscriber appears).
pub fn minit() {
    mongo_core::log::set_handler(handler);
    sync();
}

/// phongo_log_sync_handler: TRACE is only produced for a stream.
fn sync() {
    let stream = STREAM.with(|s| s.borrow().is_some());
    let loggers = LOGGERS.with(|l| !l.borrow().is_empty());
    mongo_core::log::set_max_level(if stream {
        Some(Level::Trace)
    } else if loggers {
        Some(Level::Debug)
    } else {
        None
    });
}

/// phongo_log_handler (the core's messages)
fn handler(level: Level, domain: &str, message: &str) {
    emit(level, domain.as_bytes(), message.as_bytes());
}

/// phongo_log_handler (a message logged while the thread's state is being destroyed, when its
/// clients go at the thread's exit, is dropped)
fn emit(level: Level, domain: &[u8], message: &[u8]) {
    let _ = STREAM.try_with(|s| {
        if let Some(s) = &mut *s.borrow_mut() {
            write_stream(s, level, domain, message);
        }
    });
    // trace logs are only reported via streams
    if level >= Level::Trace || LOGGERS.try_with(|l| l.borrow().is_empty()).unwrap_or(true) {
        return;
    }
    let _ = PENDING.try_with(|p| {
        let mut p = p.borrow_mut();
        if p.len() < MAX_PENDING {
            p.push_back((level, domain.to_vec(), message.to_vec(), mongo_core::apm::hrtime()));
            HAS_PENDING.with(|h| h.set(true));
        }
    });
}

/// phongo_log_to_stream: "[%s.%06ld+00:00] %10s: %-8s> %s\n" with the UTC time.
fn write_stream(s: &mut Stream, level: Level, domain: &[u8], message: &[u8]) {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let mut line = format!("[{}.{:06}+00:00] {:>w$}", utc(now.as_secs() as i64), now.subsec_micros(), "", w = 10usize.saturating_sub(domain.len())).into_bytes();
    line.extend_from_slice(domain);
    line.extend_from_slice(format!(": {:<8}> ", level.as_str()).as_bytes());
    line.extend_from_slice(message);
    line.push(b'\n');
    let _ = match s {
        Stream::Stderr => std::io::stderr().write_all(&line),
        Stream::Stdout => {
            let mut o = std::io::stdout();
            o.write_all(&line).and_then(|()| o.flush())
        }
        Stream::File(f) => f.write_all(&line),
    };
}

/// php_format_date("Y-m-d\TH:i:s", t, 0)
fn utc(t: i64) -> String {
    let (days, secs) = (t.div_euclid(86400), t.rem_euclid(86400));
    // civil_from_days (H. Hinnant, "chrono-Compatible Low-Level Date Algorithms")
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)
}

/// Hands the queued messages to the subscribers (phongo_log_dispatch: method `log`, stops at
/// an exception). Messages logged meanwhile are delivered in the same pass.
pub unsafe fn flush() {
    if !HAS_PENDING.with(|h| h.get()) {
        return;
    }
    // the GC is freeing garbage: subscribers may be part of it (they get the messages later)
    if super::apm::is_quiet() {
        return;
    }
    loop {
        let Some((level, domain, message, at)) = PENDING.with(|p| p.borrow_mut().pop_front()) else {
            HAS_PENDING.with(|h| h.set(false));
            return;
        };
        let loggers: Vec<(sys::zval, bool)> = LOGGERS.with(|l| l.borrow().iter().map(|(_, z, t)| (zval::copy(z), *t)).collect());
        for (lg, takes_hrtime) in &loggers {
            if call::has_exception() {
                break;
            }
            if !obj::is_live(lg.value.obj) {
                continue;
            }
            let mut args = [zval::long(level as i64), zval::string(&domain), zval::string(&message), zval::long(at as i64)];
            let n = if *takes_hrtime { 4 } else { 3 };
            drop(call::method_n(lg.value.obj, b"log", &mut args[..n]));
        }
        if call::has_exception() {
            PENDING.with(|p| p.borrow_mut().clear());
            HAS_PENDING.with(|h| h.set(false));
            return;
        }
    }
}

/// OnUpdateDebug: "", "0", "off", "no", "false" (any case) close the stream; "stderr"/"stdout";
/// "1", "on", "yes", "true": a new file in the system temporary directory; anything else: a new
/// file in that directory (php_open_temporary_fd: falls back to the temporary directory).
pub unsafe fn set_debug(v: Option<&[u8]>) {
    let new = match v {
        None => None,
        Some(v) => {
            let l = v.to_ascii_lowercase();
            match &l[..] {
                b"" | b"0" | b"off" | b"no" | b"false" => None,
                b"stderr" => Some(Stream::Stderr),
                b"stdout" => Some(Stream::Stdout),
                b"1" | b"on" | b"yes" | b"true" => tmp_file(None),
                _ => tmp_file(Some(v)),
            }
        }
    };
    // the previous stream (a file) is closed when replaced
    STREAM.with(|s| *s.borrow_mut() = new);
    sync();
}

/// phongo_ini_tmp_file: "PHONGO-<time>" prefix, opened for appending.
unsafe fn tmp_file(dir: Option<&[u8]>) -> Option<Stream> {
    let dir = dir.and_then(|d| std::ffi::CString::new(d).ok());
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let prefix = std::ffi::CString::new(format!("PHONGO-{now}")).ok()?;
    let mut name: *mut sys::zend_string = null_mut();
    let fd = php_open_temporary_fd(dir.as_ref().map_or(std::ptr::null(), |d| d.as_ptr()), prefix.as_ptr(), &mut name);
    if fd != -1 {
        libc::close(fd);
    }
    if name.is_null() {
        return None;
    }
    let path = std::ffi::OsStr::from_encoded_bytes_unchecked(zstr::bytes(name)).to_owned();
    zstr::release(name);
    std::fs::OpenOptions::new().append(true).create(true).open(path).ok().map(Stream::File)
}

unsafe fn logsubscriber_ce() -> *mut sys::zend_class_entry {
    super::lookup(c"mongodb\\driver\\monitoring\\logsubscriber")
}

pub unsafe fn is_logger(z: *const sys::zval) -> bool {
    obj::instanceof(zval::obj_ce(z), logsubscriber_ce())
}

/// phongo_log_add_logger: a NOP when already registered.
pub unsafe fn add(z: *mut sys::zval) {
    let h = (*(*z).value.obj).handle;
    LOGGERS.with(|l| {
        let mut l = l.borrow_mut();
        if !l.iter().any(|(x, _, _)| *x == h) {
            l.push((h, zval::copy(z), super::apm::takes_hrtime(zval::obj_ce(z), b"log", 3)));
        }
    });
    sync();
}

/// phongo_log_remove_logger: a NOP when not registered.
pub unsafe fn remove(z: *mut sys::zval) {
    let h = (*(*z).value.obj).handle;
    let taken = LOGGERS.with(|l| {
        let mut l = l.borrow_mut();
        l.iter().position(|(x, _, _)| *x == h).map(|i| l.remove(i).1)
    });
    sync();
    // the logger's destructor may run here: no registry borrow held
    drop(taken);
}

/// RSHUTDOWN: the loggers are released with the request.
pub unsafe fn rshutdown() {
    PENDING.with(|p| p.borrow_mut().clear());
    HAS_PENDING.with(|h| h.set(false));
    let loggers = LOGGERS.with(|l| std::mem::take(&mut *l.borrow_mut()));
    sync();
    drop(loggers);
}

/// MSHUTDOWN (GSHUTDOWN): phongo_log_set_stream(NULL)
pub fn mshutdown() {
    STREAM.with(|s| *s.borrow_mut() = None);
    sync();
}

/// MongoDB\Driver\Monitoring\mongoc_log(int $level, string $domain, string $message): void
pub unsafe extern "C" fn zif_MongoDB_Driver_Monitoring_mongoc_log(ex: Ex, _rv: *mut sys::zval) {
    let mut level: i64 = 0;
    let (mut d, mut dl, mut m, mut ml): (*const u8, usize, *const u8, usize) = (null_mut(), 0, null_mut(), 0);
    if sys::zend_parse_parameters(call::num_args(ex), c"lss".as_ptr(), &mut level, &mut d, &mut dl, &mut m, &mut ml) != 0 {
        return;
    }
    let Some(level) = Level::from_i64(level) else {
        throw(E::InvalidArgument, format!("Expected level to be >= 0 and <= 6, {level} given"));
        return;
    };
    let domain = std::slice::from_raw_parts(d, dl);
    let message = std::slice::from_raw_parts(m, ml);
    if let Some(i) = domain.iter().position(|&b| b == 0) {
        throw(E::InvalidArgument, format!("Domain cannot contain null bytes. Unexpected null byte after \"{}\".", String::from_utf8_lossy(&domain[..i])));
        return;
    }
    if let Some(i) = message.iter().position(|&b| b == 0) {
        throw(E::InvalidArgument, format!("Message cannot contain null bytes. Unexpected null byte after \"{}\".", String::from_utf8_lossy(&message[..i])));
        return;
    }
    // mongoc_log(level, domain, "%s", message): the bytes as given
    if mongo_core::log::enabled(level) {
        emit(level, domain, message);
    }
    flush();
}
