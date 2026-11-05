//! Command assembly and execution (mongoc-cmd.c `mongoc_cmd_parts_*`, the OP_MSG paths of
//! mongoc-cluster.c): $db, $readPreference, readConcern, writeConcern, lsid, txnNumber,
//! $clusterTime and the versioned API fields are added to the user's command here.

use crate::bson::iter::{Iter, Value};
use crate::bson::writer::Writer;
use crate::client::Client;
use crate::error::{code, domain, Error};
use crate::sdam::ServerType;
use crate::session::ServerSession;
use crate::topology::TopologyType;
use crate::uri::ReadMode;

/// The command and what is assembled into it (mongoc_cmd_parts_t).
#[derive(Default)]
pub struct Parts<'a> {
    pub db: &'a str,
    /// the command document as given (first key = command name)
    pub body: &'a [u8],
    pub is_read: bool,
    pub is_write: bool,
    /// The read preference: its mode and the $readPreference document for that mode.
    pub read_prefs: Option<(ReadMode, Vec<u8>)>,
    /// readConcern document (empty: none)
    pub read_concern: Vec<u8>,
    /// writeConcern document (empty: none)
    pub write_concern: Vec<u8>,
    pub is_acknowledged: bool,
    /// elements appended after the body (mongoc_cmd_parts_t.extra)
    pub extra: Writer,
    /// MONGOC_CMD_PARTS_ALLOW_TXN_NUMBER_{UNKNOWN (None), YES, NO}
    pub allow_txn_number: Option<bool>,
    pub prohibit_lsid: bool,
    /// An explicit session's lsid and $clusterTime (None: an implicit session is used).
    pub explicit_lsid: Option<Vec<u8>>,
    pub explicit_cluster_time: Option<Vec<u8>>,
    /// the user asked for a non-default read preference within a transaction (error)
    pub in_txn: bool,
}

impl<'a> Parts<'a> {
    pub fn new(db: &'a str, body: &'a [u8]) -> Self {
        Parts { db, body, is_acknowledged: true, ..Default::default() }
    }

    /// mongoc_cmd_parts_set_write_concern
    pub fn set_write_concern(&mut self, wc: &crate::concern::WriteConcern) {
        self.is_acknowledged = wc.is_acknowledged();
        self.write_concern = wc.to_bson();
    }
}

/// An assembled command (mongoc_cmd_t).
pub struct Cmd {
    pub name: String,
    pub db: String,
    pub body: Vec<u8>,
    pub is_acknowledged: bool,
    pub is_retryable_write: bool,
    pub is_retryable_read: bool,
    pub server_id: u32,
    /// the implicit session (returned to the pool after the command)
    pub temp_session: Option<ServerSession>,
}

/// A failed command: the error and the server's reply (empty for client-side errors).
#[derive(Debug)]
pub struct Failure {
    pub error: Error,
    pub reply: Vec<u8>,
}

impl From<Error> for Failure {
    fn from(error: Error) -> Self {
        Failure { error, reply: Vec::new() }
    }
}

/// _mongoc_get_command_name
pub fn command_name(body: &[u8]) -> Option<String> {
    let mut it = Iter::new(body)?;
    if !it.next() {
        return None;
    }
    Some(String::from_utf8_lossy(it.key()).into_owned())
}

fn has_field(doc: &[u8], k: &[u8]) -> bool {
    Iter::new(doc).is_some_and(|mut it| it.find(k))
}

/// _mongoc_validate_db_name (mongoc-util.c)
pub fn validate_db_name(db: &str) -> Result<(), Error> {
    let bad = |m: String| Err(Error::new(domain::NAMESPACE, code::NAMESPACE_INVALID, m));
    if db.contains('\0') {
        return bad("database name invalid: contains a NUL byte".into());
    }
    if db.contains('.') {
        return bad(format!("database name \"{db}\" invalid: contains \".\""));
    }
    Ok(())
}

