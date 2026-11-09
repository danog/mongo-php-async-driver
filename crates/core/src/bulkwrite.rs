//! Client-level bulk writes: the `bulkWrite` command of MongoDB 8.0+ (port of libmongoc's
//! mongoc-bulkwrite.c and mcd-nsinfo.c). Models are appended to one `ops` document sequence;
//! `execute` splits it into batches by maxWriteBatchSize and maxMessageSizeBytes, each with its
//! own `nsInfo` sequence, and reads the per-operation results from the reply cursor.

use crate::bson::iter::{Iter, Value};
use crate::bson::writer::{index_key, itoa_buf, Writer};
use crate::client::Client;
use crate::cmdparts::{Cmd, Failure, Parts};
use crate::concern::WriteConcern;
use crate::error::{code, domain, Error};
use crate::session::ClientSession;
use crate::topology::OpType;
use std::collections::HashMap;

/// Offset of the placeholder int32 value of an op's first element ("insert", "update" and
/// "delete" all have 6 letters): length (4) + type (1) + key with its NUL (7).
const OP_INDEX_OFFSET: usize = 12;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Insert,
    Update,
    Delete,
}

struct Model {
    op: Op,
    ns: String,
    /// inserts: {"_id": <the document's _id>}
    id: Option<Vec<u8>>,
}

/// Options of the update, replace and delete models (absent: not sent).
#[derive(Default, Clone, Debug)]
pub struct ModelOpts {
    pub array_filters: Option<Vec<u8>>,
    pub collation: Option<Vec<u8>>,
    /// {"hint": <string or document>}
    pub hint: Option<Vec<u8>>,
    pub upsert: Option<bool>,
    pub sort: Option<Vec<u8>>,
}

/// mongoc_bulkwriteopts_t
#[derive(Default, Clone, Debug)]
pub struct ExecOpts {
    pub ordered: Option<bool>,
    pub bypass_document_validation: Option<bool>,
    pub let_: Option<Vec<u8>>,
    pub write_concern: Option<WriteConcern>,
    pub verbose_results: Option<bool>,
    /// {"comment": <value>}
    pub comment: Option<Vec<u8>>,
    pub server_id: u32,
}

/// mongoc_bulkwriteresult_t
#[derive(Default)]
pub struct BulkWriteResult {
    pub inserted_count: i64,
    pub upserted_count: i64,
    pub matched_count: i64,
    pub modified_count: i64,
    pub deleted_count: i64,
    errors_count: i64,
    first_error_index: Option<i64>,
    pub server_id: u32,
    insert_results: Writer,
    update_results: Writer,
    delete_results: Writer,
    verbose_results: bool,
    parsed_some_results: bool,
}

impl BulkWriteResult {
    /// The insert, update and delete results, keyed by model index (None without verboseResults).
    pub fn verbose_results(&self) -> Option<[Vec<u8>; 3]> {
        self.verbose_results.then(|| [self.insert_results.buf_as_doc(), self.update_results.buf_as_doc(), self.delete_results.buf_as_doc()])
    }
}

/// mongoc_bulkwriteexception_t
#[derive(Default)]
pub struct BulkWriteException {
    /// the top-level error (code 0: none)
    pub error: Error,
    /// the error reply (empty: none)
    pub error_reply: Vec<u8>,
    write_concern_errors: Writer,
    write_concern_errors_len: usize,
    write_errors: Writer,
    has_any_error: bool,
}

impl BulkWriteException {
    /// [{code, message, details}, ...]
    pub fn write_concern_errors(&self) -> Vec<u8> {
        self.write_concern_errors.buf_as_doc()
    }

    /// {"<model index>": {code, message, details}, ...}
    pub fn write_errors(&self) -> Vec<u8> {
        self.write_errors.buf_as_doc()
    }

    fn set_error(&mut self, e: Error) {
        self.error = e;
        self.has_any_error = true;
    }

    fn set_error_reply(&mut self, reply: &[u8]) {
        self.error_reply = reply.to_vec();
        self.has_any_error = true;
    }
}

/// mongoc_bulkwritereturn_t
#[derive(Default)]
pub struct BulkWriteReturn {
    pub res: Option<BulkWriteResult>,
    pub exc: Option<BulkWriteException>,
}

/// mongoc_bulkwrite_t
#[derive(Default)]
pub struct BulkWrite {
    executed: bool,
    /// the client it last ran with (mongoc_bulkwrite_set_client: a new one takes a new operation id)
    client_key: usize,
    pub server_id: Option<u32>,
    pub is_acknowledged: Option<bool>,
    ops: Vec<u8>,
    models: Vec<Model>,
    max_insert_len: u32,
    has_multi_write: bool,
    operation_id: i64,
}

fn invalid_arg(msg: impl Into<String>) -> Error {
    Error::new(domain::COMMAND, code::COMMAND_INVALID_ARG, msg)
}

/// The bytes up to the first NUL (strings are appended with strlen, as C does).
fn c_part(b: &[u8]) -> &[u8] {
    &b[..b.iter().position(|&c| c == 0).unwrap_or(b.len())]
}

