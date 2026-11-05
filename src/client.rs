//! Minimal MongoDB wire client: OP_MSG, replica-set discovery, read preference with tag sets and
//! latency window, per-server connection pools, cursors drained with getMore, retryable reads.
//!
//! Single-threaded (the PHP thread). An operation runs either blocking (`Io::Sync`: poll-free
//! blocking send/recv with SO_RCVTIMEO, like libmongoc) or suspending the current fiber
//! (`Io::Async`, see aio.rs). Topology scans are blocking, run in parallel on scoped threads
//! (no PHP API there), at first use, every heartbeatFrequencyMS (60 s, as libmongoc
//! single-threaded) and after a server error, at most every 500 ms.
//!
//! Not supported (rejected at URI parse): auth, TLS, SRV, maxStalenessSeconds, load balancers.
//! Compression: OP_COMPRESSED with zstd or zlib when the URI's `compressors` lists them and the
//! server's handshake agrees (snappy is skipped). No sessions/lsid, no $clusterTime.
//!
//! Reads only: every command is checked against an allowlist before anything is sent (find,
//! aggregate without $out/$merge, count, distinct; getMore only for their cursors).
//!
//! Pool (per server): at most maxPoolSize connections per process, idle LIFO. Asynchronous
//! operations (inside a fiber) are driven by the event loop (aio.rs), not by their fiber: send,
//! receive, getMore until the cursor is exhausted, then the connection goes straight to the next
//! queued operation of that server, and the fiber is resumed with the raw replies to decode. So
//! a connection is released when the server has answered, whatever the fibers do (a lazy scheduler
//! resumes a fiber only when its future is pulled: a fiber holding a connection across its own
//! suspension could deadlock a waiter). When the pool is exhausted an asynchronous operation
//! queues (FIFO) and a blocking one ({main}) polls the event set until a connection frees.

use crate::bson::{self, Doc, Val};
use crate::error::{Error, R};
use crate::aio;
use std::cell::{Cell, RefCell};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::os::fd::{AsRawFd, RawFd};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Mode {
    Primary,
    PrimaryPreferred,
    Secondary,
    SecondaryPreferred,
    Nearest,
}

impl Mode {
    pub fn parse(s: &str) -> R<Self> {
        Ok(match s.to_ascii_lowercase().as_str() {
            "primary" => Mode::Primary,
            "primarypreferred" => Mode::PrimaryPreferred,
            "secondary" => Mode::Secondary,
            "secondarypreferred" => Mode::SecondaryPreferred,
            "nearest" => Mode::Nearest,
            _ => return Err(Error::Invalid(format!("unknown read preference {s}"))),
        })
    }

