//! Topology descriptions: SDAM state transitions and server selection (port of libmongoc's
//! mongoc-topology-description.c, plus the application-error rules of mongoc-topology.c).

use crate::error::{code, domain, Error};
use crate::sdam::{topology_version_cmp, ServerDescription, ServerType, NO_SESSIONS, NO_SET_VERSION, WIRE_VERSION_5_0, WIRE_VERSION_6_0, WIRE_VERSION_MAX, WIRE_VERSION_MIN, ZERO_OID};
use crate::uri::{ReadMode, NO_MAX_STALENESS};

/// mongoc_topology_description_type_t
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TopologyType {
    Unknown,
    Sharded,
    RsNoPrimary,
    RsWithPrimary,
    Single,
    LoadBalanced,
}

impl TopologyType {
    /// mongoc_topology_description_type()
    pub fn name(self) -> &'static str {
        match self {
            TopologyType::Unknown => "Unknown",
            TopologyType::Sharded => "Sharded",
            TopologyType::RsNoPrimary => "ReplicaSetNoPrimary",
            TopologyType::RsWithPrimary => "ReplicaSetWithPrimary",
            TopologyType::Single => "Single",
            TopologyType::LoadBalanced => "LoadBalanced",
        }
    }
}

/// mongoc_ss_optype_t
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpType {
    Read,
    Write,
    AggregateWithWrite,
}

/// The parts of mongoc_read_prefs_t server selection looks at.
#[derive(Clone, Debug, PartialEq)]
pub struct ReadPrefs {
    pub mode: ReadMode,
    /// tag sets, each an ordered list of (tag, value)
    pub tags: Vec<Vec<(String, String)>>,
    pub max_staleness_seconds: i64,
}

impl Default for ReadPrefs {
    fn default() -> Self {
        ReadPrefs { mode: ReadMode::Primary, tags: Vec::new(), max_staleness_seconds: NO_MAX_STALENESS as i64 }
    }
}

/// MONGOC_TOPOLOGY_HEARTBEAT_FREQUENCY_MS_SINGLE_THREADED / _MULTI_THREADED
pub const HEARTBEAT_FREQUENCY_MS_SINGLE_THREADED: i64 = 60_000;
pub const HEARTBEAT_FREQUENCY_MS_MULTI_THREADED: i64 = 10_000;
/// MONGOC_TOPOLOGY_MIN_HEARTBEAT_FREQUENCY_MS
pub const MIN_HEARTBEAT_FREQUENCY_MS: i64 = 500;
/// MONGOC_TOPOLOGY_COOLDOWN_MS
pub const COOLDOWN_MS: i64 = 5000;
/// MONGOC_TOPOLOGY_LOCAL_THRESHOLD_MS
pub const LOCAL_THRESHOLD_MS: i64 = 15;
/// MONGOC_TOPOLOGY_SERVER_SELECTION_TIMEOUT_MS
pub const SERVER_SELECTION_TIMEOUT_MS: i64 = 30_000;
/// MONGOC_DEFAULT_CONNECTTIMEOUTMS
pub const DEFAULT_CONNECT_TIMEOUT_MS: i64 = 10_000;
impl ReadPrefs {
    /// mongoc_read_prefs_is_valid (hedge is not kept: deprecated, ignored by servers 8.0+)
    pub fn is_valid(&self) -> bool {
        if self.mode == ReadMode::Primary && (!self.tags.is_empty() || self.max_staleness_seconds != NO_MAX_STALENESS as i64) {
            return false;
        }
        !(self.max_staleness_seconds != NO_MAX_STALENESS as i64 && self.max_staleness_seconds <= 0)
    }
}

/// MONGOC_IDLE_WRITE_PERIOD_MS
pub const IDLE_WRITE_PERIOD_MS: i64 = 10_000;
/// MONGOC_SMALLEST_MAX_STALENESS_SECONDS
pub const SMALLEST_MAX_STALENESS_SECONDS: i64 = 90;

/// mongoc_topology_description_t
#[derive(Clone, Debug)]
pub struct TopologyDescription {
    pub ty: TopologyType,
    /// sorted by id (mongoc_set_t)
    pub servers: Vec<ServerDescription>,
    pub max_server_id: u32,
    pub set_name: Option<String>,
    pub max_set_version: i64,
    pub max_election_id: [u8; 12],
    pub heartbeat_msec: i64,
    pub compatibility_error: Option<Error>,
    pub session_timeout_minutes: i64,
    /// $clusterTime document
    pub cluster_time: Option<Vec<u8>>,
    pub max_hosts: i32,
    pub rand_seed: u32,
    pub topology_id: [u8; 12],
    /// whether the TopologyOpening event was sent (topology "opening" is deferred until server
    /// selection)
    pub opened: bool,
    /// APM events not yet handed to the subscribers (copies start empty)
    pub events: crate::apm::Queue,
}

/// _mongoc_rand_simple (rand_r)
fn rand_simple(seed: &mut u32) -> u32 {
    // glibc's rand_r
    let mut next = *seed;
    next = next.wrapping_mul(1103515245).wrapping_add(12345);
    let mut result = (next / 65536) % 2048;
    next = next.wrapping_mul(1103515245).wrapping_add(12345);
    result <<= 10;
    result ^= (next / 65536) % 1024;
    next = next.wrapping_mul(1103515245).wrapping_add(12345);
    result <<= 10;
    result ^= (next / 65536) % 1024;
    *seed = next;
    result
}