fn find<'a>(doc: &'a [u8], k: &[u8]) -> Option<Value<'a>> {
    let mut it = Iter::new(doc)?;
    it.find(k).then(|| it.value())
}

/// validate_update (with _mongoc_document_is_pipeline)
fn validate_update(update: &[u8]) -> Result<bool, Error> {
    if crate::bulk::is_pipeline(update) {
        return Ok(true);
    }
    if let Some(mut it) = Iter::new(update) {
        if it.next() && it.key().first() != Some(&b'$') {
            return Err(invalid_arg(format!("Invalid key '{}': update only works with $ operators and pipelines", String::from_utf8_lossy(it.key()))));
        }
    }
    Ok(false)
}

/// validate_replace
fn validate_replace(doc: &[u8]) -> Result<(), Error> {
    if let Some(mut it) = Iter::new(doc) {
        if it.next() && it.key().first() == Some(&b'$') {
            return Err(invalid_arg(format!("Invalid key '{}': replace prohibits $ operators", String::from_utf8_lossy(it.key()))));
        }
    }
    Ok(())
}

impl BulkWrite {
    pub fn new() -> Self {
        Self::default()
    }

    /// The number of models appended.
    pub fn len(&self) -> usize {
        self.models.len()
    }

    pub fn is_empty(&self) -> bool {
        self.models.is_empty()
    }

    fn check_not_executed(&self) -> Result<(), Error> {
        if self.executed {
            return Err(invalid_arg("bulk write already executed"));
        }
        Ok(())
    }

    fn push(&mut self, op: Op, ns: &str, w: Writer, id: Option<Vec<u8>>) {
        self.ops.extend_from_slice(&w.finish());
        self.models.push(Model { op, ns: ns.to_owned(), id });
    }

    /// mongoc_bulkwrite_append_insertone (`doc` already has an _id when it comes from the
    /// driver; one is generated otherwise).
    pub fn append_insert_one(&mut self, ns: &str, doc: &[u8]) -> Result<(), Error> {
        self.check_not_executed()?;
        let mut w = Writer::with_capacity(doc.len() + 32);
        w.int32(b"insert", -1);
        let mut inserted = doc.to_vec();
        if find(doc, b"_id").is_none() {
            let mut d = Writer::with_capacity(doc.len() + 17);
            d.oid(b"_id", &crate::bson::oid::generate());
            d.concat(doc);
            inserted = d.finish();
        }
        w.document(b"document", &inserted);
        self.max_insert_len = self.max_insert_len.max(inserted.len() as u32);
        let mut id = Writer::with_capacity(32);
        if let Some(mut it) = Iter::new(&inserted) {
            if it.find(b"_id") {
                id.element_from(None, &it);
            }
        }
        self.push(Op::Insert, ns, w, Some(id.finish()));
        Ok(())
    }

    fn append_opts(w: &mut Writer, o: &ModelOpts, with_array_filters: bool, with_sort: bool) {
        if with_array_filters {
            if let Some(a) = &o.array_filters {
                w.array(b"arrayFilters", a);
            }
        }
        if let Some(c) = &o.collation {
            w.document(b"collation", c);
        }
        if let Some(h) = &o.hint {
            w.concat(h);
        }
        if let Some(u) = o.upsert {
            w.bool(b"upsert", u);
        }
        if with_sort {
            if let Some(s) = &o.sort {
                w.document(b"sort", s);
            }
        }
    }

    /// mongoc_bulkwrite_append_updateone / _updatemany (`sort` is ignored for many)
    pub fn append_update(&mut self, ns: &str, filter: &[u8], update: &[u8], o: &ModelOpts, multi: bool) -> Result<(), Error> {
        self.check_not_executed()?;
        let is_pipeline = validate_update(update)?;
        let mut w = Writer::with_capacity(filter.len() + update.len() + 64);
        w.int32(b"update", -1);
        w.document(b"filter", filter);
        if is_pipeline {
            w.array(b"updateMods", update);
        } else {
            w.document(b"updateMods", update);
        }
        w.bool(b"multi", multi);
        Self::append_opts(&mut w, o, true, !multi);
        if multi {
            self.has_multi_write = true;
        }
        self.push(Op::Update, ns, w, None);
        Ok(())
    }

    /// mongoc_bulkwrite_append_replaceone
    pub fn append_replace_one(&mut self, ns: &str, filter: &[u8], replacement: &[u8], o: &ModelOpts) -> Result<(), Error> {
        self.check_not_executed()?;
        validate_replace(replacement)?;
        let mut w = Writer::with_capacity(filter.len() + replacement.len() + 64);
        w.int32(b"update", -1);
        w.document(b"filter", filter);
        w.document(b"updateMods", replacement);
        w.bool(b"multi", false);
        Self::append_opts(&mut w, o, false, true);
        self.max_insert_len = self.max_insert_len.max(replacement.len() as u32);
        self.push(Op::Update, ns, w, None);
        Ok(())
    }

