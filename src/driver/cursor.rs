//! MongoDB\Driver\Cursor (src/MongoDB/Cursor.c over libmongoc's command cursors).

use mongo_core::log;
use super::ClientRef;
use crate::bson::decode::TypeMap;
use crate::bson::{throw, E};
use crate::zend::call::{self, Ex};
use crate::zend::{hash, obj, sys, zval};
use mongo_core::bson::iter::{Iter, Value};
use mongo_core::bson::writer::Writer;
use mongo_core::cmdparts::{Failure, Parts};
use mongo_core::error::Error;
use mongo_core::session::ClientSession;
use std::collections::VecDeque;
use std::ptr::{addr_of_mut, null_mut};

/// mongoc_cursor_t's cursor-level options
#[derive(Clone, Debug, Default)]
pub struct CursorOpts {
    pub batch_size: i64,
    pub limit: i64,
    pub single_batch: bool,
    pub tailable: bool,
    pub await_data: bool,
    pub max_await_time_ms: i64,
    /// {"comment": value}
    pub comment: Option<Vec<u8>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Unprimed,
    InBatch,
    Done,
}

pub struct Cursor {
    pub client: Option<ClientRef>,
    pub server_id: u32,
    pub db: String,
    pub coll: String,
    pub id: i64,
    state: State,
    batch: VecDeque<Vec<u8>>,
    current_doc: Option<Vec<u8>>,
    zchild: sys::zval,
    current: i64,
    advanced: bool,
    pub typemap: TypeMap,
    pub opts: CursorOpts,
    /// documents returned so far (mongoc_cursor_t.count)
    count: i64,
    error: Option<(Error, Option<Vec<u8>>)>,
    /// the client generation at creation (mongoc_cursor_t.client_generation)
    client_generation: u32,
    /// find: the filter and the options appended to the find command
    find: Option<(Vec<u8>, Vec<u8>)>,
    pub read_prefs: Option<(mongo_core::uri::ReadMode, Vec<u8>)>,
    pub read_concern: Vec<u8>,
    /// the Session object (explicit, or phongo's implicit one for commands)
    pub session: sys::zval,
    /// libmongoc's implicit session of a find cursor
    implicit: Option<ClientSession>,
    /// the selection read preference (a retried find selects with it)
    pub retry_prefs: Option<mongo_core::topology::ReadPrefs>,
    /// phongo's database/collection/query/command/readPreference (debug info)
    pub database: Option<String>,
    pub collection: Option<String>,
    pub zquery: sys::zval,
    pub zcommand: sys::zval,
    pub zread_preference: sys::zval,
    /// the Manager it came from (phongo_cursor_t.manager); declared last: released after the
    /// cursor is killed and its events delivered
    pub manager: super::ManagerRef,
}

impl Default for Cursor {
    fn default() -> Self {
        Cursor {
            client: None,
            server_id: 0,
            db: String::new(),
            coll: String::new(),
            id: 0,
            state: State::InBatch,
            batch: VecDeque::new(),
            current_doc: None,
            zchild: zval::undef(),
            current: 0,
            advanced: false,
            typemap: TypeMap::default(),
            opts: CursorOpts::default(),
            count: 0,
            error: None,
            client_generation: 0,
            find: None,
            read_prefs: None,
            read_concern: Vec::new(),
            session: zval::undef(),
            implicit: None,
            retry_prefs: None,
            database: None,
            collection: None,
            zquery: zval::undef(),
            zcommand: zval::undef(),
            zread_preference: zval::undef(),
            manager: super::ManagerRef::default(),
        }
    }
}

