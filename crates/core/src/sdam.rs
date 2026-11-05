//! Server descriptions (port of libmongoc's mongoc-server-description.c).

use crate::bson::iter::{Iter, Value};
use crate::error::{code, domain, Error};

/// mongoc_server_description_type_t (values = MongoDB\Driver\Server::TYPE_*).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum ServerType {
    Unknown = 0,
    Standalone = 1,
    Mongos = 2,
    PossiblePrimary = 3,
    RsPrimary = 4,
    RsSecondary = 5,
    RsArbiter = 6,
    RsOther = 7,
    RsGhost = 8,
    LoadBalancer = 9,
}

impl ServerType {
    /// mongoc_server_description_type()'s strings.
    pub fn name(self) -> &'static str {
        match self {
            ServerType::Unknown => "Unknown",
            ServerType::Standalone => "Standalone",
            ServerType::Mongos => "Mongos",
            ServerType::PossiblePrimary => "PossiblePrimary",
            ServerType::RsPrimary => "RSPrimary",
            ServerType::RsSecondary => "RSSecondary",
            ServerType::RsArbiter => "RSArbiter",
            ServerType::RsOther => "RSOther",
            ServerType::RsGhost => "RSGhost",
            ServerType::LoadBalancer => "LoadBalancer",
        }
    }
}

/// MONGOC_DEFAULT_MAX_MSG_SIZE
pub const DEFAULT_MAX_MSG_SIZE: i32 = 48_000_000;
/// MONGOC_DEFAULT_BSON_OBJ_SIZE
pub const DEFAULT_BSON_OBJ_SIZE: i32 = 16 * 1024 * 1024;
/// MONGOC_DEFAULT_WRITE_BATCH_SIZE
pub const DEFAULT_WRITE_BATCH_SIZE: i32 = 100_000;
/// MONGOC_NO_SESSIONS
pub const NO_SESSIONS: i64 = -1;
/// MONGOC_NO_SET_VERSION
pub const NO_SET_VERSION: i64 = -1;
/// MONGOC_RTT_UNSET
pub const RTT_UNSET: i64 = -1;

/// MONGOC_DEFAULT_WIRE_VERSION
pub const DEFAULT_WIRE_VERSION: i32 = 0;
/// MONGOC_NO_SERVER_CONNECTION_ID
pub const NO_SERVER_CONNECTION_ID: i64 = -1;
/// WIRE_VERSION_MIN (MongoDB 4.2) / WIRE_VERSION_MAX (9.0) of libmongoc 2.5.8
pub const WIRE_VERSION_MIN: i32 = 8;
pub const WIRE_VERSION_MAX: i32 = 29;
pub const WIRE_VERSION_5_0: i32 = 13;
pub const WIRE_VERSION_6_0: i32 = 17;
/// The zero ObjectId (kZeroObjectId: "no service id" / "no election id").
pub const ZERO_OID: [u8; 12] = [0; 12];

/// bson_get_monotonic_time: microseconds on a monotonic clock.
pub fn monotonic_usec() -> i64 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    // offset by one second so that a fresh process never reports 0
    START.get_or_init(std::time::Instant::now).elapsed().as_micros() as i64 + 1_000_000
}