    /// mongoc_bulkwrite_append_deleteone / _deletemany (collation and hint only)
    pub fn append_delete(&mut self, ns: &str, filter: &[u8], o: &ModelOpts, multi: bool) -> Result<(), Error> {
        self.check_not_executed()?;
        let mut w = Writer::with_capacity(filter.len() + 64);
        w.int32(b"delete", -1);
        w.document(b"filter", filter);
        w.bool(b"multi", multi);
        if let Some(c) = &o.collation {
            w.document(b"collation", c);
        }
        if let Some(h) = &o.hint {
            w.concat(h);
        }
        if multi {
            self.has_multi_write = true;
        }
        self.push(Op::Delete, ns, w, None);
        Ok(())
    }

    /// mongoc_bulkwrite_set_client: a client it has not run with takes a new operation id.
    fn set_client(&mut self, client: &mut Client) {
        // the client, whichever of its clusters runs the operation
        let key = std::rc::Rc::as_ptr(client.shared()) as usize;
        if self.client_key != key {
            self.client_key = key;
            self.operation_id = client.next_operation_id();
        }
    }

    /// mongoc_bulkwrite_execute
    pub fn execute(&mut self, client: &mut Client, opts: &ExecOpts, mut session: Option<&mut ClientSession>) -> BulkWriteReturn {
        self.set_client(client);
        let mut res = BulkWriteResult::default();
        let mut exc = BulkWriteException::default();
        let is_ordered = opts.ordered.unwrap_or(true);
        let mut sid = None;
        self.run(client, opts, session.as_deref_mut(), is_ordered, &mut res, &mut exc, &mut sid);
        // whether some writes succeeded
        let mut has_successful_results = false;
        if res.parsed_some_results {
            if is_ordered {
                if res.errors_count == 0 || res.first_error_index.is_some_and(|i| i > 0) {
                    has_successful_results = true;
                }
            } else if res.errors_count < self.models.len() as i64 {
                has_successful_results = true;
            }
        }
        let mut ret = BulkWriteReturn::default();
        if let Some(s) = sid {
            self.server_id = Some(s);
            res.server_id = s;
        }
        if self.is_acknowledged == Some(true) && has_successful_results {
            ret.res = Some(res);
        }
        if exc.has_any_error {
            ret.exc = Some(exc);
        }
        ret
    }

