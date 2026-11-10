//! Extended JSON output: a port of libbson's common-json.c (`bson_as_canonical_extended_json`,
//! `bson_as_relaxed_extended_json`, `bson_as_legacy_extended_json`, `bson_as_json_with_opts`),
//! byte-for-byte, including its corruption handling.

use super::fmt;
use super::iter::{self, Iter, Value, Visitor};
use super::oid;
use super::swar;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Legacy,
    Canonical,
    Relaxed,
}

/// BSON_MAX_RECURSION
pub const MAX_DEPTH: u32 = 200;

/// The document (or, with `is_array`, the array) as Extended JSON; None when the BSON is corrupt.
pub fn to_json(doc: &[u8], mode: Mode, is_array: bool) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(doc.len() * 2);
    let ok = if is_array { append_array(&mut out, doc, mode, MAX_DEPTH) } else { append_document(&mut out, doc, mode, MAX_DEPTH) };
    ok.then_some(out)
}

fn append_container(out: &mut Vec<u8>, doc: &[u8], mode: Mode, depth: u32, has_keys: bool, delim: (&str, &str, &str, &str)) -> bool {
    if iter::is_empty(doc) {
        out.extend_from_slice(delim.0.as_bytes());
        true
    } else if depth == 0 {
        out.extend_from_slice(delim.3.as_bytes());
        true
    } else {
        out.extend_from_slice(delim.1.as_bytes());
        let r = append_values(out, doc, mode, has_keys, depth - 1);
        out.extend_from_slice(delim.2.as_bytes());
        r
    }
}

pub fn append_document(out: &mut Vec<u8>, doc: &[u8], mode: Mode, depth: u32) -> bool {
    append_container(out, doc, mode, depth, true, ("{ }", "{ ", " }", "{ ... }"))
}

pub fn append_array(out: &mut Vec<u8>, doc: &[u8], mode: Mode, depth: u32) -> bool {
    append_container(out, doc, mode, depth, false, ("[ ]", "[ ", " ]", "[ ... ]"))
}

struct State<'o> {
    out: &'o mut Vec<u8>,
    mode: Mode,
    depth: u32,
    has_keys: bool,
    not_first: bool,
    corrupt: bool,
}

fn append_values(out: &mut Vec<u8>, doc: &[u8], mode: Mode, has_keys: bool, depth: u32) -> bool {
    let Some(mut it) = Iter::new(doc) else { return false };
    let mut st = State { out, mode, depth, has_keys, not_first: false, corrupt: false };
    iter::visit_all(&mut it, &mut st);
    it.err_off == 0 && !st.corrupt
}

