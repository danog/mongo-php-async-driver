//! A single-threaded client (libmongoc's mongoc_topology_t in single-threaded mode, its topology
//! scanner and the single-threaded cluster): blocking scans that obey the cooldown and check
//! every server at once (crate::scan), the server selection loop with serverSelectionTryOnce,
//! and one connection per server, shared by the scanner and the commands (as libmongoc does when
//! single-threaded).

use crate::log;
use std::cell::{Cell, RefCell, RefMut};
use std::rc::Rc;
use std::time::Duration;

use crate::error::{code, domain, Error};
use crate::handshake::{self, HelloOpts, Metadata};
use crate::net::{Conn, NetError};
use crate::sdam::{monotonic_usec, ServerDescription, ServerType, RTT_UNSET};
use crate::topology::{self as tp, AppError, OpType, ReadPrefs, TopologyDescription, TopologyType};
use crate::uri::Uri;

/// The client settings libmongoc reads from the URI (mongoc_topology_new / mongoc_cluster_init).
#[derive(Clone, Debug)]
pub struct Settings {
    pub connect_timeout_ms: i64,
    pub socket_timeout_ms: i64,
    pub server_selection_timeout_ms: i64,
    pub server_selection_try_once: bool,
    pub local_threshold_ms: i64,
    pub min_heartbeat_ms: i64,
    /// socketCheckIntervalMS: an idle connection is pinged before an operation selects it again
    pub socket_check_interval_ms: i64,
    pub metadata: Metadata,
    pub hello: HelloOpts,
    /// TLS settings (None: plain TCP)
    pub tls: Option<crate::tls::TlsOptions>,
}

/// MONGOC_DEFAULT_SOCKETTIMEOUTMS
pub const DEFAULT_SOCKET_TIMEOUT_MS: i64 = 1000 * 60 * 5;
/// MONGOC_TOPOLOGY_SOCKET_CHECK_INTERVAL_MS
pub const SOCKET_CHECK_INTERVAL_MS: i64 = 5000;
/// CHECK_CLOSED_DURATION_MSEC: an idle connection is checked for a peer close after this long
const CHECK_CLOSED_DURATION_MS: i64 = 1000;
/// DNS_CACHE_TIMEOUT_MS: a node's resolved addresses are reused for this long
const DNS_CACHE_TIMEOUT_MS: i64 = 10 * 60 * 1000;

impl Settings {
    pub fn from_uri(uri: &Uri) -> Self {
        let i = |k: &str, d: i64| uri.get_int32(k, d as i32) as i64;
        Settings {
            connect_timeout_ms: i("connecttimeoutms", tp::DEFAULT_CONNECT_TIMEOUT_MS),
            socket_timeout_ms: i("sockettimeoutms", DEFAULT_SOCKET_TIMEOUT_MS),
            server_selection_timeout_ms: i("serverselectiontimeoutms", tp::SERVER_SELECTION_TIMEOUT_MS),
            server_selection_try_once: uri.get_bool("serverselectiontryonce", true),
            local_threshold_ms: i("localthresholdms", tp::LOCAL_THRESHOLD_MS),
            min_heartbeat_ms: tp::MIN_HEARTBEAT_FREQUENCY_MS,
            socket_check_interval_ms: i("socketcheckintervalms", SOCKET_CHECK_INTERVAL_MS),
            metadata: Metadata { appname: uri.options.str("appname").map(str::to_owned), ..Default::default() },
            hello: HelloOpts {
                load_balanced: uri.get_bool("loadbalanced", false),
                compressors: uri.compressors.clone(),
                zlib_level: uri.get_int32("zlibcompressionlevel", -1),
                sasl_user: crate::auth::sasl_user(uri),
                ..Default::default()
            },
            tls: uri.tls().then(|| crate::tls::TlsOptions::from_uri(uri)),
        }
    }
}

fn ms(v: i64) -> Option<Duration> {
    (v > 0).then(|| Duration::from_millis(v as u64))
}

/// A scanner node (mongoc_topology_scanner_node_t) with the cluster node's connection.
struct Node {
    id: u32,
    addr: String,
    conn: Option<Conn>,
    /// the handshake's server description (handshake_sd)
    handshake: Option<ServerDescription>,
    last_failed: i64,
    last_error: Option<Error>,
    last_used: i64,
    /// the connection's authentication state (reset with every new connection)
    auth: NodeAuth,
    /// the server's saslSupportedMechs, negotiated in the node's first handshake
    mechs: Option<crate::auth::SaslMechs>,
    /// the resolved addresses and when (monotonic usec) they were resolved
    dns: Option<(Vec<std::net::SocketAddr>, i64)>,
    /// the address the last connection succeeded with (reused without happy eyeballs)
    successful: Option<std::net::SocketAddr>,
}

/// A scan in progress: its checks and its bounds.
struct ScanRun {
    probes: Vec<crate::scan::Probe>,
    /// addresses checked in this scan (lower case)
    seen: std::collections::HashSet<String>,
    /// addresses re-checked after a failure
    retried: std::collections::HashSet<String>,
    discovered: usize,
}

/// The authentication state of a node's connection (scanner_node->has_auth,
/// speculative_auth_response, scram).
#[derive(Default)]
struct NodeAuth {
    has_auth: bool,
    speculative: Vec<u8>,
    scram: Option<crate::auth::scram::Scram>,
}

/// The state every cluster of a client shares (mongoc_topology_t and the client-level state:
/// the topology description and its event queue, the scan bookkeeping, the server-session pool,
/// the operation and request counters). Borrowed only briefly: never across a network wait.
pub struct Shared {
    pub uri: Uri,
    pub settings: Settings,
    pub td: RefCell<TopologyDescription>,
    last_scan: Cell<i64>,
    stale: Cell<bool>,
    scan_error: RefCell<Option<Error>>,
    /// the scanner's error when the topology is invalid (mongoc_topology_scanner_valid)
    pub invalid: Option<Error>,
    /// The process that created the client: only it destroys the client with I/O
    /// (phongo_pclient_destroy).
    created_pid: u32,
    /// the server session pool
    pub sessions: RefCell<crate::session::Pool>,
    /// mongoc_cluster_t.operation_id
    pub operation_id: Cell<i64>,
    /// mongoc_cluster_t.request_id
    pub request_id: Cell<i32>,
    /// The TLS configuration (_mongoc_openssl_ctx_new at client creation; its error fails
    /// every connection)
    tls: Option<Result<std::sync::Arc<rustls::ClientConfig>, Error>>,
    /// SRV polling of a mongodb+srv:// client (crate::srv::initial's result)
    pub srv: Cell<Option<crate::srv::Polling>>,
    /// automatic encryption (mongoc_client_enable_auto_encryption): None for most clients.
    /// Taken out while a hook runs (see Client::take_auto_crypt).
    pub auto_crypt: RefCell<Option<Box<dyn crate::autocrypt::AutoCrypt>>>,
    /// The fiber (crate::aio::current) whose hook holds `auto_crypt`.
    crypt_holder: Cell<Option<usize>>,
    /// Pooled mode (libmongoc's mongoc_client_pool_t): set for good once two clusters run
    /// operations at once (fibers). Scans then use `monitor`, and a cluster connects only to the
    /// server an operation selected (Client::connect_pooled).
    pooled: Cell<bool>,
    /// The scanner nodes of pooled mode (libmongoc's server monitors, with their own
    /// connections), taken out by the cluster running a scan.
    monitor: RefCell<Vec<Node>>,
    /// A scan is running in some cluster; others wait for it (`scan_waiters`, park ids).
    scanning: Cell<bool>,
    scan_waiters: RefCell<Vec<u64>>,
}

impl Shared {
    /// Whether automatic encryption applies to a command of the calling fiber: configured, and
    /// not held by this fiber's own hook (the hook's nested commands, such as key-vault finds
    /// and listCollections, skip it, as libmongoc's).
    pub fn auto_crypt_applies(&self) -> bool {
        match self.crypt_holder.get() {
            Some(h) => h != crate::aio::current(),
            None => self.auto_crypt.borrow().is_some(),
        }
    }

    /// The automatic-encryption state for one hook call (None: it does not apply, see
    /// `auto_crypt_applies`). Held by another fiber's hook (which may suspend): waits for it,
    /// never skipping the hook; an interrupted wait is an error.
    pub fn take_auto_crypt(&self) -> Result<Option<Box<dyn crate::autocrypt::AutoCrypt>>, Error> {
        loop {
            let ac = self.auto_crypt.borrow_mut().take();
            if let Some(ac) = ac {
                self.crypt_holder.set(Some(crate::aio::current()));
                return Ok(Some(ac));
            }
            match self.crypt_holder.get() {
                Some(h) if h != crate::aio::current() => {
                    crate::aio::sleep(Duration::from_millis(1)).map_err(|_| interrupted_error())?;
                }
                _ => return Ok(None),
            }
        }
    }

    /// Returns the state `take_auto_crypt` gave.
    pub fn put_auto_crypt(&self, ac: Box<dyn crate::autocrypt::AutoCrypt>) {
        *self.auto_crypt.borrow_mut() = Some(ac);
        self.crypt_holder.set(None);
    }