impl TopologyDescription {
    /// mongoc_topology_description_init
    pub fn new(ty: TopologyType, heartbeat_msec: i64) -> Self {
        TopologyDescription {
            ty,
            servers: Vec::new(),
            max_server_id: 0,
            set_name: None,
            max_set_version: NO_SET_VERSION,
            max_election_id: ZERO_OID,
            heartbeat_msec,
            compatibility_error: None,
            session_timeout_minutes: NO_SESSIONS,
            cluster_time: None,
            max_hosts: 0,
            rand_seed: (crate::sdam::monotonic_usec() as u32) ^ std::process::id(),
            topology_id: ZERO_OID,
            opened: false,
            events: Default::default(),
        }
    }

    /// The initial topology type mongoc_topology_new picks for `uri`.
    pub fn initial_type(uri: &crate::uri::Uri) -> TopologyType {
        let has_dc = uri.options.has("directconnection");
        let dc = has_dc && uri.get_bool("directconnection", false);
        let rs = uri.options.str("replicaset").is_some();
        if uri.get_bool("loadbalanced", false) {
            TopologyType::LoadBalanced
        } else if uri.srv.is_some() && !has_dc {
            TopologyType::Unknown
        } else if has_dc {
            if dc {
                TopologyType::Single
            } else if rs {
                TopologyType::RsNoPrimary
            } else {
                TopologyType::Unknown
            }
        } else if rs {
            TopologyType::RsNoPrimary
        } else if uri.hosts.len() > 1 {
            TopologyType::Unknown
        } else {
            TopologyType::Single
        }
    }

    /// mongoc_topology_new's description for `uri` (seeded with the URI's hosts).
    pub fn from_uri(uri: &crate::uri::Uri, single_threaded: bool) -> Self {
        let hb_default = if single_threaded { HEARTBEAT_FREQUENCY_MS_SINGLE_THREADED } else { HEARTBEAT_FREQUENCY_MS_MULTI_THREADED };
        let mut td = Self::new(Self::initial_type(uri), uri.get_int32("heartbeatfrequencyms", hb_default as i32) as i64);
        td.topology_id = crate::bson::oid::generate();
        td.set_name = uri.options.str("replicaset").map(str::to_owned);
        td.max_hosts = uri.get_int32("srvmaxhosts", 0);
        let hosts: Vec<String> = uri.hosts.iter().map(|h| h.host_and_port.clone()).collect();
        for h in td.apply_srv_max_hosts(&hosts) {
            td.add_server(&h);
        }
        td
    }

    /// _mongoc_apply_srv_max_hosts: all hosts, or a random srvMaxHosts of them.
    pub fn apply_srv_max_hosts(&mut self, hosts: &[String]) -> Vec<String> {
        let mut v = hosts.to_vec();
        if self.max_hosts <= 0 || self.max_hosts as usize >= v.len() {
            return v;
        }
        // Fisher-Yates, as libmongoc
        for i in (1..v.len()).rev() {
            let j = rand_simple(&mut self.rand_seed) as usize % (i + 1);
            v.swap(i, j);
        }
        v.truncate(self.max_hosts as usize);
        v
    }

    /// _mongoc_apply_srv_max_hosts without the limit (SRV polling): every host, in random order
    /// when srvMaxHosts applies (fewer than the hosts).
    pub fn srv_order(&mut self, hosts: &[String]) -> Vec<String> {
        let mut v = hosts.to_vec();
        if self.max_hosts > 0 && (self.max_hosts as usize) < v.len() {
            for i in (1..v.len()).rev() {
                let j = rand_simple(&mut self.rand_seed) as usize % (i + 1);
                v.swap(i, j);
            }
        }
        v
    }

    /// _mongoc_topology_description_monitor_opening: TopologyOpening, the initial
    /// TopologyChanged and ServerOpening for every server, once; a load-balanced topology's one
    /// server becomes a LoadBalancer.
    pub fn open(&mut self) {
        if self.opened {
            return;
        }
        self.opened = true;
        let topology_id = self.topology_id;
        self.events.push(|| crate::apm::Event::TopologyOpening { topology_id });
        // the initial "previous" description: Unknown, no servers, zero id
        let initial = TopologyDescription::new(TopologyType::Unknown, self.heartbeat_msec);
        self.monitor_changed(Box::new(initial));
        let ids: Vec<u32> = self.servers.iter().map(|s| s.id).collect();
        for id in ids {
            self.monitor_server_opening(id);
        }
        if self.ty == TopologyType::LoadBalanced {
            let previous = Box::new(self.clone());
            if let Some(s) = self.servers.first_mut() {
                let prev_sd = s.clone();
                s.ty = ServerType::LoadBalancer;
                let new_sd = s.clone();
                self.events.push(|| crate::apm::Event::ServerChanged { topology_id, addr: new_sd.addr.clone(), previous: Box::new(prev_sd), new: Box::new(new_sd) });
            }
            self.monitor_changed(previous);
        }
    }

    /// _mongoc_topology_description_monitor_changed
    fn monitor_changed(&mut self, previous: Box<TopologyDescription>) {
        let topology_id = self.topology_id;
        if crate::apm::enabled() {
            let new = Box::new(self.clone());
            self.events.push(|| crate::apm::Event::TopologyChanged { topology_id, previous, new });
        }
    }