impl<'a> Visitor<'a> for State<'_> {
    fn before(&mut self, key: &'a [u8]) -> bool {
        if self.not_first {
            self.out.extend_from_slice(b", ");
        } else {
            self.not_first = true;
        }
        if self.has_keys {
            self.out.push(b'"');
            if !escape(self.out, key, false) {
                return true;
            }
            self.out.extend_from_slice(b"\" : ");
        }
        false
    }

    fn corrupt(&mut self, _it: &Iter<'a>) {
        self.corrupt = true;
    }

    fn visit(&mut self, _it: &Iter<'a>, _key: &'a [u8], v: Value<'a>) -> bool {
        let o = &mut *self.out;
        let mode = self.mode;
        let ext = mode != Mode::Legacy;
        match v {
            Value::Utf8(s) => {
                o.push(b'"');
                escape(o, s, true);
                o.push(b'"');
            }
            Value::Int32(i) => {
                if mode == Mode::Canonical {
                    o.extend_from_slice(b"{ \"$numberInt\" : \"");
                    fmt::push_i64(o, i as i64);
                    o.extend_from_slice(b"\" }");
                } else {
                    fmt::push_i64(o, i as i64);
                }
            }
            Value::Int64(i) => {
                if mode == Mode::Canonical {
                    o.extend_from_slice(b"{ \"$numberLong\" : \"");
                    fmt::push_i64(o, i);
                    o.extend_from_slice(b"\" }");
                } else {
                    fmt::push_i64(o, i);
                }
            }
            Value::Decimal128(d) => {
                o.extend_from_slice(b"{ \"$numberDecimal\" : \"");
                o.extend_from_slice(d.to_string().as_bytes());
                o.extend_from_slice(b"\" }");
            }
            Value::Double(d) => append_double(o, d, mode),
            Value::Undefined => o.extend_from_slice(b"{ \"$undefined\" : true }"),
            Value::Null => o.extend_from_slice(b"null"),
            Value::Oid(id) => append_oid(o, id),
            Value::Binary(st, data) => {
                if ext {
                    o.extend_from_slice(b"{ \"$binary\" : { \"base64\" : \"");
                    fmt::base64_encode_into(o, data);
                    o.extend_from_slice(b"\", \"subType\" : \"");
                    o.extend_from_slice(&hex2(st));
                    o.extend_from_slice(b"\" } }");
                } else {
                    o.extend_from_slice(b"{ \"$binary\" : \"");
                    fmt::base64_encode_into(o, data);
                    o.extend_from_slice(b"\", \"$type\" : \"");
                    o.extend_from_slice(&hex2(st));
                    o.extend_from_slice(b"\" }");
                }
            }
            Value::Bool(b) => o.extend_from_slice(if b { b"true" } else { b"false" }),
            Value::DateTime(ms) => {
                const Y10K: i64 = 253402300800000;
                if mode == Mode::Canonical || (mode == Mode::Relaxed && !(0..Y10K).contains(&ms)) {
                    o.extend_from_slice(b"{ \"$date\" : { \"$numberLong\" : \"");
                    fmt::push_i64(o, ms);
                    o.extend_from_slice(b"\" } }");
                } else if mode == Mode::Relaxed {
                    o.extend_from_slice(b"{ \"$date\" : \"");
                    fmt::iso8601_into(o, ms);
                    o.extend_from_slice(b"\" }");
                } else {
                    o.extend_from_slice(b"{ \"$date\" : ");
                    fmt::push_i64(o, ms);
                    o.extend_from_slice(b" }");
                }
            }
            Value::Regex(p, opts) => {
                o.extend_from_slice(if ext { b"{ \"$regularExpression\" : { \"pattern\" : \"" as &[u8] } else { b"{ \"$regex\" : \"" });
                if !escape(o, p, false) {
                    return true;
                }
                o.extend_from_slice(if ext { b"\", \"options\" : \"" as &[u8] } else { b"\", \"$options\" : \"" });
                for &c in b"ilmsux" {
                    if opts.contains(&c) {
                        o.push(c);
                    }
                }
                o.extend_from_slice(if ext { b"\" } }" as &[u8] } else { b"\" }" });
            }
            Value::Timestamp(t, i) => {
                o.extend_from_slice(b"{ \"$timestamp\" : { \"t\" : ");
                fmt::push_u64(o, t as u64);
                o.extend_from_slice(b", \"i\" : ");
                fmt::push_u64(o, i as u64);
                o.extend_from_slice(b" } }");
            }
            Value::DbPointer(c, id) => {
                o.extend_from_slice(if ext { b"{ \"$dbPointer\" : { \"$ref\" : \"" as &[u8] } else { b"{ \"$ref\" : \"" });
                if !escape(o, c, false) {
                    return true;
                }
                o.push(b'"');
                if ext {
                    o.extend_from_slice(b", \"$id\" : ");
                    append_oid(o, id);
                    o.extend_from_slice(b" } }");
                } else {
                    o.extend_from_slice(b", \"$id\" : \"");
                    o.extend_from_slice(&oid::to_hex(id));
                    o.extend_from_slice(b"\" }");
                }
            }
            Value::MinKey => o.extend_from_slice(b"{ \"$minKey\" : 1 }"),
            Value::MaxKey => o.extend_from_slice(b"{ \"$maxKey\" : 1 }"),
            Value::Code(c) => {
                o.extend_from_slice(b"{ \"$code\" : \"");
                escape(o, c, true);
                o.extend_from_slice(b"\" }");
            }
            Value::Symbol(s) => {
                if ext {
                    o.extend_from_slice(b"{ \"$symbol\" : \"");
                    escape(o, s, true);
                    o.extend_from_slice(b"\" }");
                } else {
                    o.push(b'"');
                    escape(o, s, true);
                    o.push(b'"');
                }
            }
            Value::CodeWScope(c, scope) => {
                o.extend_from_slice(b"{ \"$code\" : \"");
                escape(o, c, true);
                o.extend_from_slice(b"\", \"$scope\" : ");
                let ok = append_document(o, scope, mode, self.depth);
                o.extend_from_slice(b" }");
                if !ok {
                    self.corrupt = true;
                    return true;
                }
            }
            Value::Document(d) => {
                if !append_document(o, d, mode, self.depth) {
                    self.corrupt = true;
                    return true;
                }
            }
            Value::Array(d) => {
                if !append_array(o, d, mode, self.depth) {
                    self.corrupt = true;
                    return true;
                }
            }
        }
        false
    }
}

