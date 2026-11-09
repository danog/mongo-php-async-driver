//! Bulk writes: mongoc-bulk-operation.c and mongoc-write-command.c (insert/update/delete
//! commands with OP_MSG document sequences, batch splitting, result merging).

use crate::bson::iter::{Iter, Value};
use crate::bson::writer::Writer;
use crate::client::Client;
use crate::cmdparts::{find, Parts};
use crate::concern::WriteConcern;
use crate::error::{code, domain, Category, Error};
use crate::topology::OpType;

/// BSON_OBJECT_ALLOWANCE: room for the command fields around a maximal document.
const BSON_OBJECT_ALLOWANCE: i64 = 16 * 1024;
/// WIRE_VERSION_UPDATE_HINT / WIRE_VERSION_DELETE_HINT (unacknowledged hints need 4.2 / 4.4)
const WIRE_VERSION_UPDATE_HINT: i32 = 8;
const WIRE_VERSION_DELETE_HINT: i32 = 9;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Delete = 0,
    Insert = 1,
    Update = 2,
}

impl Kind {
    pub fn name(self) -> &'static str {
        ["delete", "insert", "update"][self as usize]
    }
    fn field(self) -> &'static str {
        ["deletes", "documents", "updates"][self as usize]
    }
}

/// mongoc_bulk_write_flags_t
#[derive(Clone, Copy, Debug, Default)]
pub struct Flags {
    pub ordered: bool,
    pub bypass_document_validation: bool,
    pub has_collation: bool,
    pub has_multi_write: bool,
    pub has_array_filters: bool,
    pub has_update_hint: bool,
    pub has_delete_hint: bool,
}

/// mongoc_write_command_t
pub struct WriteCommand {
    pub kind: Kind,
    pub flags: Flags,
    /// comment / let
    pub cmd_opts: Vec<u8>,
    /// the concatenated documents
    pub payload: Vec<u8>,
    pub n_documents: u32,
    pub operation_id: i64,
}

/// mongoc_write_result_t
#[derive(Default)]
pub struct WriteResult {
    pub failed: bool,
    pub must_stop: bool,
    pub n_inserted: i32,
    pub n_matched: i32,
    pub n_modified: i32,
    pub n_removed: i32,
    pub n_upserted: i32,
    pub upserted: Vec<Vec<u8>>,
    pub write_errors: Vec<Vec<u8>>,
    pub write_concern_errors: Vec<Vec<u8>>,
    pub error_replies: Vec<Vec<u8>>,
    pub error_labels: Vec<Vec<u8>>,
    pub error: Error,
    pub retry_server_id: u32,
}

fn array_of(docs: &[Vec<u8>]) -> Vec<u8> {
    let mut w = Writer::with_capacity(docs.iter().map(|d| d.len() + 8).sum::<usize>() + 8);
    for (i, d) in docs.iter().enumerate() {
        w.document(i.to_string().as_bytes(), d);
    }
    w.finish()
}

fn int32_of(doc: &[u8], k: &[u8]) -> Option<i32> {
    match find(doc, k)? {
        Value::Int32(v) => Some(v),
        _ => None,
    }
}