/// mongoc_server_description_t
#[derive(Clone, Debug)]
pub struct ServerDescription {
    pub id: u32,
    /// connection_address (host.host_and_port)
    pub addr: String,
    pub ty: ServerType,
    /// last_hello_response, without speculativeAuthenticate (empty document when none)
    pub hello: Vec<u8>,
    pub has_hello_response: bool,
    /// Why the server is Unknown, if a check failed.
    pub error: Option<Error>,
    pub rtt_ms: i64,
    pub last_update_time_usec: i64,
    pub hello_ok: bool,
    pub me: Option<String>,
    pub opened: bool,
    pub max_msg_size: i32,
    pub max_bson_obj_size: i32,
    pub max_write_batch_size: i32,
    pub session_timeout_minutes: i64,
    pub min_wire_version: i32,
    pub max_wire_version: i32,
    pub set_name: Option<String>,
    pub set_version: i64,
    /// zero when absent
    pub election_id: [u8; 12],
    pub hosts: Vec<String>,
    pub passives: Vec<String>,
    pub arbiters: Vec<String>,
    /// current_primary
    pub primary: Option<String>,
    /// tags document (raw BSON; empty document when absent)
    pub tags: Vec<u8>,
    pub last_write_date_ms: i64,
    pub compressors: Vec<String>,
    /// empty document when unknown
    pub topology_version: Vec<u8>,
    /// zero when absent
    pub service_id: [u8; 12],
    pub connection_id: i64,
    /// Connection-pool generation per service id (mongoc_generation_map_t).
    pub generations: Vec<([u8; 12], u32)>,
}

const EMPTY_DOC: [u8; 5] = [5, 0, 0, 0, 0];
/// ALPHA of the RTT moving average
const RTT_ALPHA: f64 = 0.2;

fn type_str(v: &Value) -> &'static str {
    // _mongoc_bson_type_to_str
    match v {
        Value::Double(_) => "DOUBLE",
        Value::Utf8(_) => "UTF8",
        Value::Document(_) => "DOCUMENT",
        Value::Array(_) => "ARRAY",
        Value::Binary(..) => "BINARY",
        Value::Undefined => "UNDEFINED",
        Value::Oid(_) => "OID",
        Value::Bool(_) => "BOOL",
        Value::DateTime(_) => "DATE_TIME",
        Value::Null => "NULL",
        Value::Regex(..) => "REGEX",
        Value::DbPointer(..) => "DBPOINTER",
        Value::Code(_) => "CODE",
        Value::Symbol(_) => "SYMBOL",
        Value::CodeWScope(..) => "CODEWSCOPE",
        Value::Int32(_) => "INT32",
        Value::Timestamp(..) => "TIMESTAMP",
        Value::Int64(_) => "INT64",
        Value::Decimal128(_) => "DECIMAL128",
        Value::MaxKey => "MAXKEY",
        Value::MinKey => "MINKEY",
    }
}

/// bson_iter_as_bool
pub fn as_bool(v: &Value) -> bool {
    match *v {
        Value::Bool(b) => b,
        Value::Double(d) => d != 0.0,
        Value::Int64(i) => i != 0,
        Value::Int32(i) => i != 0,
        Value::Null | Value::Undefined => false,
        _ => true,
    }
}

/// bson_iter_as_int64
pub fn as_i64(v: &Value) -> i64 {
    match *v {
        Value::Bool(b) => b as i64,
        Value::Double(d) => d as i64,
        Value::Int64(i) => i,
        Value::Int32(i) => i as i64,
        _ => 0,
    }
}

fn strings(arr: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(mut it) = Iter::new(arr) {
        while it.next() {
            if let Value::Utf8(s) = it.value() {
                out.push(String::from_utf8_lossy(s).into_owned());
            }
        }
    }
    out
}

fn utf8(s: &[u8]) -> String {
    String::from_utf8_lossy(s).into_owned()
}

impl ServerDescription {
    /// mongoc_server_description_init: an Unknown server.
    pub fn new(id: u32, addr: &str) -> Self {
        let mut sd = ServerDescription {
            id,
            addr: addr.to_owned(),
            ty: ServerType::Unknown,
            hello: EMPTY_DOC.to_vec(),
            has_hello_response: false,
            error: None,
            rtt_ms: RTT_UNSET,
            last_update_time_usec: 0,
            hello_ok: false,
            me: None,
            opened: false,
            max_msg_size: DEFAULT_MAX_MSG_SIZE,
            max_bson_obj_size: DEFAULT_BSON_OBJ_SIZE,
            max_write_batch_size: DEFAULT_WRITE_BATCH_SIZE,
            session_timeout_minutes: NO_SESSIONS,
            min_wire_version: DEFAULT_WIRE_VERSION,
            max_wire_version: DEFAULT_WIRE_VERSION,
            set_name: None,
            set_version: NO_SET_VERSION,
            election_id: ZERO_OID,
            hosts: Vec::new(),
            passives: Vec::new(),
            arbiters: Vec::new(),
            primary: None,
            tags: EMPTY_DOC.to_vec(),
            last_write_date_ms: -1,
            compressors: Vec::new(),
            topology_version: EMPTY_DOC.to_vec(),
            service_id: ZERO_OID,
            connection_id: NO_SERVER_CONNECTION_ID,
            generations: Vec::new(),
        };
        sd.reset();
        sd
    }

