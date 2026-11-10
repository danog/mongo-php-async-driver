//! MongoDB\Driver\ReadPreference (src/MongoDB/ReadPreference.c over mongoc_read_prefs_t).

use crate::bson::{throw, E};
use crate::zend::call::{self, Ex};
use crate::zend::{class, hash, obj, sys, zstr, zval};
use mongo_core::bson::iter::{Iter, Value};
use mongo_core::bson::writer::Writer;
use mongo_core::topology::{ReadPrefs, SMALLEST_MAX_STALENESS_SECONDS};
use mongo_core::uri::{ReadMode, NO_MAX_STALENESS};
use std::ptr::{addr_of_mut, null_mut};

pub const EMPTY: [u8; 5] = [5, 0, 0, 0, 0];

#[derive(Clone)]
pub struct ReadPreference {
    pub mode: ReadMode,
    /// tag sets as a BSON array (empty document when none: borrowed, no allocation per
    /// operation for the usual read preference)
    pub tags: std::borrow::Cow<'static, [u8]>,
    pub max_staleness_seconds: i64,
    pub hedge: Option<Vec<u8>>,
}

impl Default for ReadPreference {
    fn default() -> Self {
        ReadPreference { mode: ReadMode::Primary, tags: std::borrow::Cow::Borrowed(&EMPTY), max_staleness_seconds: NO_MAX_STALENESS as i64, hedge: None }
    }
}

pub fn mode_str(m: ReadMode) -> &'static str {
    match m {
        ReadMode::Primary => "primary",
        ReadMode::PrimaryPreferred => "primaryPreferred",
        ReadMode::Secondary => "secondary",
        ReadMode::SecondaryPreferred => "secondaryPreferred",
        ReadMode::Nearest => "nearest",
    }
}

fn parse_mode(s: &[u8]) -> Option<ReadMode> {
    [ReadMode::Primary, ReadMode::PrimaryPreferred, ReadMode::Secondary, ReadMode::SecondaryPreferred, ReadMode::Nearest].into_iter().find(|m| mode_str(*m).as_bytes().eq_ignore_ascii_case(s))
}

impl ReadPreference {
    /// The selection view of this read preference.
    pub fn prefs(&self) -> ReadPrefs {
        let mut tags = Vec::new();
        if let Some(mut it) = Iter::new(&self.tags) {
            while it.next() {
                let mut set = Vec::new();
                if let Value::Document(d) = it.value() {
                    if let Some(mut t) = Iter::new(d) {
                        while t.next() {
                            // bson_iter_utf8: a value of another type reads as "" (it matches
                            // only an empty server tag, as in libmongoc's _match_tag_set)
                            let v = match t.value() {
                                Value::Utf8(v) => String::from_utf8_lossy(v).into_owned(),
                                _ => String::new(),
                            };
                            set.push((String::from_utf8_lossy(t.key()).into_owned(), v));
                        }
                    }
                }
                tags.push(set);
            }
        }
        ReadPrefs { mode: self.mode, tags, max_staleness_seconds: self.max_staleness_seconds }
    }

    /// mongoc_read_prefs_is_valid
    fn is_valid(&self) -> bool {
        if self.mode == ReadMode::Primary && (self.tags.len() > 5 || self.max_staleness_seconds != NO_MAX_STALENESS as i64 || self.hedge.as_ref().is_some_and(|h| h.len() > 5)) {
            return false;
        }
        !(self.max_staleness_seconds != NO_MAX_STALENESS as i64 && self.max_staleness_seconds <= 0)
    }

    /// mongoc_read_prefs_append_contents_to_bson (mode, tags, maxStalenessSeconds, hedge)
    pub fn append_contents(&self, w: &mut Writer, mode: ReadMode) {
        w.utf8(b"mode", mode_str(mode).as_bytes());
        if self.tags.len() > 5 {
            w.array(b"tags", &self.tags);
        }
        if self.max_staleness_seconds != NO_MAX_STALENESS as i64 {
            w.int64(b"maxStalenessSeconds", self.max_staleness_seconds);
        }
        if let Some(h) = self.hedge.as_ref().filter(|h| h.len() > 5) {
            w.document(b"hedge", h);
        }
    }
}

static mut H_RP: sys::zend_object_handlers = unsafe { std::mem::zeroed() };
pub static mut CE: *mut sys::zend_class_entry = null_mut();

unsafe extern "C" fn create(c: *mut sys::zend_class_entry) -> *mut sys::zend_object {
    obj::alloc(c, ReadPreference::default(), addr_of_mut!(H_RP))
}