    #[allow(clippy::too_many_arguments)]
    fn run(&mut self, client: &mut Client, opts: &ExecOpts, mut session: Option<&mut ClientSession>, is_ordered: bool, res: &mut BulkWriteResult, exc: &mut BulkWriteException, sid_out: &mut Option<u32>) {
        if self.executed {
            return exc.set_error(invalid_arg("bulk write already executed"));
        }
        self.executed = true;
        if self.models.is_empty() {
            return exc.set_error(invalid_arg("cannot do `bulkWrite` with no models"));
        }
        // select a stream
        let sel = if opts.server_id != 0 {
            if let Err(e) = client.stream_for_server(opts.server_id, true) {
                // the stream's reply (pooled mode's backpressure labels)
                let f = client.stream_failure(e);
                if !f.reply.is_empty() {
                    exc.set_error_reply(&f.reply);
                }
                return exc.set_error(f.error);
            }
            client.check_server_for_session(opts.server_id, session.as_deref_mut()).map(|_| opts.server_id)
        } else {
            client.select_for_optype(OpType::Write, None, session.as_deref_mut(), &[]).map(|r| r.0)
        };
        let mut sid = match sel {
            Ok(s) => s,
            Err(e) => return exc.set_error(e),
        };
        *sid_out = Some(sid);
        let verbose = opts.verbose_results.unwrap_or(false);
        res.verbose_results = verbose;
        let max_bson = client.with_stream_description(sid, |d| d.max_bson_obj_size).unwrap_or(16 * 1024 * 1024);
        // payload 0
        let mut body = Writer::with_capacity(128);
        body.int32(b"bulkWrite", 1);
        body.bool(b"errorsOnly", !verbose);
        body.bool(b"ordered", is_ordered);
        if let Some(c) = &opts.comment {
            body.concat(c);
        }
        if let Some(b) = opts.bypass_document_validation {
            body.bool(b"bypassDocumentValidation", b);
        }
        if let Some(l) = &opts.let_ {
            body.document(b"let", l);
        }
        let body = body.finish();
        let in_txn = session.as_deref().is_some_and(|s| s.in_txn());
        let wc = match &opts.write_concern {
            Some(w) if in_txn => {
                let _ = w;
                return exc.set_error(invalid_arg("Cannot set write concern after starting a transaction."));
            }
            Some(w) => w.clone(),
            None => WriteConcern::from_uri(&client.uri),
        };
        let acked = wc.is_acknowledged();
        if !acked && i64::from(self.max_insert_len) > i64::from(max_bson) {
            return exc.set_error(invalid_arg(format!("Unacknowledged `bulkWrite` includes insert of size: {}, exceeding maxBsonObjectSize: {max_bson}", self.max_insert_len)));
        }
        self.is_acknowledged = Some(acked);
        if verbose && !acked {
            return exc.set_error(invalid_arg("Cannot request unacknowledged write concern and verbose results."));
        }
        if is_ordered && !acked {
            return exc.set_error(invalid_arg("Cannot request unacknowledged write concern and ordered writes."));
        }
        let mut parts = Parts::new("admin", &body);
        parts.is_write = true;
        // multi-document writes are not retryable
        parts.allow_txn_number = Some(!self.has_multi_write);
        parts.set_write_concern(&wc);
        parts.session = session.as_deref_mut();
        let mut cmd = match client.assemble(parts, sid, false) {
            Ok(c) => c,
            Err(e) => return exc.set_error(e),
        };
        cmd.operation_id = self.operation_id;
        let (max_batch, max_msg) = client.with_stream_description(sid, |d| (d.max_write_batch_size, d.max_msg_size)).unwrap_or((100_000, 48_000_000));
        // a server reporting no batch size must not stall the loop: at least one op per batch
        let max_batch = max_batch.max(0) as usize;
        let max_msg = if client.auto_crypt_applies() {
            // MONGOC_REDUCED_MAX_MSG_SIZE_FOR_FLE
            crate::autocrypt::REDUCED_MAX_MSG_SIZE_FOR_FLE
        } else {
            max_msg.max(0) as usize
        };
        // OP_MSG and bulkWrite command overhead (bulk write specification)
        let overhead = 1000 + body.len();
        let (mut doc_offset, mut byte_offset) = (0usize, 0usize);
        loop {
            if byte_offset == self.ops.len() {
                break;
            }
            let (byte_len, doc_len, nsinfo) = match self.fill_batch(byte_offset, doc_offset, max_batch, max_msg, overhead) {
                Ok(b) => b,
                Err(e) => {
                    exc.set_error(e);
                    break;
                }
            };
            // a previous retry may have invalidated the stream: select again (without the session)
            if client.stream_for_server(cmd.server_id, false).is_err() {
                match client.select_for_optype(OpType::Write, None, None, &[]) {
                    Ok((s, _)) => cmd.server_id = s,
                    Err(e) => {
                        exc.set_error(e);
                        break;
                    }
                }
            }
            let ops = &self.ops[byte_offset..byte_offset + byte_len];
            let payloads: [(&str, &[u8]); 2] = [("nsInfo", &nsinfo.payload), ("ops", ops)];
            let (r, retry) = client.run_retryable_write(&mut cmd, &payloads, session.as_deref_mut());
            if let Some(s) = retry {
                cmd.server_id = s;
            }
            sid = cmd.server_id;
            *sid_out = Some(sid);
            let reply = match r {
                Ok(rep) => rep,
                Err(f) => {
                    if f.error.code != 0 {
                        exc.set_error(f.error);
                    }
                    exc.set_error_reply(&f.reply);
                    break;
                }
            };
            let mut has_write_errors = false;
            if acked {
                if !apply_reply(res, exc, &reply) {
                    break;
                }
                let r = read_cursor(client, &cmd, session.as_deref_mut(), &reply, doc_len, |result| apply_result(res, exc, result, doc_offset, &self.models));
                if let Err(f) = r {
                    if let Some(f) = f {
                        exc.set_error(f.error);
                        if !f.reply.is_empty() {
                            exc.set_error_reply(&f.reply);
                        }
                    }
                    break;
                }
                has_write_errors = !exc.write_errors.is_empty();
            }
            doc_offset += doc_len;
            byte_offset += byte_len;
            if has_write_errors && is_ordered {
                break;
            }
        }
        client.release_temp_session(&mut cmd);
    }
}

impl BulkWrite {
    /// The next batch from `byte_offset` (model `doc_offset`): its byte and document counts and
    /// its nsInfo, the ops' placeholders set to their nsInfo indexes. At least one operation per
    /// batch, whatever maxWriteBatchSize the server reports (0 would stall libmongoc's loop).
    fn fill_batch(&mut self, byte_offset: usize, doc_offset: usize, max_batch: usize, max_msg: usize, overhead: usize) -> Result<(usize, usize, NsInfo), Error> {
        let max_batch = max_batch.max(1);
        let (mut byte_len, mut doc_len) = (0usize, 0usize);
        let mut nsinfo = NsInfo::default();
        while byte_offset + byte_len < self.ops.len() && doc_len < max_batch {
            let at = byte_offset + byte_len;
            let len = crate::bson::iter::rd_u32(&self.ops, at) as usize;
            let md = &self.models[doc_offset + doc_len];
            let ns_index = nsinfo.find(&md.ns);
            let ns_size = if ns_index.is_none() { NsInfo::bson_size(&md.ns) } else { 0 };
            if overhead + byte_len + len + ns_size > max_msg {
                if byte_len == 0 {
                    return Err(invalid_arg(format!("unable to send document at index {doc_len}. Sending would exceed maxMessageSizeBytes={max_msg}")));
                }
                break;
            }
            let idx = match ns_index {
                Some(i) => i,
                None => nsinfo.append(&md.ns)?,
            };
            self.ops[at + OP_INDEX_OFFSET..at + OP_INDEX_OFFSET + 4].copy_from_slice(&idx.to_le_bytes());
            byte_len += len;
            doc_len += 1;
        }
        Ok((byte_len, doc_len, nsinfo))
    }
}

/// mcd_nsinfo_t: the nsInfo document sequence of one batch.
#[derive(Default, Debug)]
struct NsInfo {
    index: HashMap<String, i32>,
    payload: Vec<u8>,
}