impl WriteResult {
    /// _mongoc_write_result_merge
    fn merge(&mut self, kind: Kind, reply: &[u8], offset: u32) {
        let affected = int32_of(reply, b"n").unwrap_or(0);
        if let Some(Value::Array(a)) = find(reply, b"writeErrors") {
            if Iter::new(a).is_some_and(|mut i| i.next()) {
                self.failed = true;
            }
        }
        match kind {
            Kind::Insert => self.n_inserted = self.n_inserted.wrapping_add(affected),
            Kind::Delete => self.n_removed = self.n_removed.wrapping_add(affected),
            Kind::Update => {
                if let Some(v) = find(reply, b"upserted") {
                    let mut n_upserted = 0;
                    if let Value::Array(a) = v {
                        if let Some(mut ar) = Iter::new(a) {
                            while ar.next() {
                                let Value::Document(d) = ar.value() else { continue };
                                let Some(idx) = int32_of(d, b"index") else { continue };
                                let Some(mut c) = Iter::new(d) else { continue };
                                if c.find(b"_id") {
                                    let mut w = Writer::with_capacity(64);
                                    w.int32(b"index", (offset as i32).wrapping_add(idx));
                                    w.element_from(Some(b"_id"), &c);
                                    self.upserted.push(w.finish());
                                    n_upserted += 1;
                                }
                            }
                        }
                    }
                    self.n_upserted = self.n_upserted.wrapping_add(n_upserted);
                    self.n_matched = self.n_matched.wrapping_add(0.max(affected.wrapping_sub(n_upserted)));
                } else {
                    self.n_matched = self.n_matched.wrapping_add(affected);
                }
                if let Some(m) = int32_of(reply, b"nModified") {
                    self.n_modified = self.n_modified.wrapping_add(m);
                }
            }
        }
        if let Some(Value::Array(a)) = find(reply, b"writeErrors") {
            if let Some(mut ar) = Iter::new(a) {
                while ar.next() {
                    let Value::Document(d) = ar.value() else { continue };
                    let Some(mut c) = Iter::new(d) else { continue };
                    let mut w = Writer::with_capacity(d.len() + 8);
                    while c.next() {
                        if c.key() == b"index" {
                            let v = match c.value() {
                                Value::Int32(v) => v,
                                _ => 0,
                            };
                            w.int32(b"index", v.wrapping_add(offset as i32));
                        } else {
                            w.element_from(None, &c);
                        }
                    }
                    self.write_errors.push(w.finish());
                }
            }
        }
        if let Some(Value::Document(d)) = find(reply, b"writeConcernError") {
            self.write_concern_errors.push(d.to_vec());
        }
        if !reply.is_empty() && crate::cmd::check_ok(reply).is_err() {
            self.error_replies.push(reply.to_vec());
        }
        if let Some(Value::Array(a)) = find(reply, b"errorLabels") {
            if let Some(mut it) = Iter::new(a) {
                while it.next() {
                    if let Value::Utf8(s) = it.value() {
                        if !self.error_labels.iter().any(|l| l == s) {
                            self.error_labels.push(s.to_vec());
                        }
                    }
                }
            }
        }
    }

    /// _mongoc_write_result_complete (error API v2, default fields): the reply document and
    /// whether the bulk write succeeded; `self.error` holds the error otherwise.
    pub fn complete(&mut self, wc: &WriteConcern) -> (Vec<u8>, bool) {
        let mut w = Writer::with_capacity(256);
        if wc.is_acknowledged() {
            w.int32(b"nInserted", self.n_inserted);
            w.int32(b"nMatched", self.n_matched);
            w.int32(b"nModified", self.n_modified);
            w.int32(b"nRemoved", self.n_removed);
            w.int32(b"nUpserted", self.n_upserted);
            if !self.upserted.is_empty() {
                w.array(b"upserted", &array_of(&self.upserted));
            }
            w.array(b"writeErrors", &array_of(&self.write_errors));
            if !self.write_concern_errors.is_empty() {
                w.array(b"writeConcernErrors", &array_of(&self.write_concern_errors));
            }
        }
        if !self.error_replies.is_empty() {
            w.array(b"errorReplies", &array_of(&self.error_replies));
        }
        set_error_from_response(&self.write_errors, domain::SERVER, "write", &mut self.error);
        if self.error.code == 0 {
            set_error_from_response(&self.write_concern_errors, domain::WRITE_CONCERN, "write concern", &mut self.error);
        }
        if !self.error_labels.is_empty() {
            let mut a = Writer::with_capacity(64);
            for (i, l) in self.error_labels.iter().enumerate() {
                a.utf8(i.to_string().as_bytes(), l);
            }
            w.array(b"errorLabels", &a.finish());
        }
        (w.finish(), !self.failed && self.error.code == 0)
    }
}