impl Drop for Cursor {
    fn drop(&mut self) {
        unsafe { self.free_current() };
        // mongoc_cursor_destroy: kill a live server cursor, unless the client was reset (fork)
        // since: the cursor belongs to the parent process then
        if let Some(c) = self.client.take() {
            if let Ok(mut cl) = c.try_borrow_mut() {
                cl.check_fork();
                if self.id != 0 && cl.generation == self.client_generation {
                    let mut w = Writer::with_capacity(64);
                    w.utf8(b"killCursors", self.coll.as_bytes());
                    let a = w.begin(4, b"cursors");
                    w.int64(b"0", self.id);
                    w.end(a);
                    let body = w.finish();
                    // _mongoc_client_kill_cursor: no reconnection in load-balanced mode
                    let reconnect_ok = cl.td.ty != mongo_core::topology::TopologyType::LoadBalanced;
                    let id = self.id;
                    if let Err(e) = cl.stream_for_server(self.server_id, reconnect_ok) {
                        log::info(log::CLIENT, || format!("Ignoring failure to connect to kill cursor {id}: {}", e.message));
                    } else if let Err(f) = self.run(&mut cl, &body) {
                        log::info(log::CLIENT, || format!("Ignoring failure to kill cursor {id}: {}", f.error.message));
                    }
                }
                if let Some(mut s) = self.implicit.take() {
                    cl.end_session(&mut s);
                }
            }
            unsafe { super::apm::flush(&c) };
        }
        // the PHP values are released here, in phongo_cursor_free's order (assigning drops the old)
        for z in [&mut self.session, &mut self.zquery, &mut self.zcommand, &mut self.zread_preference] {
            *z = zval::undef();
        }
    }
}

static mut H_CURSOR: sys::zend_object_handlers = unsafe { std::mem::zeroed() };
pub static mut CE: *mut sys::zend_class_entry = null_mut();

unsafe extern "C" fn create(c: *mut sys::zend_class_entry) -> *mut sys::zend_object {
    obj::alloc(c, Cursor::default(), addr_of_mut!(H_CURSOR))
}

/// A Cursor object holding `c`, of the Manager object `manager`.
pub unsafe fn new_cursor(mut c: Cursor, manager: *mut sys::zend_object) -> sys::zval {
    c.manager = super::ManagerRef::new(manager);
    zval::obj(obj::alloc(CE, c, addr_of_mut!(H_CURSOR)))
}

/// The PHP values it holds, for the GC.
unsafe extern "C" fn get_gc(o: *mut sys::zend_object, table: *mut *mut sys::zval, n: *mut i32) -> *mut sys::HashTable {
    let c = obj::data::<Cursor>(o);
    let extra: [*mut sys::zval; 6] = [&mut c.zchild, &mut c.session, &mut c.zquery, &mut c.zcommand, &mut c.zread_preference, c.manager.gc()];
    obj::get_gc_with(o, table, n, extra)
}

/// phongo_cursor_get_debug_info
unsafe extern "C" fn get_debug_info(o: *mut sys::zend_object, is_temp: *mut i32) -> *mut sys::HashTable {
    use crate::zend::hash;
    *is_temp = 1;
    let c = obj::data::<Cursor>(o);
    let ht = hash::new(10);
    let opt_str = |v: &Option<String>| v.as_ref().map_or_else(zval::null, |s| zval::string(s.as_bytes()));
    let opt_zval = |z: &sys::zval| if zval::is(z, sys::IS_UNDEF) { zval::null() } else { zval::copy(z) };
    hash::str_update(ht, b"database", opt_str(&c.database));
    hash::str_update(ht, b"collection", opt_str(&c.collection));
    hash::str_update(ht, b"query", opt_zval(&c.zquery));
    hash::str_update(ht, b"command", opt_zval(&c.zcommand));
    hash::str_update(ht, b"readPreference", opt_zval(&c.zread_preference));
    hash::str_update(ht, b"session", opt_zval(&c.session));
    hash::str_update(ht, b"isDead", zval::bool(!c.more()));
    hash::str_update(ht, b"currentIndex", zval::long(c.current));
    hash::str_update(ht, b"currentDocument", opt_zval(&c.zchild));
    hash::str_update(ht, b"server", c.client.as_ref().map_or_else(zval::null, |cl| super::server::new_server(cl.clone(), c.server_id, c.manager.obj())));
    ht
}

pub unsafe fn init_MongoDB_Driver_Cursor(c: *mut sys::zend_class_entry) {
    CE = c;
    (*c).__bindgen_anon_2.create_object = Some(create);
    let h = obj::handlers::<Cursor>(addr_of_mut!(H_CURSOR));
    h.get_debug_info = Some(get_debug_info);
    h.get_gc = Some(get_gc);
    h.free_obj = Some(super::free_obj::<Cursor>);
}

