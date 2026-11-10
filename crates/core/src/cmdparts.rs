//! Command assembly and execution (mongoc-cmd.c `mongoc_cmd_parts_*`, the OP_MSG paths of
//! mongoc-cluster.c): $db, $readPreference, readConcern, writeConcern, lsid, txnNumber,
//! $clusterTime and the versioned API fields are added to the user's command here.

use crate::bson::iter::{Iter, Value};
use crate::bson::writer::Writer;
use crate::client::Client;
use crate::error::{code, domain, Error};
use crate::sdam::ServerType;
use crate::session::{ClientSession, ServerSession, TxnState};
use crate::topology::{OpType, TopologyType};
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
    /// An explicit session (None: an implicit session is used when possible).
    pub session: Option<&'a mut ClientSession>,
    /// MONGOC_CMD_RAW (mongoc_client_command_with_opts)
    pub is_raw: bool,
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
    pub is_txn_finish: bool,
    pub server_id: u32,
    /// the implicit session (returned to the pool after the command)
    pub temp_session: Option<ServerSession>,
    /// mongoc_cmd_t.operation_id
    pub operation_id: i64,
    /// the read preference used for selection (a read retry selects with it)
    pub read_prefs: Option<crate::topology::ReadPrefs>,
    /// the wire version and mongos-ness of the stream the last attempt ran on (libmongoc's
    /// server_stream->sd, captured before SDAM error handling can reset the description)
    pub stream: (i32, bool),
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
pub fn command_name(body: &[u8]) -> Option<std::borrow::Cow<'_, str>> {
    let mut it = Iter::new(body)?;
    if !it.next() {
        return None;
    }
    // borrowed from `body` unless the key is not UTF-8
    Some(String::from_utf8_lossy(it.key()))
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
    /// mongoc_cmd_parts_assemble (OP_MSG): the command for `server_id`. A transaction's state
    /// advances here (_mongoc_client_session_append_txn).
    pub fn assemble(&mut self, mut parts: Parts, server_id: u32, must_use_primary: bool) -> Result<Cmd, Error> {
        // the fields used below (no copy of the whole description on this per-command path)
        struct Sd {
            ty: ServerType,
            max_wire_version: i32,
        }
        let sd = self.with_stream_description(server_id, |d| match d.ty {
            ServerType::Unknown => Err(d.error.as_ref().map(|e| e.message.clone()).unwrap_or_default()),
            ty => Ok(Sd { ty, max_wire_version: d.max_wire_version }),
        });
        let sd = match sd {
            None => return Err(Error::new(domain::COMMAND, code::COMMAND_INVALID_ARG, format!("Could not find server with id: {server_id}"))),
            Some(Err(m)) => return Err(Error::new(domain::COMMAND, code::COMMAND_INVALID_ARG, format!("Cannot assemble command for invalidated server: {m}"))),
            Some(Ok(sd)) => sd,
        };
        validate_db_name(parts.db)?;
        let Some(name) = command_name(parts.body) else {
            return Err(Error::new(domain::COMMAND, code::COMMAND_INVALID_ARG, "Empty command document"));
        };
        let is_get_more = name == "getMore";
        let is_txn_finish = name == "commitTransaction" || name == "abortTransaction";
        let topo = self.td().ty;
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
        let mut cs = if parts.prohibit_lsid { None } else { parts.session.take() };
        let in_txn = cs.as_ref().is_some_and(|s| s.in_txn());
        if in_txn {
            if cs.as_ref().is_some_and(|s| s.txn.read_primary == Some(false)) && !parts.is_write {
                return Err(Error::new(domain::TRANSACTION, code::TRANSACTION_INVALID_STATE, "Read preference in a transaction must be primary"));
            }
        } else if mode != ReadMode::Primary && sd.ty != ServerType::Standalone {
            if let Some(d) = &rp_doc {
                extra.document(b"$readPreference", d);
            }
        }
        out.append_raw_elements(&extra.finish());
        // an implicit session when acknowledged and none was given
        let mut temp_session = None;
        if cs.is_none() && !parts.prohibit_lsid && parts.is_acknowledged && self.sessions_supported() {
            temp_session = Some(self.pop_server_session());
        }
        let mut cluster_time = None;
        if let Some(s) = cs.as_deref_mut() {
            if !parts.is_acknowledged {
                return Err(Error::new(domain::COMMAND, code::COMMAND_INVALID_ARG, "Cannot use client session with unacknowledged command"));
            }
            out.document(b"lsid", s.lsid());
            if let Some(ss) = s.server_session.as_mut() {
                ss.last_used_usec = crate::sdam::monotonic_usec();
            }
            cluster_time = s.cluster_time.clone();
        } else if let Some(t) = temp_session.as_mut() {
            out.document(b"lsid", &t.lsid);
            t.last_used_usec = crate::sdam::monotonic_usec();
        }
        let has_session = cs.is_some() || temp_session.is_some();
        let txn_in_progress = cs.as_ref().is_some_and(|s| s.txn_state == TxnState::InProgress);
        if !txn_in_progress && parts.is_write && parts.allow_txn_number.is_none() {
            parts.allow_txn_number = Some(parts.is_acknowledged && name.eq_ignore_ascii_case("findandmodify"));
        }
        let is_retryable_write = has_session
            && parts.is_write
            && parts.allow_txn_number == Some(true)
            && sd.ty != ServerType::Standalone
            && !in_txn
            && self.uri.get_bool("retrywrites", true);
        if is_retryable_write {
            out.int64(b"txnNumber", 0);
        }
        let is_retryable_read = parts.is_read && !parts.is_write && !in_txn && !is_get_more && self.uri.get_bool("retryreads", true);
        if let Some(t) = &self.td().cluster_time {
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
        if !is_get_more {
            if let Some(s) = cs.as_deref() {
                if s.snapshot && sd.max_wire_version < WIRE_VERSION_SNAPSHOT_READS {
                    return Err(Error::new(domain::CLIENT, code::CLIENT_SESSION_FAILURE, "Snapshot reads require MongoDB 5.0 or later"));
                }
                s.append_read_concern(&parts.read_concern, parts.is_read, parts.is_write, &name, &mut out);
            } else if !parts.read_concern.is_empty() && !crate::bson::iter::is_empty(&parts.read_concern) {
                out.document(b"readConcern", &parts.read_concern);
            }
        }
        if (is_txn_finish || !in_txn) && !parts.write_concern.is_empty() && !crate::bson::iter::is_empty(&parts.write_concern) {
            out.document(b"writeConcern", &parts.write_concern);
        }
        if let Some(s) = cs {
            s.append_txn(&mut out, &name);
        }
        Ok(Cmd {
            name: name.into_owned(),
            db: parts.db.to_owned(),
            body: out.finish(),
            is_acknowledged: parts.is_acknowledged,
            is_retryable_write,
            is_retryable_read,
            is_txn_finish,
            server_id,
            temp_session,
            operation_id: self.operation_id.get(),
            read_prefs: None,
            stream: (0, false),
        })
    }

    /// Whether the deployment supports sessions (implicit sessions are used then).
    pub fn sessions_supported(&self) -> bool {
        let td = self.shared().td.borrow();
        td.session_timeout_minutes != crate::sdam::NO_SESSIONS || td.ty == TopologyType::LoadBalanced
    }


    /// Runs an assembled command (with an optional document sequence) and releases its
    /// implicit session: the reply, or the error with the reply (empty for network errors).
    pub fn run_cmd(&mut self, cmd: &mut Cmd, payload: crate::wire::Payloads<'_>) -> Result<Vec<u8>, Failure> {
        let r = self.run_monitored(cmd, payload, None);
        self.release_temp_session(cmd);
        r
    }

    /// Returns the command's implicit session to the pool.
    pub fn release_temp_session(&mut self, cmd: &mut Cmd) {
        if let Some(s) = cmd.temp_session.take() {
            self.push_server_session(s);
        }
    }

    /// Runs an assembled command without releasing its implicit session.
    pub fn run_cmd_keep_session(&mut self, cmd: &mut Cmd, payload: crate::wire::Payloads<'_>) -> Result<Vec<u8>, Failure> {
        self.run_monitored(cmd, payload, None)
    }

    /// mongoc_cluster_run_command_monitored (+ _private): runs `cmd`, feeds the reply to the
    /// session, applies the network-error and transaction rules. The failure's reply carries
    /// the error labels added here.
    pub fn run_monitored(&mut self, cmd: &mut Cmd, payload: crate::wire::Payloads<'_>, mut session: Option<&mut ClientSession>) -> Result<Vec<u8>, Failure> {
        // _mongoc_cse_auto_encrypt: before the command is monitored; a failure sends no events
        let mut payload = payload;
        // the command actually sent (cmd.body stays the caller's, for retries)
        let mut sent: Option<Vec<u8>> = None;
        let ac = if self.auto_crypt_applies() { self.take_auto_crypt().map_err(Failure::from)? } else { None };
        let ac = match ac {
            Some(ac) if !ac.encrypts() => {
                self.put_auto_crypt(ac);
                None
            }
            ac => ac,
        };
        if let Some(mut ac) = ac {
            if self.with_stream_description(cmd.server_id, |d| d.max_wire_version < 8).unwrap_or(false) {
                self.put_auto_crypt(ac);
                return Err(Failure {
                    error: Error::new(domain::PROTOCOL, code::PROTOCOL_BAD_WIRE_VERSION, "Auto-encryption requires a minimum MongoDB version of 4.2"),
                    reply: Vec::new(),
                });
            }
            let full = command_with_payload(&cmd.body, payload);
            let r = ac.encrypt(self, &cmd.db, &full);
            self.put_auto_crypt(ac);
            match r {
                Ok(Some(encrypted)) => {
                    sent = Some(encrypted);
                    payload = &[];
                }
                Ok(None) if !payload.is_empty() => {
                    // libmongoc sends the converted command (sequences as arrays) either way
                    sent = Some(full);
                    payload = &[];
                }
                Ok(None) => {}
                Err(error) => return Err(Failure { error, reply: Vec::new() }),
            }
        }
        let body: &[u8] = match &sent {
            Some(b) => b,
            None => &cmd.body,
        };
        let request_id = self.next_request_id();
        let started = std::time::Instant::now();
        let info = crate::apm::enabled().then(|| self.command_info(cmd, request_id));
        if let Some(info) = &info {
            let command = if crate::apm::is_sensitive(&cmd.name, body) { vec![5, 0, 0, 0, 0] } else { command_with_payload(body, payload) };
            self.td().events.push_now(crate::apm::Event::CommandStarted { info: info.clone(), command });
        }
        let r = self.run_command_payload(cmd.server_id, &cmd.name, &cmd.db, body, payload, cmd.is_acknowledged);
        if cmd.is_retryable_write || cmd.is_txn_finish {
            cmd.stream = self.with_stream_description(cmd.server_id, |d| (d.max_wire_version, d.ty == ServerType::Mongos)).unwrap_or((0, false));
        }
        let result = match r {
            Err(e) => {
                let mut reply = Vec::new();
                if let Some(t) = cmd.temp_session.as_mut() {
                    t.dirty = true;
                }
                if let Some(s) = session.as_deref_mut() {
                    s.mark_dirty();
                    if s.in_txn() {
                        add_error_label(&mut reply, "TransientTransactionError");
                        s.unpin();
                    }
                }
                Err(Failure { error: e, reply })
            }
            Ok(None) => Ok(Vec::new()),
            Ok(Some(reply)) => {
                self.td().update_cluster_time(&reply);
                let ok = crate::cmd::check_ok(&reply);
                if let Some(s) = session.as_deref_mut() {
                    s.handle_reply(cmd.is_acknowledged, &cmd.name, &reply);
                }
                match ok {
                    Ok(()) => Ok(reply),
                    Err(e) => Err(Failure { error: e.into_error(), reply }),
                }
            }
        };
        if let Some(info) = info {
            let duration_us = started.elapsed().as_micros() as i64;
            let redact = crate::apm::is_sensitive(&cmd.name, body);
            let empty = || vec![5u8, 0, 0, 0, 0];
            self.td().events.push_now(match &result {
                Ok(r) => {
                    // unacknowledged writes report {ok: 1}
                    let reply = if !cmd.is_acknowledged {
                        let mut w = Writer::with_capacity(16);
                        w.int32(b"ok", 1);
                        w.finish()
                    } else if redact || crate::apm::is_sensitive(&cmd.name, r) {
                        empty()
                    } else {
                        r.clone()
                    };
                    crate::apm::Event::CommandSucceeded { info, duration_us, reply }
                }
                Err(f) => {
                    let reply = if f.reply.is_empty() || redact || crate::apm::is_sensitive(&cmd.name, &f.reply) { empty() } else { f.reply.clone() };
                    crate::apm::Event::CommandFailed { info, duration_us, error: f.error.clone(), reply }
                }
            });
        }
        let mut result = result;
        // _mongoc_cse_auto_decrypt: a successful reply of a client with automatic encryption, after
        // the succeeded event; a failure skips the handlers below (fail_no_events)
        if self.auto_crypt_applies() {
            if let Ok(r) = &result {
                if !r.is_empty() {
                    let Some(mut ac) = self.take_auto_crypt().map_err(Failure::from)? else { return result };
                    let d = ac.decrypt(self, &cmd.db, r);
                    self.put_auto_crypt(ac);
                    match d {
                        Ok(d) => result = Ok(d),
                        Err(error) => return Err(Failure { error, reply: Vec::new() }),
                    }
                }
            }
        }
        if let Err(f) = &result {
            if !f.reply.is_empty() && !f.error.is_network() {
                self.handle_command_error(cmd.server_id, &f.reply);
            }
        }
        if cmd.is_txn_finish {
            let (max_wire, mongos) = cmd.stream;
            match &mut result {
                Ok(r) => write_error_handle_labels(None, r, max_wire, mongos),
                Err(f) => write_error_handle_labels(Some(&f.error), &mut f.reply, max_wire, mongos),
            }
        }
        if let (Ok(reply), Some(s)) = (&result, session.as_deref_mut()) {
            if s.in_txn_or_ending() && matches!(self.td().ty, TopologyType::Sharded | TopologyType::LoadBalanced) {
                if let Some(v) = find(reply, b"recoveryToken") {
                    s.recovery_token = match v {
                        Value::Document(d) => Some(d.to_vec()),
                        _ => {
                            crate::log::error(crate::log::CLUSTER, || "Malformed recovery token from server".to_owned());
                            None
                        }
                    };
                }
            }
        }
        result
    }

    /// An internal find (mongoc_collection_find_with_opts iterated to its end, as libmongoc's
    /// key vault reads do): an implicit session, the given read preference and read concern.
    pub fn find_all(&mut self, db: &str, coll: &str, filter: &[u8], rp: Option<(&crate::topology::ReadPrefs, (crate::uri::ReadMode, Vec<u8>))>, read_concern: &[u8]) -> Result<Vec<Vec<u8>>, Error> {
        let (sid, _) = self.select_server_id(OpType::Read, rp.as_ref().map(|r| r.0), &[])?;
        let mut session = self.start_session(true, false).ok();
        let mut w = Writer::with_capacity(filter.len() + coll.len() + 32);
        w.utf8(b"find", coll.as_bytes());
        w.document(b"filter", filter);
        let body = w.finish();
        let r = (|| -> Result<Vec<Vec<u8>>, Error> {
            let mut parts = Parts::new(db, &body);
            parts.is_read = true;
            parts.read_prefs = rp.as_ref().map(|r| r.1.clone());
            parts.read_concern = read_concern.to_vec();
            parts.session = session.as_mut();
            self.next_operation_id();
            let mut c = self.assemble(parts, sid, false)?;
            let el = self.read_eligibility(&c, false, true);
            let reply = self.run_retryable_read(&mut c, session.as_mut(), el).map_err(|f| f.error);
            self.release_temp_session(&mut c);
            let reply = reply?;
            let mut out = Vec::new();
            let (mut id, mut batch) = cursor_batch(&reply, b"firstBatch");
            loop {
                out.append(&mut batch);
                if id == 0 {
                    return Ok(out);
                }
                let mut g = Writer::with_capacity(64);
                g.int64(b"getMore", id);
                g.utf8(b"collection", coll.as_bytes());
                let gb = g.finish();
                let mut parts = Parts::new(db, &gb);
                parts.is_read = true;
                parts.session = session.as_mut();
                let mut c = self.assemble(parts, sid, false)?;
                let reply = self.run_monitored(&mut c, &[], session.as_mut()).map_err(|f| f.error);
                self.release_temp_session(&mut c);
                (id, batch) = cursor_batch(&reply?, b"nextBatch");
            }
        })();
        if let Some(mut s) = session.take() {
            self.end_session(&mut s);
        }
        r
    }

    /// mongoc_cluster_run_retryable_write: runs a write once, then once more on a retryable
    /// error (overload retries apart). Returns the outcome and the server of a retry.
    pub fn run_retryable_write(&mut self, cmd: &mut Cmd, payload: crate::wire::Payloads<'_>, mut session: Option<&mut ClientSession>) -> (Result<Vec<u8>, Failure>, Option<u32>) {
        if cmd.is_retryable_write {
            let n = match (session.as_deref_mut(), cmd.temp_session.as_mut()) {
                (Some(s), _) => s.next_txn_number(),
                (None, Some(t)) => {
                    t.txn_number += 1;
                    t.txn_number
                }
                _ => 0,
            };
            crate::bulk::set_txn_number(&mut cmd.body, n);
        }
        let eligibility = if cmd.is_retryable_write {
            Eligibility::RetryableWrite
        } else if self.uri.get_bool("retrywrites", true) {
            Eligibility::OverloadOnly
        } else {
            Eligibility::None
        };
        let mut kept: Option<Result<Vec<u8>, Failure>> = None;
        let mut retry_server = None;
        let mut attempt = 0;
        let mut allowed = if eligibility == Eligibility::OverloadOnly { 0 } else { 1 };
        let mut deprioritized: Vec<u32> = Vec::new();
        let mut backoff = Backoff::default();
        loop {
            let mut r = self.run_monitored(cmd, payload, session.as_deref_mut());
            if eligibility == Eligibility::RetryableWrite {
                let (max_wire, mongos) = cmd.stream;
                match &mut r {
                    Ok(rep) => write_error_handle_labels(None, rep, max_wire, mongos),
                    Err(f) => {
                        write_error_handle_labels(Some(&f.error), &mut f.reply, max_wire, mongos);
                        update_if_unsupported_storage_engine(f);
                    }
                }
            }
            // everything the retry decision needs is read here (errorLabels looked up once), so
            // the reply is not copied before r is kept
            let reply = match &r {
                Ok(rep) | Err(Failure { reply: rep, .. }) => rep.as_slice(),
            };
            let labels = crate::cmd::error_labels(reply);
            let is_retryable_write = eligibility == Eligibility::RetryableWrite && write_error_type_with(reply, labels) == WriteErr::Retry;
            let (is_overload, overload_retryable) = overload(labels, eligibility);
            let keep_previous = kept.is_some() && crate::cmd::labels_have(labels, "NoWritesPerformed");
            let base_backoff = if is_overload { backoff_base_ms(reply) } else { 0 };
            if !keep_previous {
                kept = Some(r);
            }
            if !(is_retryable_write || overload_retryable) {
                break;
            }
            attempt += 1;
            if is_overload {
                allowed = self.max_adaptive_retries();
            }
            if attempt > allowed {
                break;
            }
            if self.td().server(cmd.server_id).is_some_and(|s| s.ty == ServerType::Mongos) || (is_overload && self.uri.get_bool("enableoverloadretargeting", false)) {
                deprioritized.push(cmd.server_id);
            }
            backoff.wait(is_overload, base_backoff);
            let sel = self.select_for_optype(OpType::Write, None, session.as_deref_mut(), &deprioritized);
            let Ok((sid, _)) = sel else { break };
            // mongoc_cluster_stream_for_writes: no stream for the retry ends it (the previous
            // attempt's outcome is kept)
            if !self.retry_stream(sid, self.uri.get_bool("retrywrites", true)) {
                break;
            }
            cmd.server_id = sid;
            retry_server = Some(sid);
        }
        (kept.expect("at least one attempt"), retry_server)
    }

    /// _mongoc_client_retryable_read_command_with_stream: runs a read once, then once more
    /// on a retryable error.
    pub fn run_retryable_read(&mut self, cmd: &mut Cmd, mut session: Option<&mut ClientSession>, eligibility: Eligibility) -> Result<Vec<u8>, Failure> {
        let mut attempt = 0;
        let mut allowed = if eligibility == Eligibility::OverloadOnly { 0 } else { 1 };
        let mut deprioritized: Vec<u32> = Vec::new();
        let mut backoff = Backoff::default();
        loop {
            let r = self.run_monitored(cmd, &[], session.as_deref_mut());
            let (reply, err) = match &r {
                Ok(rep) => (rep.as_slice(), None),
                Err(f) => (f.reply.as_slice(), Some(&f.error)),
            };
            let is_retryable_read = eligibility == Eligibility::RetryableRead && read_error_is_retryable(err, reply);
            let (is_overload, overload_retryable) = overload(crate::cmd::error_labels(reply), eligibility);
            if !(is_retryable_read || overload_retryable) {
                return r;
            }
            attempt += 1;
            if is_overload {
                allowed = self.max_adaptive_retries();
            }
            if attempt > allowed {
                return r;
            }
            if self.td().server(cmd.server_id).is_some_and(|s| s.ty == ServerType::Mongos) || (is_overload && self.uri.get_bool("enableoverloadretargeting", false)) {
                deprioritized.push(cmd.server_id);
            }
            backoff.wait(is_overload, if is_overload { backoff_base_ms(reply) } else { 0 });
            let prefs = cmd.read_prefs.clone();
            let sel = self.select_for_optype(OpType::Read, prefs.as_ref(), session.as_deref_mut(), &deprioritized);
            let Ok((sid, _)) = sel else { return r };
            // mongoc_cluster_stream_for_reads: no stream for the retry ends it
            if !self.retry_stream(sid, self.uri.get_bool("retryreads", true)) {
                return r;
            }
            cmd.server_id = sid;
        }
    }

    /// The stream part of _mongoc_cluster_stream_for_optype for a retry attempt
    /// (select_retry_server, which asks for no reply or error): a network or authentication
    /// error is tried once more when `retryable`. False when there is no stream.
    fn retry_stream(&mut self, server_id: u32, retryable: bool) -> bool {
        match self.stream_for_server(server_id, true) {
            Ok(_) => true,
            Err(e) if retryable && (e.is_network() || e.is_auth()) => self.stream_for_server(server_id, true).is_ok(),
            Err(_) => false,
        }
    }

    /// The retry eligibility of a read (or raw) command.
    pub fn read_eligibility(&self, cmd: &Cmd, is_raw: bool, is_read: bool) -> Eligibility {
        if cmd.is_retryable_read {
            return Eligibility::RetryableRead;
        }
        let rr = self.uri.get_bool("retryreads", true);
        if (is_raw && (!rr || !self.uri.get_bool("retrywrites", true))) || (is_read && !rr) {
            return Eligibility::None;
        }
        Eligibility::OverloadOnly
    }

    /// _mongoc_uri_get_max_adaptive_retries
    fn max_adaptive_retries(&self) -> i32 {
        match self.uri.options.get("maxadaptiveretries") {
            Some(crate::uri::OptVal::Int32(v)) => *v,
            _ => DEFAULT_MAX_ADAPTIVE_RETRIES,
        }
    }

    /// _mongoc_cluster_select_server_id: server selection that honours (and maintains) a
    /// sharded transaction's pinned server.
    pub fn select_for_session(&mut self, optype: OpType, rp: Option<&crate::topology::ReadPrefs>, cs: Option<&mut ClientSession>, deprioritized: &[u32]) -> Result<(u32, bool), Error> {
        let sharded_txn = cs.as_ref().is_some_and(|s| s.in_txn_or_ending()) && self.td().ty == TopologyType::Sharded;
        match cs {
            Some(s) if sharded_txn => {
                if s.pinned_server_id != 0 {
                    return Ok((s.pinned_server_id, false));
                }
                let r = self.select_server_id(optype, rp, deprioritized)?;
                s.pinned_server_id = r.0;
                Ok(r)
            }
            cs => {
                let r = self.select_server_id(optype, rp, deprioritized)?;
                if let Some(s) = cs {
                    if !s.in_txn_or_ending() {
                        s.unpin();
                    }
                }
                Ok(r)
            }
        }
    }

    /// The selection of _mongoc_cluster_stream_for_optype: select_for_session, then the
    /// connection check of mongoc_cluster_check_interval; a failed check selects once more.
    pub fn select_for_optype(&mut self, optype: OpType, rp: Option<&crate::topology::ReadPrefs>, mut cs: Option<&mut ClientSession>, deprioritized: &[u32]) -> Result<(u32, bool), Error> {
        let r = self.select_for_session(optype, rp, cs.as_deref_mut(), deprioritized)?;
        if self.check_interval(r.0) {
            return Ok(r);
        }
        self.select_for_session(optype, rp, cs, deprioritized)
    }

    /// mongoc_cluster_stream_for_server's session rules: a pinned session only runs on its
    /// server; a sharded transaction pins, other operations unpin.
    pub fn check_server_for_session(&mut self, server_id: u32, cs: Option<&mut ClientSession>) -> Result<(), Error> {
        let Some(s) = cs else { return Ok(()) };
        if s.pinned_server_id != 0 && s.pinned_server_id != server_id {
            return Err(Error::new(domain::COMMAND, code::SERVER_SELECTION_INVALID_ID, "Requested server id does not matched pinned server id"));
        }
        if s.in_txn_or_ending() && self.td().ty == TopologyType::Sharded {
            s.pinned_server_id = server_id;
        } else if !s.in_txn_or_ending() {
            s.unpin();
        }
        Ok(())
    }

    /// The event fields of `cmd` (mongoc_apm_command_started_init_with_cmd).
    fn command_info(&self, cmd: &Cmd, request_id: i32) -> crate::apm::CommandInfo {
        let (addr, sid, connection_id) = self.with_stream_description(cmd.server_id, |d| (d.addr.clone(), d.service_id, d.connection_id)).unwrap_or((String::new(), crate::sdam::ZERO_OID, -1));
        let (host, port) = crate::client::split_host_port(&addr);
        let service_id = Some(sid).filter(|s| *s != crate::sdam::ZERO_OID);
        crate::apm::CommandInfo {
            command_name: cmd.name.clone(),
            database_name: cmd.db.clone(),
            request_id: request_id as i64,
            operation_id: cmd.operation_id,
            host,
            port,
            server_id: cmd.server_id,
            service_id,
            server_connection_id: connection_id,
        }
    }

    fn next_request_id(&mut self) -> i32 {
        let id = self.request_id.get().wrapping_add(1);
        self.request_id.set(id);
        id
    }
}

