//! MongoDB\Driver\Cursor (src/MongoDB/Cursor.c over libmongoc's command cursors).

use super::ClientRef;
use crate::bson::decode::TypeMap;
use crate::bson::{throw, E};
use crate::zend::call::{self, Ex};
use crate::zend::{hash, obj, sys, zval};
use mongo_core::bson::iter::{Iter, Value};
use mongo_core::bson::writer::Writer;
use mongo_core::error::Error;
use std::collections::VecDeque;
use std::ptr::{addr_of_mut, null_mut};

pub struct Cursor {
    pub client: Option<ClientRef>,
    pub server_id: u32,
    pub db: String,
    pub coll: String,
    pub id: i64,
    batch: VecDeque<Vec<u8>>,
    current_doc: Option<Vec<u8>>,
    zchild: sys::zval,
    current: i64,
    advanced: bool,
    pub typemap: TypeMap,
    pub batch_size: u32,
    pub max_await_time_ms: u32,
    error: Option<(Error, Option<Vec<u8>>)>,
    /// the first batch was taken from the reply
    started: bool,
}

impl Default for Cursor {
    fn default() -> Self {
        Cursor {
            client: None,
            server_id: 0,
            db: String::new(),
            coll: String::new(),
            id: 0,
            batch: VecDeque::new(),
            current_doc: None,
            zchild: zval::undef(),
            current: 0,
            advanced: false,
            typemap: TypeMap::default(),
            batch_size: 0,
            max_await_time_ms: 0,
            error: None,
            started: false,
        }
    }
}

impl Drop for Cursor {
    fn drop(&mut self) {
        unsafe { self.free_current() };
        // mongoc_cursor_destroy: kill a live server cursor
        if self.id != 0 {
            if let Some(c) = self.client.take() {
                let mut w = Writer::with_capacity(64);
                w.utf8(b"killCursors", self.coll.as_bytes());
                let a = w.begin(4, b"cursors");
                w.int64(b"0", self.id);
                w.end(a);
                w.utf8(b"$db", self.db.as_bytes());
                if let Ok(mut cl) = c.try_borrow_mut() {
                    let _ = cl.run_command(self.server_id, "killCursors", &self.db.clone(), &w.finish());
                }
            }
        }
    }
}

static mut H_CURSOR: sys::zend_object_handlers = unsafe { std::mem::zeroed() };
pub static mut CE: *mut sys::zend_class_entry = null_mut();

unsafe extern "C" fn create(c: *mut sys::zend_class_entry) -> *mut sys::zend_object {
    obj::alloc(c, Cursor::default(), addr_of_mut!(H_CURSOR))
}

/// A Cursor object holding `c`.
pub unsafe fn new_cursor(c: Cursor) -> sys::zval {
    zval::obj(obj::alloc(CE, c, addr_of_mut!(H_CURSOR)))
}

pub unsafe fn init_MongoDB_Driver_Cursor(c: *mut sys::zend_class_entry) {
    CE = c;
    (*c).__bindgen_anon_2.create_object = Some(create);
    obj::handlers::<Cursor>(addr_of_mut!(H_CURSOR));
}

fn find<'a>(d: &'a [u8], k: &[u8]) -> Option<Value<'a>> {
    let mut it = Iter::new(d)?;
    it.find(k).then(|| it.value())
}

impl Cursor {
    /// mongoc_cursor_new_from_command_reply: a cursor over reply.cursor.firstBatch, or over
    /// the reply itself.
    pub fn from_reply(reply: Vec<u8>, client: ClientRef, server_id: u32, db: &str) -> Cursor {
        let mut c = Cursor::default();
        c.client = Some(client);
        c.server_id = server_id;
        c.db = db.to_owned();
        if let Some(Value::Document(cur)) = find(&reply, b"cursor") {
            if let Some(v @ (Value::Int32(_) | Value::Int64(_))) = find(cur, b"id") {
                c.id = mongo_core::sdam::as_i64(&v);
            }
            if let Some(Value::Utf8(ns)) = find(cur, b"ns") {
                let ns = String::from_utf8_lossy(ns);
                if let Some((d, coll)) = ns.split_once('.') {
                    c.db = d.to_owned();
                    c.coll = coll.to_owned();
                }
            }
            if let Some(Value::Array(b)) = find(cur, b"firstBatch") {
                c.push_batch(b);
            }
        } else {
            c.batch.push_back(reply);
        }
        c
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
        if !zval::is(&self.zchild, sys::IS_UNDEF) {
            zval::dtor(&mut self.zchild);
            self.zchild = zval::undef();
        }
    }