fn find<'a>(d: &'a [u8], k: &[u8]) -> Option<Value<'a>> {
    let mut it = Iter::new(d)?;
    it.find(k).then(|| it.value())
}

impl Cursor {
    /// mongoc_cursor_new_from_command_reply_with_opts: a cursor over reply.cursor.firstBatch
    /// (the reply is wrapped as a one-document batch by the caller otherwise).
    pub fn from_reply(reply: Vec<u8>, client: ClientRef, server_id: u32, db: &str) -> Cursor {
        let mut c = Cursor::default();
        c.client_generation = client.borrow().generation;
        c.client = Some(client);
        c.server_id = server_id;
        c.db = db.to_owned();
        c.state = State::InBatch;
        if !c.read_response(&reply) {
            c.error = Some((Error::new(mongo_core::error::domain::PROTOCOL, mongo_core::error::code::PROTOCOL_INVALID_REPLY, "Invalid reply to find command."), None));
        }
        c
    }

    /// _mongoc_cursor_find_new: an unprimed find cursor.
    pub fn for_find(client: ClientRef, server_id: u32, db: &str, coll: &str, filter: Vec<u8>, find_opts: Vec<u8>) -> Cursor {
        let mut c = Cursor::default();
        c.client_generation = client.borrow().generation;
        c.client = Some(client);
        c.server_id = server_id;
        c.db = db.to_owned();
        c.coll = coll.to_owned();
        c.state = State::Unprimed;
        c.find = Some((filter, find_opts));
        c
    }

    /// _mongoc_cursor_start_reading_response: cursor id, namespace and batch of a reply.
    fn read_response(&mut self, reply: &[u8]) -> bool {
        self.batch.clear();
        let mut in_batch = false;
        let mut ns: Option<Vec<u8>> = None;
        let mut ns_invalid = false;
        if let Some(Value::Document(cur)) = find(reply, b"cursor") {
            if let Some(mut it) = Iter::new(cur) {
                while it.next() {
                    match (it.key(), it.value()) {
                        (b"id", v) => self.id = mongo_core::sdam::as_i64(&v),
                        (b"ns", Value::Utf8(s)) => ns = Some(s.to_vec()),
                        (b"ns", _) => ns_invalid = true,
                        (b"firstBatch" | b"nextBatch", Value::Array(a)) => {
                            in_batch = true;
                            let a = a.to_vec();
                            self.push_batch(&a);
                        }
                        _ => {}
                    }
                }
            }
        }
        if let Some(ns) = ns {
            let dot = ns.iter().position(|&b| b == b'.');
            let has_coll = dot.is_some_and(|d| d + 1 < ns.len());
            if has_coll && !ns.contains(&0) {
                let s = String::from_utf8_lossy(&ns);
                let (d, c) = s.split_once('.').unwrap_or_default();
                self.db = d.to_owned();
                self.coll = c.to_owned();
            } else if self.id != 0 {
                self.id = 0;
                return false;
            }
        } else if ns_invalid && self.id != 0 {
            self.id = 0;
            return false;
        }
        if self.id != 0 && self.coll.is_empty() {
            self.id = 0;
            return false;
        }
        in_batch
    }

    fn push_batch(&mut self, arr: &[u8]) {
        if let Some(mut it) = Iter::new(arr) {
            while it.next() {
                if let Value::Document(d) = it.value() {
                    self.batch.push_back(d.to_vec());
                }
            }
        }
    }

    unsafe fn free_current(&mut self) {
        // releases the old value
        self.zchild = zval::undef();
    }

    /// mongoc_cursor_more
    pub fn more(&self) -> bool {
        self.error.is_none() && self.state != State::Done
    }

    /// mongoc_cursor_get_limit
    fn limit(&self) -> i64 {
        if self.opts.limit > 0 && self.opts.single_batch {
            -self.opts.limit
        } else {
            self.opts.limit
        }
    }

    /// _mongoc_n_return
    fn n_return(&self) -> i32 {
        let (limit, bs) = (self.limit(), self.opts.batch_size);
        let mut n = if limit < 0 {
            limit
        } else if limit == 0 {
            bs
        } else if bs == 0 || limit < bs {
            limit
        } else {
            bs
        };
        if limit > 0 && self.count > 0 {
            let remaining = limit - self.count;
            if remaining <= 0 {
                return 1;
            }
            n = n.min(remaining);
        }
        n.clamp(i32::MIN as i64, i32::MAX as i64) as i32
    }