/// The command with its document sequences appended as arrays (_mongoc_cmd_append_payload_as_array).
fn command_with_payload(body: &[u8], payload: crate::wire::Payloads<'_>) -> Vec<u8> {
    if payload.is_empty() {
        return body.to_vec();
    }
    let mut w = Writer::with_capacity(body.len() + payload.iter().map(|p| p.1.len()).sum::<usize>() + 64);
    w.concat(body);
    for (field, docs) in payload {
        append_payload_array(&mut w, field, docs);
    }
    w.finish()
}

fn append_payload_array(w: &mut Writer, field: &str, docs: &[u8]) {
    let a = w.begin(4, field.as_bytes());
    let mut buf = crate::bson::writer::itoa_buf::Buf::default();
    let mut pos = 0usize;
    let mut i = 0usize;
    while pos + 4 <= docs.len() {
        let len = i32::from_le_bytes(docs[pos..pos + 4].try_into().unwrap()) as usize;
        if len < 5 || pos + len > docs.len() {
            break;
        }
        w.document(crate::bson::writer::index_key(i, &mut buf), &docs[pos..pos + len]);
        pos += len;
        i += 1;
    }
    w.end(a);
}

/// MONGOC_DEFAULT_MAXADAPTIVERETRIES
pub const DEFAULT_MAX_ADAPTIVE_RETRIES: i32 = 2;
/// WIRE_VERSION_SNAPSHOT_READS
pub const WIRE_VERSION_SNAPSHOT_READS: i32 = 13;
/// WIRE_VERSION_ERROR_LABEL_RETRYABLEWRITEERROR
const WIRE_VERSION_ERROR_LABEL_RETRYABLE_WRITE_ERROR: i32 = 9;

