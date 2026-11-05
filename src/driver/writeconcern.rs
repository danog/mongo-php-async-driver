//! MongoDB\Driver\WriteConcern (src/MongoDB/WriteConcern.c over mongoc_write_concern_t).

use crate::bson::{throw, E};
use crate::zend::call::{self, Ex};
use crate::zend::{class, hash, obj, sys, zval};
use mongo_core::uri::{W_DEFAULT, W_MAJORITY, W_TAG, W_UNACKNOWLEDGED};
use std::ptr::{addr_of_mut, null_mut};

/// mongoc_write_concern_t
#[derive(Clone, Debug)]
pub struct WriteConcern {
    pub w: i32,
    pub wtag: Option<Vec<u8>>,
    pub wtimeout: i64,
    /// None: MONGOC_WRITE_CONCERN_JOURNAL_DEFAULT
    pub journal: Option<bool>,
    pub is_default: bool,
}

impl Default for WriteConcern {
    fn default() -> Self {
        WriteConcern { w: W_DEFAULT, wtag: None, wtimeout: 0, journal: None, is_default: true }
    }
}

impl WriteConcern {
    fn set_w(&mut self, w: i32) {
        self.w = w;
        if w != W_DEFAULT {
            self.is_default = false;
        }
    }
    fn set_wtag(&mut self, t: &[u8]) {
        self.wtag = Some(t.to_vec());
        self.w = W_TAG;
        self.is_default = false;
    }
    fn set_wtimeout(&mut self, v: i64) {
        if v >= 0 {
            self.wtimeout = v;
            self.is_default = false;
        }
    }
    fn set_journal(&mut self, j: bool) {
        self.journal = Some(j);
        self.is_default = false;
    }
    /// mongoc_write_concern_is_valid
    fn is_valid(&self) -> bool {
        !(self.journal == Some(true) && self.w == W_UNACKNOWLEDGED) && self.wtimeout >= 0
    }
    /// mongoc_write_concern_is_acknowledged
    pub fn is_acknowledged(&self) -> bool {
        self.w != W_UNACKNOWLEDGED || self.journal == Some(true)
    }
    /// The client's write concern (from the URI).
    pub fn from_uri(u: &mongo_core::uri::Uri) -> Self {
        let wc = &u.write_concern;
        let mut r = WriteConcern::default();
        if let Some(t) = &wc.wtag {
            r.set_wtag(t.as_bytes());
        } else {
            r.set_w(wc.w);
        }
        if wc.wtimeout != 0 {
            r.set_wtimeout(wc.wtimeout);
        }
        if let Some(j) = wc.journal {
            r.set_journal(j);
        }
        r
    }
}

static mut H_WC: sys::zend_object_handlers = unsafe { std::mem::zeroed() };
pub static mut CE: *mut sys::zend_class_entry = null_mut();

unsafe extern "C" fn create(c: *mut sys::zend_class_entry) -> *mut sys::zend_object {
    obj::alloc(c, WriteConcern::default(), addr_of_mut!(H_WC))
}

unsafe extern "C" fn clone(o: *mut sys::zend_object) -> *mut sys::zend_object {
    let n = create((*o).ce);
    sys::zend_objects_clone_members(n, o);
    *obj::data::<WriteConcern>(n) = obj::data::<WriteConcern>(o).clone();
    n
}

pub unsafe fn init_MongoDB_Driver_WriteConcern(c: *mut sys::zend_class_entry) {
    CE = c;
    (*c).__bindgen_anon_2.create_object = Some(create);
    obj::handlers::<WriteConcern>(addr_of_mut!(H_WC)).clone_obj = Some(clone);
}

pub unsafe fn new_writeconcern(wc: WriteConcern) -> sys::zval {
    let o = obj::alloc(CE, wc, addr_of_mut!(H_WC));
    update_properties(o);
    zval::obj(o)
}

/// getW()'s value
unsafe fn w_zval(wc: &WriteConcern) -> sys::zval {
    if let Some(t) = &wc.wtag {
        zval::string(t)
    } else if wc.w == W_MAJORITY {
        zval::string(b"majority")
    } else if wc.w != W_DEFAULT {
        zval::long(wc.w as i64)
    } else {
        zval::null()
    }
}