    /// _mongoc_cursor_run_command: assembles `body` with the cursor's read preference,
    /// read concern and session, runs it and feeds the reply to the session.
    fn run(&mut self, cl: &mut mongo_core::client::Client, body: &[u8]) -> Result<Vec<u8>, Failure> {
        let mut parts = Parts::new(&self.db, body);
        parts.is_read = true;
        parts.read_prefs = self.read_prefs.clone();
        parts.read_concern = self.read_concern.clone();
        let has_obj = unsafe { !zval::is(&self.session, sys::IS_UNDEF) };
        if !has_obj && self.implicit.is_none() && cl.sessions_supported() {
            // an implicit session for the cursor's lifetime
            self.implicit = cl.start_session(true, false).ok();
        }
        let rc_obj = if has_obj { Some(unsafe { super::session_rc(&mut self.session) }) } else { None };
        let mut guard = rc_obj.as_ref().map(|r| r.borrow_mut());
        let mut sess: Option<&mut ClientSession> = match guard.as_deref_mut() {
            Some(g) => Some(g),
            None => self.implicit.as_mut(),
        };
        let is_get_more = matches!(mongo_core::cmdparts::command_name(body).as_deref(), Some("getMore" | "killCursors"));
        if !is_get_more {
            cl.check_server_for_session(self.server_id, sess.as_deref_mut())?;
        }
        parts.session = sess.as_deref_mut();
        let mut cmd = cl.assemble(parts, self.server_id, false)?;
        cmd.read_prefs = self.retry_prefs.clone();
        let r = if is_get_more {
            cl.run_monitored(&mut cmd, &[], sess)
        } else {
            let el = cl.read_eligibility(&cmd, false, true);
            cl.run_retryable_read(&mut cmd, sess, el)
        };
        self.server_id = cmd.server_id;
        cl.release_temp_session(&mut cmd);
        r
    }

    /// _mongoc_cursor_response_refresh with the find or getMore command.
    fn refresh(&mut self, body: Vec<u8>, name: &str) -> bool {
        let Some(c) = self.client.clone() else { return false };
        let r = {
            let mut cl = c.borrow_mut();
            self.run(&mut cl, &body)
        };
        unsafe { super::apm::flush(&c) };
        match r {
            Ok(reply) => {
                if self.read_response(&reply) {
                    self.release_implicit_if_exhausted(&c);
                    return true;
                }
                self.release_implicit_if_exhausted(&c);
                self.error = Some((Error::new(mongo_core::error::domain::PROTOCOL, mongo_core::error::code::PROTOCOL_INVALID_REPLY, format!("Invalid reply to {name} command.")), None));
                false
            }
            Err(f) => {
                let reply = (!f.reply.is_empty()).then_some(f.reply);
                self.error = Some((f.error, reply));
                false
            }
        }
    }

    /// The implicit session goes back to the pool once the cursor is exhausted.
    fn release_implicit_if_exhausted(&mut self, c: &ClientRef) {
        if self.id == 0 {
            if let Some(mut s) = self.implicit.take() {
                c.borrow_mut().end_session(&mut s);
            }
        }
    }

    /// _prime of a find cursor: {find: <coll>, filter: <filter>} + options.
    fn prime(&mut self) {
        let Some((filter, opts)) = self.find.clone() else { return };
        if let Some(c) = &self.client {
            c.borrow_mut().operation_id += 1;
        }
        let mut w = Writer::with_capacity(filter.len() + opts.len() + 64);
        w.utf8(b"find", self.coll.as_bytes());
        w.document(b"filter", &filter);
        w.concat(&opts);
        self.refresh(w.finish(), "find");
    }