/// mongoc_retry_eligibility_t
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Eligibility {
    None,
    OverloadOnly,
    RetryableRead,
    RetryableWrite,
}

/// The overload labels of a reply (its errorLabels): (SystemOverloadedError, and retryable as
/// such).
fn overload(labels: &[u8], e: Eligibility) -> (bool, bool) {
    let o = crate::cmd::labels_have(labels, "SystemOverloadedError");
    (o, o && crate::cmd::labels_have(labels, "RetryableError") && e != Eligibility::None)
}

/// The reply's baseBackoffMS (0: none).
fn backoff_base_ms(reply: &[u8]) -> i64 {
    match find(reply, b"baseBackoffMS") {
        Some(v @ (Value::Int32(_) | Value::Int64(_))) => crate::sdam::as_i64(&v),
        _ => 0,
    }
}

/// The exponential backoff of overload retries (mongoc-retry-backoff-generator.c: base
/// 100 ms or the reply's baseBackoffMS, growth 2, max 10 s, full jitter).
#[derive(Default)]
struct Backoff {
    attempt: u32,
}

impl Backoff {
    /// `base_ms`: the reply's baseBackoffMS (backoff_base_ms)
    fn wait(&mut self, is_overload: bool, base_ms: i64) {
        let n = self.attempt;
        self.attempt += 1;
        if !is_overload {
            return;
        }
        let base = if base_ms > 0 { base_ms as f64 } else { 100.0 };
        let max = (base * 2f64.powi(n as i32)).min(10_000.0);
        let r = u32::from_le_bytes(crate::bson::oid::random_bytes(4)[..4].try_into().unwrap()) as f64 / u32::MAX as f64;
        std::thread::sleep(std::time::Duration::from_micros((max * r * 1000.0) as u64));
    }
}