    fn name(self) -> &'static str {
        match self {
            Mode::Primary => "primary",
            Mode::PrimaryPreferred => "primaryPreferred",
            Mode::Secondary => "secondary",
            Mode::SecondaryPreferred => "secondaryPreferred",
            Mode::Nearest => "nearest",
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
pub enum Io {
    Sync,
    Async,
}

type Tags = Vec<(String, String)>;

pub struct Opts {
    seeds: Vec<String>,
    replica_set: Option<String>,
    direct: bool,
    mode: Mode,
    tag_sets: Vec<Tags>,
    local_threshold: f64,
    heartbeat: Duration,
    max_pool: u32,
    conn: ConnOpts,
    /// Reads never go to the primary: secondaryPreferred/nearest/primaryPreferred select secondaries
    /// only and fail instead of falling back (URI option strictSecondary=true).
    pub strict_secondary: bool,
    /// Environment MONGO_ASYNC_REQUIRE_TAG=k:v (safety guard): only secondaries with that tag are ever
    /// selected, whatever the mode, tag sets or topology type.
    require_tag: Option<(String, String)>,
}

/// Wire compressors implemented here (ids of the OP_COMPRESSED spec).
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Comp {
    Zlib,
    Zstd,
}

impl Comp {
    fn from_name(s: &str) -> Option<Self> {
        match s {
            "zstd" => Some(Comp::Zstd),
            "zlib" => Some(Comp::Zlib),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Comp::Zstd => "zstd",
            Comp::Zlib => "zlib",
        }
    }

    fn id(self) -> u8 {
        match self {
            Comp::Zlib => 2,
            Comp::Zstd => 3,
        }
    }
}

#[derive(Clone)]
pub struct ConnOpts {
    connect_timeout: Duration,
    socket_timeout: Option<Duration>,
    appname: Option<String>,
    /// From the URI's compressors, in its order (unsupported names dropped, as libmongoc warns).
    compressors: Vec<Comp>,
    zlib_level: i32,
}

fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

impl Opts {
    pub fn parse(uri: &str, max_pool: Option<u32>) -> R<Self> {
        let bad = |m: String| Error::Invalid(format!("mongodb URI: {m}"));
        let rest = uri.strip_prefix("mongodb://").ok_or_else(|| bad("only mongodb:// URIs are supported".into()))?;
        let (hosts, query) = match rest.find('/') {
            Some(i) => (&rest[..i], rest[i + 1..].split_once('?').map_or("", |(_, q)| q)),
            None => match rest.split_once('?') {
                Some((h, q)) => (h, q),
                None => (rest, ""),
            },
        };
        if hosts.contains('@') {
            return Err(bad("authentication is not supported".into()));
        }
        let seeds: Vec<String> = hosts
            .split(',')
            .filter(|h| !h.is_empty())
            .map(|h| if h.contains(':') { h.to_owned() } else { format!("{h}:27017") })
            .collect();
        if seeds.is_empty() {
            return Err(bad("no hosts".into()));
        }
        let mut o = Opts {
            seeds,
            replica_set: None,
            direct: false,
            mode: Mode::Primary,
            tag_sets: vec![],
            local_threshold: 15.0,
            heartbeat: Duration::from_millis(60_000),
            max_pool: 100,
            strict_secondary: false,
            require_tag: None,
            conn: ConnOpts {
                connect_timeout: Duration::from_millis(10_000),
                socket_timeout: Some(Duration::from_millis(300_000)),
                appname: None,
                compressors: vec![],
                zlib_level: -1,
            },
        };
        if let Ok(t) = std::env::var("MONGO_ASYNC_REQUIRE_TAG") {
            if !t.is_empty() {
                let (k, v) = t.split_once(':').ok_or_else(|| bad(format!("bad MONGO_ASYNC_REQUIRE_TAG {t}")))?;
                o.require_tag = Some((k.to_owned(), v.to_owned()));
            }
        }
        let ms = |v: &str| v.parse::<u64>().map_err(|_| bad(format!("bad number {v}")));
        let mut direct = None;
        for kv in query.split(['&', ';']).filter(|s| !s.is_empty()) {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            let v = pct_decode(v);
            match k.to_ascii_lowercase().as_str() {
                "replicaset" => o.replica_set = Some(v),
                "directconnection" => direct = Some(v == "true"),
                "readpreference" => o.mode = Mode::parse(&v)?,
                "readpreferencetags" => o.tag_sets.push(
                    v.split(',')
                        .filter(|t| !t.is_empty())
                        .map(|t| t.split_once(':').map(|(a, b)| (a.to_owned(), b.to_owned())).ok_or_else(|| bad(format!("bad tag {t}"))))
                        .collect::<R<Tags>>()?,
                ),
                "localthresholdms" => o.local_threshold = ms(&v)? as f64,
                "connecttimeoutms" => o.conn.connect_timeout = Duration::from_millis(ms(&v)?),
                "sockettimeoutms" => o.conn.socket_timeout = match ms(&v)? { 0 => None, n => Some(Duration::from_millis(n)) },
                "heartbeatfrequencyms" => o.heartbeat = Duration::from_millis(ms(&v)?.max(500)),
                "maxpoolsize" => o.max_pool = (ms(&v)? as u32).max(1),
                "appname" => o.conn.appname = Some(v),
                "strictsecondary" => o.strict_secondary = v == "true",
                "compressors" => o.conn.compressors = v.split(',').filter_map(|c| Comp::from_name(c.trim())).collect(),
                "zlibcompressionlevel" => {
                    o.conn.zlib_level = v.parse::<i32>().ok().filter(|l| (-1..=9).contains(l)).ok_or_else(|| bad(format!("bad zlibCompressionLevel {v}")))?
                }
                // Not needed by a reads-only client without sessions, or applied by the caller
                // (readConcern is in the command), or affecting writes only.
                "retryreads" | "retrywrites" | "w" | "wtimeoutms" | "journal"
                | "readconcernlevel" | "serverselectiontimeoutms" | "serverselectiontryonce" | "minpoolsize" | "maxidletimems"
                | "waitqueuetimeoutms" => {}
                _ => return Err(bad(format!("unsupported option {k}"))),
            }
        }
        if let Some(n) = max_pool {
            o.max_pool = n.max(1);
        }
        // libmongoc: one seed without replicaSet is a direct connection.
        o.direct = direct.unwrap_or(o.replica_set.is_none() && o.seeds.len() == 1);
        if o.direct && o.seeds.len() > 1 {
            return Err(bad("directConnection with several hosts".into()));
        }
        Ok(o)
    }
}

// ---- connections ----------------------------------------------------------------------------

pub struct Conn {
    sock: TcpStream,
    pub buf: Vec<u8>,
    spare: Vec<u8>,
    /// (De)compression scratch buffer.
    zbuf: Vec<u8>,
    addr: String,
    registered: bool,
    gen: u64,
    /// Negotiated in the handshake: (compressor, zlib level).
    comp: Option<(Comp, i32)>,
}

thread_local! {
    static REQ_ID: Cell<i32> = const { Cell::new(1) };
    pub static ST: Stats = const { Stats::new() };
}

pub struct Stats {
    pub checkouts: Cell<i64>,
    pub wait_ns: Cell<i64>,
    pub created: Cell<i64>,
    pub closed: Cell<i64>,
    pub open: Cell<i64>,
    pub out_now: Cell<i64>,
    pub out_peak: Cell<i64>,
    pub scans: Cell<i64>,
    pub retries: Cell<i64>,
    /// Operations that found the pool exhausted and queued for a connection.
    pub queued: Cell<i64>,
    pub rejected: Cell<i64>,
}

impl Stats {
    const fn new() -> Self {
        Stats {
            checkouts: Cell::new(0),
            wait_ns: Cell::new(0),
            created: Cell::new(0),
            closed: Cell::new(0),
            open: Cell::new(0),
            out_now: Cell::new(0),
            out_peak: Cell::new(0),
            scans: Cell::new(0),
            retries: Cell::new(0),
            queued: Cell::new(0),
            rejected: Cell::new(0),
        }
    }
}

fn st(f: impl FnOnce(&Stats)) {
    ST.with(f)
}

thread_local! {
    /// Wire counters (`cmd:<name>`, `rejected:<name>`, `checkout@<addr>`, `created@<addr>`,
    /// `queued@<addr>`, `zip:<compressor>`, `unzip:<compressor>`), for monitoring.
    pub static WIRE: RefCell<Vec<(String, i64)>> = const { RefCell::new(Vec::new()) };
}

/// Increments the wire counter `prefix + name` (no allocation once it exists).
pub fn wire(prefix: &str, name: &str) {
    WIRE.with_borrow_mut(|w| {
        let n = prefix.len() + name.len();
        if let Some(e) = w.iter_mut().find(|e| e.0.len() == n && e.0.starts_with(prefix) && e.0.ends_with(name)) {
            e.1 += 1;
            return;
        }
        w.push((format!("{prefix}{name}"), 1));
    })
}

fn note_created(addr: &str) {
    st(|s| {
        s.created.set(s.created.get() + 1);
        s.open.set(s.open.get() + 1);
    });
    wire("created@", addr);
}

fn next_req() -> i32 {
    REQ_ID.with(|r| {
        let v = r.get();
        r.set(v.wrapping_add(1).max(1));
        v
    })
}

/// Starts an OP_MSG with a section 0 body; returns the body's start (for bson::end).
fn msg_begin(buf: &mut Vec<u8>) -> usize {
    buf.clear();
    buf.extend_from_slice(&[0; 4]);
    buf.extend_from_slice(&next_req().to_le_bytes());
    buf.extend_from_slice(&0i32.to_le_bytes());
    buf.extend_from_slice(&2013i32.to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes());
    buf.push(0);
    21
}

fn msg_end(buf: &mut Vec<u8>) {
    let len = buf.len() as i32;
    buf[0..4].copy_from_slice(&len.to_le_bytes());
}

/// The section-0 document of an OP_MSG reply.
fn reply_doc(m: &[u8]) -> R<Doc<'_>> {
    if m.len() < 21 {
        return Err(Error::bson("short reply"));
    }
    let op = i32::from_le_bytes(m[12..16].try_into().unwrap());
    if op != 2013 {
        return Err(Error::bson("unexpected reply opcode"));
    }
    if m[20] != 0 {
        return Err(Error::bson("unexpected reply section"));
    }
    Doc::at(&m[21..])
}

fn check_ok(d: Doc) -> R<()> {
    if d.num("ok")? == Some(1.0) {
        return Ok(());
    }
    let code = d.num("code")?.unwrap_or(0.0) as i32;
    let msg = d.str("errmsg")?.unwrap_or("command failed").to_owned();
    Err(Error::Server { code, msg })
}

impl Conn {
    /// Connects and runs the handshake hello (blocking).
    fn open(sa: Option<SocketAddr>, addr: &str, o: &ConnOpts) -> R<(Conn, Hello, f64, SocketAddr)> {
        let sa = match sa {
            Some(sa) => sa,
            None => addr.to_socket_addrs().map_err(|e| Error::net("resolve", addr, e))?.next().ok_or_else(|| Error::Net(format!("resolve {addr}: no address")))?,
        };
        let mut c = Conn::connect(sa, addr, o)?;
        let (h, rtt) = c.hello(o.appname.as_deref(), true, &o.compressors)?;
        // the first compressor of the server's answer (the ones it shares with ours, in our order)
        c.comp = h.compression.iter().find_map(|n| Comp::from_name(n).filter(|cp| o.compressors.contains(cp))).map(|cp| (cp, o.zlib_level));
        Ok((c, h, rtt, sa))
    }

    fn hello(&mut self, appname: Option<&str>, handshake: bool, compressors: &[Comp]) -> R<(Hello, f64)> {
        let mut msg = Vec::with_capacity(256);
        hello_msg(&mut msg, appname, handshake, compressors);
        let t0 = Instant::now();
        // hello is never compressed (OP_COMPRESSED spec)
        self.send(&msg, false)?;
        self.recv()?;
        let rtt = t0.elapsed().as_secs_f64() * 1e3;
        Ok((parse_hello(reply_doc(&self.buf)?)?, rtt))
    }

    fn connect(sa: SocketAddr, addr: &str, o: &ConnOpts) -> R<Conn> {
        let sock = TcpStream::connect_timeout(&sa, o.connect_timeout).map_err(|e| Error::net("connect", addr, e))?;
        sock.set_nodelay(true).map_err(|e| Error::net("setsockopt", addr, e))?;
        sock.set_read_timeout(o.socket_timeout).map_err(|e| Error::net("setsockopt", addr, e))?;
        sock.set_write_timeout(o.socket_timeout).map_err(|e| Error::net("setsockopt", addr, e))?;
        // counted by the caller (note_created) on the PHP thread: scans connect on scoped threads
        Ok(Conn { sock, buf: Vec::new(), spare: Vec::new(), zbuf: Vec::new(), addr: addr.to_owned(), registered: false, gen: 0, comp: None })
    }

    fn fd(&self) -> RawFd {
        self.sock.as_raw_fd()
    }