unsafe extern "C" fn clone(o: *mut sys::zend_object) -> *mut sys::zend_object {
    let n = create((*o).ce);
    sys::zend_objects_clone_members(n, o);
    *obj::data::<ReadPreference>(n) = obj::data::<ReadPreference>(o).clone();
    n
}

unsafe extern "C" fn read_property(o: *mut sys::zend_object, name: *mut sys::zend_string, ty: i32, cache: *mut *mut std::ffi::c_void, rv: *mut sys::zval) -> *mut sys::zval {
    if zstr::bytes(name) == b"hedge" {
        sys::php_error_docref(null_mut(), sys::E_DEPRECATED as i32, c"Property MongoDB\\Driver\\ReadPreference::hedge is deprecated".as_ptr());
    }
    sys::zend_std_read_property(o, name, ty, cache, rv)
}

pub unsafe fn init_MongoDB_Driver_ReadPreference(c: *mut sys::zend_class_entry) {
    CE = c;
    (*c).__bindgen_anon_2.create_object = Some(create);
    let h = obj::handlers::<ReadPreference>(addr_of_mut!(H_RP));
    h.clone_obj = Some(clone);
    h.read_property = Some(read_property);
}

/// A ReadPreference object for `rp` (phongo_readpreference_init).
pub unsafe fn new_readpreference(rp: ReadPreference) -> sys::zval {
    let o = obj::alloc(CE, rp, addr_of_mut!(H_RP));
    update_properties(o);
    zval::obj(o)
}

unsafe fn update_properties(o: *mut sys::zend_object) {
    let rp = obj::data::<ReadPreference>(o).clone();
    let m = mode_str(rp.mode);
    sys::zend_update_property_string(CE, o, c"mode".as_ptr(), 4, call::cs(m).as_ptr());
    sys::zend_update_property_long(CE, o, c"maxStalenessSeconds".as_ptr(), 19, rp.max_staleness_seconds);
    if rp.tags.len() > 5 {
        let tm = crate::bson::decode::TypeMap { root: crate::bson::decode::TM::Array, ..Default::default() };
        let mut st = crate::bson::decode::State::new(&tm);
        st.is_visiting_array = false;
        match crate::bson::decode::to_zval(&rp.tags, &st) {
            Some(mut z) => sys::zend_update_property(CE, o, c"tags".as_ptr(), 4, &mut z),
            None => return,
        }
    } else {
        sys::zend_update_property_null(CE, o, c"tags".as_ptr(), 4);
    }
    match rp.hedge.filter(|h| h.len() > 5) {
        Some(h) => match super::to_zval(&h, &Default::default()) {
            Some(mut z) => sys::zend_update_property(CE, o, c"hedge".as_ptr(), 5, &mut z),
            None => (),
        },
        None => sys::zend_update_property_null(CE, o, c"hedge".as_ptr(), 5),
    }
}

/// phongo_read_preference_prep_tagsets + encode: the tag sets as BSON (arrays become documents).
unsafe fn tags_bson(z: *mut sys::zval) -> Option<Vec<u8>> {
    let ht = (*z).value.arr;
    let copy = hash::new(hash::count(ht));
    hash::each(ht, true, |k, v| {
        let v = zval::deref(v);
        let mut c = zval::copy(v);
        if zval::is(&c, sys::IS_ARRAY) {
            sys::convert_to_object(&mut c);
        }
        match k {
            hash::Key::Index(i) => hash::index_update(copy, i, c),
            hash::Key::Str(s) => hash::str_update(copy, zstr::bytes(s), c),
        }
        true
    });
    let mut cz = zval::arr(copy);
    crate::bson::encode::to_bson(&mut cz, crate::bson::encode::NONE)
}

/// phongo_read_preference_tags_are_valid
fn tags_are_valid(tags: &[u8]) -> bool {
    match Iter::new(tags) {
        Some(mut it) => {
            while it.next() {
                if !matches!(it.value(), Value::Document(_)) {
                    return false;
                }
            }
            true
        }
        None => false,
    }
}