/// _set_error_from_response
fn set_error_from_response(docs: &[Vec<u8>], dom: u32, kind: &str, error: &mut Error) {
    let n = docs.len();
    let mut msg = String::new();
    if n > 1 {
        msg.push_str(&format!("Multiple {kind} errors: "));
    }
    let mut code = 0u32;
    for (i, d) in docs.iter().enumerate() {
        let Some(mut it) = Iter::new(d) else { continue };
        while it.next() {
            if it.key() == b"code" && code == 0 {
                code = crate::sdam::as_i64(&it.value()) as u32;
            } else if it.key() == b"errmsg" {
                let s = match it.value() {
                    Value::Utf8(s) => String::from_utf8_lossy(s.split(|&b| b == 0).next().unwrap_or(s)).into_owned(),
                    _ => String::new(),
                };
                if n > 1 {
                    msg.push_str(&format!("\"{s}\""));
                    if i < n - 1 {
                        msg.push_str(", ");
                    }
                } else {
                    msg.push_str(&s);
                }
            }
        }
    }
    // bson_error_t.message is 504 bytes (503 + NUL)
    truncate_message(&mut msg);
    if code != 0 && !msg.is_empty() {
        *error = Error { domain: dom, code, category: Category::Server, message: msg };
    }
}

/// bson_error_t.message holds 503 bytes.
pub fn truncate_message(msg: &mut String) {
    if msg.len() > 503 {
        let mut n = 503;
        while !msg.is_char_boundary(n) {
            n -= 1;
        }
        msg.truncate(n);
    }
}

/// bson_validate with BSON_VALIDATE_EMPTY_KEYS (recursively).
pub fn validate_empty_keys(doc: &[u8]) -> Result<(), String> {
    let Some(mut it) = Iter::new(doc) else { return Err("corrupt BSON".into()) };
    while it.next() {
        if it.key().is_empty() {
            return Err("Element key cannot be an empty string".into());
        }
        match it.value() {
            Value::Document(d) | Value::Array(d) => validate_empty_keys(d)?,
            Value::CodeWScope(_, s) => validate_empty_keys(s)?,
            _ => {}
        }
    }
    Ok(())
}

fn invalid_arg(m: impl Into<String>) -> Error {
    Error::new(domain::COMMAND, code::COMMAND_INVALID_ARG, m)
}

/// _mongoc_document_is_pipeline
pub fn is_pipeline(doc: &[u8]) -> bool {
    let Some(mut it) = Iter::new(doc) else { return false };
    let mut i = 0usize;
    while it.next() {
        if it.key() != i.to_string().as_bytes() {
            return false;
        }
        i += 1;
        let Value::Document(d) = it.value() else { return false };
        let Some(mut c) = Iter::new(d) else { return false };
        if !c.next() || c.key().first() != Some(&b'$') {
            return false;
        }
    }
    i != 0
}

/// Options of one update operation (the parsed mongoc_bulk_update_opts_t).
#[derive(Default)]
pub struct UpdateOpts {
    pub upsert: bool,
    pub multi: bool,
    pub array_filters: Option<Vec<u8>>,
    pub collation: Option<Vec<u8>>,
    /// the hint as a value element named "hint"
    pub hint: Option<Vec<u8>>,
    pub sort: Option<Vec<u8>>,
}

/// mongoc_bulk_operation_t
pub struct BulkOperation {
    pub flags: Flags,
    pub let_: Option<Vec<u8>>,
    /// the comment as a document {"comment": value}
    pub comment: Option<Vec<u8>>,
    pub commands: Vec<WriteCommand>,
    pub result: WriteResult,
    pub server_id: u32,
    pub executed: bool,
    pub operation_id: i64,
}

fn non_empty(d: &Option<Vec<u8>>) -> Option<&[u8]> {
    d.as_deref().filter(|d| !crate::bson::iter::is_empty(d))
}