impl NsInfo {
    fn find(&self, ns: &str) -> Option<i32> {
        self.index.get(ns).copied()
    }

    /// The size of {"ns": "<ns>"}.
    fn bson_size(ns: &str) -> usize {
        4 + 1 + 3 + 4 + ns.len() + 1 + 1
    }

    fn append(&mut self, ns: &str) -> Result<i32, Error> {
        let i = self.index.len();
        if i >= i32::MAX as usize {
            return Err(invalid_arg(format!("Only {} distinct collections may be used", i32::MAX)));
        }
        self.index.insert(ns.to_owned(), i as i32);
        let mut w = Writer::with_capacity(ns.len() + 16);
        w.utf8(b"ns", ns.as_bytes());
        self.payload.extend_from_slice(&w.finish());
        Ok(i as i32)
    }
}

fn lookup_err(exc: &mut BulkWriteException, what: &str, key: &str, source: Option<&str>) {
    let m = match source {
        Some(s) => format!("expected to find {what} `{key}` in {s}, but did not"),
        None => format!("expected to find {what} `{key}`, but did not"),
    };
    exc.set_error(invalid_arg(m));
}

fn lookup_int32(doc: &[u8], key: &str, source: Option<&str>, exc: &mut BulkWriteException) -> Option<i32> {
    match find(doc, key.as_bytes()) {
        Some(Value::Int32(v)) => Some(v),
        _ => {
            lookup_err(exc, "int32", key, source);
            None
        }
    }
}

/// lookup_as_int64: int32, int64 or double (truncated)
fn lookup_as_int64(doc: &[u8], key: &str, source: Option<&str>, exc: &mut BulkWriteException) -> Option<i64> {
    match find(doc, key.as_bytes()) {
        Some(Value::Int32(v)) => Some(v as i64),
        Some(Value::Int64(v)) => Some(v),
        Some(Value::Double(v)) => Some(v as i64),
        _ => {
            lookup_err(exc, "int32, int64, or double", key, source);
            None
        }
    }
}

fn lookup_string<'a>(doc: &'a [u8], key: &str, source: Option<&str>, exc: &mut BulkWriteException) -> Option<&'a [u8]> {
    match find(doc, key.as_bytes()) {
        Some(Value::Utf8(s)) => Some(c_part(s)),
        _ => {
            lookup_err(exc, "string", key, source);
            None
        }
    }
}

/// _mongoc_iter_document_as_bson: a document, else an error.
fn document_as_bson<'a>(v: Option<Value<'a>>, key: &str) -> Result<Option<&'a [u8]>, Error> {
    match v {
        None => Ok(None),
        Some(Value::Document(d)) => Ok(Some(d)),
        Some(_) => Err(Error::new(domain::BSON, code::BSON_INVALID, format!("expected '{key}' to be a document"))),
    }
}

/// {code, message, details} appended under `key`
fn append_error(w: &mut Writer, key: &[u8], code: i32, errmsg: &[u8], info: &[u8]) {
    let d = w.begin(3, key);
    w.int32(b"code", code);
    w.utf8(b"message", errmsg);
    w.document(b"details", info);
    w.end(d);
}

const EMPTY: [u8; 5] = [5, 0, 0, 0, 0];

/// _bulkwritereturn_apply_reply: the counts and the write concern error of a reply.
fn apply_reply(res: &mut BulkWriteResult, exc: &mut BulkWriteException, reply: &[u8]) -> bool {
    let mut counts = [0i64; 6];
    for (i, k) in ["nInserted", "nMatched", "nModified", "nDeleted", "nUpserted", "nErrors"].iter().enumerate() {
        match lookup_as_int64(reply, k, None, exc) {
            Some(v) => counts[i] = v,
            None => return false,
        }
    }
    res.inserted_count = res.inserted_count.wrapping_add(counts[0]);
    res.matched_count = res.matched_count.wrapping_add(counts[1]);
    res.modified_count = res.modified_count.wrapping_add(counts[2]);
    res.deleted_count = res.deleted_count.wrapping_add(counts[3]);
    res.upserted_count = res.upserted_count.wrapping_add(counts[4]);
    res.errors_count = res.errors_count.wrapping_add(counts[5]);
    match document_as_bson(find(reply, b"writeConcernError"), "writeConcernError") {
        Err(e) => {
            exc.set_error(e);
            exc.set_error_reply(reply);
            return false;
        }
        Ok(Some(wce)) => {
            let Some(code) = lookup_int32(wce, "code", Some("writeConcernError"), exc) else { return false };
            let Some(errmsg) = lookup_string(wce, "errmsg", Some("writeConcernError"), exc) else { return false };
            let info = match document_as_bson(find(wce, b"errInfo"), "errInfo") {
                Ok(i) => i.unwrap_or(&EMPTY),
                Err(e) => {
                    exc.set_error(e);
                    exc.set_error_reply(reply);
                    return false;
                }
            };
            let mut buf = itoa_buf::Buf::default();
            let key = index_key(exc.write_concern_errors_len, &mut buf).to_vec();
            exc.write_concern_errors_len += 1;
            append_error(&mut exc.write_concern_errors, &key, code, errmsg, info);
            exc.has_any_error = true;
        }
        Ok(None) => {}
    }
    res.parsed_some_results = true;
    true
}

