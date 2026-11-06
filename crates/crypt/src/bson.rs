//! BSON helpers on top of mongo-core's reader and writer.

use mongo_core::bson::iter::{is_valid_static, Iter, Value};
use mongo_core::bson::writer::Writer;

pub const SUBTYPE_UUID: u8 = 4;
pub const SUBTYPE_ENCRYPTED: u8 = 6;

/// `bson_validate(.., BSON_VALIDATE_NONE)`: the structure of every element, recursively
pub fn validate(doc: &[u8]) -> bool {
    validate_depth(doc, 0)
}

fn validate_depth(doc: &[u8], depth: u32) -> bool {
    if depth > 100 || !is_valid_static(doc) {
        return false;
    }
    let Some(mut it) = Iter::new(doc) else { return false };
    while it.next() {
        match it.value() {
            Value::Document(d) | Value::Array(d) => {
                if !validate_depth(d, depth + 1) {
                    return false;
                }
            }
            Value::CodeWScope(_, s) => {
                if !validate_depth(s, depth + 1) {
                    return false;
                }
            }
            _ => {}
        }
    }
    it.err_off == 0 && !it.unsupported
}

/// The value bytes of the current element (after its type byte and key)
pub fn value_bytes<'a>(it: &Iter<'a>) -> &'a [u8] {
    let e = it.element_bytes();
    &e[1 + it.key().len() + 1..]
}

/// `_mongocrypt_buffer_to_bson_value`: wraps `{"": <type> <bytes>}` and reads the value back;
/// None when that is not valid BSON
pub fn wrap_value(type_code: u8, bytes: &[u8]) -> Option<Vec<u8>> {
    let len = 4 + 1 + 1 + bytes.len() + 1;
    let mut d = Vec::with_capacity(len);
    d.extend_from_slice(&(len as u32).to_le_bytes());
    d.push(type_code);
    d.push(0);
    d.extend_from_slice(bytes);
    d.push(0);
    if !validate(&d) {
        return None;
    }
    let mut it = Iter::new(&d)?;
    if !it.next() || !it.key().is_empty() {
        return None;
    }
    Some(d)
}

/// Appends the single element of a `wrap_value` document under `key`
pub fn append_wrapped(w: &mut Writer, key: &[u8], wrapped: &[u8]) {
    let t = wrapped[4];
    let bytes = &wrapped[6..wrapped.len() - 1];
    raw_element(w, t, key, bytes);
}

/// Appends an element from its type code and value bytes
pub fn raw_element(w: &mut Writer, t: u8, key: &[u8], bytes: &[u8]) {
    w.buf.push(t);
    w.buf.extend_from_slice(key);
    w.buf.push(0);
    w.buf.extend_from_slice(bytes);
}

/// UUID binary (subtype 4, 16 bytes)
pub fn as_uuid(v: &Value) -> Option<[u8; 16]> {
    match v {
        Value::Binary(SUBTYPE_UUID, d) if d.len() == 16 => Some((*d).try_into().expect("16 bytes")),
        _ => None,
    }
}

pub fn doc1(key: &[u8], f: impl FnOnce(&mut Writer, &[u8])) -> Vec<u8> {
    let mut w = Writer::new();
    f(&mut w, key);
    w.finish()
}