pub unsafe extern "C" fn zim_MongoDB_Driver_ReadPreference___construct(ex: Ex, _rv: *mut sys::zval) {
    let mut mode: *mut sys::zend_string = null_mut();
    let mut tag_sets: *mut sys::zval = null_mut();
    let mut options: *mut sys::zval = null_mut();
    if sys::zend_parse_parameters(call::num_args(ex), c"S|a!a!".as_ptr(), &mut mode, &mut tag_sets, &mut options) != 0 {
        return;
    }
    let o = call::this(ex);
    let rp = obj::data::<ReadPreference>(o);
    match parse_mode(zstr::bytes(mode)) {
        Some(m) => rp.mode = m,
        None => {
            throw(E::InvalidArgument, format!("Invalid mode: '{}'", crate::bson::cstr_lossy(zstr::bytes(mode))));
            return;
        }
    }
    if !tag_sets.is_null() {
        let Some(tags) = tags_bson(tag_sets) else { return };
        if !tags_are_valid(&tags) {
            throw(E::InvalidArgument, "tagSets must be an array of zero or more documents");
            return;
        }
        if tags.len() > 5 && rp.mode == ReadMode::Primary {
            throw(E::InvalidArgument, "tagSets may not be used with primary mode");
            return;
        }
        rp.tags = tags.into();
    }
    if !options.is_null() {
        let oh = (*options).value.arr;
        if let Some(z) = hash::find(oh, b"maxStalenessSeconds") {
            let v = sys::zval_get_long_func(z, false);
            if v != NO_MAX_STALENESS as i64 {
                if v < SMALLEST_MAX_STALENESS_SECONDS {
                    throw(E::InvalidArgument, format!("Expected maxStalenessSeconds to be >= {SMALLEST_MAX_STALENESS_SECONDS}, {v} given"));
                    return;
                }
                if v > i32::MAX as i64 {
                    throw(E::InvalidArgument, format!("Expected maxStalenessSeconds to be <= {}, {v} given", i32::MAX));
                    return;
                }
                if rp.mode == ReadMode::Primary {
                    throw(E::InvalidArgument, "maxStalenessSeconds may not be used with primary mode");
                    return;
                }
            }
            rp.max_staleness_seconds = v;
        }
        if let Some(h) = hash::find(oh, b"hedge") {
            let h = zval::deref(h);
            if zval::is(h, sys::IS_ARRAY) || zval::is(h, sys::IS_OBJECT) {
                sys::php_error_docref(null_mut(), sys::E_DEPRECATED as i32, c"The \"hedge\" option is deprecated as of MongoDB 8.0 and will be removed in a future release".as_ptr());
                if rp.mode == ReadMode::Primary {
                    throw(E::InvalidArgument, "hedge may not be used with primary mode");
                    return;
                }
                let Some(b) = crate::bson::encode::to_bson(h, crate::bson::encode::NONE) else { return };
                rp.hedge = Some(b);
            } else {
                throw(E::InvalidArgument, format!("{} initialization requires \"hedge\" field to be an array or object", class::name(CE)));
                return;
            }
        }
    }
    if !rp.is_valid() {
        throw(E::InvalidArgument, "Read preference is not valid");
        return;
    }
    update_properties(o);
}

/// phongo_readpreference_init_from_hash
unsafe fn init_from_hash(o: *mut sys::zend_object, props: *mut sys::HashTable) -> bool {
    let name = class::name(CE);
    let rp = obj::data::<ReadPreference>(o);
    *rp = ReadPreference::default();
    match hash::find(props, b"mode") {
        Some(m) if zval::is(m, sys::IS_STRING) => match parse_mode(zval::str_bytes(m)) {
            Some(md) => rp.mode = md,
            None => {
                throw(E::InvalidArgument, format!("{name} initialization requires specific values for \"mode\" string field"));
                return false;
            }
        },
        _ => {
            throw(E::InvalidArgument, format!("{name} initialization requires \"mode\" field to be string"));
            return false;
        }
    }
    if let Some(t) = hash::find(props, b"tags") {
        let t = zval::deref(t);
        if zval::is(t, sys::IS_ARRAY) {
            let Some(tags) = tags_bson(t) else { return false };
            if !tags_are_valid(&tags) {
                throw(E::InvalidArgument, format!("{name} initialization requires \"tags\" array field to have zero or more documents"));
                return false;
            }
            if tags.len() > 5 && rp.mode == ReadMode::Primary {
                throw(E::InvalidArgument, format!("{name} initialization requires \"tags\" array field to not be present with \"primary\" mode"));
                return false;
            }
            rp.tags = tags.into();
        } else if !zval::is(t, sys::IS_NULL) {
            throw(E::InvalidArgument, format!("{name} initialization requires \"tags\" field to be array"));
            return false;
        }
    }
    if let Some(m) = hash::find(props, b"maxStalenessSeconds") {
        if zval::is(m, sys::IS_LONG) {
            let v = (*m).value.lval;
            if v != NO_MAX_STALENESS as i64 {
                if rp.mode == ReadMode::Primary {
                    throw(E::InvalidArgument, format!("{name} initialization requires \"maxStalenessSeconds\" field to not be present with \"primary\" mode"));
                    return false;
                }
                if v < SMALLEST_MAX_STALENESS_SECONDS {
                    throw(E::InvalidArgument, format!("{name} initialization requires \"maxStalenessSeconds\" integer field to be >= {SMALLEST_MAX_STALENESS_SECONDS}"));
                    return false;
                }
                if v > i32::MAX as i64 {
                    throw(E::InvalidArgument, format!("{name} initialization requires \"maxStalenessSeconds\" integer field to be <= {}", i32::MAX));
                    return false;
                }
            }
            rp.max_staleness_seconds = v;
        } else {
            throw(E::InvalidArgument, format!("{name} initialization requires \"maxStalenessSeconds\" field to be integer"));
            return false;
        }
    }
    if let Some(h) = hash::find(props, b"hedge") {
        if zval::is(h, sys::IS_ARRAY) || zval::is(h, sys::IS_OBJECT) {
            sys::php_error_docref(null_mut(), sys::E_DEPRECATED as i32, c"The \"hedge\" option is deprecated as of MongoDB 8.0 and will be removed in a future release".as_ptr());
            if rp.mode == ReadMode::Primary {
                throw(E::InvalidArgument, format!("{name} initialization requires \"hedge\" field to not be present with \"primary\" mode"));
                return false;
            }
            let Some(b) = crate::bson::encode::to_bson(h, crate::bson::encode::NONE) else { return false };
            rp.hedge = Some(b);
        } else if !zval::is(h, sys::IS_NULL) {
            throw(E::InvalidArgument, format!("{name} initialization requires \"hedge\" field to be an array or object"));
            return false;
        }
    }
    if !rp.is_valid() {
        throw(E::InvalidArgument, "Read preference is not valid");
        return false;
    }
    update_properties(o);
    true
}