/// `%02x`
fn hex2(b: u8) -> [u8; 2] {
    const H: &[u8; 16] = b"0123456789abcdef";
    [H[(b >> 4) as usize], H[(b & 15) as usize]]
}

fn append_oid(o: &mut Vec<u8>, id: &[u8; 12]) {
    o.extend_from_slice(b"{ \"$oid\" : \"");
    o.extend_from_slice(&oid::to_hex(id));
    o.extend_from_slice(b"\" }");
}

/// `mcommon_json_append_value_double`.
pub fn append_double(o: &mut Vec<u8>, v: f64, mode: Mode) {
    let legacy = mode == Mode::Legacy || (mode == Mode::Relaxed && v.is_finite());
    if !legacy {
        o.extend_from_slice(b"{ \"$numberDouble\" : \"");
    }
    if !legacy && v.is_nan() {
        o.extend_from_slice(b"NaN");
    } else if !legacy && v.is_infinite() {
        o.extend_from_slice(if v > 0.0 { b"Infinity" as &[u8] } else { b"-Infinity" });
    } else {
        let start = o.len();
        fmt::fmt_g_into(o, v, 20);
        if o[start..].iter().all(|&c| c.is_ascii_digit() || c == b'-') {
            o.extend_from_slice(b".0");
        }
    }
    if !legacy {
        o.extend_from_slice(b"\" }");
    }
}

#[inline]
fn special(b: u8) -> bool {
    b < 0x20 || b == b'"' || b == b'\\' || b == 0xc0
}

/// `mcommon_json_append_escaped`: false when a NUL (or C0 80) is met and `allow_nul` is false.
pub fn escape(o: &mut Vec<u8>, s: &[u8], allow_nul: bool) -> bool {
    let mut i = 0;
    let n = s.len();
    while i < n {
        let start = i;
        // eight bytes at a time over runs without a byte to escape
        while i + 8 <= n {
            let w = swar::load(s, i);
            if swar::lt_bytes(w, 0x20) | swar::eq_bytes(w, b'"') | swar::eq_bytes(w, b'\\') | swar::eq_bytes(w, 0xc0) != 0 {
                break;
            }
            i += 8;
        }
        while i < n && !special(s[i]) {
            i += 1;
        }
        o.extend_from_slice(&s[start..i]);
        if i >= n {
            break;
        }
        let c = s[i];
        match c {
            b'"' => o.extend_from_slice(b"\\\""),
            b'\\' => o.extend_from_slice(b"\\\\"),
            8 => o.extend_from_slice(b"\\b"),
            12 => o.extend_from_slice(b"\\f"),
            b'\n' => o.extend_from_slice(b"\\n"),
            b'\r' => o.extend_from_slice(b"\\r"),
            b'\t' => o.extend_from_slice(b"\\t"),
            0 => {
                if !allow_nul {
                    return false;
                }
                o.extend_from_slice(b"\\u0000");
            }
            0xc0 => {
                if i + 1 < n && s[i + 1] == 0x80 {
                    if !allow_nul {
                        return false;
                    }
                    o.extend_from_slice(b"\\u0000");
                    i += 1;
                } else {
                    o.push(0xc0);
                }
            }
            _ => o.extend_from_slice(format!("\\u{:04x}", c).as_bytes()),
        }
        i += 1;
    }
    true
}