    /// _mongoc_topology_description_monitor_server_opening: deferred until the topology opened.
    fn monitor_server_opening(&mut self, id: u32) {
        let (opened, topology_id) = (self.opened, self.topology_id);
        let Some(sd) = self.servers.iter_mut().find(|s| s.id == id) else { return };
        if opened && !sd.opened {
            sd.opened = true;
            let addr = sd.addr.clone();
            self.events.push(|| crate::apm::Event::ServerOpening { topology_id, addr });
        }
    }

    /// The monitoring part of mongoc_topology_destroy: ServerClosed for every opened server,
    /// TopologyChanged to an empty Unknown description, then TopologyClosed if opened.
    pub fn close(&mut self) {
        let topology_id = self.topology_id;
        let opened: Vec<String> = self.servers.iter().filter(|s| s.opened).map(|s| s.addr.clone()).collect();
        for addr in opened {
            self.events.push(|| crate::apm::Event::ServerClosed { topology_id, addr });
        }
        if crate::apm::enabled() {
            let mut next = TopologyDescription::new(TopologyType::Unknown, self.heartbeat_msec);
            next.topology_id = topology_id;
            next.opened = self.opened;
            let previous = Box::new(self.clone());
            self.events.push(|| crate::apm::Event::TopologyChanged { topology_id, previous, new: Box::new(next) });
        }
        if self.opened {
            self.events.push(|| crate::apm::Event::TopologyClosed { topology_id });
        }
    }

    pub fn server(&self, id: u32) -> Option<&ServerDescription> {
        self.servers.iter().find(|s| s.id == id)
    }
    pub fn server_mut(&mut self, id: u32) -> Option<&mut ServerDescription> {
        self.servers.iter_mut().find(|s| s.id == id)
    }
    pub fn server_by_address(&self, address: &str) -> Option<&ServerDescription> {
        self.servers.iter().find(|s| s.addr.eq_ignore_ascii_case(address))
    }
    fn id_of(&self, address: &str) -> Option<u32> {
        self.server_by_address(address).map(|s| s.id)
    }

    /// mongoc_topology_description_add_server: the id of the (possibly new) server.
    pub fn add_server(&mut self, address: &str) -> u32 {
        if let Some(id) = self.id_of(address) {
            return id;
        }
        self.max_server_id += 1;
        let id = self.max_server_id;
        self.servers.push(ServerDescription::new(id, address));
        self.monitor_server_opening(id);
        id
    }

    /// _mongoc_topology_description_remove_server
    fn remove_server(&mut self, id: u32) {
        if let Some(sd) = self.server(id).filter(|s| s.opened) {
            let (topology_id, addr) = (self.topology_id, sd.addr.clone());
            self.events.push(|| crate::apm::Event::ServerClosed { topology_id, addr });
        }
        self.servers.retain(|s| s.id != id);
        if self.servers.is_empty() {
            crate::log::warning(crate::log::MONGOC, || "Last server removed from topology".to_owned());
        }
    }

    pub fn has_primary(&self) -> bool {
        self.servers.iter().any(|s| s.ty == ServerType::RsPrimary)
    }

    fn update_rs_type(&mut self) {
        self.ty = if self.has_primary() { TopologyType::RsWithPrimary } else { TopologyType::RsNoPrimary };
    }

    fn label_unknown_member(&mut self, address: &str, ty: ServerType) {
        if let Some(s) = self.servers.iter_mut().find(|s| s.addr.eq_ignore_ascii_case(address) && s.ty == ServerType::Unknown) {
            s.ty = ty;
        }
    }

    fn add_new_servers(&mut self, sd: &ServerDescription) {
        for h in sd.hosts.iter().chain(&sd.arbiters).chain(&sd.passives) {
            self.add_server(h);
        }
    }

    /// mongoc_topology_description_invalidate_server
    pub fn invalidate_server(&mut self, id: u32, error: &Error) {
        if self.ty == TopologyType::LoadBalanced {
            return;
        }
        self.handle_hello(id, None, crate::sdam::RTT_UNSET, Some(error));
    }

    /// _mongoc_topology_description_clear_connection_pool
    pub fn clear_connection_pool(&mut self, id: u32, service_id: &[u8; 12]) {
        if let Some(sd) = self.server_mut(id) {
            sd.increment_generation(service_id);
        }
    }

    fn update_rs_from_primary(&mut self, sd: &ServerDescription) {
        if self.id_of(&sd.addr).is_none() {
            return;
        }
        if let Some(name) = &sd.set_name {
            match &self.set_name {
                None => self.set_name = Some(name.clone()),
                Some(n) if n != name => {
                    self.remove_server(sd.id);
                    self.update_rs_type();
                    return;
                }
                _ => {}
            }
        }
        let stale = || Error::new(domain::STREAM, code::STREAM_CONNECT, "member's setVersion or electionId is stale");
        if sd.max_wire_version >= WIRE_VERSION_6_0 {
            let not_stale = sd.election_id > self.max_election_id || (sd.election_id == self.max_election_id && sd.set_version >= self.max_set_version);
            if not_stale {
                self.max_election_id = sd.election_id;
                self.max_set_version = sd.set_version;
            } else {
                self.invalidate_server(sd.id, &stale());
                self.update_rs_type();
                return;
            }
        } else {
            if sd.set_version != NO_SET_VERSION && sd.election_id != ZERO_OID {
                let later = self.max_set_version > sd.set_version || (self.max_set_version == sd.set_version && self.max_election_id > sd.election_id);
                if later {
                    self.invalidate_server(sd.id, &stale());
                    self.update_rs_type();
                    return;
                }
                self.max_election_id = sd.election_id;
            }
            if sd.set_version != NO_SET_VERSION && (self.max_set_version == NO_SET_VERSION || sd.set_version > self.max_set_version) {
                self.max_set_version = sd.set_version;
            }
        }
        // invalidate other primaries
        for s in &mut self.servers {
            if s.id != sd.id && s.ty == ServerType::RsPrimary {
                s.ty = ServerType::Unknown;
                s.set_version = NO_SET_VERSION;
                s.election_id = ZERO_OID;
                s.reset();
            }
        }
        self.add_new_servers(sd);
        let to_remove: Vec<u32> = self.servers.iter().filter(|m| !sd.has_rs_member(&m.addr)).map(|m| m.id).collect();
        for id in to_remove {
            self.remove_server(id);
        }
        self.update_rs_type();
    }

