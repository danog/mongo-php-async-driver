//! A port of libbson's iterator (bson-iter.c: `_bson_iter_next_internal`, the accessors and
//! `bson_iter_visit_all`), keeping its exact validation rules and error offsets.

use super::decimal128::Decimal128;
use super::utf8;

pub const T_EOD: u8 = 0x00;
pub const T_DOUBLE: u8 = 0x01;
pub const T_UTF8: u8 = 0x02;
pub const T_DOCUMENT: u8 = 0x03;
pub const T_ARRAY: u8 = 0x04;
pub const T_BINARY: u8 = 0x05;
pub const T_UNDEFINED: u8 = 0x06;
pub const T_OID: u8 = 0x07;
pub const T_BOOL: u8 = 0x08;
pub const T_DATE_TIME: u8 = 0x09;
pub const T_NULL: u8 = 0x0a;
pub const T_REGEX: u8 = 0x0b;
pub const T_DBPOINTER: u8 = 0x0c;
pub const T_CODE: u8 = 0x0d;
pub const T_SYMBOL: u8 = 0x0e;
pub const T_CODEWSCOPE: u8 = 0x0f;
pub const T_INT32: u8 = 0x10;
pub const T_TIMESTAMP: u8 = 0x11;
pub const T_INT64: u8 = 0x12;
pub const T_DECIMAL128: u8 = 0x13;
pub const T_MAXKEY: u8 = 0x7f;
pub const T_MINKEY: u8 = 0xff;

pub const MAX_SIZE: usize = i32::MAX as usize;