pub unsafe extern "C" fn zim_MongoDB_Driver_ReadPreference___set_state(ex: Ex, rv: *mut sys::zval) {
    let mut a: *mut sys::zval = null_mut();
    if sys::zend_parse_parameters(call::num_args(ex), c"a".as_ptr(), &mut a) != 0 {
        return;
    }
    let z = obj::new_zval(CE);
    init_from_hash(z.value.obj, (*a).value.arr);
    zval::write(rv, z);
}

pub unsafe extern "C" fn zim_MongoDB_Driver_ReadPreference___unserialize(ex: Ex, _rv: *mut sys::zval) {
    let mut a: *mut sys::zval = null_mut();
    if sys::zend_parse_parameters(call::num_args(ex), c"a".as_ptr(), &mut a) != 0 {
        return;
    }
    init_from_hash(call::this(ex), (*a).value.arr);
}

#[inline]
unsafe fn me<'a>(ex: Ex) -> &'a mut ReadPreference {
    obj::data::<ReadPreference>(call::this(ex))
}

pub unsafe extern "C" fn zim_MongoDB_Driver_ReadPreference_getHedge(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        zval::write(rv, match me(ex).hedge.clone().filter(|h| h.len() > 5) {
            Some(h) => super::to_zval(&h, &Default::default()).unwrap_or_else(zval::null),
            None => zval::null(),
        });
    }
}
pub unsafe extern "C" fn zim_MongoDB_Driver_ReadPreference_getMaxStalenessSeconds(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        zval::write(rv, zval::long(me(ex).max_staleness_seconds));
    }
}
pub unsafe extern "C" fn zim_MongoDB_Driver_ReadPreference_getModeString(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        zval::write(rv, zval::string(mode_str(me(ex).mode).as_bytes()));
    }
}
pub unsafe extern "C" fn zim_MongoDB_Driver_ReadPreference_getTagSets(ex: Ex, rv: *mut sys::zval) {
    if call::none(ex) {
        let tags = me(ex).tags.clone();
        let tm = super::debug_typemap();
        let mut st = crate::bson::decode::State::new(&tm);
        st.is_visiting_array = true;
        zval::write(rv, if tags.len() > 5 { crate::bson::decode::to_zval(&tags, &st).unwrap_or_else(zval::null) } else { zval::empty_array() });
    }
}
pub unsafe extern "C" fn zim_MongoDB_Driver_ReadPreference_bsonSerialize(ex: Ex, rv: *mut sys::zval) {
    if !call::none(ex) {
        return;
    }
    let ht = hash::new(4);
    let props = sys::zend_std_get_properties(call::this(ex));
    let mut buf = String::new();
    hash::each(props, true, |k, v| {
        let hash::Key::Str(_) = k else { return true };
        let key = hash::key_bytes(k, &mut buf).to_vec();
        if zval::is(v, sys::IS_NULL) || (key == b"maxStalenessSeconds" && zval::is(v, sys::IS_LONG) && (*v).value.lval == NO_MAX_STALENESS as i64) {
            return true;
        }
        hash::str_update(ht, &key, zval::copy(v));
        true
    });
    let mut z = zval::arr(ht);
    sys::convert_to_object(&mut z);
    zval::write(rv, z);
}