/// mongoc_write_err_type_t
#[derive(PartialEq, Eq)]
pub enum WriteErr {
    None,
    Retry,
    WriteConcern,
    Other,
}

/// _mongoc_write_error_get_type
pub fn write_error_type(reply: &[u8]) -> WriteErr {
    write_error_type_with(reply, crate::cmd::error_labels(reply))
}

/// write_error_type with the reply's errorLabels already looked up.
fn write_error_type_with(reply: &[u8], labels: &[u8]) -> WriteErr {
    if crate::cmd::labels_have(labels, "RetryableWriteError") {
        return WriteErr::Retry;
    }
    match crate::cmd::check_ok_no_wce(reply) {
        Ok(()) => WriteErr::None,
        Err(e) if e.code == 64 => WriteErr::WriteConcern,
        Err(_) => WriteErr::Other,
    }
}

/// _mongoc_write_error_is_retryable (server codes)
fn write_code_is_retryable(c: u32) -> bool {
    matches!(c, 6 | 7 | 89 | 91 | 189 | 262 | 9001 | 10107 | 11600 | 11602 | 13435 | 13436)
}

/// _mongoc_read_error_get_type == MONGOC_READ_ERR_RETRY
pub fn read_error_is_retryable(err: Option<&Error>, reply: &[u8]) -> bool {
    if err.is_some_and(|e| e.is_network()) {
        return true;
    }
    match crate::cmd::check_ok_no_wce(reply) {
        Ok(()) => false,
        Err(e) => matches!(e.code, 262 | 11600 | 11602 | 10107 | 13435 | 13436 | 189 | 134 | 91 | 7 | 6 | 89 | 9001) || e.message.contains("not master") || e.message.contains("node is recovering"),
    }
}