    /// _mongoc_cursor_prepare_getmore_command
    fn get_more(&mut self) {
        let mut w = Writer::with_capacity(96);
        w.int64(b"getMore", self.id);
        w.utf8(b"collection", self.coll.as_bytes());
        if self.opts.batch_size != 0 {
            w.int64(b"batchSize", (self.n_return() as i64).abs());
        }
        if let Some(cm) = &self.opts.comment {
            let wire = self.client.as_ref().and_then(|c| c.borrow().stream_description(self.server_id).map(|s| s.max_wire_version)).unwrap_or(0);
            if wire >= 9 {
                w.concat(cm);
            }
        }
        if self.opts.tailable && self.opts.await_data && self.opts.max_await_time_ms != 0 {
            w.int64(b"maxTimeMS", self.opts.max_await_time_ms);
        }
        self.refresh(w.finish(), "getMore");
    }

    /// mongoc_cursor_next: the next document (current_doc), false at the end or on error.
    fn next_doc(&mut self) -> bool {
        self.current_doc = None;
        // mongoc_cursor_next: a cursor of a client reset since (fork) cannot advance
        if let Some(c) = &self.client {
            if let Ok(mut cl) = c.try_borrow_mut() {
                cl.check_fork();
                if cl.generation != self.client_generation {
                    let e = Error::new(mongo_core::error::domain::CURSOR, mongo_core::error::code::CURSOR_INVALID_CURSOR, "Cannot advance cursor after client reset");
                    self.error = Some((e, None));
                    return false;
                }
            }
        }
        if self.error.is_some() {
            return false;
        }
        if self.state == State::Done {
            let e = Error::new(mongo_core::error::domain::CURSOR, mongo_core::error::code::CURSOR_INVALID_CURSOR, "Cannot advance a completed or failed cursor.");
            self.error = Some((e, None));
            return false;
        }
        let mut attempted_refresh = false;
        let got = loop {
            if self.state == State::Unprimed {
                self.prime();
                self.state = State::InBatch;
            } else if let Some(d) = self.batch.pop_front() {
                self.current_doc = Some(d);
                break true;
            } else if self.id == 0 {
                self.state = State::Done;
                break false;
            } else {
                // END_OF_BATCH: one getMore per next() (tailable cursors may stay empty)
                if attempted_refresh {
                    self.count += 1;
                    return false;
                }
                attempted_refresh = true;
                self.get_more();
            }
            if self.error.is_some() {
                self.state = State::Done;
                break false;
            }
        };
        self.count += 1;
        got
    }

    unsafe fn decode_current(&mut self) {
        if let Some(d) = &self.current_doc {
            match super::to_zval(d, &self.typemap) {
                Some(z) => self.zchild = z,
                None => self.free_current(),
            }
        }
    }

    unsafe fn throw_pending(&mut self) -> bool {
        if let Some((e, r)) = &self.error {
            super::throw_error(e, r.as_deref());
            return true;
        }
        false
    }

    /// phongo_cursor_free_session_if_exhausted
    unsafe fn free_session_if_exhausted(&mut self) {
        if self.id == 0 && !zval::is(&self.session, sys::IS_UNDEF) {
            self.session = zval::undef();
        }
    }

    /// The next document of an internal find (the key vault): None at the end
    pub fn next_raw(&mut self) -> Result<Option<Vec<u8>>, (Error, Option<Vec<u8>>)> {
        if self.next_doc() {
            return Ok(self.current_doc.take());
        }
        match self.error.take() {
            Some(e) => Err(e),
            None => Ok(None),
        }
    }

    pub fn set_error(&mut self, e: Error) {
        self.error = Some((e, None));
    }

    /// phongo_cursor_advance_and_check_for_error for executeQuery: primes the cursor.
    pub unsafe fn advance_first(&mut self) -> bool {
        if !self.next_doc() {
            if self.throw_pending() {
                return false;
            }
        }
        self.advanced = true;
        // phongo_cursor_init_for_query: the document is decoded on rewind()/current()
        true
    }
}

#[inline]
unsafe fn me<'a>(ex: Ex) -> &'a mut Cursor {
    obj::data::<Cursor>(call::this(ex))
}

pub unsafe extern "C" fn zim_MongoDB_Driver_Cursor___construct(_ex: Ex, _rv: *mut sys::zval) {
    // PHONGO_DISABLED_CONSTRUCTOR
    throw(E::Runtime, "Accessing private constructor");
}