    /// Sends one message, blocking; `compress`: as OP_COMPRESSED when a compressor was negotiated.
    fn send(&mut self, msg: &[u8], compress: bool) -> R<()> {
        let mut z = std::mem::take(&mut self.zbuf);
        let r = self.wire_msg(msg, compress, &mut z).and_then(|zipped| {
            let mut off = 0;
            let data = if zipped { &z[..] } else { msg };
            while off < data.len() {
                let n = unsafe { libc::send(self.fd(), data[off..].as_ptr() as *const _, data.len() - off, libc::MSG_NOSIGNAL) };
                if n >= 0 {
                    off += n as usize;
                    continue;
                }
                let e = std::io::Error::last_os_error();
                if e.kind() != std::io::ErrorKind::Interrupted {
                    return Err(Error::net("send to", &self.addr, e));
                }
            }
            Ok(())
        });
        self.zbuf = z;
        r
    }

    /// Compresses `msg` into `out` when asked and negotiated; true if it did.
    fn wire_msg(&self, msg: &[u8], compress: bool, out: &mut Vec<u8>) -> R<bool> {
        match (compress, self.comp) {
            (true, Some((cp, level))) => {
                compress_msg(cp, level, msg, out)?;
                wire("zip:", cp.name());
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// Non-blocking send of data[*off..]; true when all of it is sent.
    fn try_send(&mut self, data: &[u8], off: &mut usize) -> R<bool> {
        while *off < data.len() {
            let n = unsafe { libc::send(self.fd(), data[*off..].as_ptr() as *const _, data.len() - *off, libc::MSG_NOSIGNAL | libc::MSG_DONTWAIT) };
            if n >= 0 {
                *off += n as usize;
                continue;
            }
            let e = std::io::Error::last_os_error();
            match e.kind() {
                std::io::ErrorKind::Interrupted => {}
                std::io::ErrorKind::WouldBlock => return Ok(false),
                _ => return Err(Error::net("send to", &self.addr, e)),
            }
        }
        Ok(true)
    }

    /// Receives one reply into self.buf, blocking (SO_RCVTIMEO).
    fn recv(&mut self) -> R<()> {
        self.buf.clear();
        while !self.recv_some(false)? {}
        Ok(())
    }

    /// Reads what is available into self.buf (`nonblock`: until EAGAIN); true once the message is
    /// complete (and decompressed). Call with an empty buf for a new message.
    fn recv_some(&mut self, nonblock: bool) -> R<bool> {
        loop {
            let have = self.buf.len();
            // 0 = header not read yet; else the messageLength, checked before reading on.
            let need = if have >= 4 {
                let len = i32::from_le_bytes(self.buf[..4].try_into().unwrap());
                if !(16..=MAX_MSG + 16).contains(&len) {
                    return Err(Error::bson("invalid reply length"));
                }
                len as usize
            } else {
                0
            };
            if need > 0 && have >= need {
                if have > need {
                    return Err(Error::bson("trailing bytes after reply"));
                }
                if have >= 16 && i32::from_le_bytes(self.buf[12..16].try_into().unwrap()) == OP_COMPRESSED {
                    self.decompress()?;
                }
                return Ok(true);
            }
            let want = if need > 0 { need - have } else { 16 * 1024 };
            self.buf.reserve(want);
            let flags = if nonblock { libc::MSG_DONTWAIT } else if need > 0 { libc::MSG_WAITALL } else { 0 };
            let n = unsafe { libc::recv(self.fd(), self.buf.as_mut_ptr().add(have) as *mut _, want, flags) };
            if n > 0 {
                unsafe { self.buf.set_len(have + n as usize) };
                continue;
            }
            if n == 0 {
                return Err(Error::Net(format!("connection to {} closed", self.addr)));
            }
            let e = std::io::Error::last_os_error();
            match e.kind() {
                std::io::ErrorKind::Interrupted => {}
                std::io::ErrorKind::WouldBlock if nonblock => return Ok(false),
                _ => return Err(Error::net("recv from", &self.addr, e)),
            }
        }
    }

    fn roundtrip(&mut self, msg: &[u8]) -> R<()> {
        self.send(msg, true)?;
        self.recv()
    }

    /// Replaces the OP_COMPRESSED message in buf with the original message.
    fn decompress(&mut self) -> R<()> {
        let b = &self.buf;
        if b.len() < 25 {
            return Err(Error::bson("short OP_COMPRESSED"));
        }
        let op = i32::from_le_bytes(b[16..20].try_into().unwrap());
        let size = i32::from_le_bytes(b[20..24].try_into().unwrap());
        if !(0..=MAX_MSG).contains(&size) {
            return Err(Error::bson("bad OP_COMPRESSED size"));
        }
        let size = size as usize;
        let mut z = std::mem::take(&mut self.zbuf);
        z.clear();
        z.extend_from_slice(&((16 + size) as i32).to_le_bytes());
        z.extend_from_slice(&b[4..12]);
        z.extend_from_slice(&op.to_le_bytes());
        z.resize(16 + size, 0);
        let data = &b[25..];
        let name = match b[24] {
            3 => {
                let n = ZD.with_borrow_mut(|d| d.get_or_insert_with(zstd_safe::DCtx::create).decompress(&mut z[16..], data))
                    .map_err(|c| Error::Bson(format!("zstd: {}", zstd_safe::get_error_name(c))))?;
                if n != size {
                    return Err(Error::bson("zstd: size mismatch"));
                }
                "zstd"
            }
            2 => {
                let mut d = flate2::Decompress::new(true);
                let st = d.decompress(data, &mut z[16..], flate2::FlushDecompress::Finish).map_err(|e| Error::Bson(format!("zlib: {e}")))?;
                if st != flate2::Status::StreamEnd || d.total_out() as usize != size {
                    return Err(Error::bson("zlib: size mismatch"));
                }
                "zlib"
            }
            0 => {
                if data.len() != size {
                    return Err(Error::bson("noop: size mismatch"));
                }
                z[16..].copy_from_slice(data);
                "noop"
            }
            id => return Err(Error::Bson(format!("unsupported compressor id {id}"))),
        };
        wire("unzip:", name);
        self.zbuf = std::mem::replace(&mut self.buf, z);
        Ok(())
    }
}

const OP_COMPRESSED: i32 = 2012;
/// maxMessageSizeBytes of every server version.
const MAX_MSG: i32 = 48_000_000;

thread_local! {
    static ZC: RefCell<Option<zstd_safe::CCtx<'static>>> = const { RefCell::new(None) };
    static ZD: RefCell<Option<zstd_safe::DCtx<'static>>> = const { RefCell::new(None) };
}

/// `msg` (a complete message) as OP_COMPRESSED into `out`.
fn compress_msg(cp: Comp, zlib_level: i32, msg: &[u8], out: &mut Vec<u8>) -> R<()> {
    let body = &msg[16..];
    out.clear();
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&msg[4..12]);
    out.extend_from_slice(&OP_COMPRESSED.to_le_bytes());
    out.extend_from_slice(&msg[12..16]);
    out.extend_from_slice(&(body.len() as i32).to_le_bytes());
    out.push(cp.id());
    match cp {
        Comp::Zstd => {
            out.resize(25 + zstd_safe::compress_bound(body.len()), 0);
            // level 0 = zstd's default (3), as libmongoc
            let n = ZC.with_borrow_mut(|c| c.get_or_insert_with(zstd_safe::CCtx::create).compress(&mut out[25..], body, 0))
                .map_err(|c| Error::Bson(format!("zstd: {}", zstd_safe::get_error_name(c))))?;
            out.truncate(25 + n);
        }
        Comp::Zlib => {
            let level = if zlib_level < 0 { flate2::Compression::default() } else { flate2::Compression::new(zlib_level as u32) };
            let mut e = flate2::Compress::new(level, true);
            out.reserve(body.len() + body.len() / 1000 + 64);
            loop {
                let st = e.compress_vec(&body[e.total_in() as usize..], out, flate2::FlushCompress::Finish).map_err(|e| Error::Bson(format!("zlib: {e}")))?;
                if st == flate2::Status::StreamEnd {
                    break;
                }
                out.reserve(out.capacity().max(1024));
            }
        }
    }
    let len = out.len() as i32;
    out[0..4].copy_from_slice(&len.to_le_bytes());
    Ok(())
}

impl Drop for Conn {
    fn drop(&mut self) {
        st(|s| {
            s.closed.set(s.closed.get() + 1);
            s.open.set(s.open.get() - 1);
        });
    }
}

// ---- topology -------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    Unknown,
    Standalone,
    Mongos,
    Primary,
    Secondary,
    Other,
}

struct Server {
    addr: String,
    sa: Option<SocketAddr>,
    kind: Kind,
    tags: Tags,
    rtt: f64,
    idle: Vec<Conn>,
    /// Pool connections (idle + checked out), at most maxPoolSize.
    total: u32,
    /// Operations waiting for a connection of this server (tokens, FIFO).
    queue: std::collections::VecDeque<u64>,
}

impl Server {
    fn new(addr: String) -> Self {
        Server { addr, sa: None, kind: Kind::Unknown, tags: vec![], rtt: -1.0, idle: vec![], total: 0, queue: Default::default() }
    }

