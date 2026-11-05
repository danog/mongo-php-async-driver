//! Server descriptions (port of libmongoc's mongoc-server-description.c).

use crate::bson::iter::{Iter, Value};

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

#[derive(Clone, Debug)]
pub struct ServerDescription {
    pub addr: String,
    pub ty: ServerType,
    /// The last hello response without speculativeAuthenticate (Server::getInfo()).
    pub hello: Option<Vec<u8>>,
    /// Why the server is Unknown, if a check failed.
    pub error: Option<String>,
    pub rtt_ms: i64,
    pub hello_ok: bool,
    pub me: Option<String>,
    pub max_msg_size: i32,
    pub max_bson_obj_size: i32,
    pub max_write_batch_size: i32,
    pub session_timeout_minutes: i64,
    pub min_wire_version: i32,
    pub max_wire_version: i32,
    pub set_name: Option<String>,
    pub set_version: i64,
    pub election_id: Option<[u8; 12]>,
    pub hosts: Vec<String>,
    pub passives: Vec<String>,
    pub arbiters: Vec<String>,
    pub primary: Option<String>,
    /// tags document (raw BSON; empty document when absent)
    pub tags: Vec<u8>,
    pub last_write_date_ms: i64,
    pub compressors: Vec<String>,
    pub topology_version: Option<Vec<u8>>,
    pub service_id: Option<[u8; 12]>,
    pub connection_id: i64,
}