impl BulkOperation {
    pub fn new(ordered: bool) -> Self {
        BulkOperation {
            flags: Flags { ordered, ..Default::default() },
            let_: None,
            comment: None,
            commands: Vec::new(),
            result: WriteResult::default(),
            server_id: 0,
            executed: false,
            operation_id: 0,
        }
    }

    fn cmd_opts(&self, with_let: bool) -> Vec<u8> {
        let mut w = Writer::with_capacity(64);
        if let Some(c) = &self.comment {
            w.concat(c);
        }
        if with_let {
            if let Some(l) = non_empty(&self.let_) {
                w.document(b"let", l);
            }
        }
        w.finish()
    }

    fn last_of(&mut self, kind: Kind) -> Option<&mut WriteCommand> {
        self.commands.last_mut().filter(|c| c.kind == kind)
    }

    fn push(&mut self, kind: Kind, flags: Flags, with_let: bool, doc: Vec<u8>) {
        let cmd_opts = self.cmd_opts(with_let);
        self.commands.push(WriteCommand { kind, flags, cmd_opts, payload: doc, n_documents: 1, operation_id: self.operation_id });
    }

    /// mongoc_bulk_operation_insert_with_opts (no opts)
    pub fn insert(&mut self, doc: &[u8]) -> Result<(), Error> {
        validate_empty_keys(doc).map_err(|m| invalid_arg(format!("invalid document for insert: {m}")))?;
        let has_id = Iter::new(doc).is_some_and(|mut it| it.find(b"_id"));
        let d = if has_id {
            doc.to_vec()
        } else {
            let mut w = Writer::with_capacity(doc.len() + 20);
            w.oid(b"_id", &crate::bson::oid::generate());
            w.concat(doc);
            w.finish()
        };
        if let Some(last) = self.last_of(Kind::Insert) {
            last.payload.extend_from_slice(&d);
            last.n_documents += 1;
            return Ok(());
        }
        let flags = self.flags;
        self.push(Kind::Insert, flags, false, d);
        Ok(())
    }

    /// _mongoc_bulk_operation_update_append (after the opts were validated)
    fn update_append(&mut self, selector: &[u8], update: &[u8], o: &UpdateOpts) {
        let mut d = Writer::with_capacity(selector.len() + update.len() + 64);
        d.document(b"q", selector);
        if is_pipeline(update) {
            d.array(b"u", update);
        } else {
            d.document(b"u", update);
        }
        d.bool(b"upsert", o.upsert);
        d.bool(b"multi", o.multi);
        let has_array_filters = non_empty(&o.array_filters).is_some();
        if let Some(a) = non_empty(&o.array_filters) {
            d.array(b"arrayFilters", a);
        }
        let has_collation = non_empty(&o.collation).is_some();
        if let Some(c) = non_empty(&o.collation) {
            d.document(b"collation", c);
        }
        let has_hint = o.hint.is_some();
        if let Some(h) = &o.hint {
            d.concat(h);
        }
        if let Some(s) = non_empty(&o.sort) {
            d.document(b"sort", s);
        }
        let doc = d.finish();
        if let Some(last) = self.last_of(Kind::Update) {
            last.flags.has_array_filters |= has_array_filters;
            last.flags.has_collation |= has_collation;
            last.flags.has_update_hint |= has_hint;
            last.flags.has_multi_write |= o.multi;
            last.payload.extend_from_slice(&doc);
            last.n_documents += 1;
            return;
        }
        let mut flags = self.flags;
        flags.has_array_filters = has_array_filters;
        flags.has_collation = has_collation;
        flags.has_update_hint = has_hint;
        flags.has_multi_write = o.multi;
        self.push(Kind::Update, flags, true, doc);
    }