    /// The next operation id (mongoc_cluster_t.operation_id after its increment).
    pub fn next_operation_id(&self) -> i64 {
        let id = self.operation_id.get() + 1;
        self.operation_id.set(id);
        id
    }
}

/// One cluster of a client (libmongoc's single-threaded mongoc_client_t: its cluster and
/// scanner nodes, one connection per server) over the shared state. An operation uses one
/// cluster exclusively; the shared state is reached through Deref.
pub struct Client {
    shared: Rc<Shared>,
    nodes: Vec<Node>,
    /// A scan was abandoned because its wait was interrupted (crate::aio::Interrupted).
    interrupted: bool,
    /// The error every operation of this (transient) cluster fails with before any I/O.
    fail: Option<Error>,
    /// What the running scan is for (the scan callbacks differ).
    mode: ScanMode,
    /// The error of a failed Client::connect_pooled.
    connect_error: Option<Error>,
    /// connect_pooled's failure is a network error other than name resolution: it gets the
    /// backpressure labels (_handle_network_error_connecting)
    connect_overloaded: bool,
    /// The reply of the last failed stream_for_server: pooled mode's backpressure labels, else
    /// empty (mongoc_cluster_stream_for_server's reply).
    stream_reply: Vec<u8>,
}

/// What `run_scan`'s checks are for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ScanMode {
    /// a scan of the cluster's own nodes (libmongoc's single-threaded scanner)
    Own,
    /// a scan of the pooled monitors (Shared::monitor): no authentication in the handshake
    Monitor,
    /// pooled mode's connection to one server (_cluster_add_node): no heartbeat events
    Connect,
}

impl std::ops::Deref for Client {
    type Target = Shared;
    #[inline]
    fn deref(&self) -> &Shared {
        &self.shared
    }
}

/// The most addresses discovered during one scan that join it (a replica set has at most 50
/// members).
pub const MAX_DISCOVERED_PER_SCAN: usize = 100;

fn addr_lc(nodes: &[Node], id: u32) -> String {
    nodes.iter().find(|n| n.id == id).map(|n| n.addr.to_ascii_lowercase()).unwrap_or_default()
}

/// _mongoc_server_selection_error
fn selection_error(msg: &str, scanner: Option<&Error>) -> Error {
    let m = match scanner {
        Some(e) if e.code != 0 => format!("{msg}: {}", e.message),
        _ => msg.to_owned(),
    };
    Error::new(domain::SERVER_SELECTION, code::SERVER_SELECTION_FAILURE, m)
}

pub fn split_host_port(addr: &str) -> (String, u16) {
    match addr.rsplit_once(':') {
        Some((h, p)) if !h.ends_with(']') || h.starts_with('[') => (h.trim_start_matches('[').trim_end_matches(']').to_owned(), p.parse().unwrap_or(27017)),
        _ => (addr.to_owned(), 27017),
    }
}

impl Client {
    /// mongoc_client_new_from_uri_with_error: the client's shared state and its first cluster.
    pub fn new(uri: Uri, settings: Settings) -> Result<Self, Error> {
        // an SRV URI's host name is checked before its lookup (crate::srv::initial)
        if uri.srv.is_none() {
            detect_nongenuine_hosts(&uri);
        }
        let tls = settings.tls.as_ref().map(crate::tls::client_config);
        let td = TopologyDescription::from_uri(&uri, true);
        let shared = Shared {
            uri,
            td: RefCell::new(td),
            settings,
            last_scan: Cell::new(0),
            stale: Cell::new(true),
            scan_error: RefCell::new(None),
            invalid: None,
            created_pid: std::process::id(),
            sessions: Default::default(),
            operation_id: Cell::new(0),
            request_id: Cell::new(0),
            tls,
            srv: Cell::new(None),
            auto_crypt: RefCell::new(None),
            crypt_holder: Cell::new(None),
            pooled: Cell::new(false),
            monitor: RefCell::new(Vec::new()),
            scanning: Cell::new(false),
            scan_waiters: RefCell::new(Vec::new()),
        };
        Ok(Client::cluster(Rc::new(shared)))
    }

    /// Another cluster of the client whose shared state is `shared` (no connections yet).
    pub fn cluster(shared: Rc<Shared>) -> Self {
        let mut c = Client { shared, nodes: Vec::new(), interrupted: false, fail: None, mode: ScanMode::Own, connect_error: None, connect_overloaded: false, stream_reply: Vec::new() };
        c.reconcile_nodes();
        c
    }

    /// The shared state.
    pub fn shared(&self) -> &Rc<Shared> {
        &self.shared
    }