/// _bulkwritereturn_apply_result: one result document of the reply cursor.
fn apply_result(res: &mut BulkWriteResult, exc: &mut BulkWriteException, result: &[u8], doc_offset: usize, models: &[Model]) -> bool {
    let Some(ok) = lookup_as_int64(result, "ok", Some("result"), exc) else { return false };
    let Some(idx) = lookup_as_int64(result, "idx", Some("result"), exc) else { return false };
    let models_idx = usize::try_from(idx).ok().and_then(|i| i.checked_add(doc_offset)).filter(|&i| i < models.len());
    let Some(models_idx) = models_idx else {
        exc.set_error(invalid_arg(format!("bulk write result index {idx} with offset {doc_offset} is out of range for {} operations", models.len())));
        return false;
    };
    let mut buf = itoa_buf::Buf::default();
    let key = index_key(models_idx, &mut buf).to_vec();
    if ok == 0 {
        if res.first_error_index.is_none() {
            res.first_error_index = Some(idx);
        }
        let Some(code) = lookup_int32(result, "code", Some("result"), exc) else { return false };
        let Some(errmsg) = lookup_string(result, "errmsg", Some("result"), exc) else { return false };
        let info = match document_as_bson(find(result, b"errInfo"), "errInfo") {
            Ok(i) => i.unwrap_or(&EMPTY),
            Err(e) => {
                exc.set_error(e);
                return false;
            }
        };
        append_error(&mut exc.write_errors, &key, code, errmsg, info);
        exc.has_any_error = true;
        return true;
    }
    // a successful result (only reported with verboseResults)
    match models[models_idx].op {
        Op::Update => {
            let Some(n) = lookup_as_int64(result, "n", Some("result"), exc) else { return false };
            // nModified may be missing (SERVER-113026): 0 then
            let n_modified = if find(result, b"nModified").is_none() {
                0
            } else {
                match lookup_as_int64(result, "nModified", Some("result"), exc) {
                    Some(v) => v,
                    None => return false,
                }
            };
            let mut upserted_id = None;
            if find(result, b"upserted").is_some() {
                let id = match find(result, b"upserted") {
                    Some(Value::Document(u)) => {
                        let mut found = None;
                        if let Some(mut it) = Iter::new(u) {
                            if it.find(b"_id") {
                                let mut w = Writer::with_capacity(32);
                                w.element_from(Some(b"upsertedId"), &it);
                                found = Some(w.finish());
                            }
                        }
                        found
                    }
                    _ => None,
                };
                let Some(id) = id else {
                    exc.set_error(invalid_arg("expected `upserted` to be a document containing `_id`, but did not find `_id`"));
                    return false;
                };
                upserted_id = Some(id);
            }
            let w = &mut res.update_results;
            let d = w.begin(3, &key);
            w.int64(b"matchedCount", n);
            w.int64(b"modifiedCount", n_modified);
            if let Some(id) = &upserted_id {
                w.concat(id);
            }
            w.end(d);
        }
        Op::Delete => {
            let Some(n) = lookup_as_int64(result, "n", Some("result"), exc) else { return false };
            let w = &mut res.delete_results;
            let d = w.begin(3, &key);
            w.int64(b"deletedCount", n);
            w.end(d);
        }
        Op::Insert => {
            let w = &mut res.insert_results;
            let d = w.begin(3, &key);
            if let Some(id) = &models[models_idx].id {
                if let Some(mut it) = Iter::new(id) {
                    if it.next() {
                        w.element_from(Some(b"insertedId"), &it);
                    }
                }
            }
            w.end(d);
        }
    }
    true
}

