//! HashTable helpers: creation, insertion (symtable semantics for numeric-string keys),
//! iteration over packed and hash layouts (ZEND_HASH_FOREACH_*), lookups.

use super::{sys, zstr, zval};

const HASH_FLAG_PACKED: u32 = 1 << 2;
const HASH_FLAG_HAS_EMPTY_IND: u32 = 1 << 5;
const _: u32 = HASH_FLAG_HAS_EMPTY_IND;

#[inline]
pub unsafe fn new(n: u32) -> *mut sys::HashTable {
    sys::_zend_new_array(n)
}

#[inline]
pub unsafe fn flags(ht: *const sys::HashTable) -> u32 {
    (*ht).u.flags
}

#[inline]
pub unsafe fn count(ht: *const sys::HashTable) -> u32 {
    (*ht).nNumOfElements
}

/// zend_hash_release
pub unsafe fn release(ht: *mut sys::HashTable) {
    if (*ht).gc.u.type_info & sys::IS_ARRAY_IMMUTABLE == 0 {
        (*ht).gc.refcount -= 1;
        if (*ht).gc.refcount == 0 {
            sys::zend_array_destroy(ht);
        }
    }
}

/// add_next_index_zval (takes ownership of `v`)
#[inline]
pub unsafe fn push(ht: *mut sys::HashTable, v: sys::zval) {
    let mut v = zval::into_raw(v);
    if sys::zend_hash_next_index_insert(ht, &mut *v).is_null() {
        // not inserted (next index taken): the value is still ours
        std::mem::ManuallyDrop::drop(&mut v);
    }
}

/// ZEND_HANDLE_NUMERIC_STR: the integer a key is the canonical decimal form of.
pub fn numeric_key(k: &[u8]) -> Option<i64> {
    let (neg, d) = match k.first() {
        Some(b'-') => (true, &k[1..]),
        Some(_) => (false, k),
        None => return None,
    };
    if d.is_empty() || d.len() > 19 || !d.iter().all(u8::is_ascii_digit) {
        return None;
    }
    if d[0] == b'0' && (d.len() > 1 || neg) {
        return None;
    }
    let mut v: i128 = 0;
    for &c in d {
        v = v * 10 + (c - b'0') as i128;
    }
    let v = if neg { -v } else { v };
    if v > i64::MAX as i128 || v < i64::MIN as i128 {
        return None;
    }
    Some(v as i64)
}

/// zend_symtable_str_update (add_assoc_zval_ex): takes ownership of `v`.
#[inline]
pub unsafe fn sym_update(ht: *mut sys::HashTable, k: &[u8], v: sys::zval) {
    let mut v = zval::into_raw(v);
    match numeric_key(k) {
        Some(i) => {
            sys::zend_hash_index_update(ht, i as u64, &mut *v);
        }
        None => {
            sys::zend_hash_str_update(ht, k.as_ptr() as *const _, k.len(), &mut *v);
        }
    }
}

/// zend_hash_str_update: takes ownership of `v`.
#[inline]
pub unsafe fn str_update(ht: *mut sys::HashTable, k: &[u8], v: sys::zval) {
    let mut v = zval::into_raw(v);
    sys::zend_hash_str_update(ht, k.as_ptr() as *const _, k.len(), &mut *v);
}

/// zend_hash_index_update: takes ownership of `v`.
#[inline]
pub unsafe fn index_update(ht: *mut sys::HashTable, i: u64, v: sys::zval) {
    let mut v = zval::into_raw(v);
    sys::zend_hash_index_update(ht, i, &mut *v);
}

#[inline]
pub unsafe fn find(ht: *const sys::HashTable, k: &[u8]) -> Option<*mut sys::zval> {
    let z = sys::zend_hash_str_find(ht, k.as_ptr() as *const _, k.len());
    (!z.is_null()).then_some(z)
}

#[inline]
pub unsafe fn find_index(ht: *const sys::HashTable, i: u64) -> Option<*mut sys::zval> {
    let z = sys::zend_hash_index_find(ht, i);
    (!z.is_null()).then_some(z)
}

/// zend_symtable_str_find
pub unsafe fn sym_find(ht: *const sys::HashTable, k: &[u8]) -> Option<*mut sys::zval> {
    match numeric_key(k) {
        Some(i) => find_index(ht, i as u64),
        None => find(ht, k),
    }
}

#[derive(Clone, Copy)]
pub enum Key {
    Index(u64),
    Str(*mut sys::zend_string),
}

/// ZEND_HASH_FOREACH_KEY_VAL(_IND when `ind`): f returns false to stop.
pub unsafe fn each(ht: *const sys::HashTable, ind: bool, mut f: impl FnMut(Key, *mut sys::zval) -> bool) {
    let n = (*ht).nNumUsed as usize;
    if flags(ht) & HASH_FLAG_PACKED != 0 {
        let base = (*ht).__bindgen_anon_1.arPacked;
        for i in 0..n {
            let z = base.add(i);
            if zval::is(z, sys::IS_UNDEF) {
                continue;
            }
            if !f(Key::Index(i as u64), z) {
                return;
            }
        }
    } else {
        let base = (*ht).__bindgen_anon_1.arData;
        for i in 0..n {
            let b = base.add(i);
            let mut z: *mut sys::zval = &mut (*b).val;
            if zval::is(z, sys::IS_UNDEF) {
                continue;
            }
            if ind && zval::is(z, sys::IS_INDIRECT) {
                z = (*z).value.zv;
                if zval::is(z, sys::IS_UNDEF) {
                    continue;
                }
            }
            let k = if (*b).key.is_null() { Key::Index((*b).h) } else { Key::Str((*b).key) };
            if !f(k, z) {
                return;
            }
        }
    }
}