    /// mongoc_server_description_reset (keeps id, address, RTT, topology version, generations)
    pub fn reset(&mut self) {
        self.error = None;
        self.set_name = None;
        self.ty = ServerType::Unknown;
        self.min_wire_version = DEFAULT_WIRE_VERSION;
        self.max_wire_version = DEFAULT_WIRE_VERSION;
        self.max_msg_size = DEFAULT_MAX_MSG_SIZE;
        self.max_bson_obj_size = DEFAULT_BSON_OBJ_SIZE;
        self.max_write_batch_size = DEFAULT_WRITE_BATCH_SIZE;
        self.session_timeout_minutes = NO_SESSIONS;
        self.last_write_date_ms = -1;
        self.hello_ok = false;
        self.hello = EMPTY_DOC.to_vec();
        self.has_hello_response = false;
        self.last_update_time_usec = monotonic_usec();
        self.hosts.clear();
        self.passives.clear();
        self.arbiters.clear();
        self.tags = EMPTY_DOC.to_vec();
        self.compressors.clear();
        self.me = None;
        self.primary = None;
        self.set_version = NO_SET_VERSION;
        self.election_id = ZERO_OID;
        self.service_id = ZERO_OID;
        self.connection_id = NO_SERVER_CONNECTION_ID;
    }

    /// An Unknown server whose check failed with `error`.
    pub fn failed(addr: &str, error: Error) -> Self {
        let mut sd = Self::new(0, addr);
        sd.handle_hello(None, RTT_UNSET, Some(&error));
        sd
    }

    /// A server description from a successful handshake.
    pub fn from_hello(addr: &str, reply: &[u8], rtt_ms: i64) -> Self {
        let mut sd = Self::new(0, addr);
        sd.handle_hello(Some(reply), rtt_ms, None);
        sd
    }

    /// _mongoc_server_description_set_error
    fn set_error(&mut self, error: Option<&Error>) {
        self.error = Some(match error {
            Some(e) if e.code != 0 => e.clone(),
            _ => Error::new(domain::STREAM, code::STREAM_CONNECT, "unknown error calling hello"),
        });
        self.rtt_ms = RTT_UNSET;
    }

    /// mongoc_server_description_update_rtt
    pub fn update_rtt(&mut self, rtt_ms: i64) {
        if rtt_ms == RTT_UNSET {
            return;
        }
        self.rtt_ms = if self.rtt_ms == RTT_UNSET { rtt_ms } else { (RTT_ALPHA * rtt_ms as f64 + (1.0 - RTT_ALPHA) * self.rtt_ms as f64) as i64 };
    }

