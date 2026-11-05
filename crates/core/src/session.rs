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
}

/// Transaction state (mongoc_transaction_state_t)
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TxnState {
    #[default]
    None,
    Starting,
    InProgress,
    Committed,
    Aborted,
    /// committed with an empty transaction (no command sent)
    CommittedEmpty,
}

/// mongoc_client_session_t: an explicit session.
#[derive(Debug, Default)]
pub struct ClientSession {
    /// None once ended
    pub server_session: Option<ServerSession>,
    pub causal_consistency: bool,
    pub snapshot: bool,
    pub cluster_time: Option<Vec<u8>>,
    /// operationTime (t, i), (0, 0) when unset
    pub operation_time: (u32, u32),
    pub snapshot_time: Option<(u32, u32)>,
    pub txn_state: TxnState,
    /// the server a sharded transaction is pinned to (0: none)
    pub pinned_server_id: u32,
    /// the client's identity (sessions cannot be used with another Manager's client)
    pub client_id: usize,
    /// the client generation at creation (fork detection)
    pub client_generation: u32,
}

impl ClientSession {
    pub fn lsid(&self) -> &[u8] {
        self.server_session.as_ref().map_or(&[5, 0, 0, 0, 0][..], |s| s.lsid.as_slice())
    }

    /// _mongoc_client_session_in_txn
    pub fn in_txn(&self) -> bool {
        matches!(self.txn_state, TxnState::Starting | TxnState::InProgress)
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

    /// mongoc_client_session_advance_cluster_time
    pub fn advance_cluster_time(&mut self, t: &[u8]) {
        if self.cluster_time.as_deref().is_none_or(|cur| crate::topology::cluster_time_greater(t, cur)) {
            self.cluster_time = Some(t.to_vec());
        }
    }

    /// mongoc_client_session_advance_operation_time
    pub fn advance_operation_time(&mut self, t: u32, i: u32) {
        if (t, i) > self.operation_time {
            self.operation_time = (t, i);
        }
    }

    /// _mongoc_client_session_handle_reply ($clusterTime, operationTime; snapshot time)
    pub fn handle_reply(&mut self, is_acknowledged: bool, reply: &[u8]) {
        use crate::bson::iter::{Iter, Value};
        let Some(mut it) = Iter::new(reply) else { return };
        while it.next() {
            match (it.key(), it.value()) {
                (b"$clusterTime", Value::Document(d)) => self.advance_cluster_time(d),
                (b"operationTime", Value::Timestamp(t, i)) if is_acknowledged => self.advance_operation_time(t, i),
                _ => {}
            }
        }
        if let Some(s) = self.server_session.as_mut() {
            s.last_used_usec = crate::sdam::monotonic_usec();
        }
    }
}