    /// The topology description, borrowed mutably (never across a network wait).
    #[inline]
    pub fn td(&self) -> RefMut<'_, TopologyDescription> {
        self.shared.td.borrow_mut()
    }

    /// Whether this is the process that created the client (phongo_pclient_destroy).
    pub fn in_creating_process(&self) -> bool {
        self.created_pid == std::process::id()
    }

    /// The fd of the connection to `server_id` (tests).
    pub fn conn_fd(&self, server_id: u32) -> Option<crate::os::RawSock> {
        self.nodes.iter().find(|n| n.id == server_id).and_then(|n| n.conn.as_ref()).map(Conn::raw_fd)
    }

    /// The APM events not yet handed to the subscribers, in the order they happened.
    pub fn take_events(&mut self) -> Vec<crate::apm::Timed> {
        std::mem::take(&mut self.td().events.0)
    }

    /// Drops the queued events (no subscriber may observe them any more).
    pub fn clear_events(&mut self) {
        self.td().events.0.clear();
    }

    /// The monitoring part of mongoc_topology_destroy (ServerClosed, TopologyChanged,
    /// TopologyClosed), for a client being destroyed by the process that created it.
    pub fn close(&mut self) {
        if self.in_creating_process() {
            self.td().close();
        }
    }

    /// mongoc_topology_reconcile: scanner nodes for the description's servers. Returns the
    /// ids of the nodes added.
    fn reconcile_nodes(&mut self) -> Vec<u32> {
        let td = self.shared.td.borrow();
        self.nodes.retain(|n| td.servers.iter().any(|s| s.id == n.id));
        let mut added = Vec::new();
        for s in &td.servers {
            if !self.nodes.iter().any(|n| n.id == s.id) {
                self.nodes.push(Node { id: s.id, addr: s.addr.clone(), conn: None, handshake: None, last_failed: -1, last_error: None, last_used: -1, auth: NodeAuth::default(), mechs: None, dns: None, successful: None });
                added.push(s.id);
            }
        }
        added
    }

    /// _mongoc_topology_scanner_monitor_heartbeat_started (node setup)
    fn heartbeat_started(&mut self, id: u32) {
        if self.mode == ScanMode::Connect {
            return;
        }
        if let Some(n) = self.nodes.iter().find(|n| n.id == id) {
            let addr = n.addr.clone();
            self.td().events.push(|| crate::apm::Event::HeartbeatStarted { addr });
        }
    }

    fn node_in_cooldown(n: &Node, when: i64) -> bool {
        n.last_failed != -1 && n.last_failed + 1000 * tp::COOLDOWN_MS >= when
    }

    fn bypass_cooldown(&self) -> bool {
        self.td().ty == TopologyType::LoadBalanced
    }

    /// mongoc_topology_scanner_in_cooldown
    fn scanner_in_cooldown(&self, when: i64) -> bool {
        !self.bypass_cooldown() && self.nodes.iter().all(|n| Self::node_in_cooldown(n, when))
    }

    /// The monitoring hello on an established connection (not a handshake).
    fn check_command(&self, hello_ok: bool) -> Vec<u8> {
        let mut w = crate::bson::writer::Writer::with_capacity(64);
        if hello_ok || self.settings.hello.api_version.is_some() || self.settings.hello.load_balanced {
            w.int32(b"hello", 1);
        } else {
            w.int32(b"isMaster", 1);
            w.bool(b"helloOk", true);
        }
        w.utf8(b"$db", b"admin");
        w.finish()
    }

    /// mongoc_topology_scanner_node_setup: starts the check of node `id` (HeartbeatStarted).
    /// An existing connection gets the monitoring hello; else the address is resolved (cached
    /// for DNS_CACHE_TIMEOUT_MS) and one check per address starts, each HAPPY_EYEBALLS_DELAY
    /// after the previous one (only the address that last succeeded, when known). Name
    /// resolution and UNIX-socket connect errors fail the check at once.
    fn node_setup(&mut self, id: u32, run: &mut ScanRun) {
        use crate::scan::Probe;
        let Some(i) = self.nodes.iter().position(|n| n.id == id) else { return };
        self.heartbeat_started(id);
        let t0 = std::time::Instant::now();
        let timeout = ms(self.settings.connect_timeout_ms);
        // a pooled cluster's connection: its hello waits socketTimeoutMS for the reply
        let pooled = (self.mode == ScanMode::Connect).then(|| ms(self.settings.socket_timeout_ms));
        let pool = |p: Probe| match pooled {
            Some(t) => p.pooled(t),
            None => p,
        };
        if let Some(conn) = self.nodes[i].conn.take() {
            let hello_ok = self.nodes[i].handshake.as_ref().is_some_and(|h| h.hello_ok);
            let cmd = self.check_command(hello_ok);
            let probe = match crate::handshake::build_msg(None, &cmd, &[], false) {
                Ok(msg) => Probe::with_stream(id, conn, msg, timeout, false, None),
                Err(e) => return self.setup_failed(id, net_check_error(e), t0),
            };
            run.probes.push(probe);
            return;
        }
        // a new connection: new authentication state; single-threaded libmongoc negotiates the
        // mechanism in a node's first handshake and starts a speculative authentication in each
        // one (in the first check of the node only, when it has several addresses)
        // (pooled monitors do not authenticate: _server_monitor_setup_connection)
        let monitor = self.mode == ScanMode::Monitor;
        let negotiate = !monitor && self.nodes[i].mechs.is_none();
        let speculative = if monitor { None } else { crate::auth::speculative_start(&self.uri) };
        let with_spec = handshake::hello_command_auth(&self.settings.metadata, &self.settings.hello, false, negotiate, speculative.as_ref().map(|s| &s.0[..]));
        let plain = if speculative.is_some() { handshake::hello_command_auth(&self.settings.metadata, &self.settings.hello, false, negotiate, None) } else { Vec::new() };
        self.nodes[i].auth = NodeAuth { has_auth: false, speculative: Vec::new(), scram: speculative.and_then(|s| s.1) };
        let addr = self.nodes[i].addr.clone();
        let unix = addr.contains('/') && addr.contains(".sock");
        let host = if unix { addr.clone() } else { split_host_port(&addr).0 };
        let tls: crate::scan::TlsSetup = match &self.tls {
            None => None,
            Some(Err(e)) => Some(Err(e.clone())),
            Some(Ok(cfg)) => Some(crate::tls::server_name(&host).map(|n| (cfg.clone(), n))),
        };
        let msg = |first: bool| {
            let body = if first || plain.is_empty() { &with_spec } else { &plain };
            crate::handshake::build_msg(None, body, &[], false)
        };
        if unix {
            // the host parser's AF_UNIX rule (a '/' and ".sock"): a blocking connect at setup
            match Conn::connect_unix(&addr) {
                Ok(conn) => match msg(true) {
                    Ok(m) => run.probes.push(pool(Probe::with_stream(id, conn, m, timeout, true, tls))),
                    Err(e) => self.setup_failed(id, net_check_error(e), t0),
                },
                Err(e) => self.setup_failed(id, net_check_error(e), t0),
            }
            return;
        }
        let now = monotonic_usec();
        let n = &mut self.nodes[i];
        if n.dns.as_ref().is_some_and(|(_, at)| now - at > DNS_CACHE_TIMEOUT_MS * 1000) {
            n.dns = None;
            n.successful = None;
        }
        if n.dns.is_none() {
            let (h, port) = split_host_port(&addr);
            match crate::net::resolve(&h, port) {
                Ok(a) => n.dns = Some((a, now)),
                Err(_) => {
                    // the scanner quotes the host, mongoc_client_connect_tcp (pooled) does not
                    let m = if pooled.is_some() { format!("Failed to resolve {h}") } else { format!("Failed to resolve '{h}'") };
                    let e = Error::new(domain::STREAM, code::STREAM_NAME_RESOLUTION, m);
                    return self.setup_failed(id, e, t0);
                }
            }
        }
        let targets: Vec<std::net::SocketAddr> = match n.successful {
            Some(a) => vec![a],
            None => n.dns.as_ref().map(|d| d.0.clone()).unwrap_or_default(),
        };
        for (k, sa) in targets.into_iter().enumerate() {
            match msg(k == 0) {
                Ok(m) => run.probes.push(pool(Probe::connect(id, &addr, sa, crate::scan::HAPPY_EYEBALLS_DELAY * k as u32, m, timeout, tls.clone()))),
                Err(e) => return self.setup_failed(id, net_check_error(e), t0),
            }
        }
    }

    /// The setup of a check failed (mongoc_topology_scanner_node_setup's failure path and
    /// _mongoc_topology_scanner_setup_err_cb): HeartbeatFailed with the error as it is.
    fn setup_failed(&mut self, id: u32, e: Error, t0: std::time::Instant) {
        if self.mode == ScanMode::Connect {
            // _handle_network_error_connecting: only a name-resolution error invalidates the
            // server and clears its pool (not in load-balanced mode)
            if let Some(n) = self.nodes.iter().find(|n| n.id == id) {
                let addr = n.addr.clone();
                log::warning(log::CLUSTER, || format!("Failed connection to {addr} ({})", e.message));
            }
            let dns = e.domain == domain::STREAM && e.code == code::STREAM_NAME_RESOLUTION;
            if dns && self.td().ty != TopologyType::LoadBalanced {
                let mut td = self.td();
                td.invalidate_server(id, &e);
                td.clear_connection_pool(id, &crate::sdam::ZERO_OID);
            }
            self.connect_overloaded = !dns;
            self.connect_error = Some(e);
            return;
        }
        let Some(n) = self.nodes.iter_mut().find(|n| n.id == id) else { return };
        n.last_error = Some(e.clone());
        let addr = n.addr.clone();
        let duration_us = t0.elapsed().as_micros() as i64;
        let error = e.clone();
        self.td().events.push(|| crate::apm::Event::HeartbeatFailed { addr, duration_us, error });
        if self.td().ty != TopologyType::LoadBalanced {
            self.td().handle_hello(id, None, RTT_UNSET, Some(&e));
        }
    }

    /// mongoc_async_run over the scan's checks: polls every started check at once, starts the
    /// delayed ones when due, times out the late ones. Checks started by the callbacks (retries,
    /// discovered servers) join the loop.
    fn run_scan(&mut self, run: &mut ScanRun) {
        use crate::scan::State;
        let start = std::time::Instant::now();
        for p in &mut run.probes {
            p.start = start;
        }
        let mut fds: Vec<crate::os::PollFd> = Vec::new();
        let mut polled: Vec<usize> = Vec::new();
        while !run.probes.is_empty() {
            fds.clear();
            polled.clear();
            let mut timer: Option<std::time::Instant> = None;
            let soonest = |t: &mut Option<std::time::Instant>, at: std::time::Instant| *t = Some(t.map_or(at, |x| x.min(at)));
            let now = std::time::Instant::now();
            let mut i = 0;
            while i < run.probes.len() {
                if !run.probes[i].done && run.probes[i].state == State::PendingConnect {
                    if run.probes[i].delay_until <= now {
                        if !run.probes[i].initiate() {
                            self.probe_failed(run, i, "connection error");
                        }
                    } else {
                        soonest(&mut timer, run.probes[i].delay_until);
                    }
                }
                let p = &run.probes[i];
                if !p.done {
                    if let Some(fd) = p.fd() {
                        fds.push(crate::os::pollfd(fd, p.events));
                        polled.push(i);
                        if let Some(d) = p.deadline() {
                            soonest(&mut timer, d);
                        }
                    }
                }
                i += 1;
            }
            if run.probes.iter().all(|p| p.done) {
                break;
            }
            let waited = if fds.is_empty() {
                match timer {
                    Some(t) => crate::aio::sleep(t.saturating_duration_since(std::time::Instant::now())).map(|()| 0),
                    None => Ok(0),
                }
            } else {
                crate::aio::poll(&mut fds, timer)
            };
            if let Err(e) = &waited {
                if crate::aio::is_interrupted(e) {
                    // the scan is abandoned: its connections close, nothing more is learnt
                    run.probes.clear();
                    self.interrupted = true;
                    return;
                }
            }
            if let Err(e) = waited {
                // no readiness can be learnt: every polled check fails
                for &i in &polled {
                    run.probes[i].error = Some(Error::new(domain::STREAM, code::STREAM_SOCKET, e.to_string()));
                    run.probes[i].state = State::Error;
                }
            }
            for (k, &i) in polled.iter().enumerate() {
                let rev = fds[k].revents;
                if run.probes[i].done {
                    continue;
                }
                // a hang-up with data still to read (macOS reports a peer's close so, Linux
                // does not) is read first: the reply, or the TLS alert, says more
                let failed = rev & crate::os::POLLERR != 0 || (rev & crate::os::POLLHUP != 0 && rev & crate::os::POLLIN == 0);
                if !failed && rev & run.probes[i].events != 0 {
                    run.probes[i].connected = true;
                }
                if failed {
                    // during the TLS handshake the handshake's own error is reported ("TLS
                    // handshake failed: ...", DIVERGENCES "TLS errors of server checks")
                    if run.probes[i].state == State::Setup {
                        if let crate::scan::Step::Progress = run.probes[i].step() {
                            run.probes[i].poll_error(rev & crate::os::POLLHUP != 0);
                        }
                        self.probe_failed(run, i, "connection error");
                        continue;
                    }
                    run.probes[i].poll_error(rev & crate::os::POLLHUP != 0);
                }
                if rev & run.probes[i].events != 0 || run.probes[i].state == State::Error {
                    self.run_probe(run, i);
                }
            }
            let now = std::time::Instant::now();
            let mut i = 0;
            while i < run.probes.len() {
                let p = &mut run.probes[i];
                if !p.done {
                    if p.state != State::PendingConnect && p.timed_out(now) {
                        p.timeout_error();
                        self.probe_failed(run, i, "connection timeout");
                    } else if p.state == State::Cancelled {
                        self.probe_failed(run, i, "connection error");
                    }
                }
                i += 1;
            }
            run.probes.retain(|p| !p.done);
        }
    }

    /// mongoc_async_cmd_run for a ready check.
    fn run_probe(&mut self, run: &mut ScanRun, i: usize) {
        if run.probes[i].state == crate::scan::State::Send {
            self.probe_connected(run, i);
        }
        match run.probes[i].step() {
            crate::scan::Step::Progress => {}
            crate::scan::Step::Success(reply) => self.probe_succeeded(run, i, reply),
            crate::scan::Step::Failed => self.probe_failed(run, i, "connection error"),
        }
    }

    /// _async_connected: the first of a node's checks to connect cancels the others.
    fn probe_connected(&mut self, run: &mut ScanRun, i: usize) {
        let (node, addr) = (run.probes[i].node, run.probes[i].addr);
        for (k, p) in run.probes.iter_mut().enumerate() {
            if k != i && p.node == node && !p.done {
                p.cancel();
            }
        }
        if let Some(n) = self.nodes.iter_mut().find(|n| n.id == node) {
            n.successful = addr;
        }
    }

    /// _async_success: HeartbeatSucceeded, the connection becomes the node's (a new one
    /// completes its handshake), the topology learns the reply.
    fn probe_succeeded(&mut self, run: &mut ScanRun, i: usize, reply: Vec<u8>) {
        let p = &mut run.probes[i];
        p.done = true;
        let elapsed = p.start.elapsed();
        let (id, fresh) = (p.node, p.fresh);
        let Some(mut conn) = p.conn.take() else { return };
        // a server removed during the scan: its connection is closed
        let Some(ni) = self.nodes.iter().position(|n| n.id == id) else { return };
        let duration_us = elapsed.as_micros() as i64;
        let rtt = elapsed.as_millis() as i64;
        let speculative_reply = if fresh { crate::auth::speculative_reply(&reply) } else { Vec::new() };
        // the speculativeAuthenticate reply is seen by the authentication only
        let reply = crate::auth::redact_hello(&reply).into_owned();
        let n = &mut self.nodes[ni];
        n.last_used = monotonic_usec();
        n.last_failed = -1;
        let addr = n.addr.clone();
        if self.mode == ScanMode::Connect {
            return self.connected_pooled(ni, conn, &reply, speculative_reply, rtt);
        }
        self.td().events.push(|| crate::apm::Event::HeartbeatSucceeded { addr, duration_us, reply: reply.clone() });
        let n = &mut self.nodes[ni];
        if fresh {
            if n.mechs.is_none() && self.shared.settings.hello.sasl_user.is_some() {
                n.mechs = Some(crate::auth::parse_sasl_mechs(&reply));
            }
            n.auth.speculative = speculative_reply;
            let sd = ServerDescription::from_hello(&n.addr, &reply, rtt);
            conn.set_max_message_size(sd.max_msg_size);
            conn.compressor = handshake::negotiated(&sd, &self.shared.settings.hello);
            n.handshake = Some(sd);
        }
        n.conn = Some(conn);
        // in load balanced mode, scanning only establishes connections
        if self.td().ty != TopologyType::LoadBalanced {
            self.td().handle_hello(id, Some(&reply), rtt, None);
            self.join_discovered(run);
        }
    }

    /// _async_error_or_timeout: a node fails when its last check fails without a connection
    /// (HeartbeatFailed, cooldown, pool cleared, a known server re-checked once); a failed
    /// check with siblings left starts them sooner.
    fn probe_failed(&mut self, run: &mut ScanRun, i: usize, default_msg: &str) {
        let p = &mut run.probes[i];
        p.done = true;
        let elapsed = p.start.elapsed();
        let id = p.node;
        let in_hello = p.in_hello;
        let error = p.error.take();
        drop(p.conn.take());
        let Some(ni) = self.nodes.iter().position(|n| n.id == id) else { return };
        let now = monotonic_usec();
        self.nodes[ni].last_used = now;
        let siblings = run.probes.iter().filter(|q| q.node == id && !q.done).count();
        if self.mode == ScanMode::Connect && siblings == 0 {
            // _cluster_add_node: the stream's or the hello's error as it is, with the
            // backpressure labels; the topology is left alone (an application error labelled
            // SystemOverloadedError is ignored). A failure to connect is logged.
            let addr = self.nodes[ni].addr.clone();
            let e = error.unwrap_or_else(|| Error::new(domain::STREAM, code::STREAM_CONNECT, format!("Failed to connect to target host: {addr}")));
            if !in_hello {
                log::warning(log::CLUSTER, || format!("Failed connection to {addr} ({})", e.message));
            }
            self.connect_overloaded = true;
            self.connect_error = Some(e);
            return;
        }
        if self.nodes[ni].conn.is_some() || siblings > 0 {
            let back = crate::scan::HAPPY_EYEBALLS_DELAY;
            for q in run.probes.iter_mut().filter(|q| q.node == id && !q.done) {
                q.delay_until = q.delay_until.checked_sub(back).unwrap_or(q.delay_until);
            }
            return;
        }
        let n = &mut self.nodes[ni];
        n.last_failed = now;
        n.dns = None;
        n.successful = None;
        n.handshake = None;
        let raw = error.unwrap_or_else(|| Error::new(domain::STREAM, code::STREAM_CONNECT, default_msg));
        let last = Error::new(domain::CLIENT, code::STREAM_CONNECT, format!("{} calling hello on '{}'", raw.message, n.addr));
        n.last_error = Some(last.clone());
        let addr = n.addr.clone();
        let duration_us = elapsed.as_micros() as i64;
        self.td().events.push(|| crate::apm::Event::HeartbeatFailed { addr, duration_us, error: last });
        if self.td().ty == TopologyType::LoadBalanced {
            return;
        }
        // a failed check clears the pool; a known server becomes Unknown and is checked once
        // more in this scan (_mongoc_topology_scanner_cb)
        self.td().clear_connection_pool(id, &crate::sdam::ZERO_OID);
        let known = self.td().server(id).is_some_and(|s| s.ty != ServerType::Unknown);
        self.td().handle_hello(id, None, RTT_UNSET, Some(&raw));
        if known && run.retried.insert(addr_lc(&self.nodes, id)) {
            self.node_setup(id, run);
        } else {
            self.join_discovered(run);
        }
    }

    /// mongoc_topology_reconcile during a scan: servers the topology gained join the scan
    /// (bounded, see scan_once).
    fn join_discovered(&mut self, run: &mut ScanRun) {
        for added in self.reconcile_nodes() {
            let a = addr_lc(&self.nodes, added);
            if run.discovered >= MAX_DISCOVERED_PER_SCAN || !run.seen.insert(a) {
                continue;
            }
            run.discovered += 1;
            self.node_setup(added, run);
        }
    }

    /// mongoc_topology_scan_once: one scan of every node, all checked at once (libmongoc's
    /// single-threaded scanner: non-blocking connects and hellos in one poll loop). Every node's
    /// heartbeat starts first (a failed name resolution fails at once), nodes discovered during
    /// the scan join it, and a known server whose check fails is checked once more before the
    /// scan ends.
    ///
    /// Bounded (safety): an address is checked at most once per scan plus one retry (libmongoc
    /// also never re-adds an address within a scan), and at most MAX_DISCOVERED_PER_SCAN
    /// addresses discovered during the scan join it (libmongoc has no such bound: hello replies
    /// that keep inventing hosts would make its scan endless). The others are checked by later
    /// scans.
    ///
    /// One scan runs at a time: a cluster needing one while another cluster's scan runs (fibers)
    /// waits for that scan instead. In pooled mode the scan checks the monitors
    /// (Shared::monitor), not the cluster's own connections.
    pub fn scan_once(&mut self, obey_cooldown: bool) {
        if self.shared.scanning.get() && crate::aio::active() {
            return self.wait_for_scan();
        }
        self.shared.scanning.set(true);
        if self.shared.pooled.get() {
            let mut own = std::mem::take(&mut *self.shared.monitor.borrow_mut());
            std::mem::swap(&mut self.nodes, &mut own);
            self.mode = ScanMode::Monitor;
            self.scan_nodes(obey_cooldown);
            self.mode = ScanMode::Own;
            std::mem::swap(&mut self.nodes, &mut own);
            *self.shared.monitor.borrow_mut() = own;
            self.reconcile_nodes();
        } else {
            self.scan_nodes(obey_cooldown);
        }
        self.shared.scanning.set(false);
        let waiters = std::mem::take(&mut *self.shared.scan_waiters.borrow_mut());
        for id in waiters {
            crate::aio::unpark(id);
        }
    }

    /// Waits for the scan another cluster runs (at most serverSelectionTimeoutMS); an
    /// interrupted wait abandons the operation as an interrupted scan does.
    fn wait_for_scan(&mut self) {
        let shared = self.shared.clone();
        let deadline = std::time::Instant::now() + Duration::from_millis(shared.settings.server_selection_timeout_ms.max(0) as u64);
        let mut my_id = None;
        let r = crate::aio::park(
            &mut |id| {
                my_id = Some(id);
                shared.scan_waiters.borrow_mut().push(id);
            },
            Some(deadline),
        );
        if let Some(id) = my_id {
            shared.scan_waiters.borrow_mut().retain(|w| *w != id);
        }
        if r.is_err() {
            self.interrupted = true;
        }
    }

    /// The scan itself (scan_once).
    fn scan_nodes(&mut self, obey_cooldown: bool) {
        // mongoc_topology_should_rescan_srv / mongoc_topology_rescan_srv: before the scan
        if let Some(mut p) = self.srv.get() {
            let (ty, heartbeat) = { let td = self.td(); (td.ty, td.heartbeat_msec) };
            if matches!(ty, TopologyType::Sharded | TopologyType::Unknown) {
                if let Some(hosts) = crate::srv::rescan(&self.uri, &mut p, heartbeat) {
                    let mut td = self.td();
                    let hosts = td.srv_order(&hosts);
                    td.reconcile(&hosts);
                }
                self.srv.set(Some(p));
            }
        }
        self.reconcile_nodes();
        let now = monotonic_usec();
        let ids: Vec<u32> = self.nodes.iter().filter(|n| !(obey_cooldown && !self.bypass_cooldown() && Self::node_in_cooldown(n, now))).map(|n| n.id).collect();
        let seen = ids.iter().filter_map(|id| self.nodes.iter().find(|n| n.id == *id)).map(|n| n.addr.to_ascii_lowercase()).collect();
        let mut run = ScanRun { probes: Vec::new(), seen, retried: Default::default(), discovered: 0 };
        for id in ids {
            self.node_setup(id, &mut run);
        }
        self.run_scan(&mut run);
        if self.interrupted {
            return;
        }
        // _mongoc_topology_scanner_finish: "[e1] [e2]", last domain/code win
        let mut msg = String::new();
        let mut last: Option<&Error> = None;
        for n in &self.nodes {
            if let Some(e) = &n.last_error {
                if !msg.is_empty() {
                    msg.push(' ');
                }
                msg.push_str(&format!("[{}]", e.message));
                last = Some(e);
            }
        }
        *self.scan_error.borrow_mut() = last.map(|e| Error::new(e.domain, e.code, msg));
        self.last_scan.set(monotonic_usec());
        self.stale.set(false);
    }

    /// mongoc_cluster_check_interval (single-threaded): before an operation that selected
    /// `server_id` by operation type uses its connection, a connection idle for a second is
    /// checked for a peer close, and one idle for socketCheckIntervalMS is pinged (unmonitored).
    /// A failed check disconnects and invalidates the server: false, the caller selects once
    /// more. False also when the server has no connection (as libmongoc).
    pub fn check_interval(&mut self, server_id: u32) -> bool {
        // pooled: the monitors check the servers
        if self.shared.pooled.get() {
            return true;
        }
        let Some(n) = self.nodes.iter().find(|n| n.id == server_id) else { return false };
        let Some(conn) = n.conn.as_ref() else { return false };
        let now = monotonic_usec();
        let idle_from = n.last_used;
        if idle_from + 1000 * CHECK_CLOSED_DURATION_MS < now && conn.check_closed() {
            let e = Error::new(domain::STREAM, code::STREAM_SOCKET, "connection closed");
            self.disconnect_node(server_id);
            self.td().invalidate_server(server_id, &e);
            return false;
        }
        if idle_from + 1000 * self.settings.socket_check_interval_ms < now {
            let mut w = crate::bson::writer::Writer::with_capacity(16);
            w.int32(b"ping", 1);
            let body = w.finish();
            let parts = crate::cmdparts::Parts { prohibit_lsid: true, ..crate::cmdparts::Parts::new("admin", &body) };
            let r = self.assemble(parts, server_id, false).and_then(|cmd| {
                let reply = self.exchange(server_id, &cmd.name, &cmd.db, &cmd.body, &[], true)?.unwrap_or_default();
                self.td().update_cluster_time(&reply);
                crate::cmd::check_ok(&reply).map_err(|e| e.into_error())
            });
            if let Err(e) = r {
                self.disconnect_node(server_id);
                self.td().invalidate_server(server_id, &e);
                return false;
            }
        }
        true
    }

    /// Before an operation's first I/O: a transient cluster's error, the event-loop check.
    #[inline]
    fn check_entry(&self) -> Result<(), Error> {
        if let Some(e) = &self.fail {
            return Err(e.clone());
        }
        crate::aio::require()
    }

    /// The interrupted scan's error (the flag is reset).
    fn take_interrupted(&mut self) -> Result<(), Error> {
        if std::mem::take(&mut self.interrupted) {
            return Err(interrupted_error());
        }
        Ok(())
    }

    /// mongoc_cluster_disconnect_node
    fn disconnect_node(&mut self, server_id: u32) {
        if let Some(n) = self.nodes.iter_mut().find(|n| n.id == server_id) {
            n.conn = None;
            n.handshake = None;
        }
    }

    /// mongoc_topology_select_server_id (single-threaded): a server-selection error ends with
    /// the topology type the selection started with.
    pub fn select_server_id(&mut self, optype: OpType, rp: Option<&ReadPrefs>, deprioritized: &[u32]) -> Result<(u32, bool), Error> {
        let topology_type = self.td().ty;
        self.select_server_id_inner(optype, rp, deprioritized).map_err(|mut e| {
            if e.domain == domain::SERVER_SELECTION {
                e.append(&format!(". Topology type: {}", topology_type.name()));
            }
            e
        })
    }

    fn select_server_id_inner(&mut self, optype: OpType, rp: Option<&ReadPrefs>, deprioritized: &[u32]) -> Result<(u32, bool), Error> {
        const TIMEOUT_MSG: &str = "No suitable servers found: `serverSelectionTimeoutMS` expired";
        self.check_entry()?;
        if let Some(e) = &self.invalid {
            return Err(Error::new(domain::SERVER_SELECTION, code::SERVER_SELECTION_FAILURE, e.message.clone()));
        }
        if self.td().ty == TopologyType::LoadBalanced {
            return self.select_loadbalanced();
        }
        let hb = self.td().heartbeat_msec;
        let try_once = self.settings.server_selection_try_once;
        let loop_start = monotonic_usec();
        let mut loop_end = loop_start;
        let expire_at = loop_start + self.settings.server_selection_timeout_ms * 1000;
        self.td().open();
        let mut tried_once = false;
        if self.last_scan.get() + hb * 1000 < loop_start {
            self.stale.set(true);
        }
        loop {
            if self.stale.get() {
                let scan_ready = self.last_scan.get() + self.settings.min_heartbeat_ms * 1000;
                if scan_ready > expire_at && !try_once {
                    return Err(selection_error(TIMEOUT_MSG, self.scan_error.borrow().as_ref()));
                }
                let sleep = scan_ready - loop_end;
                if sleep > 0 {
                    if try_once && self.scanner_in_cooldown(scan_ready) {
                        return Err(selection_error("No servers yet eligible for rescan", self.scan_error.borrow().as_ref()));
                    }
                    crate::aio::sleep(Duration::from_micros(sleep as u64)).map_err(|_| interrupted_error())?;
                }
                self.scan_once(true);
                self.take_interrupted()?;
                loop_end = self.last_scan.get();
                tried_once = true;
            }
            self.td().compatible(rp)?;
            let (sel, must) = self.td().select(optype, rp, deprioritized, self.settings.local_threshold_ms);
            if let Some(id) = sel {
                if self.td().cluster_time.is_none() {
                    let mut td = self.td();
                    // the hello is moved out and back (not copied: this runs on every selection
                    // while no $clusterTime is known, i.e. always on a standalone)
                    if let Some(h) = td.server_mut(id).map(|s| std::mem::take(&mut s.hello)) {
                        td.update_cluster_time(&h);
                        if let Some(s) = td.server_mut(id) {
                            s.hello = h;
                        }
                    }
                }
                return Ok((id, must));
            }
            self.stale.set(true);
            if try_once {
                if tried_once {
                    return Err(selection_error("No suitable servers found (`serverSelectionTryOnce` set)", self.scan_error.borrow().as_ref()));
                }
            } else {
                loop_end = monotonic_usec();
                if loop_end > expire_at {
                    return Err(selection_error(TIMEOUT_MSG, self.scan_error.borrow().as_ref()));
                }
            }
        }
    }

    /// _mongoc_topology_select_server_id_loadbalanced
    fn select_loadbalanced(&mut self) -> Result<(u32, bool), Error> {
        self.td().open();
        let (sel, _) = self.td().select(OpType::Write, None, &[], 0);
        let Some(id) = sel else { return Err(selection_error("No suitable server found in load balanced deployment", None)) };
        let has_conn = |c: &Self| c.nodes.iter().any(|n| n.id == id && n.conn.is_some());
        if !has_conn(self) {
            self.scan_once(true);
            self.take_interrupted()?;
        }
        if !has_conn(self) {
            let addr = self.td().server(id).map(|s| s.addr.clone()).unwrap_or_default();
            let m = match &*self.scan_error.borrow() {
                Some(e) if e.code != 0 => format!("Could not establish stream for node {addr}: {}", e.message),
                _ => format!("Could not establish stream for node {addr}"),
            };
            return Err(Error::new(domain::STREAM, code::STREAM_NOT_ESTABLISHED, m));
        }
        Ok((id, false))
    }

    /// _cluster_fetch_stream_single: the connection to `server_id` (reconnecting through a
    /// blocking scan when allowed).
    pub fn stream_for_server(&mut self, server_id: u32, reconnect_ok: bool) -> Result<&mut Conn, Error> {
        self.stream_reply.clear();
        self.check_entry()?;
        let pooled = self.shared.pooled.get();
        if pooled && !self.nodes.iter().any(|n| n.id == server_id) {
            // servers the monitors found since this cluster last looked
            self.reconcile_nodes();
        }
        let Some(i) = self.nodes.iter().position(|n| n.id == server_id) else {
            return Err(Error::new(domain::COMMAND, code::COMMAND_INVALID_ARG, format!("Could not find server with id: {server_id}")));
        };
        if pooled && self.nodes[i].conn.is_some() && self.stale_generation(i) {
            // _cluster_fetch_stream_pooled: the pool was cleared since the connection was made
            self.disconnect_node(server_id);
        }
        if self.nodes[i].conn.is_none() {
            let addr = self.nodes[i].addr.clone();
            if !reconnect_ok {
                return Err(Error::new(domain::STREAM, code::STREAM_NOT_ESTABLISHED, format!("Could not find stream for node {addr}")));
            }
            if pooled {
                self.connect_pooled(server_id)?;
                return self.stream_for_server(server_id, false);
            }
            self.scan_once(true);
            self.take_interrupted()?;
            if let Some(e) = self.scan_error.borrow().as_ref().filter(|e| e.code != 0) {
                return Err(e.clone());
            }
            if !self.nodes.iter().any(|n| n.id == server_id && n.conn.is_some()) {
                return Err(Error::new(domain::STREAM, code::STREAM_NOT_ESTABLISHED, format!("Could not find stream for node {addr}")));
            }
        }
        let n = self.nodes.iter_mut().find(|n| n.id == server_id).expect("checked above");
        if let Some(h) = &n.handshake {
            if h.ty == ServerType::Unknown {
                return Err(h.error.clone().unwrap_or_default());
            }
        }
        // stream open but not authenticated: first use since connecting. A connection that
        // should be authenticated is never used unauthenticated.
        if crate::auth::requires_auth(&self.shared.uri) && !n.auth.has_auth {
            if let Err(e) = self.authenticate_node(server_id) {
                if pooled {
                    // _cluster_add_node: the new connection is closed
                    if let Some(n) = self.nodes.iter().find(|n| n.id == server_id) {
                        let addr = n.addr.clone();
                        log::warning(log::CLUSTER, || format!("Failed authentication to {addr} ({})", e.message));
                    }
                    self.disconnect_node(server_id);
                }
                return Err(e);
            }
        }
        let lb = self.td().ty == TopologyType::LoadBalanced;
        let n = self.nodes.iter_mut().find(|n| n.id == server_id).expect("checked above");
        // a load-balanced topology needs a server that reports a serviceId: else disconnect
        if lb && n.handshake.as_ref().is_none_or(|h| h.service_id == crate::sdam::ZERO_OID) {
            n.conn = None;
            n.handshake = None;
            return Err(Error::new(domain::CLIENT, code::CLIENT_INVALID_LOAD_BALANCER, "Driver attempted to initialize in load balancing mode, but the server does not support this mode."));
        }
        n.conn.as_mut().ok_or_else(|| Error::new(domain::STREAM, code::STREAM_NOT_ESTABLISHED, format!("Could not find stream for node {}", n.addr)))
    }

    /// Whether node `i`'s connection predates a clear of its server's pool (or the server left
    /// the topology).
    fn stale_generation(&self, i: usize) -> bool {
        let n = &self.nodes[i];
        let (Some(conn), Some(h)) = (n.conn.as_ref(), n.handshake.as_ref()) else { return false };
        let td = self.shared.td.borrow();
        match td.server(n.id) {
            Some(s) => (conn.generation as u32) < s.generation(&h.service_id),
            None => true,
        }
    }

    /// _cluster_add_node (pooled mode): a new connection to `server_id` only, with its
    /// handshake (no heartbeat events; the handshake updates the topology).
    fn connect_pooled(&mut self, server_id: u32) -> Result<(), Error> {
        self.mode = ScanMode::Connect;
        self.connect_error = None;
        self.connect_overloaded = false;
        // nothing joins this check: discovered servers are the monitors' business
        let mut run = ScanRun { probes: Vec::new(), seen: Default::default(), retried: Default::default(), discovered: MAX_DISCOVERED_PER_SCAN };
        self.node_setup(server_id, &mut run);
        self.run_scan(&mut run);
        self.mode = ScanMode::Own;
        self.take_interrupted()?;
        match self.connect_error.take() {
            Some(e) => {
                if self.connect_overloaded {
                    crate::cmdparts::add_error_label(&mut self.stream_reply, "SystemOverloadedError");
                    crate::cmdparts::add_error_label(&mut self.stream_reply, "RetryableError");
                }
                Err(e)
            }
            None => Ok(()),
        }
    }

    /// A failed stream_for_server as an operation's failure: the error with the stream's reply
    /// (pooled mode's backpressure labels).
    pub fn stream_failure(&mut self, error: Error) -> crate::cmdparts::Failure {
        crate::cmdparts::Failure { error, reply: std::mem::take(&mut self.stream_reply) }
    }

    /// The success of connect_pooled's check: the node gets the connection and its handshake
    /// description; _mongoc_topology_update_from_handshake.
    fn connected_pooled(&mut self, ni: usize, mut conn: Conn, reply: &[u8], speculative: Vec<u8>, rtt: i64) {
        let n = &mut self.nodes[ni];
        let id = n.id;
        if n.mechs.is_none() && self.shared.settings.hello.sasl_user.is_some() {
            n.mechs = Some(crate::auth::parse_sasl_mechs(reply));
        }
        n.auth.speculative = speculative;
        let sd = ServerDescription::from_hello(&n.addr, reply, rtt);
        conn.set_max_message_size(sd.max_msg_size);
        conn.compressor = handshake::negotiated(&sd, &self.shared.settings.hello);
        let addr = n.addr.clone();
        let mut td = self.shared.td.borrow_mut();
        if td.ty != TopologyType::LoadBalanced {
            td.handle_hello(id, Some(reply), rtt, None);
            if td.server(id).is_none() {
                drop(td);
                self.connect_error = Some(Error::new(domain::STREAM, code::STREAM_NOT_ESTABLISHED, format!("\"{addr}\" removed from topology")));
                return;
            }
        }
        conn.generation = td.server(id).map_or(0, |s| s.generation(&sd.service_id)) as u64;
        drop(td);
        let n = &mut self.nodes[ni];
        n.handshake = Some(sd);
        n.conn = Some(conn);
    }

    /// The authentication part of _cluster_fetch_stream_single: completes the handshake's
    /// speculative conversation, else authenticates explicitly.
    fn authenticate_node(&mut self, server_id: u32) -> Result<(), Error> {
        let uri = self.uri.clone();
        let Some(i) = self.nodes.iter().position(|n| n.id == server_id) else { return Ok(()) };
        let mut st = std::mem::take(&mut self.nodes[i].auth);
        let mechs = self.nodes[i].mechs.unwrap_or_default();
        let mut run = |db: &str, body: &[u8]| self.auth_command(server_id, db, body);
        let speculative = crate::auth::finish_speculative(&uri, &st.speculative, st.scram.as_mut(), &mut run);
        drop(st);
        let connected = self.nodes.iter().any(|n| n.id == server_id && n.conn.is_some());
        match speculative {
            Ok(true) => {}
            Err(e) if !connected => return Err(e),
            _ => {
                let mut run = |db: &str, body: &[u8]| self.auth_command(server_id, db, body);
                crate::auth::authenticate(&uri, mechs, &mut run)?;
            }
        }
        if let Some(n) = self.nodes.iter_mut().find(|n| n.id == server_id) {
            n.auth.has_auth = true;
        }
        Ok(())
    }

    /// _mongoc_cluster_run_scram_command / the run of _mongoc_cluster_auth_node_plain and
    /// _x509 (before their error conversion): an authentication command on the connection being
    /// authenticated, with the given $db, no lsid.
    fn auth_command(&mut self, server_id: u32, db: &str, body: &[u8]) -> Result<Vec<u8>, Error> {
        let parts = crate::cmdparts::Parts { prohibit_lsid: true, ..crate::cmdparts::Parts::new(db, body) };
        let cmd = self.assemble(parts, server_id, false)?;
        let reply = self.exchange(server_id, &cmd.name, &cmd.db, &cmd.body, &[], true)?.unwrap_or_default();
        self.td().update_cluster_time(&reply);
        crate::cmd::check_ok(&reply).map_err(|e| e.into_error())?;
        Ok(reply)
    }

    /// Runs `body` (a command with $db) on `server_id`: the reply document, or the error as
    /// mongoc-cluster.c reports it ("Failed to send ..." for network errors, which also
    /// invalidate the server and drop its connection).
    pub fn run_command(&mut self, server_id: u32, cmd_name: &str, db: &str, body: &[u8]) -> Result<Vec<u8>, Error> {
        Ok(self.run_command_payload(server_id, cmd_name, db, body, &[], true)?.unwrap_or_default())
    }

    /// `run_command` with an optional document sequence; unacknowledged commands are sent with
    /// moreToCome and yield no reply (None).
    pub fn run_command_payload(&mut self, server_id: u32, cmd_name: &str, db: &str, body: &[u8], payload: crate::wire::Payloads<'_>, acknowledged: bool) -> Result<Option<Vec<u8>>, Error> {
        self.stream_for_server(server_id, true)?;
        self.exchange(server_id, cmd_name, db, body, payload, acknowledged)
    }

    /// Sends a command on the connection to `server_id` as it is (connected, and authenticated
    /// unless this is the authentication): the reply, or the network error as mongoc-cluster.c
    /// reports it.
    fn exchange(&mut self, server_id: u32, cmd_name: &str, db: &str, body: &[u8], payload: crate::wire::Payloads<'_>, acknowledged: bool) -> Result<Option<Vec<u8>>, Error> {
        let socket_timeout = ms(self.settings.socket_timeout_ms);
        let Some(conn) = self.nodes.iter_mut().find(|n| n.id == server_id).and_then(|n| n.conn.as_mut()) else {
            return Err(Error::new(domain::STREAM, code::STREAM_NOT_ESTABLISHED, format!("Could not find stream for server id: {server_id}")));
        };
        let conn_generation = conn.generation as u32;
        // OP_MSG lengths are int32: a larger message would wrap them and the server would read
        // the rest of the bytes as further messages. Refused before anything is sent (libbson
        // refuses appends past INT32_MAX; strings appended directly by the driver are not
        // bounded otherwise)
        let len = crate::wire::HEADER_LEN + 4 + 1 + body.len() + payload.iter().map(|(id, docs)| 1 + 4 + id.len() + 1 + docs.len()).sum::<usize>();
        if len > i32::MAX as usize {
            return Err(Error::new(domain::CLIENT, code::CLIENT_TOO_BIG, format!("attempted to send a \"{cmd_name}\" command of {len} bytes, which exceeds the maximum message size of {} bytes", i32::MAX)));
        }
        match handshake::run_msg(conn, body, payload, !acknowledged, socket_timeout) {
            Ok(r) => {
                if let Some(n) = self.nodes.iter_mut().find(|n| n.id == server_id) {
                    n.last_used = monotonic_usec();
                }
                Ok(r)
            }
            Err(NetError::Interrupted) => {
                // the server is fine; the connection's protocol state is unknown: closed
                self.disconnect_node(server_id);
                Err(interrupted_error())
            }
            Err(e) => {
                let err = command_network_error(cmd_name, db, &e);
                // the connection's pool generation: pooled, the one it was made with; single-
                // threaded, the server's current one (_cluster_fetch_stream_single copies it)
                let generation = if self.shared.pooled.get() {
                    conn_generation
                } else {
                    let sid = self.handshake_description(server_id).map_or(crate::sdam::ZERO_OID, |h| h.service_id);
                    self.td().server(server_id).map_or(0, |s| s.generation(&sid))
                };
                self.handle_network_error(server_id, generation, &err, matches!(e, NetError::Timeout));
                Err(err)
            }
        }
    }

    /// The connection-time description of `server_id` (else the topology's).
    pub fn stream_description(&self, server_id: u32) -> Option<ServerDescription> {
        self.with_stream_description(server_id, ServerDescription::clone)
    }

    /// `f` of the connection-time description of `server_id` (else the topology's).
    #[inline]
    pub fn with_stream_description<R>(&self, server_id: u32, f: impl FnOnce(&ServerDescription) -> R) -> Option<R> {
        if let Some(h) = self.handshake_description(server_id) {
            return Some(f(h));
        }
        self.shared.td.borrow().server(server_id).map(f)
    }

    /// _handle_network_error: SDAM error handling and disconnecting the node.
    fn handle_network_error(&mut self, server_id: u32, generation: u32, err: &Error, timeout: bool) {
        let kind = if timeout { AppError::Timeout } else { AppError::Network };
        let sid = self.td().server(server_id).map_or(crate::sdam::ZERO_OID, |s| s.service_id);
        self.td().handle_app_error(server_id, true, kind, &[5, 0, 0, 0, 0], err, generation, &sid);
        if let Some(n) = self.nodes.iter_mut().find(|n| n.id == server_id) {
            n.conn = None;
            n.handshake = None;
        }
    }

    /// Command-error SDAM handling (_mongoc_topology_handle_app_error with a reply).
    pub fn handle_command_error(&mut self, server_id: u32, reply: &[u8]) {
        let sid = self.td().server(server_id).map_or(crate::sdam::ZERO_OID, |s| s.service_id);
        let generation = self.td().server(server_id).map_or(0, |s| s.generation(&sid));
        let out = self.td().handle_app_error(server_id, true, AppError::Command, reply, &Error::default(), generation, &sid);
        if out.stale {
            self.stale.set(true);
        }
        if out.pool_cleared {
            if let Some(n) = self.nodes.iter_mut().find(|n| n.id == server_id) {
                n.conn = None;
                n.handshake = None;
            }
        }
    }

    /// Drops every connection (mongoc_client_reset / after fork).
    pub fn reset(&mut self) {
        for n in &mut self.nodes {
            n.conn = None;
            n.handshake = None;
        }
    }

    /// The connection-time server description of `server_id` (handshake_sd).
    pub fn handshake_description(&self, server_id: u32) -> Option<&ServerDescription> {
        self.nodes.iter().find(|n| n.id == server_id).and_then(|n| n.handshake.as_ref())
    }
}