    /// mongoc_server_description_handle_hello: `None` = the check failed with `error`.
    pub fn handle_hello(&mut self, reply: Option<&[u8]>, rtt_ms: i64, error: Option<&Error>) {
        self.reset();
        let Some(reply) = reply else {
            self.set_error(error);
            return;
        };
        // the stored response leaves out speculativeAuthenticate
        let mut stored = crate::bson::writer::Writer::with_capacity(reply.len());
        if let Some(mut it) = Iter::new(reply) {
            while it.next() {
                if it.key() != b"speculativeAuthenticate" {
                    stored.element_from(None, &it);
                }
            }
        }
        let stored = stored.finish();
        self.has_hello_response = true;
        self.topology_version = EMPTY_DOC.to_vec();
        let (mut is_primary, mut is_shard, mut is_secondary, mut is_arbiter, mut is_rs, mut is_hidden) = (false, false, false, false, false, false);
        let mut num_keys = 0;
        let mut it = Iter::new(&stored).expect("a built document");
        let mut fail: Option<Error> = None;
        let mut auth_fail = false;
        while it.next() {
            num_keys += 1;
            let k = it.key();
            let v = it.value();
            let type_fail = |v: &Value| Error::new(domain::STREAM, code::STREAM_INVALID_TYPE, format!("unexpected type {} for field {} in hello response", type_str(v), utf8(k)));
            macro_rules! typed {
                ($p:pat => $e:expr) => {
                    match v {
                        $p => $e,
                        _ => {
                            fail = Some(type_fail(&v));
                            break;
                        }
                    }
                };
            }
            match k {
                b"ok" => {
                    if !as_bool(&v) {
                        // a failed hello is an authentication error (auth spec)
                        let msg = crate::cmd::error_message(&stored);
                        self.error = Some(Error::server(domain::CLIENT, code::CLIENT_AUTHENTICATE, msg));
                        auth_fail = true;
                        break;
                    }
                }
                b"isWritablePrimary" | b"ismaster" => typed!(Value::Bool(b) => is_primary = b),
                b"helloOk" => typed!(Value::Bool(b) => self.hello_ok = b),
                b"me" => typed!(Value::Utf8(s) => self.me = Some(utf8(s))),
                b"maxMessageSizeBytes" => typed!(Value::Int32(i) => self.max_msg_size = i),
                b"maxBsonObjectSize" => typed!(Value::Int32(i) => self.max_bson_obj_size = i),
                b"maxWriteBatchSize" => typed!(Value::Int32(i) => self.max_write_batch_size = i),
                b"logicalSessionTimeoutMinutes" => match v {
                    Value::Double(_) | Value::Int32(_) | Value::Int64(_) => self.session_timeout_minutes = as_i64(&v),
                    Value::Null => self.session_timeout_minutes = NO_SESSIONS,
                    _ => {
                        fail = Some(type_fail(&v));
                        break;
                    }
                },
                b"minWireVersion" => typed!(Value::Int32(i) => self.min_wire_version = i),
                b"maxWireVersion" => typed!(Value::Int32(i) => self.max_wire_version = i),
                b"msg" => typed!(Value::Utf8(s) => is_shard = s == b"isdbgrid"),
                b"setName" => typed!(Value::Utf8(s) => self.set_name = Some(utf8(s))),
                b"setVersion" => self.set_version = as_i64(&v),
                b"electionId" => typed!(Value::Oid(o) => self.election_id = *o),
                b"secondary" => typed!(Value::Bool(b) => is_secondary = b),
                b"hosts" => typed!(Value::Array(a) => self.hosts = strings(a)),
                b"passives" => typed!(Value::Array(a) => self.passives = strings(a)),
                b"arbiters" => typed!(Value::Array(a) => self.arbiters = strings(a)),
                b"primary" => typed!(Value::Utf8(s) => self.primary = Some(utf8(s))),
                b"arbiterOnly" => typed!(Value::Bool(b) => is_arbiter = b),
                b"isreplicaset" => typed!(Value::Bool(b) => is_rs = b),
                b"tags" => typed!(Value::Document(d) => self.tags = d.to_vec()),
                b"hidden" => is_hidden = as_bool(&v),
                b"lastWrite" => {
                    let date = match v {
                        Value::Document(d) => Iter::new(d).and_then(|mut c| if c.find(b"lastWriteDate") { Some(c.value()) } else { None }),
                        _ => None,
                    };
                    match date {
                        Some(Value::DateTime(ms)) => self.last_write_date_ms = ms,
                        _ => {
                            fail = Some(type_fail(&v));
                            break;
                        }
                    }
                }
                b"compression" => typed!(Value::Array(a) => self.compressors = strings(a)),
                b"topologyVersion" => typed!(Value::Document(d) => self.topology_version = d.to_vec()),
                b"serviceId" => typed!(Value::Oid(o) => self.service_id = *o),
                b"connectionId" => match v {
                    Value::Double(_) | Value::Int32(_) | Value::Int64(_) => self.connection_id = as_i64(&v),
                    _ => {
                        fail = Some(type_fail(&v));
                        break;
                    }
                },
                _ => {}
            }
        }
        self.hello = stored.clone();
        if fail.is_some() || auth_fail {
            if fail.is_some() {
                self.error = fail;
            }
            self.ty = ServerType::Unknown;
            self.rtt_ms = RTT_UNSET;
            return;
        }
        self.ty = if is_shard {
            ServerType::Mongos
        } else if self.set_name.is_some() {
            if is_hidden {
                ServerType::RsOther
            } else if is_primary {
                ServerType::RsPrimary
            } else if is_secondary {
                ServerType::RsSecondary
            } else if is_arbiter {
                ServerType::RsArbiter
            } else {
                ServerType::RsOther
            }
        } else if is_rs {
            ServerType::RsGhost
        } else if num_keys > 0 {
            ServerType::Standalone
        } else {
            ServerType::Unknown
        };
        if num_keys == 0 {
            self.set_error(error);
        }
        self.update_rtt(rtt_ms);
    }