    fn matches_me(sd: &ServerDescription) -> bool {
        sd.me.as_ref().is_none_or(|me| sd.addr.eq_ignore_ascii_case(me))
    }

    fn update_rs_without_primary(&mut self, sd: &ServerDescription) {
        if self.id_of(&sd.addr).is_none() {
            return;
        }
        if let Some(name) = &sd.set_name {
            match &self.set_name {
                None => self.set_name = Some(name.clone()),
                Some(n) if n != name => {
                    self.remove_server(sd.id);
                    return;
                }
                _ => {}
            }
        }
        self.add_new_servers(sd);
        if let Some(p) = &sd.primary {
            self.label_unknown_member(p, ServerType::PossiblePrimary);
        }
        if !Self::matches_me(sd) {
            self.remove_server(sd.id);
        }
    }

    fn update_rs_with_primary_from_member(&mut self, sd: &ServerDescription) {
        if self.id_of(&sd.addr).is_none() {
            return;
        }
        if self.set_name != sd.set_name {
            self.remove_server(sd.id);
            self.update_rs_type();
            return;
        }
        if !Self::matches_me(sd) {
            self.remove_server(sd.id);
            return;
        }
        if !self.has_primary() {
            if let Some(p) = &sd.primary {
                self.ty = TopologyType::RsNoPrimary;
                self.label_unknown_member(p, ServerType::PossiblePrimary);
            }
        }
    }

    fn update_unknown_with_standalone(&mut self, sd: &ServerDescription) {
        if self.id_of(&sd.addr).is_none() {
            return;
        }
        if self.servers.len() > 1 {
            self.remove_server(sd.id);
        } else {
            self.ty = TopologyType::Single;
        }
    }

    /// gSDAMTransitionTable
    fn transition(&mut self, sd: &ServerDescription) {
        use ServerType as S;
        use TopologyType as T;
        match (sd.ty, self.ty) {
            (S::Unknown, T::RsWithPrimary) | (S::RsGhost, T::RsWithPrimary) => self.update_rs_type(),
            (S::Standalone, T::Unknown) => self.update_unknown_with_standalone(sd),
            (S::Standalone | S::Mongos, T::RsNoPrimary) => self.remove_server(sd.id),
            (S::Standalone, T::Sharded) => self.remove_server(sd.id),
            (S::Standalone | S::Mongos, T::RsWithPrimary) => {
                self.remove_server(sd.id);
                self.update_rs_type();
            }
            (S::Mongos, T::Unknown) => self.ty = T::Sharded,
            (S::RsPrimary, T::Unknown | T::RsNoPrimary | T::RsWithPrimary) => self.update_rs_from_primary(sd),
            (S::RsPrimary | S::RsSecondary | S::RsArbiter | S::RsOther | S::RsGhost, T::Sharded) => self.remove_server(sd.id),
            (S::RsSecondary | S::RsArbiter | S::RsOther, T::Unknown) => {
                self.ty = T::RsNoPrimary;
                self.update_rs_without_primary(sd);
            }
            (S::RsSecondary | S::RsArbiter | S::RsOther, T::RsNoPrimary) => self.update_rs_without_primary(sd),
            (S::RsSecondary | S::RsArbiter | S::RsOther, T::RsWithPrimary) => self.update_rs_with_primary_from_member(sd),
            _ => {}
        }
    }

    /// _mongoc_topology_description_update_session_timeout
    fn update_session_timeout(&mut self) {
        self.session_timeout_minutes = NO_SESSIONS;
        for sd in &self.servers {
            if !sd.is_data_bearing() {
                continue;
            }
            if sd.session_timeout_minutes == NO_SESSIONS {
                self.session_timeout_minutes = NO_SESSIONS;
                return;
            } else if self.session_timeout_minutes == NO_SESSIONS || self.session_timeout_minutes > sd.session_timeout_minutes {
                self.session_timeout_minutes = sd.session_timeout_minutes;
            }
        }
    }

    /// _mongoc_topology_description_check_compatible
    fn check_compatible(&mut self) {
        self.compatibility_error = None;
        for sd in &self.servers {
            if matches!(sd.ty, ServerType::Unknown | ServerType::PossiblePrimary) {
                continue;
            }
            if sd.min_wire_version > WIRE_VERSION_MAX {
                self.compatibility_error = Some(Error::new(domain::PROTOCOL, code::PROTOCOL_BAD_WIRE_VERSION, format!("Server at {} requires wire version {}, but this version of libmongoc only supports up to {}", sd.addr, sd.min_wire_version, WIRE_VERSION_MAX)));
            } else if sd.max_wire_version < WIRE_VERSION_MIN {
                self.compatibility_error = Some(Error::new(domain::PROTOCOL, code::PROTOCOL_BAD_WIRE_VERSION, format!("Server at {} reports wire version {}, but this version of libmongoc requires at least {} (MongoDB {})", sd.addr, sd.max_wire_version, WIRE_VERSION_MIN, wire_version_to_server_version(WIRE_VERSION_MIN))));
            }
        }
    }

