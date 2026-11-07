//! Server sessions and their pool (mongoc-server-session.c, the pool parts of
//! mongoc-topology.c). Client sessions build on these (mongoc-client-session.c).

use crate::bson::writer::Writer;

/// SESSION_NEVER_USED
pub const NEVER_USED: i64 = -1;

/// mongoc_server_session_t
#[derive(Debug)]
pub struct ServerSession {
    /// {"id": UUID (binary subtype 4)}
    pub lsid: Vec<u8>,
    pub last_used_usec: i64,
    pub txn_number: i64,
    /// a network error happened while the session was in use: never pooled again
    pub dirty: bool,
}

impl ServerSession {
    /// _mongoc_server_session_new: a random (version 4) UUID.
    pub fn new() -> Self {
        let mut id = crate::bson::oid::random_bytes(16);
        id[6] = 0x40 | (id[6] & 0x0f);
        id[8] = 0x80 | (id[8] & 0x3f);
        let mut w = Writer::with_capacity(40);
        w.binary(b"id", 4, &id);
        ServerSession { lsid: w.finish(), last_used_usec: NEVER_USED, txn_number: 0, dirty: false }
    }

    /// _mongoc_server_session_timed_out: less than a minute left before the server forgets it.
    pub fn timed_out(&self, timeout_minutes: i64, now_usec: i64) -> bool {
        if timeout_minutes == crate::sdam::NO_SESSIONS || self.last_used_usec == NEVER_USED {
            return false;
        }
        let expires = self.last_used_usec + timeout_minutes * 60 * 1_000_000;
        expires - now_usec < 60 * 1_000_000
    }
}

impl Default for ServerSession {
    fn default() -> Self {
        Self::new()
    }
}

/// The topology's pool of server sessions (most recently used first).
#[derive(Debug, Default)]
pub struct Pool {
    sessions: Vec<ServerSession>,
}

impl Pool {
    /// _mongoc_topology_pop_server_session: a pooled session that is not about to expire, or
    /// a new one.
    pub fn pop(&mut self, timeout_minutes: i64, now_usec: i64) -> ServerSession {
        while let Some(s) = self.sessions.pop() {
            if !s.timed_out(timeout_minutes, now_usec) {
                return s;
            }
        }
        ServerSession::new()
    }

    /// _mongoc_topology_push_server_session: dirty or expiring sessions are discarded.
    pub fn push(&mut self, s: ServerSession, timeout_minutes: i64, now_usec: i64) {
        if s.dirty || s.timed_out(timeout_minutes, now_usec) {
            return;
        }
        // drop the expired ones at the cold end
        self.sessions.retain(|o| !o.timed_out(timeout_minutes, now_usec));
        self.sessions.push(s);
    }

    /// The pooled sessions' lsids (endSessions at client destruction).
    pub fn lsids(&self) -> impl Iterator<Item = &[u8]> {
        self.sessions.iter().map(|s| s.lsid.as_slice())
    }

    pub fn clear(&mut self) {
        self.sessions.clear();
    }

    /// Removes the sessions `lsids()` yielded first.
    pub fn drop_first(&mut self, n: usize) {
        let n = n.min(self.sessions.len());
        self.sessions.drain(..n);
    }
}

/// mongoc_internal_transaction_state_t
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TxnState {
    #[default]
    None,
    Starting,
    InProgress,
    /// commitTransaction / abortTransaction running
    Ending,
    Committed,
    Aborted,
    /// committed with an empty transaction (no command sent)
    CommittedEmpty,
}

/// DEFAULT_MAX_COMMIT_TIME_MS
pub const DEFAULT_MAX_COMMIT_TIME_MS: i64 = 0;

/// mongoc_transaction_opt_t (the read preference stays with the caller: libmongoc only
/// checks that it is primary, `read_primary`).
#[derive(Clone, Debug, Default)]
pub struct TxnOpts {
    pub read_concern: Option<ReadConcern>,
    pub write_concern: Option<WriteConcern>,
    /// None: no read preference; Some(true): primary
    pub read_primary: Option<bool>,
    pub max_commit_time_ms: i64,
}

impl TxnOpts {
    /// txn_opts_set: only the given values replace the current ones.
    pub fn set(&mut self, o: &TxnOpts) {
        if o.read_concern.is_some() {
            self.read_concern = o.read_concern.clone();
        }
        if o.write_concern.is_some() {
            self.write_concern = o.write_concern.clone();
        }
        if o.read_primary.is_some() {
            self.read_primary = o.read_primary;
        }
        if o.max_commit_time_ms != DEFAULT_MAX_COMMIT_TIME_MS {
            self.max_commit_time_ms = o.max_commit_time_ms;
        }
    }

    /// txn_opts_cleanup
    pub fn cleanup(&mut self) {
        *self = TxnOpts::default();
    }
}