#[inline]
pub fn rd_u32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
#[inline]
pub fn rd_i64(b: &[u8], o: usize) -> i64 {
    i64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

/// `bson_init_static` validity: length >= 5, header == length, trailing NUL.
#[inline]
pub fn is_valid_static(data: &[u8]) -> bool {
    data.len() >= 5 && data.len() <= MAX_SIZE && rd_u32(data, 0) as usize == data.len() && data[data.len() - 1] == 0
}

/// `bson_empty`: a 5-byte document.
#[inline]
pub fn is_empty(data: &[u8]) -> bool {
    data.len() == 5
}

/// One BSON value, borrowed from the document. Strings exclude their trailing NUL; regex
/// pattern/options stop at their NUL (C strings).
#[derive(Clone, Copy, Debug)]
pub enum Value<'a> {
    Double(f64),
    Utf8(&'a [u8]),
    Document(&'a [u8]),
    Array(&'a [u8]),
    Binary(u8, &'a [u8]),
    Undefined,
    Oid(&'a [u8; 12]),
    Bool(bool),
    DateTime(i64),
    Null,
    Regex(&'a [u8], &'a [u8]),
    DbPointer(&'a [u8], &'a [u8; 12]),
    Code(&'a [u8]),
    Symbol(&'a [u8]),
    CodeWScope(&'a [u8], &'a [u8]),
    Int32(i32),
    Timestamp(u32, u32),
    Int64(i64),
    Decimal128(Decimal128),
    MaxKey,
    MinKey,
}

impl Value<'_> {
    /// bson_iter_as_bool
    pub fn as_bool(&self) -> bool {
        match *self {
            Value::Bool(b) => b,
            Value::Double(d) => d != 0.0,
            Value::Int64(i) => i != 0,
            Value::Int32(i) => i != 0,
            Value::Utf8(_) => true,
            Value::Null | Value::Undefined => false,
            _ => true,
        }
    }

    pub fn type_code(&self) -> u8 {
        match self {
            Value::Double(_) => T_DOUBLE,
            Value::Utf8(_) => T_UTF8,
            Value::Document(_) => T_DOCUMENT,
            Value::Array(_) => T_ARRAY,
            Value::Binary(..) => T_BINARY,
            Value::Undefined => T_UNDEFINED,
            Value::Oid(_) => T_OID,
            Value::Bool(_) => T_BOOL,
            Value::DateTime(_) => T_DATE_TIME,
            Value::Null => T_NULL,
            Value::Regex(..) => T_REGEX,
            Value::DbPointer(..) => T_DBPOINTER,
            Value::Code(_) => T_CODE,
            Value::Symbol(_) => T_SYMBOL,
            Value::CodeWScope(..) => T_CODEWSCOPE,
            Value::Int32(_) => T_INT32,
            Value::Timestamp(..) => T_TIMESTAMP,
            Value::Int64(_) => T_INT64,
            Value::Decimal128(_) => T_DECIMAL128,
            Value::MaxKey => T_MAXKEY,
            Value::MinKey => T_MINKEY,
        }
    }
}

fn cstr_at(b: &[u8], o: usize) -> &[u8] {
    let s = &b[o..];
    &s[..s.iter().position(|&c| c == 0).unwrap_or(s.len())]
}

/// `bson_iter_t` over a document's bytes (assumed to satisfy `is_valid_static`).
#[derive(Clone)]
pub struct Iter<'a> {
    raw: &'a [u8],
    live: bool,
    pub off: usize,
    key: usize,
    keylen: usize,
    d1: usize,
    d2: usize,
    d3: usize,
    d4: usize,
    next_off: usize,
    pub err_off: usize,
    pub unsupported: bool,
}

impl<'a> Iter<'a> {
    /// `bson_iter_init`: None when shorter than 5 bytes.
    pub fn new(raw: &'a [u8]) -> Option<Self> {
        if raw.len() < 5 {
            return None;
        }
        Some(Iter { raw, live: true, off: 0, key: 0, keylen: 0, d1: 0, d2: 0, d3: 0, d4: 0, next_off: 4, err_off: 0, unsupported: false })
    }

    pub fn raw(&self) -> &'a [u8] {
        self.raw
    }

    #[inline]
    fn invalid(&mut self) -> bool {
        self.live = false;
        self.next_off = 0;
        false
    }

    /// `_bson_iter_next_internal`: advances; false at the end or on corruption (err_off != 0).
    pub fn next(&mut self) -> bool {
        self.unsupported = false;
        if !self.live {
            return false;
        }
        let data = self.raw;
        let len = data.len();
        self.off = self.next_off;
        self.key = self.off + 1;
        self.d1 = 0;
        self.d2 = 0;
        self.d3 = 0;
        self.d4 = 0;
        let mut o = match super::swar::find_nul(data, self.key) {
            Some(z) => z + 1,
            None => return self.invalid(),
        };
        self.d1 = o;
        self.keylen = self.d1 - 1 - self.key;
        let ty = data[self.off];
        match ty {
            T_DATE_TIME | T_DOUBLE | T_INT64 | T_TIMESTAMP => self.next_off = o + 8,
            T_CODE | T_SYMBOL | T_UTF8 => {
                if o + 4 >= len {
                    self.err_off = o;
                    return self.invalid();
                }
                self.d2 = o + 4;
                let l = rd_u32(data, self.d1) as usize;
                if l > len - (o + 4) {
                    self.err_off = o;
                    return self.invalid();
                }
                self.next_off = o + 4 + l;
                if l == 0 || self.next_off >= len {
                    self.err_off = o;
                    return self.invalid();
                }
                if data[self.d2 + l - 1] != 0 {
                    self.err_off = o + 4 + l - 1;
                    return self.invalid();
                }
            }
            T_BINARY => {
                if len < 4 || o >= len - 4 {
                    self.err_off = o;
                    return self.invalid();
                }
                self.d2 = o + 4;
                self.d3 = o + 5;
                let l = rd_u32(data, self.d1) as usize;
                if l >= len - o - 4 {
                    self.err_off = o;
                    return self.invalid();
                }
                if data[self.d2] == 2 {
                    if l < 4 {
                        self.err_off = o;
                        return self.invalid();
                    }
                    let bin_len = rd_u32(data, self.d3) as i32 as i64;
                    if bin_len != l as i64 - 4 {
                        self.err_off = self.d3;
                        return self.invalid();
                    }
                }
                self.next_off = o + 5 + l;
            }
            T_ARRAY | T_DOCUMENT => {
                if len < 4 || o >= len - 4 {
                    self.err_off = o;
                    return self.invalid();
                }
                let l = rd_u32(data, self.d1) as usize;
                if l > len || l > len - o {
                    self.err_off = o;
                    return self.invalid();
                }
                self.next_off = o + l;
            }
            T_OID => self.next_off = o + 12,
            T_BOOL => {
                if self.d1 >= len {
                    self.err_off = o;
                    return self.invalid();
                }
                if data[self.d1] > 1 {
                    self.err_off = o;
                    return self.invalid();
                }
                self.next_off = o + 1;
            }
            T_REGEX => {
                let mut eor = false;
                while o < len {
                    if data[o] == 0 {
                        o += 1;
                        self.d2 = o;
                        eor = true;
                        break;
                    }
                    o += 1;
                }
                if !eor {
                    self.err_off = self.next_off;
                    return self.invalid();
                }
                let mut eoo = false;
                while o < len {
                    if data[o] == 0 {
                        eoo = true;
                        break;
                    }
                    o += 1;
                }
                if !eoo {
                    self.err_off = self.next_off;
                    return self.invalid();
                }
                self.next_off = o + 1;
            }
            T_DBPOINTER => {
                if len < 4 || o >= len - 4 {
                    self.err_off = o;
                    return self.invalid();
                }
                self.d2 = o + 4;
                let l = rd_u32(data, self.d1) as usize;
                if l == 0 || l > len - o - 4 {
                    self.err_off = o;
                    return self.invalid();
                }
                if data[o + l + 3] != 0 {
                    self.err_off = o + l + 3;
                    return self.invalid();
                }
                self.d3 = o + 4 + l;
                self.next_off = o + 4 + l + 12;
            }
            T_CODEWSCOPE => {
                if len < 19 || o >= len - 14 {
                    self.err_off = o;
                    return self.invalid();
                }
                self.d2 = o + 4;
                self.d3 = o + 8;
                let l1 = rd_u32(data, self.d1) as usize;
                if l1 < 14 || l1 >= len - o {
                    self.err_off = o;
                    return self.invalid();
                }
                self.next_off = o + l1;
                if self.next_off >= len {
                    self.err_off = o;
                    return self.invalid();
                }
                let l2 = rd_u32(data, self.d2) as usize;
                if l2 == 0 || l2 >= len - o - 4 - 4 {
                    self.err_off = o;
                    return self.invalid();
                }
                if o + 4 + 4 + l2 + 4 >= self.next_off {
                    self.err_off = o + 4;
                    return self.invalid();
                }
                self.d4 = o + 4 + 4 + l2;
                let doclen = rd_u32(data, self.d4) as usize;
                if o + 4 + 4 + l2 + doclen != self.next_off {
                    self.err_off = o + 4 + 4 + l2;
                    return self.invalid();
                }
            }
            T_INT32 => self.next_off = o + 4,
            T_DECIMAL128 => self.next_off = o + 16,
            T_MAXKEY | T_MINKEY | T_NULL | T_UNDEFINED => self.next_off = o,
            _ => {
                if ty != T_EOD {
                    self.unsupported = true;
                }
                self.err_off = o;
                return self.invalid();
            }
        }
        if self.next_off >= len {
            self.err_off = o;
            return self.invalid();
        }
        self.err_off = 0;
        true
    }

    #[inline]
    pub fn type_code(&self) -> u8 {
        self.raw[self.off]
    }

    /// The current key (bytes up to its NUL).
    #[inline]
    pub fn key(&self) -> &'a [u8] {
        &self.raw[self.key..self.key + self.keylen]
    }

    /// The current element's raw bytes (type, key, value), for copying.
    pub fn element_bytes(&self) -> &'a [u8] {
        &self.raw[self.off..self.next_off]
    }

    /// The current value (call after a successful `next`).
    pub fn value(&self) -> Value<'a> {
        let r = self.raw;
        let d1 = self.d1;
        match self.type_code() {
            T_DOUBLE => Value::Double(f64::from_bits(rd_i64(r, d1) as u64)),
            T_UTF8 => Value::Utf8(&r[self.d2..self.d2 + rd_u32(r, d1) as usize - 1]),
            T_CODE => Value::Code(&r[self.d2..self.d2 + rd_u32(r, d1) as usize - 1]),
            T_SYMBOL => Value::Symbol(&r[self.d2..self.d2 + rd_u32(r, d1) as usize - 1]),
            T_DOCUMENT => Value::Document(&r[d1..d1 + rd_u32(r, d1) as usize]),
            T_ARRAY => Value::Array(&r[d1..d1 + rd_u32(r, d1) as usize]),
            T_BINARY => {
                let st = r[self.d2];
                let l = rd_u32(r, d1) as usize;
                if st == 2 {
                    Value::Binary(st, &r[self.d3 + 4..self.d3 + l])
                } else {
                    Value::Binary(st, &r[self.d3..self.d3 + l])
                }
            }
            T_UNDEFINED => Value::Undefined,
            T_OID => Value::Oid(r[d1..d1 + 12].try_into().unwrap()),
            T_BOOL => Value::Bool(r[d1] != 0),
            T_DATE_TIME => Value::DateTime(rd_i64(r, d1)),
            T_NULL => Value::Null,
            T_REGEX => Value::Regex(cstr_at(r, d1), cstr_at(r, self.d2)),
            T_DBPOINTER => {
                let l = (rd_u32(r, d1) as usize).saturating_sub(1);
                Value::DbPointer(&r[self.d2..self.d2 + l], r[self.d3..self.d3 + 12].try_into().unwrap())
            }
            T_CODEWSCOPE => {
                let l = rd_u32(r, self.d2) as usize - 1;
                let sl = rd_u32(r, self.d4) as usize;
                Value::CodeWScope(&r[self.d3..self.d3 + l], &r[self.d4..self.d4 + sl])
            }
            T_INT32 => Value::Int32(rd_u32(r, d1) as i32),
            T_TIMESTAMP => Value::Timestamp(rd_u32(r, d1 + 4), rd_u32(r, d1)),
            T_INT64 => Value::Int64(rd_i64(r, d1)),
            T_DECIMAL128 => Value::Decimal128(Decimal128::from_le_bytes(r[d1..d1 + 16].try_into().unwrap())),
            T_MAXKEY => Value::MaxKey,
            _ => Value::MinKey,
        }
    }

    /// `bson_iter_value` of an initialized but never advanced iterator (all offsets 0): the
    /// type is the document length's low byte and the value is read from offset 0. ext-mongodb
    /// reaches this for `PackedArray::get()` with a negative index. Err(type) for a type
    /// `bson_iter_value` leaves without a value. Every read stays in bounds: a known type byte
    /// implies a length (mod 256) at least as long as the value read.
    pub fn unpositioned_value(raw: &'a [u8]) -> Result<Value<'a>, u8> {
        let t = raw[0];
        if !matches!(t, T_DOUBLE..=T_DECIMAL128 | T_MAXKEY | T_MINKEY) || raw.len() < 16 {
            return Err(t);
        }
        let it = Iter { raw, live: true, off: 0, key: 0, keylen: 0, d1: 0, d2: 0, d3: 0, d4: 0, next_off: 4, err_off: 0, unsupported: false };
        Ok(it.value())
    }

    /// `bson_iter_find_case` (ASCII case-insensitive, from the current position).
    pub fn find_case(&mut self, key: &[u8]) -> bool {
        while self.next() {
            if self.key().eq_ignore_ascii_case(key) {
                return true;
            }
        }
        false
    }

    /// `bson_iter_init_find` (case-sensitive, from the current position).
    pub fn find(&mut self, key: &[u8]) -> bool {
        while self.next() {
            if self.key() == key {
                return true;
            }
        }
        false
    }
}

/// The values of several keys in one pass over `doc`: for each key the first element with
/// that name (as `bson_iter_init_find` per key would return it, a scan stopping at corruption
/// included), or None.
pub fn find_many<'a, const N: usize>(doc: &'a [u8], keys: [&[u8]; N]) -> [Option<Value<'a>>; N] {
    let mut out = [None; N];
    let Some(mut it) = Iter::new(doc) else { return out };
    let mut left = N;
    while left > 0 && it.next() {
        let k = it.key();
        for (i, want) in keys.iter().enumerate() {
            if out[i].is_none() && k == *want {
                out[i] = Some(it.value());
                left -= 1;
            }
        }
    }
    out
}

/// The callbacks of `bson_iter_visit_all`. Returning true stops the visit.
pub trait Visitor<'a> {
    fn before(&mut self, _key: &'a [u8]) -> bool {
        false
    }
    fn visit(&mut self, it: &Iter<'a>, key: &'a [u8], v: Value<'a>) -> bool;
    fn corrupt(&mut self, _it: &Iter<'a>) {}
    fn unsupported(&mut self, _it: &Iter<'a>, _key: &'a [u8], _type_code: u8) {}
}

/// `bson_iter_visit_all`: the UTF-8 checks of keys and string values, and nested-document
/// validity, exactly as libbson performs them. Returns true when a visitor stopped the visit.
pub fn visit_all<'a, V: Visitor<'a>>(it: &mut Iter<'a>, v: &mut V) -> bool {
    let mut key: &'a [u8] = b"";
    while it.next() {
        key = it.key();
        if !key.is_empty() && !utf8::validate(key, false) {
            it.err_off = it.off;
            break;
        }
        if v.before(key) {
            return true;
        }
        let val = it.value();
        let ok = match val {
            Value::Utf8(s) | Value::Code(s) | Value::Symbol(s) => {
                if !utf8::validate(s, true) {
                    it.err_off = it.off;
                    return true;
                }
                true
            }
            Value::Regex(p, _) => {
                if !utf8::validate(p, true) {
                    it.err_off = it.off;
                    return true;
                }
                true
            }
            Value::DbPointer(c, _) => {
                if !utf8::validate(c, true) {
                    it.err_off = it.off;
                    return true;
                }
                true
            }
            Value::Document(d) | Value::Array(d) => {
                if !is_valid_static(d) {
                    it.err_off = it.off;
                    false
                } else {
                    true
                }
            }
            Value::CodeWScope(c, s) => {
                if !utf8::validate(c, true) {
                    it.err_off = it.off;
                    return true;
                }
                if !is_valid_static(s) {
                    it.err_off = it.off;
                    false
                } else {
                    true
                }
            }
            _ => true,
        };
        if ok && v.visit(it, key, val) {
            return true;
        }
    }
    if it.err_off != 0 {
        // libbson sets *key before the type switch: the unsupported element's own key
        if it.unsupported {
            key = it.key();
        }
        if it.unsupported && utf8::validate(key, false) {
            let t = it.raw[it.off];
            v.unsupported(it, key, t);
            return false;
        }
        v.corrupt(it);
    }
    false
}

/// `bson_reader_new_from_data` + `bson_reader_read`: the first document (None when none can be
/// read) and whether it exhausts the input (a second read reaches EOF).
pub fn read_first(data: &[u8]) -> Option<(&[u8], bool)> {
    if data.len() <= 4 {
        return None;
    }
    let blen = rd_u32(data, 0) as i32;
    if blen < 5 || blen as usize > data.len() {
        return None;
    }
    let doc = &data[..blen as usize];
    if !is_valid_static(doc) {
        return None;
    }
    Some((doc, doc.len() == data.len()))
}