    /// mongoc_topology_description_update_cluster_time
    pub fn update_cluster_time(&mut self, reply: &[u8]) {
        use crate::bson::iter::{Iter, Value};
        let Some(mut it) = Iter::new(reply) else { return };
        if !it.find(b"$clusterTime") {
            return;
        }
        if let Value::Document(d) = it.value() {
            if self.cluster_time.as_deref().is_none_or(|cur| cluster_time_greater(d, cur)) {
                self.cluster_time = Some(d.to_vec());
            }
        } else {
            crate::log::error(crate::log::MONGOC, || "Can't parse $clusterTime".to_owned());
        }
    }

    /// mongoc_topology_description_handle_hello. `wrong_set_name_check`: the Single-topology
    /// replicaSet check (always on in libmongoc).
    pub fn handle_hello(&mut self, id: u32, reply: Option<&[u8]>, rtt_ms: i64, error: Option<&Error>) {
        let set_name = self.set_name.clone();
        let ty = self.ty;
        let monitoring = crate::apm::enabled();
        let prev_td = (monitoring && self.server(id).is_some()).then(|| Box::new(self.clone()));
        let topology_id = self.topology_id;
        let Some(sd) = self.server_mut(id) else { return };
        if let Some(r) = reply {
            if let Some(tv) = crate::bson::iter::Iter::new(r).and_then(|mut it| if it.find(b"topologyVersion") { if let crate::bson::iter::Value::Document(d) = it.value() { Some(d.to_vec()) } else { None } } else { None }) {
                if topology_version_cmp(&sd.topology_version, &tv) == 1 {
                    return;
                }
            }
        }
        let prev_sd = monitoring.then(|| sd.clone());
        sd.handle_hello(reply, rtt_ms, error);
        if let (Some(want), TopologyType::Single) = (&set_name, ty) {
            let err = match &sd.set_name {
                None => Some(format!("no reported set name, but expected '{want}'")),
                Some(got) if got != want => Some(format!("reported set name '{got}' does not match '{want}'")),
                _ => None,
            };
            if let Some(m) = err {
                sd.handle_hello(None, crate::sdam::RTT_UNSET, Some(&Error::new(domain::SERVER_SELECTION, code::SERVER_SELECTION_FAILURE, m)));
            }
        }
        let sd = sd.clone();
        // phongo always registers the callbacks: the change is computed whenever someone listens
        let changed = prev_sd.filter(|p| !p.sdam_equal(&sd));
        if let Some(previous) = &changed {
            let (previous, new) = (Box::new(previous.clone()), Box::new(sd.clone()));
            self.events.push(|| crate::apm::Event::ServerChanged { topology_id, addr: new.addr.clone(), previous, new });
        }
        self.transition(&sd);
        self.update_session_timeout();
        if reply.is_some() && error.is_none_or(|e| e.code == 0) {
            self.check_compatible();
        }
        if let (Some(_), Some(prev_td)) = (changed, prev_td) {
            self.monitor_changed(prev_td);
        }
    }

    /// _mongoc_topology_handle_app_error. `kind`: what failed; `reply`: the server reply (command
    /// errors; labels for network errors). Returns whether the pool was cleared.
    pub fn handle_app_error(&mut self, id: u32, handshake_complete: bool, kind: AppError, reply: &[u8], why: &Error, generation: u32, service_id: &[u8; 12]) -> AppErrorOutcome {
        let mut out = AppErrorOutcome::default();
        let Some(sd) = self.server(id) else { return out };
        if self.ty == TopologyType::LoadBalanced && !handshake_complete {
            return out;
        }
        if generation < sd.generation(service_id) {
            return out;
        }
        if kind == AppError::Timeout && handshake_complete {
            return out;
        }
        if kind == AppError::Command {
            let Err(cmd_error) = crate::cmd::check_ok_no_wce(reply) else { return out };
            let cmd_error = cmd_error.into_error();
            if !cmd_error.is_state_change() {
                return out;
            }
            let incoming = find_topology_version(reply);
            if topology_version_cmp(&sd.topology_version, &incoming) >= 0 {
                return out;
            }
            let sd = self.server_mut(id).expect("checked above");
            sd.topology_version = incoming;
            if cmd_error.is_shutdown() {
                self.clear_connection_pool(id, service_id);
                out.pool_cleared = true;
            }
            self.invalidate_server(id, &cmd_error);
            out.stale = cmd_error.is_not_primary();
        } else {
            if crate::cmd::has_label(reply, "SystemOverloadedError") {
                return out;
            }
            self.invalidate_server(id, why);
            self.clear_connection_pool(id, service_id);
            out.pool_cleared = true;
        }
        out
    }

    /// mongoc_topology_description_lowest_max_wire_version
    pub fn lowest_max_wire_version(&self) -> i32 {
        self.servers.iter().filter(|s| !matches!(s.ty, ServerType::Unknown | ServerType::PossiblePrimary)).map(|s| s.max_wire_version).min().unwrap_or(i32::MAX)
    }

    /// mongoc_topology_description_all_sds_have_write_date
    pub fn all_sds_have_write_date(&self) -> bool {
        self.servers.iter().all(|s| s.last_write_date_ms > 0 || !matches!(s.ty, ServerType::RsPrimary | ServerType::RsSecondary))
    }

