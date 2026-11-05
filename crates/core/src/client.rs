//! A single-threaded client (libmongoc's mongoc_topology_t in single-threaded mode, its topology
//! scanner and the single-threaded cluster): blocking scans that obey the cooldown, the server
//! selection loop with serverSelectionTryOnce, and one connection per server, shared by the
//! scanner and the commands (as libmongoc does when single-threaded).

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
    pub metadata: Metadata,
    pub hello: HelloOpts,
}

/// MONGOC_DEFAULT_SOCKETTIMEOUTMS
pub const DEFAULT_SOCKET_TIMEOUT_MS: i64 = 1000 * 60 * 5;

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
            metadata: Metadata { appname: uri.options.str("appname").map(str::to_owned), ..Default::default() },
            hello: HelloOpts { load_balanced: uri.get_bool("loadbalanced", false), compressors: uri.compressors.clone(), ..Default::default() },
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
}

/// mongoc_topology_t (single-threaded)
pub struct Client {
    pub uri: Uri,
    pub td: TopologyDescription,
    pub settings: Settings,
    nodes: Vec<Node>,
    last_scan: i64,
    stale: bool,
    scan_error: Option<Error>,
    /// the scanner's error when the topology is invalid (mongoc_topology_scanner_valid)
    pub invalid: Option<Error>,
    /// The process that owns the connections (fork detection).
    pid: u32,
    /// mongoc_client_t.generation: bumped by a reset (after a fork); cursors of an older
    /// generation neither advance nor kill their server cursor.
    pub generation: u32,
    /// the server session pool
    pub sessions: crate::session::Pool,
    /// mongoc_cluster_t.operation_id
    pub operation_id: i64,
}

/// The error libmongoc built without TLS gives for a URI that requests TLS (the transport is
/// not there yet: fail closed, never send anything in cleartext).
pub fn tls_unavailable() -> Error {
    Error::new(domain::COMMAND, code::COMMAND_INVALID_ARG, "Can't create SSL client, SSL not enabled in this build.")
}

/// _mongoc_server_selection_error
fn selection_error(msg: &str, scanner: Option<&Error>) -> Error {
    let m = match scanner {
        Some(e) if e.code != 0 => format!("{msg}: {}", e.message),
        _ => msg.to_owned(),
    };
    Error::new(domain::SERVER_SELECTION, code::SERVER_SELECTION_FAILURE, m)
}

fn split_host_port(addr: &str) -> (String, u16) {
    match addr.rsplit_once(':') {
        Some((h, p)) if !h.ends_with(']') || h.starts_with('[') => (h.trim_start_matches('[').trim_end_matches(']').to_owned(), p.parse().unwrap_or(27017)),
        _ => (addr.to_owned(), 27017),
    }
}

impl Client {
    /// mongoc_client_new_from_uri_with_error. Fails when the URI requests TLS: the TLS
    /// transport is not implemented yet.
    pub fn new(uri: Uri, settings: Settings) -> Result<Self, Error> {
        if uri.tls() {
            return Err(tls_unavailable());
        }
        let td = TopologyDescription::from_uri(&uri, true);
        let mut c = Client { uri, td, settings, nodes: Vec::new(), last_scan: 0, stale: true, scan_error: None, invalid: None, pid: std::process::id(), generation: 0, sessions: Default::default(), operation_id: 0 };
        c.reconcile_nodes();
        Ok(c)
    }

    /// Fork safety (mongoc_client_reset + libmongoc's socket ownership): in a child process,
    /// the inherited connections belong to the parent. They are leaked (never written to, read
    /// from, shut down or closed), every server's pool generation is bumped and the client
    /// generation advances, before any I/O.
    pub fn check_fork(&mut self) {
        let pid = std::process::id();
        if self.pid == pid {
            return;
        }
        self.pid = pid;
        for n in &mut self.nodes {
            if let Some(c) = n.conn.take() {
                std::mem::forget(c);
            }
            n.handshake = None;
        }
        let ids: Vec<(u32, [u8; 12])> = self.td.servers.iter().map(|s| (s.id, s.service_id)).collect();
        for (id, sid) in ids {
            self.td.clear_connection_pool(id, &sid);
        }
        self.generation = self.generation.wrapping_add(1);
        // server sessions in use by the parent (mongoc_client_reset clears the pool)
        self.sessions.clear();
    }