/// The reply cursor of a batch (mongoc_cursor_new_from_command_reply_with_opts with the
/// batch's server and session): every result goes to `each` (false: stop, the error is set).
/// Err(None): `each` failed; Err(Some): a cursor error. A cursor left open is killed.
/// At most `ops` results and `ops` getMore round trips are accepted (one result per operation).
fn read_cursor(client: &mut Client, cmd: &Cmd, mut session: Option<&mut ClientSession>, reply: &[u8], ops: usize, mut each: impl FnMut(&[u8]) -> bool) -> Result<(), Option<Failure>> {
    let invalid = || Failure::from(Error::new(domain::PROTOCOL, code::PROTOCOL_INVALID_REPLY, "Invalid reply to bulkWrite command."));
    let Some(Value::Document(c)) = find(reply, b"cursor") else { return Err(Some(invalid())) };
    let (Some(id), Some(Value::Utf8(ns)), Some(Value::Array(first))) = (cursor_id(c), find(c, b"ns"), find(c, b"firstBatch")) else { return Err(Some(invalid())) };
    let ns = String::from_utf8_lossy(c_part(ns)).into_owned();
    let (db, coll) = ns.split_once('.').map_or((ns.as_str(), ""), |(d, c)| (d, c));
    let (db, coll) = (db.to_owned(), coll.to_owned());
    let mut id = id;
    let mut batch = first.to_vec();
    let mut seen = 0usize;
    let mut round_trips = 0usize;
    // the cursor's own operation id (_mongoc_cursor_new_with_opts)
    client.next_operation_id();
    let result = loop {
        let mut each_failed = false;
        if let Some(mut it) = Iter::new(&batch) {
            while it.next() {
                let Value::Document(d) = it.value() else { continue };
                seen += 1;
                if seen > ops {
                    break;
                }
                if !each(d) {
                    each_failed = true;
                    break;
                }
            }
        }
        if each_failed {
            break Err(None);
        }
        if seen > ops {
            break Err(Some(Failure::from(invalid_arg(format!("bulk write returned more than {ops} results for {ops} operations")))));
        }
        if id == 0 {
            break Ok(());
        }
        round_trips += 1;
        if round_trips > ops {
            break Err(Some(Failure::from(invalid_arg(format!("bulk write results cursor needed more than {ops} getMore commands for {ops} operations")))));
        }
        let mut w = Writer::with_capacity(64 + coll.len());
        w.int64(b"getMore", id);
        w.utf8(b"collection", coll.as_bytes());
        match run_on_cursor(client, cmd, session.as_deref_mut(), &db, w.finish()) {
            Ok(r) => {
                let next = find(&r, b"cursor").and_then(|v| if let Value::Document(c) = v { Some(c.to_vec()) } else { None });
                match next.as_deref().and_then(|c| Some((cursor_id(c)?, find(c, b"nextBatch")?))) {
                    Some((i, Value::Array(b))) => {
                        id = i;
                        batch = b.to_vec();
                    }
                    _ => {
                        id = 0;
                        break Err(Some(Failure { reply: r.clone(), ..Failure::from(Error::new(domain::PROTOCOL, code::PROTOCOL_INVALID_REPLY, "Invalid reply to getMore command.")) }));
                    }
                }
            }
            // libmongoc keeps the cursor id after a failed getMore: killCursors follows
            Err(f) => break Err(Some(f)),
        }
    };
    if id != 0 {
        let mut w = Writer::with_capacity(64 + coll.len());
        w.utf8(b"killCursors", coll.as_bytes());
        let a = w.begin(4, b"cursors");
        w.int64(b"0", id);
        w.end(a);
        let _ = run_on_cursor(client, cmd, session, &db, w.finish());
    }
    result
}

fn cursor_id(c: &[u8]) -> Option<i64> {
    match find(c, b"id")? {
        Value::Int64(v) => Some(v),
        Value::Int32(v) => Some(v as i64),
        _ => None,
    }
}