    /// _mongoc_topology_description_validate_max_staleness
    pub fn validate_max_staleness(&self, max_staleness_seconds: i64) -> Result<(), Error> {
        if !matches!(self.ty, TopologyType::RsWithPrimary | TopologyType::RsNoPrimary) {
            return Ok(());
        }
        if max_staleness_seconds * 1000 < self.heartbeat_msec + IDLE_WRITE_PERIOD_MS {
            return Err(Error::new(domain::COMMAND, code::COMMAND_INVALID_ARG, format!("maxStalenessSeconds is set to {max_staleness_seconds}, it must be at least heartbeatFrequencyMS ({}) + server's idle write period ({} seconds)", self.heartbeat_msec, IDLE_WRITE_PERIOD_MS / 1000)));
        }
        if max_staleness_seconds < SMALLEST_MAX_STALENESS_SECONDS {
            return Err(Error::new(domain::COMMAND, code::COMMAND_INVALID_ARG, format!("maxStalenessSeconds is set to {max_staleness_seconds}, it must be at least {SMALLEST_MAX_STALENESS_SECONDS} seconds")));
        }
        Ok(())
    }

    /// mongoc_topology_compatible
    pub fn compatible(&self, rp: Option<&ReadPrefs>) -> Result<(), Error> {
        if let Some(e) = &self.compatibility_error {
            return Err(e.clone());
        }
        let Some(rp) = rp else { return Ok(()) };
        if rp.max_staleness_seconds != NO_MAX_STALENESS as i64 {
            if !self.all_sds_have_write_date() {
                return Err(Error::new(domain::COMMAND, code::PROTOCOL_BAD_WIRE_VERSION, "Not all servers have lastWriteDate"));
            }
            self.validate_max_staleness(rp.max_staleness_seconds)?;
        }
        Ok(())
    }

    /// _must_use_primary
    fn must_use_primary(&self, optype: OpType, mode: ReadMode) -> bool {
        if mode == ReadMode::Primary {
            return true;
        }
        match optype {
            OpType::Write | OpType::Read => false,
            OpType::AggregateWithWrite => self.servers.iter().any(|s| s.ty != ServerType::Unknown && s.max_wire_version < WIRE_VERSION_5_0),
        }
    }

    /// mongoc_topology_description_suitable_servers: the suitable servers' indexes into
    /// `self.servers`, and whether the read preference was overridden to primary.
    pub fn suitable_servers(&self, optype: OpType, rp: Option<&ReadPrefs>, deprioritized: &[u32], local_threshold_ms: i64) -> (Vec<usize>, bool) {
        if self.servers.is_empty() {
            return (Vec::new(), false);
        }
        let given = rp.map_or(ReadMode::Primary, |r| r.mode);
        let must_use_primary = self.must_use_primary(optype, given);
        let mode = if must_use_primary { ReadMode::Primary } else { given };
        let ds = (!deprioritized.is_empty()).then_some(deprioritized);
        let mut set = Vec::new();
        if !self.filter_by_topology(&mut set, ds, optype, rp, mode, local_threshold_ms) {
            set.clear();
            self.filter_by_topology(&mut set, None, optype, rp, mode, local_threshold_ms);
        }
        (set, must_use_primary)
    }

    fn is_candidate(&self, ty: ServerType, mode: ReadMode) -> bool {
        use ServerType as S;
        match self.ty {
            TopologyType::Single => ty == S::Standalone,
            TopologyType::RsNoPrimary | TopologyType::RsWithPrimary => match mode {
                ReadMode::Primary => ty == S::RsPrimary,
                ReadMode::Secondary => ty == S::RsSecondary,
                _ => matches!(ty, S::RsPrimary | S::RsSecondary),
            },
            TopologyType::Sharded => ty == S::Mongos,
            TopologyType::LoadBalanced => ty == S::LoadBalancer,
            TopologyType::Unknown => false,
        }
    }

    fn by_rtt(&self, set: &mut Vec<usize>, cands: &[usize], local_threshold_ms: i64) {
        let Some(nearest) = cands.iter().map(|&i| self.servers[i].rtt_ms).min() else { return };
        let limit = nearest.saturating_add(local_threshold_ms);
        set.extend(cands.iter().copied().filter(|&i| self.servers[i].rtt_ms <= limit));
    }

    fn filter_deprioritized(&self, cands: &mut Vec<usize>, ds: &[u32]) {
        cands.retain(|&i| !ds.contains(&self.servers[i].id));
    }

    /// mongoc_server_description_filter_stale
    fn filter_stale(&self, cands: &mut Vec<usize>, primary: Option<usize>, rp: Option<&ReadPrefs>) {
        let Some(rp) = rp else { return };
        if rp.max_staleness_seconds == NO_MAX_STALENESS as i64 {
            return;
        }
        let max_usec = rp.max_staleness_seconds * 1_000_000;
        let hb_usec = self.heartbeat_msec * 1000;
        let sec = |i: usize| self.servers[i].ty == ServerType::RsSecondary;
        if let Some(p) = primary {
            let p = &self.servers[p];
            cands.retain(|&i| {
                if !sec(i) {
                    return true;
                }
                let s = &self.servers[i];
                let staleness = p.last_write_date_ms * 1000 + (s.last_update_time_usec - p.last_update_time_usec) - s.last_write_date_ms * 1000 + hb_usec;
                staleness <= max_usec
            });
        } else {
            let max_lwd = cands.iter().filter(|&&i| sec(i)).map(|&i| self.servers[i].last_write_date_ms * 1000).max().unwrap_or(0).max(0);
            cands.retain(|&i| !sec(i) || max_lwd - self.servers[i].last_write_date_ms * 1000 + hb_usec <= max_usec);
        }
    }