    /// Whether the client is in the process that created its connections.
    pub fn same_process(&self) -> bool {
        self.pid == std::process::id()
    }

    /// The fd of the connection to `server_id` (tests).
    #[cfg(unix)]
    pub fn conn_fd(&self, server_id: u32) -> Option<std::os::fd::RawFd> {
        self.nodes.iter().find(|n| n.id == server_id).and_then(|n| n.conn.as_ref()).map(Conn::raw_fd)
    }

    /// mongoc_topology_reconcile: scanner nodes for the description's servers.
    fn reconcile_nodes(&mut self) {
        let ids: Vec<u32> = self.td.servers.iter().map(|s| s.id).collect();
        self.nodes.retain(|n| ids.contains(&n.id));
        for s in &self.td.servers {
            if !self.nodes.iter().any(|n| n.id == s.id) {
                self.nodes.push(Node { id: s.id, addr: s.addr.clone(), conn: None, handshake: None, last_failed: -1, last_error: None, last_used: -1 });
            }
        }
    }

    fn node_in_cooldown(n: &Node, when: i64) -> bool {
        n.last_failed != -1 && n.last_failed + 1000 * tp::COOLDOWN_MS >= when
    }

    fn bypass_cooldown(&self) -> bool {
        self.td.ty == TopologyType::LoadBalanced
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

    /// Checks one node: a hello on its connection, or connect + handshake. Ok((reply, rtt)).
    fn check_node(&mut self, i: usize) -> Result<(Vec<u8>, i64), Error> {
        let connect_timeout = ms(self.settings.connect_timeout_ms);
        let t0 = std::time::Instant::now();
        if let Some(mut conn) = self.nodes[i].conn.take() {
            let hello_ok = self.nodes[i].handshake.as_ref().is_some_and(|h| h.hello_ok);
            let cmd = self.check_command(hello_ok);
            match handshake::run_command(&mut conn, &cmd, connect_timeout) {
                Ok(r) => {
                    self.nodes[i].conn = Some(conn);
                    return Ok((r, t0.elapsed().as_millis() as i64));
                }
                Err(e) => return Err(net_check_error(e)),
            }
        }
        let (host, port) = split_host_port(&self.nodes[i].addr);
        let mut conn = Conn::connect(&host, port, connect_timeout).map_err(|e| match e {
            NetError::Resolve(_) => Error::new(domain::STREAM, code::STREAM_NAME_RESOLUTION, format!("Failed to resolve '{host}'")),
            e => net_check_error(e),
        })?;
        let body = handshake::hello_command(&self.settings.metadata, &self.settings.hello, false);
        let reply = handshake::run_command(&mut conn, &body, connect_timeout).map_err(net_check_error)?;
        let rtt = t0.elapsed().as_millis() as i64;
        let sd = ServerDescription::from_hello(&self.nodes[i].addr, &reply, rtt);
        conn.set_max_message_size(sd.max_msg_size);
        self.nodes[i].handshake = Some(sd);
        self.nodes[i].conn = Some(conn);
        Ok((reply, rtt))
    }

    /// mongoc_topology_scan_once: one blocking scan of every node (sequential).
    pub fn scan_once(&mut self, obey_cooldown: bool) {
        self.check_fork();
        self.reconcile_nodes();
        let now = monotonic_usec();
        let ids: Vec<u32> = self.nodes.iter().map(|n| n.id).collect();
        for id in ids {
            let Some(i) = self.nodes.iter().position(|n| n.id == id) else { continue };
            if obey_cooldown && !self.bypass_cooldown() && Self::node_in_cooldown(&self.nodes[i], now) {
                continue;
            }
            match self.check_node(i) {
                Ok((reply, rtt)) => {
                    let n = &mut self.nodes[i];
                    n.last_used = monotonic_usec();
                    n.last_failed = -1;
                    n.last_error = None;
                    self.td.handle_hello(id, Some(&reply), rtt, None);
                }
                Err(e) => {
                    let n = &mut self.nodes[i];
                    n.conn = None;
                    n.handshake = None;
                    n.last_used = monotonic_usec();
                    n.last_failed = n.last_used;
                    let msg = if e.domain == domain::STREAM && e.code == code::STREAM_NAME_RESOLUTION { e.message.clone() } else { format!("{} calling hello on '{}'", e.message, n.addr) };
                    n.last_error = Some(if e.code == code::STREAM_NAME_RESOLUTION { e.clone() } else { Error::new(domain::CLIENT, code::STREAM_CONNECT, msg) });
                    self.td.handle_hello(id, None, RTT_UNSET, Some(&e));
                }
            }
            // a scan may add or remove servers (replica-set discovery)
            self.reconcile_nodes();
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
        self.scan_error = last.map(|e| Error::new(e.domain, e.code, msg));
        self.last_scan = monotonic_usec();
        self.stale = false;
    }

    /// mongoc_topology_select_server_id (single-threaded).
    pub fn select_server_id(&mut self, optype: OpType, rp: Option<&ReadPrefs>, deprioritized: &[u32]) -> Result<(u32, bool), Error> {
        const TIMEOUT_MSG: &str = "No suitable servers found: `serverSelectionTimeoutMS` expired";
        self.check_fork();
        if let Some(e) = &self.invalid {
            return Err(Error::new(domain::SERVER_SELECTION, code::SERVER_SELECTION_FAILURE, e.message.clone()));
        }
        if self.td.ty == TopologyType::LoadBalanced {
            return self.select_loadbalanced();
        }
        let hb = self.td.heartbeat_msec;
        let try_once = self.settings.server_selection_try_once;
        let loop_start = monotonic_usec();
        let mut loop_end = loop_start;
        let expire_at = loop_start + self.settings.server_selection_timeout_ms * 1000;
        self.td.open();
        let mut tried_once = false;
        if self.last_scan + hb * 1000 < loop_start {
            self.stale = true;
        }
        loop {
            if self.stale {
                let scan_ready = self.last_scan + self.settings.min_heartbeat_ms * 1000;
                if scan_ready > expire_at && !try_once {
                    return Err(selection_error(TIMEOUT_MSG, self.scan_error.as_ref()));
                }
                let sleep = scan_ready - loop_end;
                if sleep > 0 {
                    if try_once && self.scanner_in_cooldown(scan_ready) {
                        return Err(selection_error("No servers yet eligible for rescan", self.scan_error.as_ref()));
                    }
                    std::thread::sleep(Duration::from_micros(sleep as u64));
                }
                self.scan_once(true);
                loop_end = self.last_scan;
                tried_once = true;
            }
            self.td.compatible(rp)?;
            let (sel, must) = self.td.select(optype, rp, deprioritized, self.settings.local_threshold_ms);
            if let Some(id) = sel {
                if self.td.cluster_time.is_none() {
                    if let Some(h) = self.td.server(id).map(|s| s.hello.clone()) {
                        self.td.update_cluster_time(&h);
                    }
                }
                return Ok((id, must));
            }
            self.stale = true;
            if try_once {
                if tried_once {
                    return Err(selection_error("No suitable servers found (`serverSelectionTryOnce` set)", self.scan_error.as_ref()));
                }
            } else {
                loop_end = monotonic_usec();
                if loop_end > expire_at {
                    return Err(selection_error(TIMEOUT_MSG, self.scan_error.as_ref()));
                }
            }
        }
    }

    /// _mongoc_topology_select_server_id_loadbalanced
    fn select_loadbalanced(&mut self) -> Result<(u32, bool), Error> {
        self.td.open();
        let (sel, _) = self.td.select(OpType::Write, None, &[], 0);
        let Some(id) = sel else { return Err(selection_error("No suitable server found in load balanced deployment", None)) };
        let has_conn = |c: &Self| c.nodes.iter().any(|n| n.id == id && n.conn.is_some());
        if !has_conn(self) {
            self.scan_once(true);
        }
        if !has_conn(self) {
            let addr = self.td.server(id).map(|s| s.addr.clone()).unwrap_or_default();
            let m = match &self.scan_error {
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
        self.check_fork();
        let Some(i) = self.nodes.iter().position(|n| n.id == server_id) else {
            return Err(Error::new(domain::COMMAND, code::COMMAND_INVALID_ARG, format!("Could not find server with id: {server_id}")));
        };
        if self.nodes[i].conn.is_none() {
            let addr = self.nodes[i].addr.clone();
            if !reconnect_ok {
                return Err(Error::new(domain::STREAM, code::STREAM_NOT_ESTABLISHED, format!("Could not find stream for node {addr}")));
            }
            self.scan_once(true);
            if let Some(e) = &self.scan_error {
                if e.code != 0 {
                    return Err(e.clone());
                }
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
        Ok(n.conn.as_mut().expect("checked above"))
    }

    /// Runs `body` (a command with $db) on `server_id`: the reply document, or the error as
    /// mongoc-cluster.c reports it ("Failed to send ..." for network errors, which also
    /// invalidate the server and drop its connection).
    pub fn run_command(&mut self, server_id: u32, cmd_name: &str, db: &str, body: &[u8]) -> Result<Vec<u8>, Error> {
        Ok(self.run_command_payload(server_id, cmd_name, db, body, None, true)?.unwrap_or_default())
    }

    /// `run_command` with an optional document sequence; unacknowledged commands are sent with
    /// moreToCome and yield no reply (None).
    pub fn run_command_payload(&mut self, server_id: u32, cmd_name: &str, db: &str, body: &[u8], payload: Option<(&str, &[u8])>, acknowledged: bool) -> Result<Option<Vec<u8>>, Error> {
        self.check_fork();
        let socket_timeout = ms(self.settings.socket_timeout_ms);
        let conn = self.stream_for_server(server_id, true)?;
        let generation = conn.generation as u32;
        match handshake::run_msg(conn, body, payload, !acknowledged, socket_timeout) {
            Ok(r) => {
                if let Some(n) = self.nodes.iter_mut().find(|n| n.id == server_id) {
                    n.last_used = monotonic_usec();
                }
                Ok(r)
            }
            Err(e) => {
                let detail = match &e {
                    NetError::Closed | NetError::Timeout => "Failed to read 4 bytes: socket error or timeout".to_owned(),
                    NetError::Protocol(m) => m.to_string(),
                    e => format!("Failure during socket delivery: {e}"),
                };
                let err = Error::new(domain::STREAM, code::STREAM_SOCKET, format!("Failed to send \"{cmd_name}\" command with database \"{db}\": {detail}"));
                self.handle_network_error(server_id, generation, &err, matches!(e, NetError::Timeout));
                Err(err)
            }
        }
    }

    /// The connection-time description of `server_id` (else the topology's).
    pub fn stream_description(&self, server_id: u32) -> Option<&ServerDescription> {
        self.handshake_description(server_id).or_else(|| self.td.server(server_id))
    }

    /// _handle_network_error: SDAM error handling and disconnecting the node.
    fn handle_network_error(&mut self, server_id: u32, generation: u32, err: &Error, timeout: bool) {
        let kind = if timeout { AppError::Timeout } else { AppError::Network };
        let sid = self.td.server(server_id).map_or(crate::sdam::ZERO_OID, |s| s.service_id);
        self.td.handle_app_error(server_id, true, kind, &[5, 0, 0, 0, 0], err, generation, &sid);
        if let Some(n) = self.nodes.iter_mut().find(|n| n.id == server_id) {
            n.conn = None;
            n.handshake = None;
        }
    }

    /// Command-error SDAM handling (_mongoc_topology_handle_app_error with a reply).
    pub fn handle_command_error(&mut self, server_id: u32, reply: &[u8]) {
        let sid = self.td.server(server_id).map_or(crate::sdam::ZERO_OID, |s| s.service_id);
        let generation = self.td.server(server_id).map_or(0, |s| s.generation(&sid));
        let out = self.td.handle_app_error(server_id, true, AppError::Command, reply, &Error::default(), generation, &sid);
        if out.stale {
            self.stale = true;
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
        self.check_fork();
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
    }
}