/// mongoc_client_session_t: an explicit session.
#[derive(Debug, Default)]
pub struct ClientSession {
    /// None once ended
    pub server_session: Option<ServerSession>,
    /// mongoc_session_opts_get_causal_consistency (true unless disabled or snapshot)
    pub causal_consistency: bool,
    pub snapshot: bool,
    /// empty when unset
    pub cluster_time: Option<Vec<u8>>,
    /// operationTime (t, i), (0, 0) when unset
    pub operation_time: (u32, u32),
    pub snapshot_time: Option<(u32, u32)>,
    pub txn_state: TxnState,
    /// the running transaction's options (session->txn.opts)
    pub txn: TxnOpts,
    /// session->opts.default_txn_opts
    pub default_txn: TxnOpts,
    pub recovery_token: Option<Vec<u8>>,
    /// the server a sharded transaction is pinned to (0: none)
    pub pinned_server_id: u32,
}

/// The lsid of an ended session (never sent: every caller checks liveness first).
const EMPTY_DOC: [u8; 5] = [5, 0, 0, 0, 0];

/// _mongoc_parse_cluster_time
pub fn parse_cluster_time(t: &[u8]) -> Option<(u32, u32)> {
    use crate::bson::iter::{Iter, Value};
    let r = Iter::new(t).and_then(|mut it| {
        if !it.find(b"clusterTime") {
            return None;
        }
        match it.value() {
            Value::Timestamp(t, i) => Some((t, i)),
            _ => None,
        }
    });
    if r.is_none() {
        crate::log::error(crate::log::MONGOC, || {
            let json = crate::bson::json::to_json(t, crate::bson::json::Mode::Relaxed, false).unwrap_or_default();
            format!("Cannot parse cluster time from {}\n", String::from_utf8_lossy(&json))
        });
    }
    r
}

/// The write commands that carry readConcern.afterClusterTime in a causally consistent
/// session (_mongoc_write_command_supports_after_cluster_time, DRIVERS-3274).
fn write_supports_after_cluster_time(name: &str) -> bool {
    matches!(name, "insert" | "update" | "findAndModify" | "delete" | "bulkWrite" | "create" | "createIndexes" | "drop" | "dropDatabase" | "dropIndexes")
}

impl ClientSession {
    pub fn lsid(&self) -> &[u8] {
        self.server_session.as_ref().map_or(&EMPTY_DOC[..], |s| s.lsid.as_slice())
    }

    /// _mongoc_client_session_in_txn
    pub fn in_txn(&self) -> bool {
        matches!(self.txn_state, TxnState::Starting | TxnState::InProgress)
    }

    /// _mongoc_client_session_in_txn_or_ending
    pub fn in_txn_or_ending(&self) -> bool {
        matches!(self.txn_state, TxnState::Starting | TxnState::InProgress | TxnState::Ending)
    }

    /// The public transaction state (mongoc_client_session_get_transaction_state).
    pub fn public_txn_state(&self) -> TxnState {
        match self.txn_state {
            TxnState::CommittedEmpty => TxnState::Committed,
            s => s,
        }
    }

    pub fn dirty(&self) -> bool {
        self.server_session.as_ref().is_some_and(|s| s.dirty)
    }

    pub fn txn_number(&self) -> i64 {
        self.server_session.as_ref().map_or(0, |s| s.txn_number)
    }

    /// The next txnNumber of a retryable write.
    pub fn next_txn_number(&mut self) -> i64 {
        match self.server_session.as_mut() {
            Some(s) => {
                s.txn_number += 1;
                s.txn_number
            }
            None => 0,
        }
    }

    pub fn mark_dirty(&mut self) {
        if let Some(s) = self.server_session.as_mut() {
            s.dirty = true;
        }
    }

    pub fn unpin(&mut self) {
        self.pinned_server_id = 0;
    }

    /// mongoc_client_session_advance_cluster_time
    pub fn advance_cluster_time(&mut self, t: &[u8]) {
        let greater = match &self.cluster_time {
            None => parse_cluster_time(t).is_some(),
            Some(cur) => crate::topology::cluster_time_greater(t, cur),
        };
        if greater {
            self.cluster_time = Some(t.to_vec());
        }
    }

    /// mongoc_client_session_advance_operation_time
    pub fn advance_operation_time(&mut self, t: u32, i: u32) {
        if (t, i) > self.operation_time {
            self.operation_time = (t, i);
        }
    }