    /// mongoc_server_description_filter_tags
    fn filter_tags(&self, cands: &mut Vec<usize>, rp: Option<&ReadPrefs>) {
        let Some(rp) = rp else { return };
        if rp.tags.is_empty() {
            return;
        }
        for set in &rp.tags {
            let matched: Vec<usize> = cands.iter().copied().filter(|&i| match_tag_set(&self.servers[i].tags, set)).collect();
            if !matched.is_empty() {
                *cands = matched;
                return;
            }
        }
        cands.clear();
    }

    /// _filter_suitable_servers_with_read_mode_secondary; false = retry without deprioritization
    fn secondary_filter(&self, set: &mut Vec<usize>, cands: &mut Vec<usize>, primary: Option<usize>, ds: Option<&[u32]>, rp: Option<&ReadPrefs>, local_threshold_ms: i64) -> bool {
        if let Some(ds) = ds {
            self.filter_deprioritized(cands, ds);
            if cands.is_empty() {
                return false;
            }
        }
        cands.retain(|&i| self.servers[i].ty == ServerType::RsSecondary);
        self.filter_stale(cands, primary, rp);
        self.filter_tags(cands, rp);
        if ds.is_some() && cands.is_empty() {
            return false;
        }
        self.by_rtt(set, cands, local_threshold_ms);
        true
    }

    /// _filter_suitable_servers_by_topology; false = retry without deprioritization
    fn filter_by_topology(&self, set: &mut Vec<usize>, ds: Option<&[u32]>, optype: OpType, rp: Option<&ReadPrefs>, mode: ReadMode, local_threshold_ms: i64) -> bool {
        match self.ty {
            TopologyType::Single => {
                if self.is_candidate(self.servers[0].ty, mode) {
                    set.push(0);
                }
                true
            }
            TopologyType::RsNoPrimary | TopologyType::RsWithPrimary => match optype {
                OpType::Read | OpType::AggregateWithWrite => {
                    let rs_scan = |mode: ReadMode| {
                        let mut primary = None;
                        let mut cands = Vec::new();
                        for (i, s) in self.servers.iter().enumerate() {
                            if s.ty == ServerType::RsPrimary {
                                primary = Some(i);
                            }
                            if self.is_candidate(s.ty, mode) {
                                cands.push(i);
                            }
                        }
                        (primary, cands)
                    };
                    let (primary, mut cands) = rs_scan(mode);
                    let primary_deprioritized = matches!((primary, ds), (Some(p), Some(ds)) if ds.contains(&self.servers[p].id));
                    match mode {
                        ReadMode::Primary => {
                            set.extend(primary);
                            true
                        }
                        ReadMode::PrimaryPreferred | ReadMode::Nearest => {
                            if mode == ReadMode::PrimaryPreferred {
                                if let Some(p) = primary.filter(|_| !primary_deprioritized) {
                                    set.push(p);
                                    return true;
                                }
                            }
                            if let Some(ds) = ds {
                                self.filter_deprioritized(&mut cands, ds);
                                if cands.is_empty() {
                                    return false;
                                }
                            }
                            self.filter_stale(&mut cands, primary, rp);
                            self.filter_tags(&mut cands, rp);
                            if ds.is_some() && cands.is_empty() {
                                return false;
                            }
                            self.by_rtt(set, &cands, local_threshold_ms);
                            true
                        }
                        ReadMode::SecondaryPreferred => {
                            let (inner_primary, mut inner) = rs_scan(ReadMode::Secondary);
                            self.secondary_filter(set, &mut inner, inner_primary, ds, rp, local_threshold_ms);
                            if !set.is_empty() {
                                return true;
                            }
                            if let Some(p) = inner_primary.filter(|_| !primary_deprioritized) {
                                set.push(p);
                                return true;
                            }
                            ds.is_none()
                        }
                        ReadMode::Secondary => self.secondary_filter(set, &mut cands, primary, ds, rp, local_threshold_ms),
                    }
                }
                OpType::Write => {
                    if self.ty == TopologyType::RsWithPrimary {
                        set.extend(self.servers.iter().position(|s| s.ty == ServerType::RsPrimary));
                    }
                    true
                }
            },
            TopologyType::Sharded => {
                let mut cands: Vec<usize> = (0..self.servers.len()).filter(|&i| self.is_candidate(self.servers[i].ty, mode)).collect();
                if let Some(ds) = ds {
                    self.filter_deprioritized(&mut cands, ds);
                    if cands.is_empty() {
                        return false;
                    }
                }
                self.by_rtt(set, &cands, local_threshold_ms);
                true
            }
            TopologyType::LoadBalanced => {
                set.push(0);
                true
            }
            TopologyType::Unknown => true,
        }
    }

    /// mongoc_topology_description_select: the selected server's id, and whether the read
    /// preference must be overridden to primary.
    pub fn select(&mut self, optype: OpType, rp: Option<&ReadPrefs>, deprioritized: &[u32], local_threshold_ms: i64) -> (Option<u32>, bool) {
        if self.ty == TopologyType::Single {
            let sd = &self.servers[0];
            let must = optype == OpType::AggregateWithWrite && sd.ty != ServerType::Unknown && sd.max_wire_version < WIRE_VERSION_5_0;
            return (sd.has_hello_response.then_some(sd.id), must);
        }
        let (set, must) = self.suitable_servers(optype, rp, deprioritized, local_threshold_ms);
        if set.is_empty() {
            return (None, must);
        }
        let n = rand_simple(&mut self.rand_seed) as usize % set.len();
        (Some(self.servers[set[n]].id), must)
    }