    fn mark_unknown(&mut self) {
        self.kind = Kind::Unknown;
        let n = self.idle.len() as u32;
        self.idle.clear();
        self.total -= n;
    }
}

/// Which connection a scan job used: a pool one, or a temporary one (pool full), closed after
/// the hello.
#[derive(Clone, Copy, PartialEq)]
enum Slot {
    Pool,
    Temp,
}

struct Hello {
    kind: Kind,
    set_name: Option<String>,
    tags: Tags,
    hosts: Vec<String>,
    primary_hosts: bool,
    compression: Vec<String>,
}

fn parse_hello(d: Doc) -> R<Hello> {
    check_ok(d)?;
    let set_name = d.str("setName")?.map(str::to_owned);
    let kind = if d.str("msg")? == Some("isdbgrid") {
        Kind::Mongos
    } else if set_name.is_some() {
        if d.bool("isWritablePrimary")? || d.bool("ismaster")? {
            Kind::Primary
        } else if d.bool("secondary")? {
            Kind::Secondary
        } else {
            Kind::Other
        }
    } else if d.bool("isreplicaset")? {
        Kind::Other
    } else {
        Kind::Standalone
    };
    let mut tags = vec![];
    if let Some(t) = d.doc("tags")? {
        for e in t.iter() {
            if let (k, Val::Str(v)) = e? {
                tags.push((String::from_utf8_lossy(k).into_owned(), v.to_owned()));
            }
        }
    }
    let mut hosts = vec![];
    for f in ["hosts", "passives", "arbiters"] {
        if let Some(a) = d.doc(f)? {
            for e in a.iter() {
                if let (_, Val::Str(h)) = e? {
                    hosts.push(h.to_owned());
                }
            }
        }
    }
    let mut compression = vec![];
    if let Some(a) = d.doc("compression")? {
        for e in a.iter() {
            if let (_, Val::Str(c)) = e? {
                compression.push(c.to_owned());
            }
        }
    }
    Ok(Hello { kind, set_name, tags, hosts, primary_hosts: kind == Kind::Primary, compression })
}

fn hello_msg(buf: &mut Vec<u8>, appname: Option<&str>, handshake: bool, compressors: &[Comp]) {
    let at = msg_begin(buf);
    let d = bson::begin(buf);
    bson::put_i32(buf, "hello", 1);
    bson::put_bool(buf, "helloOk", true);
    if handshake {
        let c = bson::begin_sub(buf, "client", false);
        if let Some(a) = appname {
            let ap = bson::begin_sub(buf, "application", false);
            bson::put_str(buf, "name", a);
            bson::end(buf, ap);
        }
        let dr = bson::begin_sub(buf, "driver", false);
        bson::put_str(buf, "name", "mongo-php-async-driver");
        bson::put_str(buf, "version", env!("CARGO_PKG_VERSION"));
        bson::end(buf, dr);
        let os = bson::begin_sub(buf, "os", false);
        bson::put_str(buf, "type", "Linux");
        bson::end(buf, os);
        bson::end(buf, c);
        if !compressors.is_empty() {
            let a = bson::begin_sub(buf, "compression", true);
            for (i, cp) in compressors.iter().enumerate() {
                bson::put_str(buf, &i.to_string(), cp.name());
            }
            bson::end(buf, a);
        }
    }
    bson::put_str(buf, "$db", "admin");
    bson::end(buf, d);
    let _ = at;
    msg_end(buf);
}

pub struct Topo {
    pub opts: Opts,
    servers: Vec<Server>,
    last_scan: Option<Instant>,
    rng: u64,
    pid: u32,
    /// Connections checked out by blocking operations: (addr, request generation, fd), to
    /// reclaim the ones a fatal error abandoned (their Rust frames are gone).
    out: Vec<(String, u64, RawFd)>,
}

impl Topo {
    pub fn new(opts: Opts) -> Self {
        let servers = opts.seeds.iter().cloned().map(Server::new).collect();
        Topo { opts, servers, last_scan: None, rng: 0x9E37_79B9_7F4A_7C15 ^ std::process::id() as u64, pid: std::process::id(), out: vec![] }
    }

    fn rand(&mut self) -> u64 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        self.rng
    }

    /// Hello on every server, in parallel (blocking): reuses an idle connection when there is
    /// one, else opens one (kept in the pool).
    fn scan(&mut self) {
        st(|s| s.scans.set(s.scans.get() + 1));
        self.last_scan = Some(Instant::now());
        let max = self.opts.max_pool;
        for _round in 0..2 {
            let jobs: Vec<(usize, Option<Conn>, Slot, Option<SocketAddr>, String)> = self
                .servers
                .iter_mut()
                .enumerate()
                .map(|(i, s)| {
                    let (c, slot) = if let Some(c) = s.idle.pop() {
                        (Some(c), Slot::Pool)
                    } else if s.total < max {
                        (None, Slot::Pool)
                    } else {
                        (None, Slot::Temp)
                    };
                    (i, c, slot, s.sa, s.addr.clone())
                })
                .collect();
            let o = &self.opts.conn;
            let results: Vec<(usize, bool, Slot, R<(Hello, Conn, f64, SocketAddr, bool)>)> = std::thread::scope(|sc| {
                let hs: Vec<_> = jobs
                    .into_iter()
                    .map(|(i, c, slot, sa, addr)| {
                        sc.spawn(move || {
                            let had = c.is_some();
                            let r = match c {
                                Some(mut c) => c.hello(None, false, &[]).map(|(h, rtt)| (h, c, rtt, sa.unwrap(), false)),
                                None => Conn::open(sa, &addr, o).map(|(c, h, rtt, sa)| (h, c, rtt, sa, true)),
                            };
                            (i, had, slot, r)
                        })
                    })
                    .collect();
                hs.into_iter().map(|h| h.join().expect("scan thread panicked")).collect()
            });
            let mut discovered: Vec<String> = vec![];
            let mut primary_list: Option<Vec<String>> = None;
            for (i, had, slot, r) in results {
                let s = &mut self.servers[i];
                if let Ok((_, c, _, _, true)) = &r {
                    note_created(&c.addr);
                }
                match r {
                    Ok((h, c, rtt, sa, new)) => {
                        if self.opts.replica_set.is_some() && h.set_name != self.opts.replica_set {
                            s.mark_unknown();
                            if !new && slot == Slot::Pool {
                                s.total -= 1;
                            }
                            continue;
                        }
                        s.sa = Some(sa);
                        s.kind = h.kind;
                        s.tags = h.tags;
                        s.rtt = if s.rtt < 0.0 { rtt } else { 0.2 * rtt + 0.8 * s.rtt };
                        match slot {
                            Slot::Pool => {
                                if new {
                                    s.total += 1;
                                }
                                s.idle.push(c);
                            }
                            Slot::Temp => drop(c),
                        }
                        if !self.opts.direct {
                            discovered.extend(h.hosts.iter().cloned());
                            if h.primary_hosts {
                                primary_list = Some(h.hosts);
                            }
                        }
                    }
                    Err(_) => {
                        // a reused connection that failed is gone
                        if had {
                            s.total -= 1;
                        }
                        s.mark_unknown();
                    }
                }
            }
            if self.opts.direct {
                return;
            }
            if let Some(list) = primary_list {
                self.servers.retain(|s| list.contains(&s.addr) || s.total > s.idle.len() as u32);
            }
            let new: Vec<String> = discovered.into_iter().filter(|h| !self.servers.iter().any(|s| &s.addr == h)).collect();
            if new.is_empty() {
                return;
            }
            for h in new {
                if !self.servers.iter().any(|s| s.addr == h) {
                    self.servers.push(Server::new(h));
                }
            }
        }
    }