impl Client {
    /// mongoc_cmd_parts_assemble (OP_MSG): the command for `server_id`.
    pub fn assemble(&mut self, mut parts: Parts, server_id: u32, must_use_primary: bool) -> Result<Cmd, Error> {
        let sd = self.handshake_description(server_id).or_else(|| self.td.server(server_id)).cloned();
        let Some(sd) = sd else {
            return Err(Error::new(domain::COMMAND, code::COMMAND_INVALID_ARG, format!("Could not find server with id: {server_id}")));
        };
        if sd.ty == ServerType::Unknown {
            let m = sd.error.as_ref().map(|e| e.message.clone()).unwrap_or_default();
            return Err(Error::new(domain::COMMAND, code::COMMAND_INVALID_ARG, format!("Cannot assemble command for invalidated server: {m}")));
        }
        validate_db_name(parts.db)?;
        let Some(name) = command_name(parts.body) else {
            return Err(Error::new(domain::COMMAND, code::COMMAND_INVALID_ARG, "Empty command document"));
        };
        let is_get_more = name == "getMore";
        let topo = self.td.ty;
        // primary reads on a direct connection become primaryPreferred
        let (mode, rp_doc) = match &parts.read_prefs {
            Some((m, d)) if *m != ReadMode::Primary => (*m, Some(d.clone())),
            _ if !parts.is_write && topo == TopologyType::Single && sd.ty != ServerType::Mongos => {
                let mut w = Writer::with_capacity(32);
                w.utf8(b"mode", b"primaryPreferred");
                (ReadMode::PrimaryPreferred, Some(w.finish()))
            }
            Some((m, d)) => (*m, Some(d.clone())),
            None => (ReadMode::Primary, None),
        };
        let mode = if must_use_primary { ReadMode::Primary } else { mode };
        let mut out = Writer::with_capacity(parts.body.len() + parts.extra.len() + 128);
        if let Some(mut it) = Iter::new(parts.body) {
            while it.next() {
                out.element_from(None, &it);
            }
        }
        let mut extra = std::mem::take(&mut parts.extra);
        if !has_field(parts.body, b"$db") {
            extra.utf8(b"$db", parts.db.as_bytes());
        }
        if parts.in_txn {
            if mode != ReadMode::Primary && !parts.is_write {
                return Err(Error::new(domain::TRANSACTION, code::TRANSACTION_INVALID_STATE, "Read preference in a transaction must be primary"));
            }
        } else if mode != ReadMode::Primary && sd.ty != ServerType::Standalone {
            if let Some(d) = &rp_doc {
                extra.document(b"$readPreference", d);
            }
        }
        out.append_raw_elements(&extra.finish());
        // lsid: the explicit session's, or an implicit one when acknowledged
        let mut temp_session = None;
        let mut cluster_time = parts.explicit_cluster_time.clone();
        let lsid = if parts.prohibit_lsid {
            None
        } else if let Some(l) = &parts.explicit_lsid {
            Some(l.clone())
        } else if parts.is_acknowledged && self.td.session_timeout_minutes != crate::sdam::NO_SESSIONS {
            let s = self.pop_server_session();
            let l = s.lsid.clone();
            temp_session = Some(s);
            Some(l)
        } else {
            None
        };
        if let Some(l) = &lsid {
            if !parts.is_acknowledged {
                return Err(Error::new(domain::COMMAND, code::COMMAND_INVALID_ARG, "Cannot use client session with unacknowledged command"));
            }
            out.document(b"lsid", l);
        }
        if parts.is_write && parts.allow_txn_number.is_none() {
            parts.allow_txn_number = Some(parts.is_acknowledged && name.eq_ignore_ascii_case("findandmodify"));
        }
        let is_retryable_write = lsid.is_some()
            && parts.is_write
            && parts.allow_txn_number == Some(true)
            && sd.ty != ServerType::Standalone
            && !parts.in_txn
            && self.uri.get_bool("retrywrites", true);
        if is_retryable_write {
            out.int64(b"txnNumber", 0);
        }
        let is_retryable_read = parts.is_read && !parts.is_write && !parts.in_txn && !is_get_more && self.uri.get_bool("retryreads", true);
        if let Some(t) = &self.td.cluster_time {
            cluster_time = match cluster_time {
                Some(c) if !crate::topology::cluster_time_greater(t, &c) => Some(c),
                _ => Some(t.clone()),
            };
        }
        if let Some(t) = &cluster_time {
            if sd.ty != ServerType::Standalone {
                out.document(b"$clusterTime", t);
            }
        }
        if let Some(v) = &self.settings.hello.api_version {
            out.utf8(b"apiVersion", v.as_bytes());
            if let Some(s) = self.settings.hello.api_strict {
                out.bool(b"apiStrict", s);
            }
            if let Some(d) = self.settings.hello.api_deprecation_errors {
                out.bool(b"apiDeprecationErrors", d);
            }
        }
        if !is_get_more && !parts.read_concern.is_empty() && !crate::bson::iter::is_empty(&parts.read_concern) {
            out.document(b"readConcern", &parts.read_concern);
        }
        if !parts.write_concern.is_empty() && !crate::bson::iter::is_empty(&parts.write_concern) {
            out.document(b"writeConcern", &parts.write_concern);
        }
        Ok(Cmd {
            name,
            db: parts.db.to_owned(),
            body: out.finish(),
            is_acknowledged: parts.is_acknowledged,
            is_retryable_write,
            is_retryable_read,
            server_id,
            temp_session,
        })
    }

    /// Pops a server session for an implicit session.
    pub fn pop_server_session(&mut self) -> ServerSession {
        let now = crate::sdam::monotonic_usec();
        let t = self.td.session_timeout_minutes;
        self.sessions.pop(t, now)
    }

    /// Returns a server session to the pool.
    pub fn push_server_session(&mut self, s: ServerSession) {
        let now = crate::sdam::monotonic_usec();
        let t = self.td.session_timeout_minutes;
        self.sessions.push(s, t, now);
    }

    /// Runs an assembled command (with an optional document sequence) and releases its
    /// implicit session: the reply, or the error with the reply (empty for network errors).
    pub fn run_cmd(&mut self, cmd: &mut Cmd, payload: Option<(&str, &[u8])>) -> Result<Vec<u8>, Failure> {
        let r = self.run_cmd_keep_session(cmd, payload);
        if let Some(mut s) = cmd.temp_session.take() {
            if r.as_ref().is_err_and(|f| f.error.is_network()) {
                s.dirty = true;
            }
            self.push_server_session(s);
        }
        r
    }

    /// Runs an assembled command without releasing its implicit session.
    pub fn run_cmd_keep_session(&mut self, cmd: &mut Cmd, payload: Option<(&str, &[u8])>) -> Result<Vec<u8>, Failure> {
        if let Some(s) = cmd.temp_session.as_mut() {
            s.last_used_usec = crate::sdam::monotonic_usec();
        }
        let reply = self.run_command_payload(cmd.server_id, &cmd.name, &cmd.db, &cmd.body, payload, cmd.is_acknowledged)?;
        let Some(reply) = reply else { return Ok(Vec::new()) };
        self.td.update_cluster_time(&reply);
        if let Err(e) = crate::cmd::check_ok(&reply) {
            self.handle_command_error(cmd.server_id, &reply);
            return Err(Failure { error: e.into_error(), reply });
        }
        Ok(reply)
    }
}

/// A server reply's field.
pub fn find<'a>(doc: &'a [u8], k: &[u8]) -> Option<Value<'a>> {
    let mut it = Iter::new(doc)?;
    it.find(k).then(|| it.value())
}