    /// mongoc_bulk_operation_update_{one,many}_with_opts
    pub fn update(&mut self, selector: &[u8], update: &[u8], o: &UpdateOpts, many: bool) -> Result<(), Error> {
        if many && o.sort.is_some() {
            return Err(invalid_arg("Invalid option 'sort'"));
        }
        validate_empty_keys(update).map_err(|m| invalid_arg(format!("invalid argument for update: {m}")))?;
        if !is_pipeline(update) {
            if let Some(mut it) = Iter::new(update) {
                while it.next() {
                    if it.key().first() != Some(&b'$') {
                        return Err(invalid_arg(format!("Invalid key '{}': update only works with $ operators and pipelines", String::from_utf8_lossy(it.key()))));
                    }
                }
            }
        }
        if o.multi != many {
            return Err(invalid_arg(format!("Invalid \"multi\" in opts: {}. The value must be {}, or omitted.", o.multi, many)));
        }
        self.update_append(selector, update, o);
        Ok(())
    }

    /// mongoc_bulk_operation_replace_one_with_opts
    pub fn replace_one(&mut self, selector: &[u8], doc: &[u8], o: &UpdateOpts) -> Result<(), Error> {
        if o.array_filters.is_some() {
            return Err(invalid_arg("Invalid option 'arrayFilters'"));
        }
        validate_empty_keys(doc).map_err(|m| invalid_arg(format!("invalid argument for replace: {m}")))?;
        if let Some(mut it) = Iter::new(doc) {
            while it.next() {
                if it.key().first() == Some(&b'$') {
                    return Err(invalid_arg(format!("Invalid key '{}': replace prohibits $ operators", String::from_utf8_lossy(it.key()))));
                }
            }
        }
        if o.multi {
            return Err(invalid_arg("Invalid \"multi\": true in opts for mongoc_bulk_operation_replace_one_with_opts. The value must be true, or omitted."));
        }
        self.update_append(selector, doc, o);
        Ok(())
    }

    /// mongoc_bulk_operation_remove_{one,many}_with_opts: `limit` 1 or 0.
    pub fn remove(&mut self, selector: &[u8], limit: i32, collation: Option<Vec<u8>>, hint: Option<Vec<u8>>) -> Result<(), Error> {
        let mut d = Writer::with_capacity(selector.len() + 64);
        d.document(b"q", selector);
        d.int32(b"limit", limit);
        let collation = collation.filter(|c| !crate::bson::iter::is_empty(c));
        if let Some(c) = &collation {
            d.document(b"collation", c);
        }
        if let Some(h) = &hint {
            d.concat(h);
        }
        let doc = d.finish();
        let (has_collation, has_hint) = (collation.is_some(), hint.is_some());
        if let Some(last) = self.last_of(Kind::Delete) {
            last.flags.has_collation |= has_collation;
            last.flags.has_delete_hint |= has_hint;
            last.flags.has_multi_write |= limit == 0;
            last.payload.extend_from_slice(&doc);
            last.n_documents += 1;
            return Ok(());
        }
        let mut flags = self.flags;
        flags.has_collation = has_collation;
        flags.has_delete_hint = has_hint;
        flags.has_multi_write = limit == 0;
        self.push(Kind::Delete, flags, true, doc);
        Ok(())
    }