    fn suitable(&mut self, mode: Mode, tag_sets: &[Tags]) -> Option<usize> {
        if let Some((k, v)) = &self.opts.require_tag {
            let c: Vec<usize> = (0..self.servers.len())
                .filter(|&i| self.servers[i].kind == Kind::Secondary && self.servers[i].tags.iter().any(|(a, b)| a == k && b == v))
                .collect();
            return self.window(c);
        }
        let ok = |s: &Server| s.kind != Kind::Unknown;
        if self.opts.direct {
            return ok(&self.servers[0]).then_some(0);
        }
        if self.servers.iter().any(|s| s.kind == Kind::Mongos || s.kind == Kind::Standalone) {
            let c: Vec<usize> = (0..self.servers.len()).filter(|&i| matches!(self.servers[i].kind, Kind::Mongos | Kind::Standalone)).collect();
            return self.window(c);
        }
        let primary = if self.opts.strict_secondary && mode != Mode::Primary {
            None
        } else {
            self.servers.iter().position(|s| s.kind == Kind::Primary)
        };
        let nearest_kinds: &[Kind] = if self.opts.strict_secondary { &[Kind::Secondary] } else { &[Kind::Primary, Kind::Secondary] };
        let matching = |this: &Self, kinds: &[Kind]| -> Vec<usize> {
            let c: Vec<usize> = (0..this.servers.len()).filter(|&i| kinds.contains(&this.servers[i].kind)).collect();
            if tag_sets.is_empty() {
                return c;
            }
            for ts in tag_sets {
                let m: Vec<usize> = c.iter().copied().filter(|&i| ts.iter().all(|t| this.servers[i].tags.contains(t))).collect();
                if !m.is_empty() {
                    return m;
                }
            }
            vec![]
        };
        match mode {
            Mode::Primary => primary,
            Mode::PrimaryPreferred => primary.or_else(|| {
                let c = matching(self, &[Kind::Secondary]);
                self.window(c)
            }),
            Mode::Secondary => {
                let c = matching(self, &[Kind::Secondary]);
                self.window(c)
            }
            Mode::SecondaryPreferred => {
                let c = matching(self, &[Kind::Secondary]);
                self.window(c).or(primary)
            }
            Mode::Nearest => {
                let c = matching(self, nearest_kinds);
                self.window(c)
            }
        }
    }

    fn window(&mut self, c: Vec<usize>) -> Option<usize> {
        let min = c.iter().map(|&i| self.servers[i].rtt).fold(f64::INFINITY, f64::min);
        let w: Vec<usize> = c.into_iter().filter(|&i| self.servers[i].rtt <= min + self.opts.local_threshold).collect();
        if w.is_empty() {
            return None;
        }
        let r = self.rand() as usize % w.len();
        Some(w[r])
    }

    fn select(&mut self, mode: Mode, tag_sets: &[Tags]) -> R<usize> {
        if self.pid != std::process::id() {
            // forked: the parent's connections are not ours
            for s in &mut self.servers {
                std::mem::forget(std::mem::take(&mut s.idle));
                s.total = 0;
                s.queue.clear();
                s.kind = Kind::Unknown;
            }
            self.out.clear();
            self.pid = std::process::id();
            self.last_scan = None;
        }
        let now = Instant::now();
        let due = match self.last_scan {
            None => true,
            Some(t) => {
                now - t >= self.opts.heartbeat
                    || (now - t >= Duration::from_millis(500) && self.servers.iter().any(|s| s.kind == Kind::Unknown))
            }
        };
        let mut scanned = false;
        if due {
            self.scan();
            scanned = true;
        }
        if let Some(i) = self.suitable(mode, tag_sets) {
            return Ok(i);
        }
        if !scanned && self.last_scan.is_none_or(|t| now - t >= Duration::from_millis(500)) {
            self.scan();
            if let Some(i) = self.suitable(mode, tag_sets) {
                return Ok(i);
            }
        }
        let desc: Vec<String> = self.servers.iter().map(|s| format!("{} {:?} rtt={:.2}", s.addr, s.kind, s.rtt)).collect();
        Err(Error::Selection(format!("No suitable servers found for {}: [{}]", mode.name(), desc.join(", "))))
    }

    /// The $readPreference to send to server `i` (OP_MSG rules of the server selection spec).
    fn rp_for(&self, i: usize, mode: Mode, tag_sets: &[Tags]) -> Option<(Mode, bool)> {
        match self.servers[i].kind {
            Kind::Mongos => (mode != Mode::Primary).then_some((mode, !tag_sets.is_empty())),
            _ if self.opts.direct => Some((if mode == Mode::Primary { Mode::PrimaryPreferred } else { mode }, false)),
            Kind::Standalone => None,
            _ => (mode != Mode::Primary).then_some((mode, false)),
        }
    }

    /// Reclaims connections abandoned by an earlier request (fatal error): their frames are gone.
    fn reclaim(&mut self) {
        let gen = aio::GEN.with(|g| g.get());
        if self.out.iter().all(|o| o.1 == gen) {
            return;
        }
        let (old, cur): (Vec<_>, Vec<_>) = std::mem::take(&mut self.out).into_iter().partition(|o| o.1 != gen);
        self.out = cur;
        for (addr, _, fd) in old {
            unsafe { libc::close(fd) };
            st(|s| {
                s.open.set(s.open.get() - 1);
                s.out_now.set(s.out_now.get() - 1);
            });
            if let Some(s) = self.servers.iter_mut().find(|s| s.addr == addr) {
                s.total -= 1;
            }
        }
    }
}



// ---- operations -----------------------------------------------------------------------------

use std::collections::HashMap;
use std::rc::Rc;

pub type TopoRef = Rc<RefCell<Topo>>;

/// Tokens are sequential integers: no hashing needed.
#[derive(Default)]
struct IdHash(u64);

impl std::hash::Hasher for IdHash {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, b: &[u8]) {
        for &x in b {
            self.0 = (self.0 << 8) | x as u64;
        }
    }
    fn write_u64(&mut self, v: u64) {
        self.0 = v.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }
}

type OpMap = HashMap<u64, Op, std::hash::BuildHasherDefault<IdHash>>;

thread_local! {
    static MSG: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    /// Operations that wait for a connection, are in flight, or finished but not yet taken by
    /// their caller; keyed by token (also the epoll data of their socket).
    static OPS: RefCell<OpMap> = RefCell::new(OpMap::default());
    static NEXT_OP: Cell<u64> = const { Cell::new(1) };
    /// Asynchronous operations queued or in flight.
    static IN_FLIGHT: Cell<usize> = const { Cell::new(0) };
    /// Reply buffers handed back by callers, for the next replies.
    static BUFS: RefCell<Vec<Vec<u8>>> = const { RefCell::new(Vec::new()) };
}

fn take_buf() -> Vec<u8> {
    BUFS.with_borrow_mut(|b| b.pop()).unwrap_or_default()
}

/// Gives a reply buffer back (at most 64 kept, none above 8 MB).
fn put_buf(mut v: Vec<u8>) {
    if v.capacity() <= 8 << 20 {
        v.clear();
        BUFS.with_borrow_mut(|b| {
            if b.len() < 64 {
                b.push(v);
            }
        });
    }
}

#[derive(Clone, Copy, PartialEq)]
enum OpState {
    Queued,
    Sending,
    Receiving,
    Done,
}