/// Takes a reference on a table that code which may run user code walks (GC_TRY_ADDREF ...
/// zend_array_release, as zend_std_get_properties_for's callers do): the table cannot be freed
/// under the walk, and PHP separates a shared table before changing it (copy-on-write for arrays,
/// zend_array_dup in zend_std_write_property/unset_property for property tables) instead of
/// changing it in place, so the walk may keep its pointers into it. Returns what `unhold` needs
/// (0: null or immutable, not counted). Explicit calls, not a Drop guard: cheaper per container.
#[inline]
pub unsafe fn hold(ht: *mut sys::HashTable) -> u32 {
    if ht.is_null() || (*ht).gc.u.type_info & sys::IS_ARRAY_IMMUTABLE != 0 {
        return 0;
    }
    (*ht).gc.refcount += 1;
    (*ht).gc.refcount
}

/// Drops the reference `hold` took.
#[inline]
pub unsafe fn unhold(ht: *mut sys::HashTable, held: u32) {
    if held != 0 {
        release_held(ht.cast(), held, || zval::arr(ht));
    }
}

/// `hold` for an object (kept alive: its declared properties are slots of it).
#[inline]
pub unsafe fn hold_obj(o: *mut sys::zend_object) -> u32 {
    (*o).gc.refcount += 1;
    (*o).gc.refcount
}

#[inline]
pub unsafe fn unhold_obj(o: *mut sys::zend_object, held: u32) {
    release_held(o.cast(), held, || zval::obj(o));
}

/// A refcount still at what holding it made it (`held`: the other references are as they were)
/// is just decremented, which leaves the value as it was before (zval_ptr_dtor would also buffer
/// it as a possible GC root); otherwise it is released as zval_ptr_dtor does (freed when it was
/// the last reference, else a possible GC root).
#[inline]
unsafe fn release_held(c: *mut sys::zend_refcounted_h, held: u32, owned: impl FnOnce() -> sys::zval) {
    if (*c).refcount == held {
        (*c).refcount -= 1;
    } else {
        drop(owned());
    }
}

/// The property table of an object whose get_properties handler is not the standard one
/// (ArrayObject, SimpleXMLElement, DateInterval, ...): such a handler may change its table in
/// place even while it is held, so a walk that runs user code walks a copy (zend_array_dup).
#[inline]
pub unsafe fn custom_properties(o: *mut sys::zend_object) -> bool {
    (*(*o).handlers).get_properties.map(|f| f as *const () as usize) != Some(sys::zend_std_get_properties as *const () as usize)
}

/// zend_array_dup: a copy of `ht` (refcount 1; release with `release`).
#[inline]
pub unsafe fn dup(ht: *mut sys::HashTable) -> *mut sys::HashTable {
    sys::zend_array_dup(ht)
}

/// zend_array_is_list
pub unsafe fn is_list(ht: *const sys::HashTable) -> bool {
    if count(ht) == 0 {
        return true;
    }
    if flags(ht) & HASH_FLAG_PACKED != 0 && (*ht).nNumUsed == (*ht).nNumOfElements {
        return true;
    }
    let mut expected = 0u64;
    let mut ok = true;
    each(ht, false, |k, _| {
        match k {
            Key::Index(i) if i == expected => expected += 1,
            _ => ok = false,
        }
        ok
    });
    ok
}

/// GC_PROTECT_RECURSION helpers (phongo_zend_hash_apply_protection_begin/end).
pub unsafe fn protect_begin(ht: *mut sys::HashTable) -> bool {
    if ht.is_null() {
        return true;
    }
    if (*ht).gc.u.type_info & sys::GC_PROTECTED != 0 {
        return false;
    }
    if (*ht).gc.u.type_info & sys::GC_IMMUTABLE == 0 {
        (*ht).gc.u.type_info |= sys::GC_PROTECTED;
    }
    true
}

pub unsafe fn protect_end(ht: *mut sys::HashTable) {
    if ht.is_null() {
        return;
    }
    if (*ht).gc.u.type_info & sys::GC_IMMUTABLE == 0 {
        (*ht).gc.u.type_info &= !sys::GC_PROTECTED;
    }
}

/// The key as bytes (an integer key formatted in decimal).
pub unsafe fn key_bytes(k: Key, buf: &mut String) -> &[u8] {
    match k {
        Key::Str(s) => zstr::bytes(s),
        Key::Index(i) => {
            buf.clear();
            use std::fmt::Write;
            let _ = write!(buf, "{}", i as i64);
            buf.as_bytes()
        }
    }
}