unsafe fn update_properties(o: *mut sys::zend_object) {
    let wc = obj::data::<WriteConcern>(o).clone();
    let mut w = w_zval(&wc);
    sys::zend_update_property(CE, o, c"w".as_ptr(), 1, &mut w);
    zval::dtor(&mut w);
    match wc.journal {
        Some(j) => sys::zend_update_property_bool(CE, o, c"j".as_ptr(), 1, j as i64),
        None => sys::zend_update_property_null(CE, o, c"j".as_ptr(), 1),
    }
    sys::zend_update_property_long(CE, o, c"wtimeout".as_ptr(), 8, wc.wtimeout);
}

pub unsafe extern "C" fn zim_MongoDB_Driver_WriteConcern___construct(ex: Ex, _rv: *mut sys::zval) {
    let mut w: *mut sys::zval = null_mut();
    let mut wtimeout: i64 = 0;
    let mut journal: *mut sys::zval = null_mut();
    if sys::zend_parse_parameters(call::num_args(ex), c"z|lz".as_ptr(), &mut w, &mut wtimeout, &mut journal) != 0 {
        return;
    }
    let o = call::this(ex);
    let wc = obj::data::<WriteConcern>(o);
    *wc = WriteConcern::default();
    if zval::is(w, sys::IS_LONG) {
        let v = (*w).value.lval;
        if v < -3 {
            throw(E::InvalidArgument, format!("Expected w to be >= -3, {v} given"));
            return;
        }
        wc.set_w(v as i32);
    } else if zval::is(w, sys::IS_STRING) {
        let s = crate::zend::zstr::c_part(zval::str_bytes(w));
        if s == b"majority" {
            wc.set_w(W_MAJORITY);
        } else {
            wc.set_wtag(s);
        }
    } else {
        throw(E::InvalidArgument, format!("Expected w to be integer or string, {} given", zval::type_name(w)));
        return;
    }
    let n = call::num_args(ex);
    if n >= 3 && !journal.is_null() && !zval::is(journal, sys::IS_NULL) {
        let j = sys::zend_is_true(journal);
        if j && wc.w == W_UNACKNOWLEDGED {
            throw(E::InvalidArgument, "Cannot enable journaling when using w = 0");
            return;
        }
        wc.set_journal(j);
    }
    if n >= 2 {
        if wtimeout < 0 {
            throw(E::InvalidArgument, format!("Expected wtimeout to be >= 0, {wtimeout} given"));
            return;
        }
        wc.set_wtimeout(wtimeout);
    }
    if !wc.is_valid() {
        throw(E::InvalidArgument, "Write concern is not valid");
        return;
    }
    update_properties(o);
}

/// phongo_parse_int64
fn parse_int64(s: &[u8]) -> Option<i64> {
    std::str::from_utf8(s).ok()?.parse().ok()
}