    /// mongoc_bulk_operation_execute: (success, reply, error). `wc` is the effective write
    /// concern (the bulk's or the client's); `lsid` an explicit session's.
    pub fn execute(&mut self, client: &mut Client, db: &str, coll: &str, wc: &WriteConcern, session: Option<&mut crate::session::ClientSession>) -> (bool, Vec<u8>, Error) {
        if self.executed {
            return (false, empty_doc(), invalid_arg("bulk write already executed"));
        }
        self.executed = true;
        if self.commands.is_empty() {
            return (false, empty_doc(), invalid_arg("Cannot do an empty bulk write"));
        }
        client.next_operation_id();
        let mut session = session;
        let mut offset = 0u32;
        for i in 0..self.commands.len() {
            let sel = if self.server_id != 0 {
                if let Err(e) = client.stream_for_server(self.server_id, true) {
                    // the stream's reply (pooled mode's backpressure labels)
                    let f = client.stream_failure(e);
                    return (false, if f.reply.is_empty() { empty_doc() } else { f.reply }, f.error);
                }
                client.check_server_for_session(self.server_id, session.as_deref_mut()).map(|_| (self.server_id, false))
            } else {
                client.select_for_optype(OpType::Write, None, session.as_deref_mut(), &[])
            };
            let sid = match sel {
                Ok((s, _)) => s,
                Err(e) => return (false, empty_doc(), e),
            };
            let cmd = std::mem::replace(&mut self.commands[i], WriteCommand { kind: Kind::Insert, flags: Flags::default(), cmd_opts: Vec::new(), payload: Vec::new(), n_documents: 0, operation_id: 0 });
            execute_command(&cmd, client, sid, db, coll, wc, offset, session.as_deref_mut(), &mut self.result);
            self.server_id = sid;
            if self.result.retry_server_id != 0 {
                self.server_id = self.result.retry_server_id;
            }
            let n = cmd.n_documents;
            self.commands[i] = cmd;
            if self.result.failed && (self.flags.ordered || self.result.must_stop) {
                break;
            }
            offset += n;
        }
        let (reply, ok) = self.result.complete(wc);
        (ok, reply, self.result.error.clone())
    }
}

fn empty_doc() -> Vec<u8> {
    Writer::new().finish()
}

