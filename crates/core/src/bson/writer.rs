//! BSON building (the bson_append_* family of libbson's bson.c), on a growable buffer.

use super::decimal128::Decimal128;
use super::iter::*;

/// A document (or array) under construction. Nested containers are begun/ended in place.
pub struct Writer {
    pub buf: Vec<u8>,
}

impl Default for Writer {
    fn default() -> Self {
        Self::new()
    }
}

impl Writer {
    pub fn new() -> Self {
        Self::with_capacity(128)
    }

    pub fn with_capacity(n: usize) -> Self {
        let mut buf = Vec::with_capacity(n.max(5));
        buf.extend_from_slice(&[5, 0, 0, 0]);
        Writer { buf }
    }

    /// Finishes: appends the terminator and patches the length.
    pub fn finish(mut self) -> Vec<u8> {
        self.buf.push(0);
        let n = self.buf.len() as u32;
        self.buf[0..4].copy_from_slice(&n.to_le_bytes());
        self.buf
    }

    /// A finished copy of the document so far (for lookups while building).
    pub fn buf_as_doc(&self) -> Vec<u8> {
        let mut d = self.buf.clone();
        d.push(0);
        let n = d.len() as u32;
        d[0..4].copy_from_slice(&n.to_le_bytes());
        d
    }

    pub fn len(&self) -> usize {
        self.buf.len() + 1
    }

    pub fn is_empty(&self) -> bool {
        self.buf.len() == 4
    }

    #[inline]
    fn head(&mut self, t: u8, key: &[u8]) {
        self.buf.push(t);
        self.buf.extend_from_slice(key);
        self.buf.push(0);
    }

    #[inline]
    fn str_body(&mut self, s: &[u8]) {
        self.buf.extend_from_slice(&((s.len() + 1) as u32).to_le_bytes());
        self.buf.extend_from_slice(s);
        self.buf.push(0);
    }