    /// mongoc_cursor_more
    pub fn more(&self) -> bool {
        self.error.is_none() && (!self.started || !self.batch.is_empty() || self.id != 0)
    }

    fn get_more(&mut self) -> Result<(), (Error, Option<Vec<u8>>)> {
        let Some(c) = self.client.clone() else { return Ok(()) };
        let mut w = Writer::with_capacity(96);
        w.int64(b"getMore", self.id);
        w.utf8(b"collection", self.coll.as_bytes());
        if self.batch_size > 0 {
            w.int64(b"batchSize", self.batch_size as i64);
        }
        if self.max_await_time_ms > 0 {
            w.int64(b"maxTimeMS", self.max_await_time_ms as i64);
        }
        w.utf8(b"$db", self.db.as_bytes());
        let mut cl = c.borrow_mut();
        let reply = cl.run_command(self.server_id, "getMore", &self.db.clone(), &w.finish()).map_err(|e| (e, None))?;
        if let Err(e) = mongo_core::cmd::check_ok(&reply) {
            cl.handle_command_error(self.server_id, &reply);
            self.id = 0;
            return Err((e.into_error(), Some(reply)));
        }
        drop(cl);
        self.id = 0;
        if let Some(Value::Document(cur)) = find(&reply, b"cursor") {
            if let Some(v @ (Value::Int32(_) | Value::Int64(_))) = find(cur, b"id") {
                self.id = mongo_core::sdam::as_i64(&v);
            }
            if let Some(Value::Array(b)) = find(cur, b"nextBatch") {
                let b = b.to_vec();
                self.push_batch(&b);
            }
        }
        Ok(())
    }

    /// mongoc_cursor_next: the next document (current_doc), false at the end or on error.
    fn next_doc(&mut self) -> bool {
        self.current_doc = None;
        if self.error.is_some() {
            return false;
        }
        self.started = true;
        while self.batch.is_empty() && self.id != 0 {
            if let Err(e) = self.get_more() {
                self.error = Some(e);
                return false;
            }
        }
        self.current_doc = self.batch.pop_front();
        self.current_doc.is_some()
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
    let mut this = zval::obj(call::this(ex));
    let r = sys::spl_iterator_apply(&mut this, Some(to_array_apply), &mut out as *mut _ as *mut std::ffi::c_void);
    if r != 0 {
        zval::dtor(&mut out);
        *rv = zval::null();
        return;
    }
    *rv = out;
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
        *rv = crate::bson::types::new_int64(me(ex).id);
    }
}

pub unsafe extern "C" fn zim_MongoDB_Driver_Cursor_getServer(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        let c = me(ex);
        if let Some(cl) = &c.client {
            *rv = super::server::new_server(cl.clone(), c.server_id);
        }
    }
}

pub unsafe extern "C" fn zim_MongoDB_Driver_Cursor_isDead(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        *rv = zval::bool(!me(ex).more());
    }
}

pub unsafe extern "C" fn zim_MongoDB_Driver_Cursor_current(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        let c = me(ex);
        *rv = if zval::is(&c.zchild, sys::IS_UNDEF) { zval::null() } else { zval::copy(zval::deref(&mut c.zchild)) };
    }
}

pub unsafe extern "C" fn zim_MongoDB_Driver_Cursor_key(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        let c = me(ex);
        *rv = if zval::is(&c.zchild, sys::IS_UNDEF) { zval::null() } else { zval::long(c.current) };
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
}

pub unsafe extern "C" fn zim_MongoDB_Driver_Cursor_valid(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        *rv = zval::bool(!zval::is(&me(ex).zchild, sys::IS_UNDEF));
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
}