/// A getMore or killCursors of the reply cursor: same server and session as the batch.
fn run_on_cursor(client: &mut Client, cmd: &Cmd, session: Option<&mut ClientSession>, db: &str, body: Vec<u8>) -> Result<Vec<u8>, Failure> {
    let mut b = body;
    let implicit = session.is_none();
    if implicit {
        if let Some(t) = &cmd.temp_session {
            let mut w = Writer::with_capacity(b.len() + t.lsid.len() + 16);
            w.concat(&b);
            w.document(b"lsid", &t.lsid);
            b = w.finish();
        }
    }
    let mut parts = Parts::new(db, &b);
    parts.is_read = true;
    if implicit {
        parts.prohibit_lsid = true;
    }
    let mut session = session;
    parts.session = session.as_deref_mut();
    let mut c = client.assemble(parts, cmd.server_id, false)?;
    let r = client.run_monitored(&mut c, &[], session);
    client.release_temp_session(&mut c);
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(f: impl FnOnce(&mut Writer)) -> Vec<u8> {
        let mut w = Writer::new();
        f(&mut w);
        w.finish()
    }

    fn models(n: usize) -> Vec<Model> {
        (0..n).map(|_| Model { op: Op::Delete, ns: "db.c".into(), id: None }).collect()
    }

    #[test]
    fn result_index_out_of_range() {
        for idx in [-1i64, 3, i64::MAX] {
            let (mut res, mut exc) = (BulkWriteResult::default(), BulkWriteException::default());
            let r = doc(|w| {
                w.int32(b"ok", 1);
                w.int64(b"idx", idx);
                w.int32(b"n", 1);
            });
            assert!(!apply_result(&mut res, &mut exc, &r, 1, &models(3)));
            assert!(exc.error.message.contains("is out of range for 3 operations"), "{}", exc.error.message);
        }
        let (mut res, mut exc) = (BulkWriteResult::default(), BulkWriteException::default());
        let r = doc(|w| {
            w.int32(b"ok", 1);
            w.int32(b"idx", 1);
            w.int32(b"n", 1);
        });
        assert!(apply_result(&mut res, &mut exc, &r, 1, &models(3)));
    }

    #[test]
    fn reply_counts_and_wce() {
        let (mut res, mut exc) = (BulkWriteResult::default(), BulkWriteException::default());
        let wce = doc(|w| {
            w.int32(b"code", 64);
            w.utf8(b"errmsg", b"waiting");
        });
        let r = doc(|w| {
            for k in ["nInserted", "nMatched", "nModified", "nDeleted", "nUpserted", "nErrors"] {
                w.int32(k.as_bytes(), 2);
            }
            w.document(b"writeConcernError", &wce);
        });
        assert!(apply_reply(&mut res, &mut exc, &r));
        assert_eq!((res.inserted_count, res.errors_count), (2, 2));
        let first = Iter::new(&exc.write_concern_errors()).and_then(|mut it| it.next().then(|| it.key().to_vec()));
        assert_eq!(first.as_deref(), Some(&b"0"[..]));
        let missing = doc(|w| w.int32(b"nInserted", 1));
        assert!(!apply_reply(&mut res, &mut exc, &missing));
        assert_eq!(exc.error.message, "expected to find int32, int64, or double `nMatched`, but did not");
    }

    #[test]
    fn model_validation() {
        let mut bw = BulkWrite::new();
        let filter = doc(|_| {});
        let bad = doc(|w| w.int32(b"x", 1));
        let e = bw.append_update("db.c", &filter, &bad, &ModelOpts::default(), false).unwrap_err();
        assert_eq!(e.message, "Invalid key 'x': update only works with $ operators and pipelines");
        let bad = doc(|w| w.int32(b"$set", 1));
        let e = bw.append_replace_one("db.c", &filter, &bad, &ModelOpts::default()).unwrap_err();
        assert_eq!(e.message, "Invalid key '$set': replace prohibits $ operators");
        bw.append_insert_one("db.c", &doc(|w| w.int32(b"a", 1))).unwrap();
        // the placeholder nsInfo index sits where execute overwrites it
        assert_eq!(&bw.ops[OP_INDEX_OFFSET..OP_INDEX_OFFSET + 4], &(-1i32).to_le_bytes());
        assert_eq!(&bw.ops[5..12], b"insert\0");
        assert!(bw.models[0].id.as_ref().is_some_and(|d| find(d, b"_id").is_some()));
    }

    /// A server reporting maxWriteBatchSize 0 still gets one operation per batch.
    #[test]
    fn batch_progress_with_zero_max_batch() {
        let mut bw = BulkWrite::new();
        for ns in ["db.a", "db.b", "db.a"] {
            bw.append_delete(ns, &doc(|_| {}), &ModelOpts::default(), false).unwrap();
        }
        let (bytes, docs, ns) = bw.fill_batch(0, 0, 0, 48_000_000, 1000).unwrap();
        assert_eq!(docs, 1);
        assert_eq!(bytes, crate::bson::iter::rd_u32(&bw.ops, 0) as usize);
        assert_eq!(ns.index.len(), 1);
        let (_, docs, ns) = bw.fill_batch(0, 0, 100_000, 48_000_000, 1000).unwrap();
        assert_eq!((docs, ns.index.len()), (3, 2));
        // the third op reuses nsInfo entry 0
        let third = 2 * crate::bson::iter::rd_u32(&bw.ops, 0) as usize;
        assert_eq!(&bw.ops[third + OP_INDEX_OFFSET..third + OP_INDEX_OFFSET + 4], &0i32.to_le_bytes());
        let e = bw.fill_batch(0, 0, 100_000, 1000, 1000).unwrap_err();
        assert_eq!(e.message, "unable to send document at index 0. Sending would exceed maxMessageSizeBytes=1000");
    }

    #[test]
    fn nsinfo_size() {
        let mut n = NsInfo::default();
        assert_eq!(n.append("db.coll").unwrap(), 0);
        assert_eq!(n.payload.len(), NsInfo::bson_size("db.coll"));
        assert_eq!(n.find("db.coll"), Some(0));
    }

    /// A reply cursor delivering more results than operations is refused (and nothing
    /// beyond the bound is applied).
    #[test]
    fn cursor_result_bound() {
        let uri = crate::uri::Uri::parse("mongodb://127.0.0.1:1/").unwrap();
        let settings = crate::client::Settings::from_uri(&uri);
        let mut client = Client::new(uri, settings).unwrap();
        let cmd = Cmd { name: "bulkWrite".into(), db: "admin".into(), body: Vec::new(), is_acknowledged: true, is_retryable_write: false, is_retryable_read: false, is_txn_finish: false, server_id: 1, temp_session: None, operation_id: 0, read_prefs: None, stream: (0, false) };
        let result = doc(|w| w.int32(b"ok", 1));
        let first = doc(|w| {
            for i in 0..3 {
                w.document(i.to_string().as_bytes(), &result);
            }
        });
        let c = doc(|w| {
            w.int64(b"id", 0);
            w.utf8(b"ns", b"admin.$cmd.bulkWrite");
            w.array(b"firstBatch", &first);
        });
        let reply = doc(|w| w.document(b"cursor", &c));
        let mut seen = 0;
        let r = read_cursor(&mut client, &cmd, None, &reply, 2, |_| {
            seen += 1;
            true
        });
        assert_eq!(seen, 2);
        let e = r.unwrap_err().unwrap().error;
        assert_eq!(e.message, "bulk write returned more than 2 results for 2 operations");
        let mut seen = 0;
        assert!(read_cursor(&mut client, &cmd, None, &reply, 3, |_| {
            seen += 1;
            true
        })
        .is_ok());
        assert_eq!(seen, 3);
    }
}