/// _mongoc_write_error_handle_labels: RetryableWriteError on network errors (and, for
/// servers before 4.4, on retryable server errors).
fn write_error_handle_labels(err: Option<&Error>, reply: &mut Vec<u8>, max_wire: i32, mongos: bool) {
    if err.is_some_and(|e| e.is_network()) {
        append_retryable_label(reply);
        return;
    }
    if max_wire >= WIRE_VERSION_ERROR_LABEL_RETRYABLE_WRITE_ERROR {
        return;
    }
    let r = if mongos { crate::cmd::check_ok(reply) } else { crate::cmd::check_ok_no_wce(reply) };
    if let Err(e) = r {
        if write_code_is_retryable(e.code) {
            append_retryable_label(reply);
        }
    }
}

/// _mongoc_write_error_update_if_unsupported_storage_engine
fn update_if_unsupported_storage_engine(f: &mut Failure) {
    let Err(e) = crate::cmd::check_ok_no_wce(&f.reply) else { return };
    if e.code == 20 && e.message.starts_with("Transaction numbers") {
        const M: &str = "This MongoDB deployment does not support retryable writes. Please add retryWrites=false to your connection string.";
        f.error.message = M.to_owned();
        let mut w = Writer::with_capacity(f.reply.len() + 64);
        if let Some(mut it) = Iter::new(&f.reply) {
            while it.next() {
                if it.key() != b"errmsg" {
                    w.element_from(None, &it);
                }
            }
        }
        w.utf8(b"errmsg", M.as_bytes());
        f.reply = w.finish();
    }
}