    /// _mongoc_client_session_handle_reply
    pub fn handle_reply(&mut self, is_acknowledged: bool, cmd_name: &str, reply: &[u8]) {
        use crate::bson::iter::{Iter, Value};
        let Some(mut it) = Iter::new(reply) else { return };
        if crate::cmd::has_label(reply, "TransientTransactionError") {
            self.unpin();
        }
        let fad = matches!(cmd_name, "find" | "aggregate" | "distinct");
        while it.next() {
            match (it.key(), it.value()) {
                (b"$clusterTime", Value::Document(d)) => self.advance_cluster_time(d),
                (b"operationTime", Value::Timestamp(t, i)) if is_acknowledged => self.advance_operation_time(t, i),
                (b"atClusterTime", v) if fad && self.snapshot && self.snapshot_time.is_none() => {
                    let (t, i) = match v {
                        Value::Timestamp(t, i) => (t, i),
                        _ => (0, 0),
                    };
                    self.snapshot_time = Some((t, i));
                }
                (b"cursor", Value::Document(c)) if fad && self.snapshot && self.snapshot_time.is_none() => {
                    if let Some(mut ci) = Iter::new(c) {
                        while ci.next() {
                            if let (b"atClusterTime", Value::Timestamp(t, i)) = (ci.key(), ci.value()) {
                                self.snapshot_time = Some((t, i));
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// _mongoc_client_session_append_read_concern: appends "readConcern" to `out` when needed.
    pub fn append_read_concern(&self, rc: &[u8], is_read: bool, is_write: bool, cmd_name: &str, out: &mut Writer) {
        use crate::bson::iter::Iter;
        let state = self.txn_state;
        if state == TxnState::InProgress {
            return;
        }
        let after_ok = is_read || (is_write && write_supports_after_cluster_time(cmd_name));
        let has_timestamp = (state == TxnState::Starting || after_ok) && self.causal_consistency && self.operation_time.0 != 0;
        let user_level = Iter::new(rc).is_some_and(|mut it| it.find(b"level"));
        let txn_level = match (&self.txn.read_concern, state) {
            (Some(r), TxnState::Starting) if !r.is_default() => r.level.clone(),
            _ => None,
        };
        if !has_timestamp && !self.snapshot && !user_level && txn_level.is_none() {
            return;
        }
        let mut w = Writer::with_capacity(64);
        w.concat(rc);
        if let Some(l) = &txn_level {
            if !user_level && !self.snapshot {
                w.utf8(b"level", l);
            }
        }
        if self.snapshot {
            w.utf8(b"level", b"snapshot");
        }
        if has_timestamp {
            w.timestamp(b"afterClusterTime", self.operation_time.0, self.operation_time.1);
        } else if let (true, Some((t, i))) = (self.snapshot, self.snapshot_time) {
            w.timestamp(b"atClusterTime", t, i);
        }
        out.document(b"readConcern", &w.finish());
    }

    /// _mongoc_client_session_append_txn: the transaction fields (state transitions included).
    pub fn append_txn(&mut self, out: &mut Writer, cmd_name: &str) {
        let n = self.txn_number();
        match self.txn_state {
            TxnState::Starting | TxnState::InProgress | TxnState::Ending => {
                if self.txn_state == TxnState::Starting {
                    self.txn_state = TxnState::InProgress;
                    out.bool(b"startTransaction", true);
                }
                out.int64(b"txnNumber", n);
                out.bool(b"autocommit", false);
            }
            TxnState::Committed if cmd_name == "commitTransaction" => {
                out.int64(b"txnNumber", n);
                out.bool(b"autocommit", false);
            }
            TxnState::Committed | TxnState::CommittedEmpty | TxnState::Aborted => {
                self.txn.cleanup();
                self.txn_state = TxnState::None;
                self.recovery_token = None;
            }
            TxnState::None => {}
        }
    }

    /// mongoc_client_session_start_transaction after server selection (`opts`: the
    /// transaction's own options).
    pub fn start_transaction(&mut self, opts: Option<&TxnOpts>) -> Result<(), Error> {
        let bad = |m: &str| Err(Error::new(domain::TRANSACTION, code::TRANSACTION_INVALID_STATE, m));
        if self.snapshot {
            return bad("Transactions are not supported in snapshot sessions");
        }
        if matches!(self.txn_state, TxnState::Starting | TxnState::InProgress | TxnState::Ending) {
            return bad("Transaction already in progress");
        }
        if let Some(s) = self.server_session.as_mut() {
            s.txn_number += 1;
        }
        let d = self.default_txn.clone();
        self.txn.set(&d);
        if let Some(o) = opts {
            self.txn.set(o);
        }
        if self.txn.write_concern.as_ref().is_some_and(|w| !w.is_acknowledged()) {
            return bad("Transactions do not support unacknowledged write concern");
        }
        self.unpin();
        self.txn_state = TxnState::Starting;
        self.recovery_token = None;
        Ok(())
    }
}

use crate::concern::{ReadConcern, WriteConcern};
use crate::error::{code, domain, Error};