/// The error of an operation whose wait was interrupted (crate::aio::Interrupted).
pub fn interrupted_error() -> Error {
    Error::new(domain::STREAM, code::STREAM_SOCKET, crate::aio::Interrupted.to_string())
}

/// A command's network error as mongoc-cluster.c reports it (RUN_CMD_ERR_DECORATE).
pub(crate) fn command_network_error(cmd_name: &str, db: &str, e: &NetError) -> Error {
    let detail = match e {
        NetError::Closed | NetError::Timeout => {
            log::debug(log::CLUSTER, || "could not read message length, stream probably closed or timed out".to_owned());
            "Failed to read 4 bytes: socket error or timeout".to_owned()
        }
        NetError::Protocol(m) => m.to_string(),
        e => format!("Failure during socket delivery: {e}"),
    };
    if matches!(e, NetError::Protocol(m) if *m == crate::wire::DECOMPRESS_ERROR) {
        Error::new(domain::PROTOCOL, code::PROTOCOL_INVALID_REPLY, detail)
    } else {
        Error::new(domain::STREAM, code::STREAM_SOCKET, format!("Failed to send \"{cmd_name}\" command with database \"{db}\": {detail}"))
    }
}

/// A failed check's error (the part before " calling hello on ...").
fn net_check_error(e: NetError) -> Error {
    match e {
        NetError::Resolve(m) => Error::new(domain::STREAM, code::STREAM_NAME_RESOLUTION, m),
        NetError::Connect(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => Error::new(domain::STREAM, code::STREAM_CONNECT, "connection refused"),
        NetError::Connect(e) if e.kind() == std::io::ErrorKind::TimedOut => Error::new(domain::STREAM, code::STREAM_CONNECT, "connection timeout"),
        NetError::Connect(_) => Error::new(domain::STREAM, code::STREAM_CONNECT, "unknown connection error"),
        NetError::Timeout => Error::new(domain::STREAM, code::STREAM_CONNECT, "connection timeout"),
        NetError::Closed => Error::new(domain::STREAM, code::STREAM_SOCKET, "Server closed connection."),
        NetError::Io(_) => Error::new(domain::STREAM, code::STREAM_SOCKET, "Failed to write rpc bytes."),
        NetError::Protocol(_) => Error::new(domain::PROTOCOL, code::PROTOCOL_INVALID_REPLY, "Invalid reply from server."),
        NetError::Tls(m) => Error::new(domain::STREAM, code::STREAM_SOCKET, format!("TLS handshake failed: {m}")),
        NetError::Unix(c, m) => Error::new(domain::STREAM, c, m),
        NetError::Interrupted => interrupted_error(),
    }
}

/// _detect_nongenuine_host (DRIVERS-2583): true when the host suggests CosmosDB or DocumentDB.
fn detect_nongenuine_host(host: &str) -> bool {
    let h = host.to_ascii_lowercase();
    if h.ends_with(".cosmos.azure.com") {
        crate::log::info(crate::log::MONGOC, || {
            "You appear to be connected to a CosmosDB cluster. For more information regarding feature compatibility and support please visit https://www.mongodb.com/supportability/cosmosdb".to_owned()
        });
        return true;
    }
    if h.ends_with(".docdb.amazonaws.com") || h.ends_with(".docdb-elastic.amazonaws.com") {
        crate::log::info(crate::log::MONGOC, || {
            "You appear to be connected to a DocumentDB cluster. For more information regarding feature compatibility and support please visit https://www.mongodb.com/supportability/documentdb".to_owned()
        });
        return true;
    }
    false
}

/// _detect_nongenuine_hosts: the SRV name, else the hosts until one matches.
pub fn detect_nongenuine_hosts(uri: &Uri) {
    if !crate::log::enabled(crate::log::Level::Info) {
        return;
    }
    if let Some(srv) = &uri.srv {
        detect_nongenuine_host(srv);
        return;
    }
    for h in &uri.hosts {
        if detect_nongenuine_host(&h.host) {
            return;
        }
    }
}

/// The clusters of one client (libmongoc's mongoc_client_pool_t, without threads): operations
/// check out a cluster for their duration and return it. Synchronous use only ever needs one,
/// so it behaves as libmongoc's single-threaded client. Fibers that run operations at once get
/// one cluster each, at most maxPoolSize (libmongoc's default 100; 1 in load-balanced mode,
/// whose cursors and transactions live on one connection); a fiber finding all of them in use
/// waits for one, at most serverSelectionTimeoutMS.
pub struct ClientPool {
    shared: Rc<Shared>,
    /// boxed: a checkout moves a pointer
    idle: RefCell<Vec<Box<Client>>>,
    /// clusters checked out
    leased: Cell<usize>,
    /// clusters created (idle + leased, transient ones excluded)
    total: Cell<usize>,
    max: usize,
    /// fibers waiting for a cluster (park ids, FIFO) and the clusters handed to them
    waiters: RefCell<std::collections::VecDeque<u64>>,
    handed: RefCell<Vec<(u64, Box<Client>)>>,
}

/// A cluster checked out of a pool, returned when dropped.
pub struct Lease<'a> {
    pool: &'a ClientPool,
    client: std::mem::ManuallyDrop<Box<Client>>,
    /// not one of the pool's clusters (its operation fails at once, see Client::fail)
    transient: bool,
}