/// The labels of `reply` (strings of its errorLabels array).
fn labels_of(reply: &[u8]) -> Vec<Vec<u8>> {
    let mut v = Vec::new();
    if let Some(Value::Array(a)) = find(reply, b"errorLabels") {
        if let Some(mut it) = Iter::new(a) {
            while it.next() {
                if let Value::Utf8(s) = it.value() {
                    v.push(s.to_vec());
                }
            }
        }
    }
    v
}

/// `reply` without errorLabels, then errorLabels = `labels`.
fn with_labels(reply: &[u8], labels: &[Vec<u8>]) -> Vec<u8> {
    let mut w = Writer::with_capacity(reply.len() + 64);
    if let Some(mut it) = Iter::new(reply) {
        while it.next() {
            if it.key() != b"errorLabels" {
                w.element_from(None, &it);
            }
        }
    }
    let mut a = Writer::with_capacity(64);
    let mut buf = crate::bson::writer::itoa_buf::Buf::default();
    for (i, l) in labels.iter().enumerate() {
        a.utf8(crate::bson::writer::index_key(i, &mut buf), l);
    }
    w.array(b"errorLabels", &a.finish());
    w.finish()
}

/// _mongoc_add_error_label (an empty `reply` is an empty document)
pub fn add_error_label(reply: &mut Vec<u8>, label: &str) {
    let mut labels = Vec::new();
    for l in labels_of(reply) {
        if !labels.contains(&l) {
            labels.push(l);
        }
    }
    if !labels.iter().any(|l| l == label.as_bytes()) {
        labels.push(label.as_bytes().to_vec());
    }
    let base = if reply.is_empty() { vec![5, 0, 0, 0, 0] } else { std::mem::take(reply) };
    *reply = with_labels(&base, &labels);
}

/// _mongoc_error_copy_labels_and_upsert onto the reply itself: `label` first, then the others.
pub fn upsert_label_first(reply: &mut Vec<u8>, label: &str) {
    let mut labels = vec![label.as_bytes().to_vec()];
    labels.extend(labels_of(reply).into_iter().filter(|l| l != label.as_bytes()));
    let base = if reply.is_empty() { vec![5, 0, 0, 0, 0] } else { std::mem::take(reply) };
    *reply = with_labels(&base, &labels);
}

/// _mongoc_write_error_append_retryable_label
fn append_retryable_label(reply: &mut Vec<u8>) {
    upsert_label_first(reply, "RetryableWriteError");
}

/// A server reply's field.
pub fn find<'a>(doc: &'a [u8], k: &[u8]) -> Option<Value<'a>> {
    let mut it = Iter::new(doc)?;
    it.find(k).then(|| it.value())
}

impl Client {
    /// mongoc_client_start_session (_mongoc_topology_pop_server_session). The session's
    /// default transaction options are the client's, overridden by `default_txn`.
    pub fn start_session(&mut self, causal_consistency: bool, snapshot: bool) -> Result<ClientSession, Error> {
        self.start_session_with(causal_consistency, snapshot, None)
    }

    pub fn start_session_with(&mut self, causal_consistency: bool, snapshot: bool, default_txn: Option<&crate::session::TxnOpts>) -> Result<ClientSession, Error> {
        if let Some(r) = self.shared().session_start(causal_consistency, snapshot, default_txn, true) {
            return r;
        }
        // session support unknown yet: a server selection learns it
        self.select_server_id(OpType::Read, None, &[])?;
        self.shared().session_start(causal_consistency, snapshot, default_txn, false).expect("decided without selection")
    }
}

impl crate::client::Shared {
    /// Pops a server session for an implicit session.
    pub fn pop_server_session(&self) -> ServerSession {
        let now = crate::sdam::monotonic_usec();
        let t = self.td.borrow().session_timeout_minutes;
        self.sessions.borrow_mut().pop(t, now)
    }

    /// Returns a server session to the pool (dropped if the pool is borrowed: never re-entered
    /// from a running operation).
    pub fn push_server_session(&self, s: ServerSession) {
        let now = crate::sdam::monotonic_usec();
        let t = self.td.borrow().session_timeout_minutes;
        if let Ok(mut p) = self.sessions.try_borrow_mut() {
            p.push(s, t, now);
        }
    }