    /// _is_data_node
    pub fn is_data_bearing(&self) -> bool {
        matches!(self.ty, ServerType::Standalone | ServerType::Mongos | ServerType::RsPrimary | ServerType::RsSecondary | ServerType::LoadBalancer)
    }

    /// mongoc_server_description_has_rs_member
    pub fn has_rs_member(&self, address: &str) -> bool {
        self.ty != ServerType::Unknown && self.hosts.iter().chain(&self.arbiters).chain(&self.passives).any(|h| h.eq_ignore_ascii_case(address))
    }

    /// The pool generation for `service_id` (mc_tpl_sd_get_generation).
    pub fn generation(&self, service_id: &[u8; 12]) -> u32 {
        self.generations.iter().find(|(s, _)| s == service_id).map_or(0, |g| g.1)
    }

    /// Bumps the pool generation for `service_id` (mc_tpl_sd_increment_generation).
    pub fn increment_generation(&mut self, service_id: &[u8; 12]) {
        match self.generations.iter_mut().find(|(s, _)| s == service_id) {
            Some(g) => g.1 += 1,
            None => self.generations.push((*service_id, 1)),
        }
    }
}

fn find_oid_and_int(tv: &[u8]) -> Option<([u8; 12], Option<i64>)> {
    let mut it = Iter::new(tv)?;
    let pid = if it.find(b"processId") { if let Value::Oid(o) = it.value() { *o } else { return None } } else { return None };
    let mut it = Iter::new(tv)?;
    let counter = if it.find(b"counter") { match it.value() { v @ (Value::Int32(_) | Value::Int64(_)) => Some(as_i64(&v)), _ => None } } else { None };
    Some((pid, counter))
}

/// mongoc_server_description_topology_version_cmp: -1, 0 or 1 (-1 also when not comparable).
pub fn topology_version_cmp(tv1: &[u8], tv2: &[u8]) -> i32 {
    if tv1.len() <= 5 || tv2.len() <= 5 {
        return -1;
    }
    let (Some((p1, c1)), Some((p2, c2))) = (find_oid_and_int(tv1), find_oid_and_int(tv2)) else { return -1 };
    if p1 != p2 {
        return -1;
    }
    let (Some(c1), Some(c2)) = (c1, c2) else { return -1 };
    match c1.cmp(&c2) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Greater => 1,
        std::cmp::Ordering::Equal => 0,
    }
}