const EMPTY_DOC: [u8; 5] = [5, 0, 0, 0, 0];

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
    /// mongoc_server_description_init + reset: an Unknown server.
    pub fn unknown(addr: &str) -> Self {
        ServerDescription {
            addr: addr.to_owned(),
            ty: ServerType::Unknown,
            hello: None,
            error: None,
            rtt_ms: RTT_UNSET,
            hello_ok: false,
            me: None,
            max_msg_size: DEFAULT_MAX_MSG_SIZE,
            max_bson_obj_size: DEFAULT_BSON_OBJ_SIZE,
            max_write_batch_size: DEFAULT_WRITE_BATCH_SIZE,
            session_timeout_minutes: NO_SESSIONS,
            min_wire_version: 0,
            max_wire_version: 0,
            set_name: None,
            set_version: NO_SET_VERSION,
            election_id: None,
            hosts: Vec::new(),
            passives: Vec::new(),
            arbiters: Vec::new(),
            primary: None,
            tags: EMPTY_DOC.to_vec(),
            last_write_date_ms: 0,
            compressors: Vec::new(),
            topology_version: None,
            service_id: None,
            connection_id: 0,
        }
    }

    /// An Unknown server whose check failed with `error`.
    pub fn failed(addr: &str, error: String) -> Self {
        let mut sd = Self::unknown(addr);
        sd.error = Some(error);
        sd
    }

    /// mongoc_server_description_handle_hello for a successful round trip.
    pub fn from_hello(addr: &str, reply: &[u8], rtt_ms: i64) -> Self {
        let mut sd = Self::unknown(addr);
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
        let (mut is_primary, mut is_shard, mut is_secondary, mut is_arbiter, mut is_rs, mut is_hidden) = (false, false, false, false, false, false);
        let mut num_keys = 0;
        let mut it = Iter::new(&stored).expect("a built document");
        let mut fail: Option<String> = None;
        let mut auth_fail = false;
        while it.next() {
            num_keys += 1;
            let k = it.key();
            let v = it.value();
            macro_rules! typed {
                ($p:pat => $e:expr) => {
                    match v {
                        $p => $e,
                        _ => {
                            fail = Some(format!("unexpected type {} for field {} in hello response", type_str(&v), utf8(k)));
                            break;
                        }
                    }
                };
            }
            match k {
                b"ok" => {
                    if !as_bool(&v) {
                        sd.error = Some(crate::cmd::error_message(&stored));
                        auth_fail = true;
                        break;
                    }
                }
                b"isWritablePrimary" | b"ismaster" => typed!(Value::Bool(b) => is_primary = b),
                b"helloOk" => typed!(Value::Bool(b) => sd.hello_ok = b),
                b"me" => typed!(Value::Utf8(s) => sd.me = Some(utf8(s))),
                b"maxMessageSizeBytes" => typed!(Value::Int32(i) => sd.max_msg_size = i),
                b"maxBsonObjectSize" => typed!(Value::Int32(i) => sd.max_bson_obj_size = i),
                b"maxWriteBatchSize" => typed!(Value::Int32(i) => sd.max_write_batch_size = i),
                b"logicalSessionTimeoutMinutes" => match v {
                    Value::Double(_) | Value::Int32(_) | Value::Int64(_) => sd.session_timeout_minutes = as_i64(&v),
                    Value::Null => sd.session_timeout_minutes = NO_SESSIONS,
                    _ => {
                        fail = Some(format!("unexpected type {} for field {} in hello response", type_str(&v), utf8(k)));
                        break;
                    }
                },
                b"minWireVersion" => typed!(Value::Int32(i) => sd.min_wire_version = i),
                b"maxWireVersion" => typed!(Value::Int32(i) => sd.max_wire_version = i),
                b"msg" => typed!(Value::Utf8(s) => is_shard = s == b"isdbgrid"),
                b"setName" => typed!(Value::Utf8(s) => sd.set_name = Some(utf8(s))),
                b"setVersion" => sd.set_version = as_i64(&v),
                b"electionId" => typed!(Value::Oid(o) => sd.election_id = Some(*o)),
                b"secondary" => typed!(Value::Bool(b) => is_secondary = b),
                b"hosts" => typed!(Value::Array(a) => sd.hosts = strings(a)),
                b"passives" => typed!(Value::Array(a) => sd.passives = strings(a)),
                b"arbiters" => typed!(Value::Array(a) => sd.arbiters = strings(a)),
                b"primary" => typed!(Value::Utf8(s) => sd.primary = Some(utf8(s))),
                b"arbiterOnly" => typed!(Value::Bool(b) => is_arbiter = b),
                b"isreplicaset" => typed!(Value::Bool(b) => is_rs = b),
                b"tags" => typed!(Value::Document(d) => sd.tags = d.to_vec()),
                b"hidden" => is_hidden = matches!(v, Value::Bool(true)),
                b"lastWrite" => {
                    let date = match v {
                        Value::Document(d) => Iter::new(d).and_then(|mut c| if c.find(b"lastWriteDate") { Some(c.value()) } else { None }),
                        _ => None,
                    };
                    match date {
                        Some(Value::DateTime(ms)) => sd.last_write_date_ms = ms,
                        _ => {
                            fail = Some(format!("unexpected type {} for field {} in hello response", type_str(&v), utf8(k)));
                            break;
                        }
                    }
                }
                b"compression" => typed!(Value::Array(a) => sd.compressors = strings(a)),
                b"topologyVersion" => typed!(Value::Document(d) => sd.topology_version = Some(d.to_vec())),
                b"serviceId" => typed!(Value::Oid(o) => sd.service_id = Some(*o)),
                b"connectionId" => match v {
                    Value::Double(_) | Value::Int32(_) | Value::Int64(_) => sd.connection_id = as_i64(&v),
                    _ => {
                        fail = Some(format!("unexpected type {} for field {} in hello response", type_str(&v), utf8(k)));
                        break;
                    }
                },
                _ => {}
            }
        }
        sd.hello = Some(stored.clone());
        if fail.is_some() || auth_fail {
            if fail.is_some() {
                sd.error = fail;
            }
            sd.ty = ServerType::Unknown;
            sd.rtt_ms = RTT_UNSET;
            return sd;
        }
        sd.ty = if is_shard {
            ServerType::Mongos
        } else if sd.set_name.is_some() {
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
        sd.rtt_ms = rtt_ms;
        sd
    }

    /// Whether the server can take writes / primary reads.
    pub fn is_data_bearing(&self) -> bool {
        matches!(self.ty, ServerType::Standalone | ServerType::Mongos | ServerType::RsPrimary | ServerType::RsSecondary | ServerType::LoadBalancer)
    }
}
