//! mongo-php-async-driver: MongoDB reads for PHP without threads.
//!
//! - Commands come in as PHP arrays (or raw BSON strings) and are encoded by ext-mongodb's own
//!   encoder; replies are decoded by ext-mongodb's own decoder straight from the socket buffer,
//!   so results are identical to an ext-mongodb Cursor with typeMap array (abi.rs).
//! - I/O runs on the PHP thread: blocking (sync, like libmongoc) or suspending the current fiber
//!   through Revolt (async), chosen per call; default: async inside a fiber, sync in {main}.
#![cfg_attr(windows, feature(abi_vectorcall))]

mod abi;
mod aio;
mod bson;
mod client;
mod error;

use client::{Io, Mode, Opts, Topo, TopoRef};
use error::{Error, R};
use ext_php_rs::{
    prelude::*,
    types::{ZendHashTable, Zval},
    zend::ExecutorGlobals,
};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

thread_local! {
    /// Topologies and pools, per process and URI: they stay warm across FPM requests.
    static TOPOS: RefCell<HashMap<String, TopoRef>> = RefCell::new(HashMap::new());
    static BODY: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

#[php_class]
#[php(name = "danog\\Mongo\\Client")]
pub struct PhpClient {
    topo: TopoRef,
}

/// The command body: raw BSON string, or a PHP array/object encoded by ext-mongodb.
/// Ok(false): ext-mongodb threw.
fn body(cmd: &Zval, out: &mut Vec<u8>) -> R<bool> {
    out.clear();
    if let Some(b) = cmd.binary_slice::<u8>() {
        if cmd.is_string() {
            bson::Doc::at(b)?;
            out.extend_from_slice(b);
            return Ok(true);
        }
    }
    if cmd.is_array() || cmd.is_object() {
        return abi::encode_into(cmd, out);
    }
    Err(Error::Invalid("command must be an array, an object or a BSON string".into()))
}

fn io_mode(r#async: Option<bool>) -> Io {
    let a = r#async.unwrap_or_else(|| !ExecutorGlobals::get().active_fiber.is_null());
    if a { Io::Async } else { Io::Sync }
}

fn mode(rp: Option<&str>) -> R<Option<Mode>> {
    rp.map(Mode::parse).transpose()
}

/// Runs `f` with the encoded body; maps "exception pending" to a null return.
fn with_body(cmd: &Zval, f: impl FnOnce(&[u8]) -> R<Zval>) -> PhpResult<Zval> {
    abi::check()?;
    let mut b = BODY.with_borrow_mut(std::mem::take);
    let r = match body(cmd, &mut b) {
        Ok(true) => f(&b),
        Ok(false) => Err(Error::Interrupted),
        Err(e) => Err(e),
    };
    BODY.with_borrow_mut(|x| *x = b);
    match r {
        Ok(z) => Ok(z),
        Err(Error::Interrupted) => Ok(Zval::new()),
        Err(e) => Err(e.into()),
    }
}

#[php_impl]
impl PhpClient {
    pub fn __construct(uri: &str, max_pool_size: Option<u32>) -> PhpResult<Self> {
        let key = format!("{uri}#{max_pool_size:?}");
        if let Some(t) = TOPOS.with_borrow(|m| m.get(&key).cloned()) {
            return Ok(Self { topo: t });
        }
        let t = Rc::new(RefCell::new(Topo::new(Opts::parse(uri, max_pool_size)?)));
        TOPOS.with_borrow_mut(|m| m.insert(key, t.clone()));
        Ok(Self { topo: t })
    }

    /// Runs a cursor command (find, aggregate, ...) and drains it: a list of PHP arrays.
    pub fn find(&self, db: &str, cmd: &Zval, read_preference: Option<&str>, r#async: Option<bool>) -> PhpResult<Zval> {
        let io = io_mode(r#async);
        let m = mode(read_preference)?;
        with_body(cmd, |b| {
            let mut ht = ZendHashTable::new();
            client::cursor(&self.topo, db, b, m, io, |batch| {
                for e in bson::Doc::at(batch)?.iter() {
                    let (_, v) = e?;
                    let bson::Val::Doc(d) = v else { return Err(Error::bson("batch element is not a document")) };
                    ht.push(abi::decode(d.0)?).map_err(|e| Error::bson(&e.to_string()))?;
                }
                Ok(())
            })?;
            let mut z = Zval::new();
            z.set_hashtable(ht);
            Ok(z)
        })
    }

    /// Runs a command: its reply decoded as a PHP array (server errors throw CommandException).
    pub fn command(&self, db: &str, cmd: &Zval, read_preference: Option<&str>, r#async: Option<bool>) -> PhpResult<Zval> {
        let io = io_mode(r#async);
        let m = mode(read_preference)?;
        with_body(cmd, |b| {
            let mut z = None;
            client::command(&self.topo, db, b, m, io, |reply| {
                z = Some(abi::decode(reply)?);
                Ok(())
            })?;
            Ok(z.unwrap())
        })
    }

    /// The stream to watch: Revolt\EventLoop::onReadable($stream, fn () => Client::wakeup()).
    /// A new stream per call (streams are per request).
    pub fn init() -> PhpResult<Zval> {
        Ok(aio::php_stream()?)
    }

    pub fn wakeup() {
        aio::wakeup()
    }

    pub fn pending() -> usize {
        client::pending()
    }

    /// Pool counters since the last reset (wait_ns = checkout time incl. connection creation).
    pub fn stats(reset: bool) -> PhpResult<Zval> {
        let mut ht = ZendHashTable::new();
        client::ST.with(|s| -> PhpResult<()> {
            for (k, c) in [
                ("checkouts", &s.checkouts),
                ("wait_ns", &s.wait_ns),
                ("created", &s.created),
                ("closed", &s.closed),
                ("out_peak", &s.out_peak),
                ("scans", &s.scans),
                ("retries", &s.retries),
                ("queued", &s.queued),
                ("rejected", &s.rejected),
            ] {
                ht.insert(k, c.get())?;
                if reset {
                    c.set(0);
                }
            }
            if reset {
                s.out_peak.set(s.out_now.get());
            }
            ht.insert("out_now", s.out_now.get())?;
            ht.insert("open_conns", s.open.get())?;
            Ok(())
        })?;
        let mut z = Zval::new();
        z.set_hashtable(ht);
        Ok(z)
    }

    /// Wire counters since the last reset: cmd:<name>, rejected:<name>, checkout@<addr>,
    /// created@<addr>, queued@<addr>, zip:<compressor>, unzip:<compressor>.
    pub fn wire_stats(reset: bool) -> PhpResult<Zval> {
        let mut ht = ZendHashTable::new();
        client::WIRE.with_borrow_mut(|w| -> PhpResult<()> {
            for (k, v) in w.iter() {
                ht.insert(k.as_str(), *v)?;
            }
            if reset {
                w.clear();
            }
            Ok(())
        })?;
        let mut z = Zval::new();
        z.set_hashtable(ht);
        Ok(z)
    }
}

#[php_module]
pub fn get_module(module: ModuleBuilder) -> ModuleBuilder {
    module.class::<PhpClient>()
}