    /// mongoc_client_start_session without I/O: None when session support is unknown yet and
    /// `may_select` (the caller selects a server first, then calls again with false).
    pub fn session_start(&self, causal_consistency: bool, snapshot: bool, default_txn: Option<&crate::session::TxnOpts>, may_select: bool) -> Option<Result<ClientSession, Error>> {
        {
            let td = self.td.borrow();
            if td.ty != TopologyType::LoadBalanced && td.session_timeout_minutes == crate::sdam::NO_SESSIONS {
                if may_select && !td.has_data_node() {
                    return None;
                }
                return Some(Err(Error::new(domain::CLIENT, code::CLIENT_SESSION_FAILURE, "Server does not support sessions")));
            }
        }
        if causal_consistency && snapshot {
            return Some(Err(Error::new(domain::CLIENT, code::CLIENT_SESSION_FAILURE, "Only one of causal consistency and snapshot can be enabled.")));
        }
        let ss = self.pop_server_session();
        let mut d = crate::session::TxnOpts {
            read_concern: Some(crate::concern::ReadConcern::from_uri(&self.uri)),
            write_concern: Some(crate::concern::WriteConcern::from_uri(&self.uri)),
            read_primary: Some(self.uri.read_mode == ReadMode::Primary),
            max_commit_time_ms: crate::session::DEFAULT_MAX_COMMIT_TIME_MS,
        };
        if let Some(o) = default_txn {
            d.set(o);
        }
        Some(Ok(ClientSession {
            server_session: Some(ss),
            causal_consistency,
            snapshot,
            default_txn: d,
            ..Default::default()
        }))
    }

    /// mongoc_client_session_destroy without I/O: false when a running transaction must be
    /// aborted first (the caller uses a cluster: Client::end_session).
    pub fn session_end(&self, cs: &mut ClientSession) -> bool {
        if cs.server_session.is_none() {
            return true;
        }
        if cs.in_txn() {
            return false;
        }
        if let Some(ss) = cs.server_session.take() {
            self.push_server_session(ss);
        }
        true
    }
}

impl Client {
    /// mongoc_client_session_destroy: a running transaction is aborted, the server session
    /// goes back to the pool.
    pub fn end_session(&mut self, cs: &mut ClientSession) {
        if cs.server_session.is_none() {
            return;
        }
        if cs.in_txn() {
            let _ = self.abort_transaction(cs);
        }
        if let Some(ss) = cs.server_session.take() {
            self.push_server_session(ss);
        }
    }

    /// mongoc_client_session_start_transaction
    pub fn start_transaction(&mut self, cs: &mut ClientSession, opts: Option<&crate::session::TxnOpts>) -> Result<(), Error> {
        self.select_for_optype(OpType::Write, None, Some(cs), &[])?;
        cs.start_transaction(opts)
    }

    /// mongoc_client_session_commit_transaction: Ok(reply) or the error with the reply.
    pub fn commit_transaction(&mut self, cs: &mut ClientSession) -> Result<Vec<u8>, Failure> {
        let invalid = |m: &str| Err(Failure { error: Error::new(domain::TRANSACTION, code::TRANSACTION_INVALID_STATE, m), reply: vec![5, 0, 0, 0, 0] });
        match cs.txn_state {
            TxnState::None => invalid("No transaction started"),
            TxnState::Starting | TxnState::CommittedEmpty => {
                cs.txn_state = TxnState::CommittedEmpty;
                Ok(vec![5, 0, 0, 0, 0])
            }
            TxnState::Committed | TxnState::InProgress => {
                let explicitly_retrying = cs.txn_state == TxnState::Committed;
                cs.txn_state = TxnState::Ending;
                let r = self.txn_commit(cs, explicitly_retrying);
                cs.txn_state = TxnState::Committed;
                r
            }
            TxnState::Ending => invalid("No transaction started"),
            TxnState::Aborted => invalid("Cannot call commitTransaction after calling abortTransaction"),
        }
    }

    /// mongoc_client_session_abort_transaction (errors of the command itself are ignored).
    pub fn abort_transaction(&mut self, cs: &mut ClientSession) -> Result<(), Error> {
        let invalid = |m: &str| Err(Error::new(domain::TRANSACTION, code::TRANSACTION_INVALID_STATE, m));
        match cs.txn_state {
            TxnState::Starting => {
                cs.txn_state = TxnState::Aborted;
                cs.unpin();
                cs.txn.cleanup();
                Ok(())
            }
            TxnState::InProgress => {
                cs.txn_state = TxnState::Ending;
                self.txn_abort(cs);
                cs.txn_state = TxnState::Aborted;
                cs.unpin();
                Ok(())
            }
            TxnState::Committed | TxnState::CommittedEmpty => invalid("Cannot call abortTransaction after calling commitTransaction"),
            TxnState::Aborted => invalid("Cannot call abortTransaction twice"),
            TxnState::Ending | TxnState::None => invalid("No transaction started"),
        }
    }

    fn txn_finish_command(name: &[u8], cs: &ClientSession) -> Vec<u8> {
        let mut w = Writer::with_capacity(64);
        w.int32(name, 1);
        if let Some(t) = &cs.recovery_token {
            w.document(b"recoveryToken", t);
        }
        w.finish()
    }

    /// txn_abort
    fn txn_abort(&mut self, cs: &mut ClientSession) {
        let body = Self::txn_finish_command(b"abortTransaction", cs);
        let wc = cs.txn.write_concern.clone();
        let r = self.write_command_in_session(&body, wc.as_ref(), None, cs);
        let retry = match &r {
            Ok(rep) | Err(Failure { reply: rep, .. }) => crate::cmd::has_label(rep, "RetryableWriteError"),
        };
        let r = if retry {
            cs.unpin();
            self.write_command_in_session(&body, wc.as_ref(), None, cs)
        } else {
            r
        };
        if let Err(f) = r {
            // abortTransaction never returns an error, so it warns
            crate::log::warning(crate::log::MONGOC, || format!("Error in abortTransaction: {}", f.error.message));
            cs.unpin();
        }
    }