struct Op {
    topo: TopoRef,
    addr: String,
    db: String,
    /// The request (a getMore replaces it), and its OP_COMPRESSED form when `zipped`.
    msg: Vec<u8>,
    wire: Vec<u8>,
    zipped: bool,
    off: usize,
    conn: Option<Conn>,
    /// Drain the cursor with getMore.
    cursor: bool,
    /// A blocking caller waiting only for a connection: it gets `conn`, nothing is sent.
    grant: bool,
    replies: Vec<Vec<u8>>,
    err: Option<Error>,
    state: OpState,
    deadline: Option<Instant>,
    suspension: Option<ext_php_rs::types::Zval>,
    /// The caller is gone (its fiber was destroyed): drop the result when done.
    abandoned: bool,
}

fn next_token() -> u64 {
    NEXT_OP.with(|n| {
        let v = n.get();
        n.set(v + 1);
        v
    })
}

/// Number of operations waiting for a connection or in flight.
pub fn pending() -> usize {
    IN_FLIGHT.get()
}

fn mode_tags(t: &TopoRef, mode: Option<Mode>) -> (Mode, Vec<Tags>) {
    let tp = t.borrow();
    (mode.unwrap_or(tp.opts.mode), if mode.is_none() || mode == Some(tp.opts.mode) { tp.opts.tag_sets.clone() } else { vec![] })
}

fn deadline(t: &TopoRef) -> Option<Instant> {
    t.borrow().opts.conn.socket_timeout.map(|d| Instant::now() + d)
}

/// Selects a server and checks out an idle connection or opens a new one (below maxPoolSize);
/// else queues `token` on that server (None). Returns (connection, server, $readPreference).
fn acquire(t: &TopoRef, mode: Mode, tag_sets: &[Tags], token: u64) -> R<(Option<Conn>, String, Option<(Mode, bool)>)> {
    let (sa, addr, rp) = {
        let mut tp = t.borrow_mut();
        tp.reclaim();
        let i = tp.select(mode, tag_sets)?;
        let rp = tp.rp_for(i, mode, tag_sets);
        let max = tp.opts.max_pool;
        let s = &mut tp.servers[i];
        if let Some(c) = s.idle.pop() {
            return Ok((Some(c), s.addr.clone(), rp));
        }
        if s.total >= max {
            s.queue.push_back(token);
            st(|s| s.queued.set(s.queued.get() + 1));
            wire("queued@", &s.addr);
            return Ok((None, s.addr.clone(), rp));
        }
        s.total += 1;
        (s.sa, s.addr.clone(), rp)
    };
    open_for(t, sa, &addr).map(|c| (Some(c), addr, rp))
}

/// Opens a pool connection whose slot (`total`) is already taken.
fn open_for(t: &TopoRef, sa: Option<SocketAddr>, addr: &str) -> R<Conn> {
    let o = t.borrow().opts.conn.clone();
    let r = Conn::open(sa, addr, &o);
    let mut tp = t.borrow_mut();
    let s = tp.servers.iter_mut().find(|s| s.addr == addr);
    match r {
        Ok((c, _, _, sa)) => {
            note_created(addr);
            if let Some(s) = s {
                s.sa = Some(sa);
            }
            Ok(c)
        }
        Err(e) => {
            if let Some(s) = s {
                s.total -= 1;
                s.mark_unknown();
            }
            Err(e)
        }
    }
}

fn out_stats(c: &Conn) {
    st(|s| {
        s.checkouts.set(s.checkouts.get() + 1);
        let n = s.out_now.get() + 1;
        s.out_now.set(n);
        s.out_peak.set(s.out_peak.get().max(n));
    });
    wire("checkout@", &c.addr);
}

/// Returns a connection. ok = false: it failed and is closed (state_change: the server's role
/// changed, mark it unknown). A connection that is fine goes to the server's first queued
/// operation, else to the idle list (at most maxPoolSize kept).
fn release(t: &TopoRef, mut c: Conn, ok: bool, state_change: bool) {
    st(|s| s.out_now.set(s.out_now.get() - 1));
    let mut tp = t.borrow_mut();
    let max = tp.opts.max_pool;
    let Some(s) = tp.servers.iter_mut().find(|s| s.addr == c.addr) else { return };
    if ok {
        for b in [&mut c.buf, &mut c.spare, &mut c.zbuf] {
            if b.capacity() > 8 << 20 {
                *b = Vec::new();
            }
        }
        if let Some(next) = pop_live(&mut s.queue) {
            drop(tp);
            start(next, c);
            return;
        }
        if (s.idle.len() as u32) < max {
            s.idle.push(c);
        } else {
            s.total -= 1;
        }
        return;
    }
    drop(c);
    s.total -= 1;
    if state_change {
        s.mark_unknown();
    }
    // the slot is free again: open a connection for the first queued operation
    if s.total < max {
        if let Some(next) = pop_live(&mut s.queue) {
            s.total += 1;
            let (sa, addr) = (s.sa, s.addr.clone());
            drop(tp);
            match open_for(t, sa, &addr) {
                Ok(c) => start(next, c),
                Err(e) => fail(next, e),
            }
        }
    }
}

/// The first queued token whose operation still exists.
fn pop_live(q: &mut std::collections::VecDeque<u64>) -> Option<u64> {
    while let Some(t) = q.pop_front() {
        if OPS.with_borrow(|o| o.contains_key(&t)) {
            return Some(t);
        }
    }
    None
}

/// Gives `c` to the operation `token` and starts it (send, then wait for the reply).
fn start(token: u64, mut c: Conn) {
    let Some(mut op) = OPS.with_borrow_mut(|o| o.remove(&token)) else {
        // unreachable: queued tokens are removed with their operation
        drop(c);
        return;
    };
    out_stats(&c);
    if op.grant {
        op.conn = Some(c);
        op.state = OpState::Done;
        OPS.with_borrow_mut(|o| o.insert(token, op));
        return;
    }
    let r = (|| {
        if !c.registered {
            aio::register(c.fd())?;
            c.registered = true;
        }
        op.zipped = c.wire_msg(&op.msg, true, &mut op.wire)?;
        Ok(())
    })();
    op.conn = Some(c);
    op.off = 0;
    op.state = OpState::Sending;
    op.deadline = deadline(&op.topo);
    if let Some(d) = op.deadline {
        aio::timer_at(d);
    }
    match r {
        Ok(()) => drive(token, op),
        Err(e) => finish(token, op, Err(e)),
    }
}

/// Fails a queued operation that holds no connection.
fn fail(token: u64, e: Error) {
    if let Some(op) = OPS.with_borrow_mut(|o| o.remove(&token)) {
        finish(token, op, Err(e));
    }
}

enum Next {
    Wait,
    Done,
}

/// Sends, receives and issues getMores as far as the socket allows.
fn progress(op: &mut Op, token: u64) -> R<Next> {
    let Op { conn, msg, wire: zmsg, zipped, off, state, cursor, replies, db, .. } = op;
    let c = conn.as_mut().ok_or_else(|| Error::bson("operation without connection"))?;
    loop {
        match *state {
            OpState::Sending => {
                let data = if *zipped { &zmsg[..] } else { &msg[..] };
                if !c.try_send(data, off)? {
                    aio::arm(c.fd(), libc::EPOLLOUT as u32, token)?;
                    return Ok(Next::Wait);
                }
                c.buf.clear();
                *state = OpState::Receiving;
            }
            OpState::Receiving => {
                if !c.recv_some(true)? {
                    aio::arm(c.fd(), libc::EPOLLIN as u32, token)?;
                    return Ok(Next::Wait);
                }
                let more = {
                    let d = reply_doc(&c.buf)?;
                    check_ok(d)?;
                    if *cursor { cursor_more(d)? } else { None }
                };
                replies.push(std::mem::replace(&mut c.buf, take_buf()));
                let Some((id, coll)) = more else { return Ok(Next::Done) };
                get_more(msg, id, &coll, db);
                wire("cmd:", "getMore");
                *zipped = c.wire_msg(msg, true, zmsg)?;
                *off = 0;
                *state = OpState::Sending;
            }
            OpState::Queued | OpState::Done => return Ok(Next::Done),
        }
    }
}