pub unsafe extern "C" fn zim_MongoDB_Driver_Cursor_setTypeMap(ex: Ex, _rv: *mut sys::zval) {
    let mut tm: *mut sys::zval = null_mut();
    if sys::zend_parse_parameters(call::num_args(ex), c"a!".as_ptr(), &mut tm) != 0 {
        return;
    }
    let mut map = TypeMap::default();
    if !tm.is_null() && !crate::bson::decode::typemap(tm, &mut map) {
        return;
    }
    let c = me(ex);
    let restore = !zval::is(&c.zchild, sys::IS_UNDEF);
    c.free_current();
    c.typemap = map;
    if restore {
        c.decode_current();
    }
}

pub unsafe extern "C" fn zim_MongoDB_Driver_Cursor_toArray(ex: Ex, rv: *mut sys::zval) {
    if !call::none(ex) {
        return;
    }
    let ht = hash::new(0);
    let mut out = zval::arr(ht);
    // $this, borrowed (no reference of ours)
    let mut this = zval::borrowed_obj(call::this(ex));
    let r = sys::spl_iterator_apply(&mut *this, Some(to_array_apply), &mut out as *mut _ as *mut std::ffi::c_void);
    if r != 0 {
        drop(out);
        zval::write(rv, zval::null());
        return;
    }
    zval::write(rv, out);
}

unsafe extern "C" fn to_array_apply(iter: *mut sys::zend_object_iterator, puser: *mut std::ffi::c_void) -> i32 {
    let data = ((*(*iter).funcs).get_current_data.unwrap())(iter);
    if call::has_exception() || data.is_null() || zval::is(data, sys::IS_UNDEF) {
        return sys::ZEND_HASH_APPLY_STOP as i32;
    }
    let out = puser as *mut sys::zval;
    hash::push((*out).value.arr, zval::copy(data));
    sys::ZEND_HASH_APPLY_KEEP as i32
}

pub unsafe extern "C" fn zim_MongoDB_Driver_Cursor_getId(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        zval::write(rv, crate::bson::types::new_int64(me(ex).id));
    }
}

pub unsafe extern "C" fn zim_MongoDB_Driver_Cursor_getServer(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        let c = me(ex);
        if let Some(cl) = &c.client {
            zval::write(rv, super::server::new_server(cl.clone(), c.server_id, c.manager.obj()));
        }
    }
}

pub unsafe extern "C" fn zim_MongoDB_Driver_Cursor_isDead(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        zval::write(rv, zval::bool(!me(ex).more()));
    }
}

pub unsafe extern "C" fn zim_MongoDB_Driver_Cursor_current(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        let c = me(ex);
        zval::write(rv, if zval::is(&c.zchild, sys::IS_UNDEF) { zval::null() } else { zval::copy(zval::deref(&mut c.zchild)) });
    }
}

pub unsafe extern "C" fn zim_MongoDB_Driver_Cursor_key(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        let c = me(ex);
        zval::write(rv, if zval::is(&c.zchild, sys::IS_UNDEF) { zval::null() } else { zval::long(c.current) });
    }
}

pub unsafe extern "C" fn zim_MongoDB_Driver_Cursor_next(ex: Ex, _rv: *mut sys::zval) {
    if !call::none(ex) {
        return;
    }
    let c = me(ex);
    c.free_current();
    if c.advanced {
        c.current += 1;
    } else {
        c.advanced = true;
    }
    if c.next_doc() {
        c.decode_current();
    } else {
        c.throw_pending();
    }
    c.free_session_if_exhausted();
}

pub unsafe extern "C" fn zim_MongoDB_Driver_Cursor_valid(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        zval::write(rv, zval::bool(!zval::is(&me(ex).zchild, sys::IS_UNDEF)));
    }
}

pub unsafe extern "C" fn zim_MongoDB_Driver_Cursor_rewind(ex: Ex, _rv: *mut sys::zval) {
    if !call::none(ex) {
        return;
    }
    let c = me(ex);
    if !c.advanced {
        c.advanced = true;
        // phongo_cursor_advance_and_check_for_error
        if !c.next_doc() && c.throw_pending() {
            return;
        }
    }
    if c.current > 0 {
        throw(E::Logic, "Cursors cannot rewind after starting iteration");
        return;
    }
    c.free_current();
    c.decode_current();
    c.free_session_if_exhausted();
}