    /// mongoc_topology_description_has_data_node
    pub fn has_data_node(&self) -> bool {
        self.servers.iter().any(ServerDescription::is_data_bearing)
    }

    /// mongoc_topology_description_get_servers: the known (not Unknown) servers.
    pub fn known_servers(&self) -> impl Iterator<Item = &ServerDescription> {
        self.servers.iter().filter(|s| s.ty != ServerType::Unknown)
    }

    /// mongoc_topology_description_reconcile (SRV polling): `hosts` = host_and_port strings,
    /// already shuffled for srvMaxHosts as libmongoc does.
    pub fn reconcile(&mut self, hosts: &[String]) {
        let num_missing = self.servers.iter().filter(|s| !hosts.iter().any(|h| h.eq_ignore_ascii_case(&s.addr))).count();
        if self.max_hosts == 0 || self.max_hosts as usize >= hosts.len() {
            for h in hosts {
                self.add_server(h);
            }
        } else {
            let max_with_missing = self.max_hosts as usize + num_missing;
            for h in hosts {
                if self.servers.len() >= max_with_missing {
                    break;
                }
                self.add_server(h);
            }
        }
        let gone: Vec<u32> = self.servers.iter().filter(|s| !hosts.iter().any(|h| h.eq_ignore_ascii_case(&s.addr))).map(|s| s.id).collect();
        for id in gone {
            self.remove_server(id);
        }
    }
}

/// _mongoc_sdam_app_error_type_t
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppError {
    Command,
    Network,
    Timeout,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AppErrorOutcome {
    pub pool_cleared: bool,
    /// single-threaded clients mark the topology stale on a "not primary" error
    pub stale: bool,
}

/// _match_tag_set: every (tag, value) of the set is in the server's tags (exact match; a
/// server value that is not a string reads as "", as bson_iter_utf8 gives it).
fn match_tag_set(tags: &[u8], set: &[(String, String)]) -> bool {
    use crate::bson::iter::{Iter, Value};
    set.iter().all(|(k, v)| {
        let Some(mut it) = Iter::new(tags) else { return false };
        it.find(k.as_bytes())
            && match it.value() {
                Value::Utf8(s) => s == v.as_bytes(),
                _ => v.is_empty(),
            }
    })
}

/// _find_topology_version: the reply's topologyVersion (empty document when absent).
fn find_topology_version(reply: &[u8]) -> Vec<u8> {
    use crate::bson::iter::{Iter, Value};
    if let Some(mut it) = Iter::new(reply) {
        if it.find(b"topologyVersion") {
            if let Value::Document(d) = it.value() {
                return d.to_vec();
            }
        }
    }
    vec![5, 0, 0, 0, 0]
}

/// _mongoc_cluster_time_greater: compares the clusterTime timestamps.
pub fn cluster_time_greater(new: &[u8], old: &[u8]) -> bool {
    let ts = crate::session::parse_cluster_time;
    // the old one is not parsed (nor logged about) when the new one fails
    ts(new).zip_with_old(|| ts(old)).is_some_and(|(n, o)| n > o)
}

trait ZipWithOld<T> {
    fn zip_with_old(self, old: impl FnOnce() -> Option<T>) -> Option<(T, T)>;
}

impl<T> ZipWithOld<T> for Option<T> {
    fn zip_with_old(self, old: impl FnOnce() -> Option<T>) -> Option<(T, T)> {
        let n = self?;
        Some((n, old()?))
    }
}

/// _mongoc_wire_version_to_server_version
pub fn wire_version_to_server_version(v: i32) -> &'static str {
    match v {
        1 | 2 => "2.6",
        3 => "3.0",
        4 => "3.2",
        5 => "3.4",
        6 => "3.6",
        7 => "4.0",
        8 => "4.2",
        9 => "4.4",
        10 => "4.7",
        11 => "4.8",
        12 => "4.9",
        13 => "5.0",
        14 => "5.1",
        15 => "5.2",
        16 => "5.3",
        17 => "6.0",
        21 => "7.0",
        25 => "8.0",
        29 => "9.0",
        _ => "Unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::match_tag_set;
    use crate::bson::writer::Writer;

    #[test]
    fn tag_values_read_as_bson_iter_utf8() {
        let mut w = Writer::with_capacity(64);
        w.utf8(b"dc", b"ny");
        w.utf8(b"empty", b"");
        w.int32(b"n", 1);
        let tags = w.finish();
        let set = |p: &[(&str, &str)]| p.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<Vec<_>>();
        assert!(match_tag_set(&tags, &set(&[("dc", "ny")])));
        assert!(!match_tag_set(&tags, &set(&[("dc", "NY")])));
        assert!(!match_tag_set(&tags, &set(&[("rack", "")])));
        // a read preference value that is not a string reads as "": it matches empty values,
        // and a server value that is not a string reads as "" too
        assert!(match_tag_set(&tags, &set(&[("empty", "")])));
        assert!(match_tag_set(&tags, &set(&[("n", "")])));
        assert!(!match_tag_set(&tags, &set(&[("n", "1")])));
        assert!(match_tag_set(&tags, &set(&[])));
    }
}