fn drive(token: u64, mut op: Op) {
    match progress(&mut op, token) {
        Ok(Next::Wait) => {
            OPS.with_borrow_mut(|o| o.insert(token, op));
        }
        Ok(Next::Done) => finish(token, op, Ok(())),
        Err(e) => finish(token, op, Err(e)),
    }
}

/// Releases the operation's connection, records the outcome and wakes its caller.
fn finish(token: u64, mut op: Op, r: R<()>) {
    let conn = op.conn.take();
    let topo = op.topo.clone();
    if let Err(e) = r {
        if let Some(c) = conn {
            let sc = e.state_change();
            // a server error leaves the connection usable (its reply was read completely)
            release(&topo, c, matches!(e, Error::Server { .. }) && !sc, sc);
        }
        op.err = Some(e);
    } else if let Some(c) = conn {
        release(&topo, c, true, false);
    }
    if op.state != OpState::Done && !op.grant {
        IN_FLIGHT.set(IN_FLIGHT.get() - 1);
    }
    op.state = OpState::Done;
    if op.abandoned {
        return;
    }
    if let Some(s) = op.suspension.take() {
        aio::ready(s);
    }
    OPS.with_borrow_mut(|o| o.insert(token, op));
}

/// aio: the socket of operation `token` is ready.
pub fn on_event(token: u64) {
    let Some(op) = OPS.with_borrow_mut(|o| o.remove(&token)) else { return };
    if !matches!(op.state, OpState::Sending | OpState::Receiving) {
        OPS.with_borrow_mut(|o| o.insert(token, op));
        return;
    }
    drive(token, op);
}

/// aio: the timer fired. Fails the operations past their deadline; returns the next deadline.
pub fn on_timer() -> Option<Instant> {
    let now = Instant::now();
    let late: Vec<u64> = OPS.with_borrow(|o| {
        o.iter().filter(|(_, op)| op.state != OpState::Done && op.deadline.is_some_and(|d| d <= now)).map(|(k, _)| *k).collect()
    });
    for token in late {
        let Some(op) = OPS.with_borrow_mut(|o| o.remove(&token)) else { continue };
        if op.state == OpState::Queued {
            unqueue(&op, token);
        }
        let e = Error::Timeout(format!("{}: socketTimeoutMS exceeded", op.addr));
        // the connection's state is unknown: closed by finish (Timeout is not a server error)
        finish(token, op, Err(e));
    }
    OPS.with_borrow(|o| o.values().filter(|op| op.state != OpState::Done).filter_map(|op| op.deadline).min())
}

fn unqueue(op: &Op, token: u64) {
    let mut tp = op.topo.borrow_mut();
    if let Some(s) = tp.servers.iter_mut().find(|s| s.addr == op.addr) {
        s.queue.retain(|&q| q != token);
    }
}

/// aio, at request end: operations still in flight are dropped, their connections closed.
pub fn abort_ops() {
    let ops: Vec<(u64, Op)> = OPS.with_borrow_mut(|o| o.drain().collect());
    IN_FLIGHT.set(0);
    for (token, op) in &ops {
        unqueue(op, *token);
    }
    for (_, mut op) in ops {
        if let Some(c) = op.conn.take() {
            release(&op.topo.clone(), c, false, false);
        }
    }
}

/// The caller of a pending operation is gone.
fn abandon(token: u64) {
    OPS.with_borrow_mut(|o| {
        if let Some(op) = o.get_mut(&token) {
            match op.state {
                OpState::Done => {
                    o.remove(&token);
                }
                OpState::Queued => {
                    let op = o.remove(&token).unwrap();
                    unqueue(&op, token);
                    if !op.grant {
                        IN_FLIGHT.set(IN_FLIGHT.get() - 1);
                    }
                }
                _ => {
                    op.abandoned = true;
                    op.suspension = None;
                }
            }
        }
    });
}

fn new_op(t: &TopoRef, addr: String, db: &str, msg: Vec<u8>, cursor: bool, grant: bool, deadline: Option<Instant>) -> Op {
    Op {
        topo: t.clone(),
        addr,
        db: db.to_owned(),
        msg,
        wire: Vec::new(),
        zipped: false,
        off: 0,
        conn: None,
        cursor,
        grant,
        replies: vec![],
        err: None,
        state: OpState::Queued,
        deadline,
        suspension: None,
        abandoned: false,
    }
}

/// One asynchronous attempt: the raw replies (first batch, then getMores).
fn async_once(t: &TopoRef, db: &str, body: &[u8], mode: Mode, tags: &[Tags], cursor: bool) -> R<Vec<Vec<u8>>> {
    let token = next_token();
    let (c, addr, rp) = acquire(t, mode, tags, token)?;
    let mut msg = Vec::new();
    build(&mut msg, body, db, rp, tags);
    let queued = c.is_none();
    // a queued operation fails after socketTimeoutMS like one in flight
    let op = new_op(t, addr, db, msg, cursor, false, if queued { deadline(t) } else { None });
    if let Some(d) = op.deadline {
        aio::timer_at(d);
    }
    OPS.with_borrow_mut(|o| o.insert(token, op));
    IN_FLIGHT.set(IN_FLIGHT.get() + 1);
    if let Some(c) = c {
        start(token, c);
    }
    loop {
        match OPS.with_borrow(|o| o.get(&token).map(|op| op.state)) {
            None => return Err(Error::Interrupted),
            Some(OpState::Done) => break,
            Some(_) => {}
        }
        let s = aio::get_suspension()?;
        OPS.with_borrow_mut(|o| {
            if let Some(op) = o.get_mut(&token) {
                op.suspension = Some(s.shallow_clone());
            }
        });
        if let Err(e) = aio::suspend(&s) {
            abandon(token);
            return Err(e);
        }
    }
    let op = OPS.with_borrow_mut(|o| o.remove(&token)).unwrap();
    match op.err {
        Some(e) => Err(e),
        None => Ok(op.replies),
    }
}

fn run_async(t: &TopoRef, db: &str, body: &[u8], mode: Option<Mode>, cursor: bool) -> R<Vec<Vec<u8>>> {
    allowed(body)?;
    let (mode, tags) = mode_tags(t, mode);
    match async_once(t, db, body, mode, &tags, cursor) {
        Err(e) if e.retryable() => {
            st(|s| s.retries.set(s.retries.get() + 1));
            async_once(t, db, body, mode, &tags, cursor)
        }
        r => r,
    }
}

/// A blocking checkout: an idle or new connection, or, with the pool exhausted, the first one
/// the asynchronous operations release (the event set is polled meanwhile: they progress
/// without their fibers).
fn checkout_sync(t: &TopoRef, mode: Mode, tags: &[Tags]) -> R<(Conn, Option<(Mode, bool)>)> {
    let t0 = Instant::now();
    let token = next_token();
    let (c, addr, rp) = acquire(t, mode, tags, token)?;
    let c = match c {
        Some(c) => {
            out_stats(&c);
            c
        }
        None => {
            let until = Instant::now() + t.borrow().opts.conn.socket_timeout.unwrap_or(Duration::from_secs(300));
            OPS.with_borrow_mut(|o| o.insert(token, new_op(t, addr.clone(), "", vec![], false, true, None)));
            loop {
                if OPS.with_borrow(|o| o.get(&token).map(|op| op.state)) == Some(OpState::Done) {
                    break OPS.with_borrow_mut(|o| o.remove(&token)).unwrap().conn.unwrap();
                }
                let in_flight = OPS.with_borrow(|o| o.values().any(|op| op.conn.is_some() && !op.grant));
                let now = Instant::now();
                if !in_flight || now >= until {
                    let op = OPS.with_borrow_mut(|o| o.remove(&token)).unwrap();
                    unqueue(&op, token);
                    return Err(if in_flight {
                        Error::Timeout(format!("{addr}: no connection released in time"))
                    } else {
                        Error::Net(format!("{addr}: pool exhausted with no operation in flight"))
                    });
                }
                aio::pump(((until - now).as_millis() as i32).clamp(1, 1000));
            }
        }
    };
    st(|s| s.wait_ns.set(s.wait_ns.get() + t0.elapsed().as_nanos() as i64));
    let gen = aio::GEN.with(|g| g.get());
    let mut c = c;
    c.gen = gen;
    let rec = (c.addr.clone(), gen, c.fd());
    t.borrow_mut().out.push(rec);
    Ok((c, rp))
}