unsafe fn init_from_hash(o: *mut sys::zend_object, props: *mut sys::HashTable) -> bool {
    let name = class::name(CE);
    let wc = obj::data::<WriteConcern>(o);
    *wc = WriteConcern::default();
    if let Some(w) = hash::find(props, b"w") {
        if zval::is(w, sys::IS_LONG) {
            if (*w).value.lval < -3 {
                throw(E::InvalidArgument, format!("{name} initialization requires \"w\" integer field to be >= -3"));
                return false;
            }
            wc.set_w((*w).value.lval as i32);
        } else if zval::is(w, sys::IS_STRING) {
            let s = crate::zend::zstr::c_part(zval::str_bytes(w));
            if s == b"majority" {
                wc.set_w(W_MAJORITY);
            } else {
                wc.set_wtag(s);
            }
        } else if !zval::is(w, sys::IS_NULL) {
            throw(E::InvalidArgument, format!("{name} initialization requires \"w\" field to be integer or string"));
            return false;
        }
    }
    if let Some(t) = hash::find(props, b"wtimeout") {
        if zval::is(t, sys::IS_LONG) {
            if (*t).value.lval < 0 {
                throw(E::InvalidArgument, format!("{name} initialization requires \"wtimeout\" integer field to be >= 0"));
                return false;
            }
            wc.set_wtimeout((*t).value.lval);
        } else if zval::is(t, sys::IS_STRING) {
            match parse_int64(zval::str_bytes(t)) {
                Some(v) => wc.set_wtimeout(v),
                None => {
                    throw(E::InvalidArgument, format!("Error parsing \"{}\" as 64-bit value for {name} initialization", crate::bson::cstr_lossy(zval::str_bytes(t))));
                    return false;
                }
            }
        } else if !zval::is(t, sys::IS_NULL) {
            throw(E::InvalidArgument, format!("{name} initialization requires \"wtimeout\" field to be integer or string"));
            return false;
        }
    }
    if let Some(j) = hash::find(props, b"j") {
        if zval::is(j, sys::IS_TRUE) || zval::is(j, sys::IS_FALSE) {
            let jv = zval::is(j, sys::IS_TRUE);
            if jv && wc.w == W_UNACKNOWLEDGED {
                throw(E::InvalidArgument, "Cannot enable journaling when using w = 0");
                return false;
            }
            wc.set_journal(jv);
        } else if !zval::is(j, sys::IS_NULL) {
            throw(E::InvalidArgument, format!("{name} initialization requires \"j\" field to be boolean"));
            return false;
        }
    }
    if !wc.is_valid() {
        throw(E::InvalidArgument, "Write concern is not valid");
        return false;
    }
    update_properties(o);
    true
}

pub unsafe extern "C" fn zim_MongoDB_Driver_WriteConcern___set_state(ex: Ex, rv: *mut sys::zval) {
    let mut a: *mut sys::zval = null_mut();
    if sys::zend_parse_parameters(call::num_args(ex), c"a".as_ptr(), &mut a) != 0 {
        return;
    }
    let z = obj::new_zval(CE);
    init_from_hash(z.value.obj, (*a).value.arr);
    *rv = z;
}

pub unsafe extern "C" fn zim_MongoDB_Driver_WriteConcern___unserialize(ex: Ex, _rv: *mut sys::zval) {
    let mut a: *mut sys::zval = null_mut();
    if sys::zend_parse_parameters(call::num_args(ex), c"a".as_ptr(), &mut a) != 0 {
        return;
    }
    init_from_hash(call::this(ex), (*a).value.arr);
}

#[inline]
unsafe fn me<'a>(ex: Ex) -> &'a WriteConcern {
    obj::data::<WriteConcern>(call::this(ex))
}

pub unsafe extern "C" fn zim_MongoDB_Driver_WriteConcern_getW(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        *rv = w_zval(me(ex));
    }
}
pub unsafe extern "C" fn zim_MongoDB_Driver_WriteConcern_getWtimeout(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        *rv = zval::long(me(ex).wtimeout);
    }
}
pub unsafe extern "C" fn zim_MongoDB_Driver_WriteConcern_getJournal(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        *rv = me(ex).journal.map_or_else(zval::null, zval::bool);
    }
}
pub unsafe extern "C" fn zim_MongoDB_Driver_WriteConcern_isDefault(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        *rv = zval::bool(me(ex).is_default);
    }
}
pub unsafe extern "C" fn zim_MongoDB_Driver_WriteConcern_bsonSerialize(ex: Ex, rv: *mut sys::zval) {
    if !call::none(ex) {
        return;
    }
    let wc = me(ex).clone();
    let ht = hash::new(3);
    let w = w_zval(&wc);
    if !zval::is(&w, sys::IS_NULL) {
        hash::str_update(ht, b"w", w);
    }
    if let Some(j) = wc.journal {
        hash::str_update(ht, b"j", zval::bool(j));
    }
    if wc.wtimeout != 0 {
        // ADD_ASSOC_INT64: an Int64 object only when it does not fit a zend_long (64-bit: never)
        hash::str_update(ht, b"wtimeout", zval::long(wc.wtimeout));
    }
    let mut z = zval::arr(ht);
    sys::convert_to_object(&mut z);
    *rv = z;
}