/// _mongoc_write_command_execute(_idl) and _mongoc_write_opmsg
#[allow(clippy::too_many_arguments)]
pub fn execute_command(command: &WriteCommand, client: &mut Client, server_id: u32, db: &str, coll: &str, wc: &WriteConcern, mut index_offset: u32, session: Option<&mut crate::session::ClientSession>, result: &mut WriteResult) {
    let fail = |result: &mut WriteResult, e: Error| {
        result.failed = true;
        result.error = e;
    };
    if !wc.is_valid() {
        return fail(result, invalid_arg("The write concern is invalid."));
    }
    let acked = wc.is_acknowledged();
    struct Sd {
        max_wire_version: i32,
        max_bson_obj_size: i32,
        max_msg_size: i32,
        max_write_batch_size: i32,
    }
    let Some(sd) = client.with_stream_description(server_id, |d| Sd { max_wire_version: d.max_wire_version, max_bson_obj_size: d.max_bson_obj_size, max_msg_size: d.max_msg_size, max_write_batch_size: d.max_write_batch_size }) else {
        return fail(result, invalid_arg(format!("Could not find server with id: {server_id}")));
    };
    if command.flags.has_collation && !acked {
        return fail(result, invalid_arg("Cannot set collation for unacknowledged writes"));
    }
    if command.flags.has_array_filters && !acked {
        return fail(result, invalid_arg("Cannot use array filters with unacknowledged writes"));
    }
    if command.flags.has_update_hint && sd.max_wire_version < WIRE_VERSION_UPDATE_HINT && !acked {
        return fail(result, Error::new(domain::COMMAND, code::PROTOCOL_BAD_WIRE_VERSION, "The selected server does not support hint for update"));
    }
    if command.flags.has_delete_hint && sd.max_wire_version < WIRE_VERSION_DELETE_HINT && !acked {
        return fail(result, Error::new(domain::COMMAND, code::PROTOCOL_BAD_WIRE_VERSION, "The selected server does not support hint for delete"));
    }
    if command.flags.bypass_document_validation && !acked {
        return fail(result, invalid_arg("Cannot set bypassDocumentValidation for unacknowledged writes"));
    }
    if session.is_some() && !acked {
        return fail(result, invalid_arg("Cannot use client session with unacknowledged writes"));
    }
    if command.payload.is_empty() {
        let c = [code::COLLECTION_DELETE_FAILED, code::COLLECTION_INSERT_FAILED, code::COLLECTION_UPDATE_FAILED][command.kind as usize];
        result.error = Error::new(domain::COLLECTION, c, format!("Cannot do an empty {}", command.kind.name()));
        return;
    }
    let max_bson = sd.max_bson_obj_size as i64;
    // MONGOC_REDUCED_MAX_MSG_SIZE_FOR_FLE: a client with automatic encryption batches by 2 MiB
    let max_msg = if client.auto_crypt_applies() { crate::autocrypt::REDUCED_MAX_MSG_SIZE_FOR_FLE as i64 } else { sd.max_msg_size as i64 };
    let max_count = sd.max_write_batch_size as i64;
    let mut body = Writer::with_capacity(64 + coll.len());
    body.utf8(command.kind.name().as_bytes(), coll.as_bytes());
    body.bool(b"ordered", command.flags.ordered);
    if command.flags.bypass_document_validation {
        body.bool(b"bypassDocumentValidation", true);
    }
    let body = body.finish();
    let mut parts = Parts::new(db, &body);
    parts.is_write = true;
    parts.set_write_concern(wc);
    parts.allow_txn_number = Some(!(command.flags.has_multi_write || !acked));
    parts.extra.concat(&command.cmd_opts);
    let mut session = session;
    if acked {
        parts.session = session.as_deref_mut();
    }
    let mut cmd = match client.assemble(parts, server_id, false) {
        Ok(c) => c,
        Err(e) => return fail(result, e),
    };
    let overhead = 16 + 4 + 1 + cmd.body.len() as i64 + 1 + 4 + command.kind.field().len() as i64 + 1;
    let data = &command.payload;
    let (mut batch_size, mut total_offset, mut doc_count) = (0usize, 0usize, 0i64);
    let mut ok = false;
    let mut retry_server = None;
    while total_offset < data.len() && !result.must_stop {
        let at = total_offset + batch_size;
        let len = i32::from_le_bytes(data[at..at + 4].try_into().unwrap()) as i64;
        let ship;
        if len > max_bson + BSON_OBJECT_ALLOWANCE {
            result.error = Error::new(domain::BSON, code::BSON_INVALID, format!("Document {index_offset} is too large for the cluster. Document is {len} bytes, max is {max_bson}."));
            result.failed = true;
            break;
        } else if batch_size as i64 + overhead + len <= max_msg || doc_count == 0 {
            batch_size += len as usize;
            doc_count += 1;
            ship = doc_count == max_count || batch_size + total_offset == data.len();
        } else {
            ship = true;
        }
        if ship {
            let payload = &data[total_offset..total_offset + batch_size];
            let (r, retry) = client.run_retryable_write(&mut cmd, &[(command.kind.field(), payload)], session.as_deref_mut());
            if let Some(rs) = retry {
                retry_server = Some(rs);
            }
            total_offset += batch_size;
            batch_size = 0;
            let reply = match r {
                Ok(rep) => {
                    ok = true;
                    rep
                }
                Err(f) => {
                    ok = false;
                    result.failed = true;
                    if command.flags.ordered || f.error.is_network() {
                        result.must_stop = true;
                    }
                    result.error = f.error;
                    f.reply
                }
            };
            result.merge(command.kind, &reply, index_offset);
            index_offset = index_offset.wrapping_add(doc_count as u32);
            doc_count = 0;
        }
    }
    client.release_temp_session(&mut cmd);
    if let (Some(rs), true) = (retry_server, ok) {
        result.retry_server_id = rs;
    }
    if ok {
        result.error = Error::default();
    }
}

/// bson_iter_overwrite_int64 on the command's "txnNumber".
pub fn set_txn_number(body: &mut [u8], n: i64) {
    let Some(mut it) = Iter::new(body) else { return };
    if it.find(b"txnNumber") {
        let e = it.element_bytes();
        let off = e.as_ptr() as usize - body.as_ptr() as usize + 1 + b"txnNumber".len() + 1;
        body[off..off + 8].copy_from_slice(&n.to_le_bytes());
    }
}