/// Returns a blocking operation's connection.
fn checkin_sync(t: &TopoRef, c: Conn, ok: bool, state_change: bool) {
    {
        let mut tp = t.borrow_mut();
        let fd = c.fd();
        if let Some(p) = tp.out.iter().position(|o| o.2 == fd) {
            tp.out.swap_remove(p);
        }
    }
    release(t, c, ok, state_change);
}

/// Reads only: the command's name if it is allowed (find, aggregate without $out/$merge, count,
/// distinct), checked before anything is sent.
fn allowed(body: &[u8]) -> R<()> {
    let d = Doc::at(body)?;
    let name = match d.iter().next() {
        Some(e) => String::from_utf8_lossy(e?.0).into_owned(),
        None => String::new(),
    };
    let mut ok = matches!(name.as_str(), "find" | "aggregate" | "count" | "distinct");
    if ok && name == "aggregate" {
        if let Some(p) = d.doc("pipeline")? {
            for e in p.iter() {
                let (_, v) = e?;
                let Val::Doc(stage) = v else { continue };
                if let Some(first) = stage.iter().next() {
                    let k = first?.0;
                    if k == b"$out" || k == b"$merge" {
                        ok = false;
                    }
                }
            }
        }
    }
    if !ok {
        st(|s| s.rejected.set(s.rejected.get() + 1));
        wire("rejected:", &name);
        return Err(Error::Invalid(format!("this driver is reads-only: {name} is not allowed")));
    }
    wire("cmd:", &name);
    Ok(())
}

/// Builds the command message: the caller's BSON body + $db (+ $readPreference).
fn build(msg: &mut Vec<u8>, body: &[u8], db: &str, rp: Option<(Mode, bool)>, tag_sets: &[Tags]) {
    msg_begin(msg);
    let at = msg.len();
    msg.extend_from_slice(&body[..body.len() - 1]);
    bson::put_str(msg, "$db", db);
    if let Some((mode, with_tags)) = rp {
        let r = bson::begin_sub(msg, "$readPreference", false);
        bson::put_str(msg, "mode", mode.name());
        if with_tags {
            let a = bson::begin_sub(msg, "tags", true);
            for (n, ts) in tag_sets.iter().enumerate() {
                let d = bson::begin_sub(msg, &n.to_string(), false);
                for (k, v) in ts {
                    bson::put_str(msg, k, v);
                }
                bson::end(msg, d);
            }
            bson::end(msg, a);
        }
        bson::end(msg, r);
    }
    bson::end(msg, at);
    msg_end(msg);
}

fn get_more(msg: &mut Vec<u8>, id: i64, coll: &str, db: &str) {
    msg_begin(msg);
    let at = bson::begin(msg);
    bson::put_i64(msg, "getMore", id);
    bson::put_str(msg, "collection", coll);
    bson::put_str(msg, "$db", db);
    bson::end(msg, at);
    msg_end(msg);
}

/// The cursor of a reply: (id, collection) when there is more to fetch.
fn cursor_more(d: Doc) -> R<Option<(i64, String)>> {
    let cur = d.doc("cursor")?.ok_or_else(|| Error::bson("reply has no cursor"))?;
    let id = match cur.get("id")? {
        Some(Val::I64(i)) => i,
        Some(v) => v.num().unwrap_or(0.0) as i64,
        None => 0,
    };
    if id == 0 {
        return Ok(None);
    }
    let ns = cur.str("ns")?.ok_or_else(|| Error::bson("cursor has no ns"))?;
    Ok(Some((id, ns.split_once('.').map_or(ns, |(_, c)| c).to_owned())))
}

/// The batch (raw BSON array) of a cursor reply.
fn batch_of<'a>(d: Doc<'a>) -> R<&'a [u8]> {
    let cur = d.doc("cursor")?.ok_or_else(|| Error::bson("reply has no cursor"))?;
    Ok(cur.doc("firstBatch")?.or(cur.doc("nextBatch")?).ok_or_else(|| Error::bson("cursor has no batch"))?.0)
}

/// Runs `body` blocking on a server chosen by the read preference, with one retry for retryable
/// errors. Returns the checked-out connection holding the reply in `buf`.
fn run_first(t: &TopoRef, db: &str, body: &[u8], mode: Option<Mode>) -> R<Conn> {
    allowed(body)?;
    let (mode, tags) = mode_tags(t, mode);
    let mut msg = MSG.with_borrow_mut(std::mem::take);
    let mut attempt = 0;
    let res = loop {
        let r = (|| {
            let (mut c, rp) = checkout_sync(t, mode, &tags)?;
            build(&mut msg, body, db, rp, &tags);
            if let Err(e) = c.roundtrip(&msg) {
                checkin_sync(t, c, false, false);
                return Err(e);
            }
            match reply_doc(&c.buf).and_then(check_ok) {
                Ok(()) => Ok(c),
                Err(e) => {
                    let sc = e.state_change();
                    checkin_sync(t, c, !sc, sc);
                    Err(e)
                }
            }
        })();
        match r {
            Err(e) if attempt == 0 && e.retryable() => {
                st(|s| s.retries.set(s.retries.get() + 1));
                attempt += 1;
            }
            r => break r,
        }
    };
    MSG.with_borrow_mut(|m| *m = msg);
    res
}

/// A command; `out` gets the raw reply (ok: 1).
pub fn command(t: &TopoRef, db: &str, body: &[u8], mode: Option<Mode>, io: Io, out: impl FnOnce(&[u8]) -> R<()>) -> R<()> {
    if io == Io::Async {
        let mut replies = run_async(t, db, body, mode, false)?;
        let r = out(reply_doc(&replies[0])?.0);
        put_buf(replies.swap_remove(0));
        return r;
    }
    let c = run_first(t, db, body, mode)?;
    let r = out(reply_doc(&c.buf)?.0);
    checkin_sync(t, c, true, false);
    r
}

/// A cursor command, drained with getMore on the same connection; `batch` gets each batch's
/// raw BSON array. Blocking: decoded while the next getMore is in flight. Asynchronous: the
/// event loop drains the cursor, the batches are decoded when the fiber runs again.
pub fn cursor(t: &TopoRef, db: &str, body: &[u8], mode: Option<Mode>, io: Io, mut batch: impl FnMut(&[u8]) -> R<()>) -> R<()> {
    if io == Io::Async {
        let mut res = Ok(());
        for r in run_async(t, db, body, mode, true)? {
            if res.is_ok() {
                res = reply_doc(&r).and_then(batch_of).and_then(&mut batch);
            }
            put_buf(r);
        }
        return res;
    }
    let mut c = run_first(t, db, body, mode)?;
    let mut msg = Vec::new();
    let r = (|| loop {
        let (more, range) = {
            let d = reply_doc(&c.buf)?;
            let b = batch_of(d)?;
            let off = b.as_ptr() as usize - c.buf.as_ptr() as usize;
            (cursor_more(d)?, off..off + b.len())
        };
        let cur_buf = std::mem::replace(&mut c.buf, std::mem::take(&mut c.spare));
        if let Some((id, coll)) = &more {
            get_more(&mut msg, *id, coll, db);
            wire("cmd:", "getMore");
            c.send(&msg, true)?;
        }
        let r = batch(&cur_buf[range]);
        c.spare = cur_buf;
        r?;
        if more.is_none() {
            return Ok(());
        }
        c.recv()?;
        check_ok(reply_doc(&c.buf)?)?;
    })();
    match &r {
        Ok(()) => checkin_sync(t, c, true, false),
        Err(e @ Error::Server { .. }) => {
            let sc = e.state_change();
            checkin_sync(t, c, !sc, sc)
        }
        // network, decode or interruption mid-cursor: the connection's state is unknown
        Err(_) => checkin_sync(t, c, false, false),
    }
    r
}