    pub fn double(&mut self, key: &[u8], v: f64) {
        self.head(T_DOUBLE, key);
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    pub fn utf8(&mut self, key: &[u8], s: &[u8]) {
        self.head(T_UTF8, key);
        self.str_body(s);
    }
    pub fn code(&mut self, key: &[u8], s: &[u8]) {
        self.head(T_CODE, key);
        self.str_body(s);
    }
    pub fn symbol(&mut self, key: &[u8], s: &[u8]) {
        self.head(T_SYMBOL, key);
        self.str_body(s);
    }
    /// Appends an already-encoded document/array.
    pub fn document(&mut self, key: &[u8], doc: &[u8]) {
        self.head(T_DOCUMENT, key);
        self.buf.extend_from_slice(doc);
    }
    pub fn array(&mut self, key: &[u8], doc: &[u8]) {
        self.head(T_ARRAY, key);
        self.buf.extend_from_slice(doc);
    }
    /// Begins a nested document/array: returns the position to pass to `end`.
    pub fn begin(&mut self, t: u8, key: &[u8]) -> usize {
        self.head(t, key);
        let p = self.buf.len();
        self.buf.extend_from_slice(&[0, 0, 0, 0]);
        p
    }
    pub fn end(&mut self, p: usize) {
        self.buf.push(0);
        let n = (self.buf.len() - p) as u32;
        self.buf[p..p + 4].copy_from_slice(&n.to_le_bytes());
    }
    pub fn binary(&mut self, key: &[u8], subtype: u8, data: &[u8]) {
        self.head(T_BINARY, key);
        if subtype == 2 {
            self.buf.extend_from_slice(&((data.len() + 4) as u32).to_le_bytes());
            self.buf.push(2);
            self.buf.extend_from_slice(&(data.len() as u32).to_le_bytes());
        } else {
            self.buf.extend_from_slice(&(data.len() as u32).to_le_bytes());
            self.buf.push(subtype);
        }
        self.buf.extend_from_slice(data);
    }
    pub fn undefined(&mut self, key: &[u8]) {
        self.head(T_UNDEFINED, key);
    }
    pub fn oid(&mut self, key: &[u8], oid: &[u8; 12]) {
        self.head(T_OID, key);
        self.buf.extend_from_slice(oid);
    }
    pub fn bool(&mut self, key: &[u8], v: bool) {
        self.head(T_BOOL, key);
        self.buf.push(v as u8);
    }
    pub fn date_time(&mut self, key: &[u8], ms: i64) {
        self.head(T_DATE_TIME, key);
        self.buf.extend_from_slice(&ms.to_le_bytes());
    }
    pub fn null(&mut self, key: &[u8]) {
        self.head(T_NULL, key);
    }
    /// `bson_append_regex`: the options are stored sorted, unknown ones dropped.
    pub fn regex(&mut self, key: &[u8], pattern: &[u8], options: &[u8]) {
        self.head(T_REGEX, key);
        self.buf.extend_from_slice(pattern);
        self.buf.push(0);
        append_regex_options_sorted(&mut self.buf, options);
        self.buf.push(0);
    }
    pub fn dbpointer(&mut self, key: &[u8], coll: &[u8], oid: &[u8; 12]) {
        self.head(T_DBPOINTER, key);
        self.str_body(coll);
        self.buf.extend_from_slice(oid);
    }
    pub fn code_with_scope(&mut self, key: &[u8], code: &[u8], scope: &[u8]) {
        self.head(T_CODEWSCOPE, key);
        let total = 4 + 4 + code.len() + 1 + scope.len();
        self.buf.extend_from_slice(&(total as u32).to_le_bytes());
        self.str_body(code);
        self.buf.extend_from_slice(scope);
    }
    pub fn int32(&mut self, key: &[u8], v: i32) {
        self.head(T_INT32, key);
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    /// BSON_APPEND_INT of phongo: int32 when it fits.
    pub fn int(&mut self, key: &[u8], v: i64) {
        if v > i32::MAX as i64 || v < i32::MIN as i64 {
            self.int64(key, v)
        } else {
            self.int32(key, v as i32)
        }
    }
    pub fn timestamp(&mut self, key: &[u8], ts: u32, inc: u32) {
        self.head(T_TIMESTAMP, key);
        self.buf.extend_from_slice(&inc.to_le_bytes());
        self.buf.extend_from_slice(&ts.to_le_bytes());
    }
    pub fn int64(&mut self, key: &[u8], v: i64) {
        self.head(T_INT64, key);
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    pub fn decimal128(&mut self, key: &[u8], d: &Decimal128) {
        self.head(T_DECIMAL128, key);
        self.buf.extend_from_slice(&d.to_le_bytes());
    }
    pub fn maxkey(&mut self, key: &[u8]) {
        self.head(T_MAXKEY, key);
    }
    pub fn minkey(&mut self, key: &[u8]) {
        self.head(T_MINKEY, key);
    }
    /// `bson_append_iter` with the iterator's key replaced by `key` (None keeps it).
    pub fn element_from(&mut self, key: Option<&[u8]>, it: &Iter) {
        match key {
            None => self.buf.extend_from_slice(it.element_bytes()),
            Some(k) => {
                let e = it.element_bytes();
                let klen = it.key().len();
                self.head(e[0], k);
                self.buf.extend_from_slice(&e[1 + klen + 1..]);
            }
        }
    }
    /// Appends any value.
    pub fn value(&mut self, key: &[u8], v: &Value) {
        match *v {
            Value::Double(d) => self.double(key, d),
            Value::Utf8(s) => self.utf8(key, s),
            Value::Document(d) => self.document(key, d),
            Value::Array(d) => self.array(key, d),
            Value::Binary(st, d) => self.binary(key, st, d),
            Value::Undefined => self.undefined(key),
            Value::Oid(o) => self.oid(key, o),
            Value::Bool(b) => self.bool(key, b),
            Value::DateTime(ms) => self.date_time(key, ms),
            Value::Null => self.null(key),
            Value::Regex(p, o) => {
                self.head(T_REGEX, key);
                self.buf.extend_from_slice(p);
                self.buf.push(0);
                self.buf.extend_from_slice(o);
                self.buf.push(0);
            }
            Value::DbPointer(c, o) => self.dbpointer(key, c, o),
            Value::Code(s) => self.code(key, s),
            Value::Symbol(s) => self.symbol(key, s),
            Value::CodeWScope(c, s) => self.code_with_scope(key, c, s),
            Value::Int32(i) => self.int32(key, i),
            Value::Timestamp(t, i) => self.timestamp(key, t, i),
            Value::Int64(i) => self.int64(key, i),
            Value::Decimal128(d) => self.decimal128(key, &d),
            Value::MaxKey => self.maxkey(key),
            Value::MinKey => self.minkey(key),
        }
    }
}

/// `mcommon_string_append_selected_chars(BSON_REGEX_OPTIONS_SORTED, options)`: the flags of
/// "ilmsux" present in `options`, in that order (others are dropped).
pub fn append_regex_options_sorted(out: &mut Vec<u8>, options: &[u8]) {
    for &c in b"ilmsux" {
        if options.contains(&c) {
            out.push(c);
        }
    }
}

/// The decimal string of an array index (bson_uint32_to_string).
#[inline]
pub fn index_key(i: usize, buf: &mut itoa_buf::Buf) -> &[u8] {
    buf.format(i)
}

pub mod itoa_buf {
    pub struct Buf([u8; 20], usize);
    impl Default for Buf {
        fn default() -> Self {
            Buf([0; 20], 20)
        }
    }
    impl Buf {
        pub fn format(&mut self, mut n: usize) -> &[u8] {
            let mut i = 20;
            loop {
                i -= 1;
                self.0[i] = b'0' + (n % 10) as u8;
                n /= 10;
                if n == 0 {
                    break;
                }
            }
            self.1 = i;
            &self.0[i..]
        }
    }
}