/// MONGOC_DEFAULT_MAX_POOL_SIZE
pub const DEFAULT_MAX_POOL_SIZE: usize = 100;

impl ClientPool {
    pub fn new(client: Client) -> Self {
        let max = if client.settings.hello.load_balanced {
            1
        } else {
            match client.uri.get_int32("maxpoolsize", DEFAULT_MAX_POOL_SIZE as i32) {
                n if n > 0 => n as usize,
                _ => DEFAULT_MAX_POOL_SIZE,
            }
        };
        ClientPool { shared: client.shared.clone(), idle: RefCell::new(vec![Box::new(client)]), leased: Cell::new(0), total: Cell::new(1), max, waiters: Default::default(), handed: Default::default() }
    }

    /// The shared state (no connection is used through it).
    #[inline]
    pub fn shared(&self) -> &Shared {
        &self.shared
    }

    /// The number of clusters checked out.
    #[inline]
    pub fn leased(&self) -> usize {
        self.leased.get()
    }

    #[inline]
    fn lease(&self, c: Box<Client>, transient: bool) -> Lease<'_> {
        self.leased.set(self.leased.get() + 1);
        Lease { pool: self, client: std::mem::ManuallyDrop::new(c), transient }
    }

    /// A cluster for one operation: an idle one, else a new one; at maxPoolSize (only possible
    /// with fibers), the next one returned, waiting at most serverSelectionTimeoutMS. A wait
    /// that times out (or is interrupted) yields a cluster whose operation fails at once.
    #[inline]
    pub fn checkout(&self) -> Lease<'_> {
        let c = self.idle.borrow_mut().pop();
        match c {
            Some(c) => self.lease(c, false),
            None => self.checkout_slow(),
        }
    }

    #[cold]
    fn checkout_slow(&self) -> Lease<'_> {
        if self.total.get() < self.max || !crate::aio::active() {
            // a second cluster at once: pooled mode from now on (fibers only; a synchronous
            // re-entrant operation keeps single-threaded semantics)
            if crate::aio::active() {
                self.shared.pooled.set(true);
            }
            self.total.set(self.total.get() + 1);
            return self.lease(Box::new(Client::cluster(self.shared.clone())), false);
        }
        let deadline = std::time::Instant::now() + Duration::from_millis(self.shared.settings.server_selection_timeout_ms.max(0) as u64);
        let mut my_id = None;
        let r = crate::aio::park(
            &mut |id| {
                my_id = Some(id);
                self.waiters.borrow_mut().push_back(id);
            },
            Some(deadline),
        );
        let id = my_id.unwrap_or(u64::MAX);
        self.waiters.borrow_mut().retain(|w| *w != id);
        let handed = {
            let mut h = self.handed.borrow_mut();
            h.iter().position(|(w, _)| *w == id).map(|i| h.swap_remove(i).1)
        };
        match (handed, r) {
            (Some(c), Ok(_)) => self.lease(c, false),
            (handed, r) => {
                // a cluster handed over too late goes to the next waiter
                if let Some(c) = handed {
                    self.checkin(c);
                }
                let mut t = Box::new(Client::cluster(self.shared.clone()));
                t.fail = Some(match r {
                    Err(_) => interrupted_error(),
                    Ok(_) => Error::new(domain::STREAM, code::STREAM_SOCKET, "Timed out while checking out a connection from connection pool"),
                });
                self.lease(t, true)
            }
        }
    }

    /// Returns a cluster: to the first waiting fiber, else to the idle ones.
    fn checkin(&self, c: Box<Client>) {
        let next = self.waiters.borrow_mut().pop_front();
        match next {
            Some(id) => {
                self.handed.borrow_mut().push((id, c));
                crate::aio::unpark(id);
            }
            None => self.idle.borrow_mut().push(c),
        }
    }

    /// mongoc_client_start_session: a cluster is used only to learn session support.
    pub fn start_session(&self, causal_consistency: bool, snapshot: bool) -> Result<crate::session::ClientSession, Error> {
        self.start_session_with(causal_consistency, snapshot, None)
    }

    /// `start_session` with default transaction options.
    pub fn start_session_with(&self, causal_consistency: bool, snapshot: bool, default_txn: Option<&crate::session::TxnOpts>) -> Result<crate::session::ClientSession, Error> {
        match self.shared.session_start(causal_consistency, snapshot, default_txn, true) {
            Some(r) => r,
            None => self.checkout().start_session_with(causal_consistency, snapshot, default_txn),
        }
    }

    /// mongoc_client_session_destroy: a cluster is used only to abort a running transaction.
    pub fn end_session(&self, cs: &mut crate::session::ClientSession) {
        if !self.shared.session_end(cs) {
            self.checkout().end_session(cs);
        }
    }

    /// `end_session` from object destruction: a running transaction is aborted only on an
    /// idle cluster (else the server session is dropped, not pooled).
    pub fn end_session_idle(&self, cs: &mut crate::session::ClientSession) {
        if !self.shared.session_end(cs) {
            match self.try_checkout() {
                Some(mut cl) => cl.end_session(cs),
                None => cs.server_session = None,
            }
        }
    }

    /// An idle cluster, if there is one (object destruction: no new connections are made).
    pub fn try_checkout(&self) -> Option<Lease<'_>> {
        let c = self.idle.borrow_mut().pop()?;
        Some(self.lease(c, false))
    }
}

impl std::ops::Deref for Lease<'_> {
    type Target = Client;
    #[inline]
    fn deref(&self) -> &Client {
        &self.client
    }
}

impl std::ops::DerefMut for Lease<'_> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Client {
        &mut self.client
    }
}

impl Drop for Lease<'_> {
    #[inline]
    fn drop(&mut self) {
        // SAFETY: taken once, here; the field is never used again
        let c = unsafe { std::mem::ManuallyDrop::take(&mut self.client) };
        self.pool.leased.set(self.pool.leased.get() - 1);
        if self.transient {
            return;
        }
        if self.pool.waiters.borrow().is_empty() {
            self.pool.idle.borrow_mut().push(c);
        } else {
            self.pool.checkin(c);
        }
    }
}