    /// txn_commit
    fn txn_commit(&mut self, cs: &mut ClientSession, explicitly_retrying: bool) -> Result<Vec<u8>, Failure> {
        let body = Self::txn_finish_command(b"commitTransaction", cs);
        let max_time = (cs.txn.max_commit_time_ms != crate::session::DEFAULT_MAX_COMMIT_TIME_MS).then_some(cs.txn.max_commit_time_ms);
        let retry_wc = |cs: &ClientSession, client: &Client| {
            let mut wc = cs.txn.write_concern.clone().unwrap_or_else(|| crate::concern::WriteConcern::from_uri(&client.uri));
            if wc.wtimeout <= 0 {
                wc.set_wtimeout(MONGOC_DEFAULT_WTIMEOUT_FOR_COMMIT_RETRY);
            }
            wc.set_w(crate::uri::W_MAJORITY);
            wc
        };
        let mut retrying_after_error = false;
        let mut wc = if explicitly_retrying { Some(retry_wc(cs, self)) } else { cs.txn.write_concern.clone() };
        let mut r;
        loop {
            r = self.write_command_in_session(&body, wc.as_ref(), max_time, cs);
            let reply = match &r {
                Ok(rep) | Err(Failure { reply: rep, .. }) => rep.as_slice(),
            };
            if !retrying_after_error && write_error_type(reply) == WriteErr::Retry {
                retrying_after_error = true;
                cs.unpin();
                wc = Some(retry_wc(cs, self));
                continue;
            }
            break;
        }
        if let Err(f) = &mut r {
            let ty = write_error_type(&f.reply);
            if f.error.domain == domain::SERVER_SELECTION || ty == WriteErr::Retry || ty == WriteErr::WriteConcern || f.error.code == code::MAX_TIME_MS_EXPIRED {
                cs.unpin();
                upsert_label_first(&mut f.reply, "UnknownTransactionCommitResult");
            }
        }
        r
    }

    /// mongoc_client_write_command_with_opts("admin", body, {session, writeConcern, maxTimeMS})
    /// for the transaction commands.
    fn write_command_in_session(&mut self, body: &[u8], wc: Option<&crate::concern::WriteConcern>, max_time_ms: Option<i64>, cs: &mut ClientSession) -> Result<Vec<u8>, Failure> {
        let sid = match self.select_for_optype(OpType::Write, None, Some(cs), &[]) {
            Ok((s, _)) => s,
            Err(e) => {
                let mut reply = Vec::new();
                if cs.in_txn() {
                    add_error_label(&mut reply, "TransientTransactionError");
                }
                return Err(Failure { error: e, reply: if reply.is_empty() { vec![5, 0, 0, 0, 0] } else { reply } });
            }
        };
        let mut parts = Parts::new("admin", body);
        parts.is_write = true;
        if let Some(t) = max_time_ms {
            parts.extra.int64(b"maxTimeMS", t);
        }
        match wc {
            Some(w) => parts.set_write_concern(w),
            None => {
                let cw = crate::concern::WriteConcern::from_uri(&self.uri);
                if !cw.is_default {
                    parts.set_write_concern(&cw);
                }
            }
        }
        parts.session = Some(&mut *cs);
        self.next_operation_id();
        let mut cmd = self.assemble(parts, sid, false)?;
        let (r, _) = self.run_retryable_write(&mut cmd, &[], Some(cs));
        self.release_temp_session(&mut cmd);
        match r {
            Ok(reply) => match crate::cmd::parse_wc_error(&reply) {
                Some(e) => Err(Failure { error: e, reply }),
                None => Ok(reply),
            },
            Err(f) => Err(f),
        }
    }
}

impl Client {
    /// _mongoc_client_end_sessions (mongoc_client_destroy): endSessions for the pooled
    /// server sessions, in batches of 10000, on a primaryPreferred server; only by the process
    /// that created the client (phongo_pclient_destroy).
    pub fn end_pooled_sessions(&mut self) {
        if !self.in_creating_process() {
            return;
        }
        let rp = crate::topology::ReadPrefs { mode: ReadMode::PrimaryPreferred, ..Default::default() };
        loop {
            let lsids: Vec<Vec<u8>> = self.sessions.borrow().lsids().take(10_000).map(<[u8]>::to_vec).collect();
            if lsids.is_empty() {
                return;
            }
            let sid = match self.select_server_id(OpType::Read, Some(&rp), &[]) {
                Ok((sid, _)) => sid,
                Err(e) => return end_sessions_warning("Couldn't send", &e),
            };
            let mut w = Writer::with_capacity(64 + lsids.len() * 40);
            let a = w.begin(4, b"endSessions");
            let mut buf = crate::bson::writer::itoa_buf::Buf::default();
            for (i, l) in lsids.iter().enumerate() {
                w.document(crate::bson::writer::index_key(i, &mut buf), l);
            }
            w.end(a);
            let body = w.finish();
            self.sessions.borrow_mut().drop_first(lsids.len());
            if let Err(e) = self.stream_for_server(sid, false) {
                return end_sessions_warning("Couldn't send", &e);
            }
            let mut parts = Parts::new("admin", &body);
            parts.prohibit_lsid = true;
            self.next_operation_id();
            let mut cmd = match self.assemble(parts, sid, false) {
                Ok(c) => c,
                Err(e) => return end_sessions_warning("Couldn't construct", &e),
            };
            if let Err(f) = self.run_monitored(&mut cmd, &[], None) {
                end_sessions_warning("Couldn't send", &f.error);
            }
        }
    }
}

fn end_sessions_warning(what: &str, e: &Error) {
    crate::log::warning(crate::log::CLIENT, || format!("{what} \"endSessions\": {}", e.message));
}

/// MONGOC_DEFAULT_WTIMEOUT_FOR_COMMIT_RETRY
pub const MONGOC_DEFAULT_WTIMEOUT_FOR_COMMIT_RETRY: i64 = 10_000;

/// The cursor id and the documents of a find/getMore reply's batch
pub(crate) fn cursor_batch(reply: &[u8], key: &[u8]) -> (i64, Vec<Vec<u8>>) {
    let Some(Value::Document(c)) = find(reply, b"cursor") else { return (0, Vec::new()) };
    let id = match find(c, b"id") {
        Some(Value::Int64(i)) => i,
        Some(Value::Int32(i)) => i as i64,
        _ => 0,
    };
    let mut docs = Vec::new();
    if let Some(Value::Array(a)) = find(c, key) {
        if let Some(mut it) = crate::bson::iter::Iter::new(a) {
            while it.next() {
                if let Value::Document(d) = it.value() {
                    docs.push(d.to_vec());
                }
            }
        }
    }
    (id, docs)
}
