//! Extended JSON input: `bson_new_from_json` (libbson's bson-json.c on top of jsonsl). The JSON
//! text is parsed into a tree first; the tree is then converted with bson-json.c's rules (which
//! keys start a type wrapper, which value kinds each wrapper key accepts, the checks done when a
//! wrapper ends) and its error messages.

use super::decimal128::Decimal128;
use super::fmt::base64_decode;
use super::iter::{self, Iter, Value};
use super::oid;
use super::utf8;
use super::writer::Writer;

/// jsonsl's level limit (libbson's STACK_MAX).
const MAX_LEVELS: usize = 100;

enum J {
    Str(Vec<u8>),
    /// an integer token: negative?, magnitude (None: does not fit u64), text
    Int(bool, Option<u64>, Vec<u8>),
    Float(Vec<u8>),
    Bool(bool),
    Null,
    Arr(Vec<J>),
    Obj(Vec<(Vec<u8>, J)>),
}

struct P<'a> {
    s: &'a [u8],
    i: usize,
}

type R<T> = Result<T, String>;

impl P<'_> {
    fn err(&self, what: &str) -> String {
        if self.i >= self.s.len() {
            return "Incomplete JSON".into();
        }
        format!("Got parse error at \"{}\", position {}: \"{}\"", self.s[self.i] as char, self.i, what)
    }
    fn ws(&mut self) {
        while self.i < self.s.len() && matches!(self.s[self.i], b' ' | b'\t' | b'\n' | b'\r') {
            self.i += 1;
        }
    }
    fn value(&mut self, depth: usize) -> R<J> {
        self.ws();
        if self.i >= self.s.len() {
            return Err("Incomplete JSON".into());
        }
        match self.s[self.i] {
            b'{' => {
                if depth + 1 >= MAX_LEVELS {
                    return Err(self.err("LEVELS_EXCEEDED"));
                }
                self.i += 1;
                let mut v = Vec::new();
                self.ws();
                if self.i < self.s.len() && self.s[self.i] == b'}' {
                    self.i += 1;
                    return Ok(J::Obj(v));
                }
                loop {
                    self.ws();
                    if self.i >= self.s.len() {
                        return Err("Incomplete JSON".into());
                    }
                    if self.s[self.i] != b'"' {
                        return Err(self.err("EXPECTED_HKEY"));
                    }
                    let k = self.string()?;
                    self.ws();
                    if self.i >= self.s.len() {
                        return Err("Incomplete JSON".into());
                    }
                    if self.s[self.i] != b':' {
                        return Err(self.err("MISSING_TOKEN"));
                    }
                    self.i += 1;
                    let val = self.value(depth + 1)?;
                    v.push((k, val));
                    self.ws();
                    if self.i >= self.s.len() {
                        return Err("Incomplete JSON".into());
                    }
                    match self.s[self.i] {
                        b',' => self.i += 1,
                        b'}' => {
                            self.i += 1;
                            return Ok(J::Obj(v));
                        }
                        _ => return Err(self.err("MISSING_TOKEN")),
                    }
                }
            }
            b'[' => {
                if depth + 1 >= MAX_LEVELS {
                    return Err(self.err("LEVELS_EXCEEDED"));
                }
                self.i += 1;
                let mut v = Vec::new();
                self.ws();
                if self.i < self.s.len() && self.s[self.i] == b']' {
                    self.i += 1;
                    return Ok(J::Arr(v));
                }
                loop {
                    v.push(self.value(depth + 1)?);
                    self.ws();
                    if self.i >= self.s.len() {
                        return Err("Incomplete JSON".into());
                    }
                    match self.s[self.i] {
                        b',' => self.i += 1,
                        b']' => {
                            self.i += 1;
                            return Ok(J::Arr(v));
                        }
                        _ => return Err(self.err("MISSING_TOKEN")),
                    }
                }
            }
            b'"' => Ok(J::Str(self.string()?)),
            b't' => self.lit(b"true", J::Bool(true)),
            b'f' => self.lit(b"false", J::Bool(false)),
            b'n' => self.lit(b"null", J::Null),
            b'-' | b'0'..=b'9' => self.number(),
            _ => Err(self.err("STRAY_TOKEN")),
        }
    }
    fn lit(&mut self, w: &[u8], v: J) -> R<J> {
        for &c in w {
            if self.i >= self.s.len() {
                return Err("Incomplete JSON".into());
            }
            if self.s[self.i] != c {
                return Err(self.err("SPECIAL_EXPECTED"));
            }
            self.i += 1;
        }
        if self.i < self.s.len() && self.s[self.i].is_ascii_alphanumeric() {
            return Err(self.err("SPECIAL_EXPECTED"));
        }
        Ok(v)
    }
    fn number(&mut self) -> R<J> {
        let st = self.i;
        let mut float = false;
        while self.i < self.s.len() {
            match self.s[self.i] {
                b'0'..=b'9' | b'-' | b'+' => {}
                b'.' | b'e' | b'E' => float = true,
                _ => break,
            }
            self.i += 1;
        }
        if self.i < self.s.len() && self.s[self.i].is_ascii_alphabetic() {
            return Err(self.err("INVALID_NUMBER"));
        }
        let t = self.s[st..self.i].to_vec();
        if float {
            return Ok(J::Float(t));
        }
        let (neg, digits) = if t[0] == b'-' { (true, &t[1..]) } else { (false, &t[..]) };
        if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
            self.i = st;
            return Err(self.err("INVALID_NUMBER"));
        }
        let mut m: Option<u64> = Some(0);
        for &d in digits {
            m = m.and_then(|m| m.checked_mul(10)).and_then(|m| m.checked_add((d - b'0') as u64));
        }
        Ok(J::Int(neg, m, t))
    }
    fn hex4(&mut self) -> R<u32> {
        if self.i + 4 > self.s.len() {
            return Err("Incomplete JSON".into());
        }
        let h = std::str::from_utf8(&self.s[self.i..self.i + 4]).ok().and_then(|h| u32::from_str_radix(h, 16).ok());
        match h {
            Some(v) => {
                self.i += 4;
                Ok(v)
            }
            None => Err(self.err("UESCAPE_TOOSHORT")),
        }
    }
    fn string(&mut self) -> R<Vec<u8>> {
        self.i += 1;
        let mut out = Vec::new();
        loop {
            let st = self.i;
            while self.i < self.s.len() && self.s[self.i] != b'"' && self.s[self.i] != b'\\' {
                self.i += 1;
            }
            out.extend_from_slice(&self.s[st..self.i]);
            if self.i >= self.s.len() {
                return Err("Incomplete JSON".into());
            }
            if self.s[self.i] == b'"' {
                self.i += 1;
                return Ok(out);
            }
            self.i += 1;
            if self.i >= self.s.len() {
                return Err("Incomplete JSON".into());
            }
            let c = self.s[self.i];
            self.i += 1;
            match c {
                b'"' | b'\\' | b'/' => out.push(c),
                b'b' => out.push(8),
                b'f' => out.push(12),
                b'n' => out.push(b'\n'),
                b'r' => out.push(b'\r'),
                b't' => out.push(b'\t'),
                b'u' => {
                    let mut cp = self.hex4()?;
                    if (0xd800..0xdc00).contains(&cp) {
                        if self.s.get(self.i) == Some(&b'\\') && self.s.get(self.i + 1) == Some(&b'u') {
                            self.i += 2;
                            let lo = self.hex4()?;
                            if !(0xdc00..0xe000).contains(&lo) {
                                self.i -= 6;
                                return Err(self.err("INVALID_CODEPOINT"));
                            }
                            cp = 0x10000 + ((cp - 0xd800) << 10) + (lo - 0xdc00);
                        } else {
                            return Err(self.err("INVALID_CODEPOINT"));
                        }
                    } else if (0xdc00..0xe000).contains(&cp) {
                        return Err(self.err("INVALID_CODEPOINT"));
                    }
                    let ch = char::from_u32(cp).unwrap();
                    let mut b = [0u8; 4];
                    out.extend_from_slice(ch.encode_utf8(&mut b).as_bytes());
                }
                _ => {
                    self.i -= 1;
                    return Err(self.err("ESCAPE_INVALID"));
                }
            }
        }
    }
}

const KNOWN: &[&[u8]] = &[
    b"$binary", b"$code", b"$date", b"$dbPointer", b"$maxKey", b"$minKey", b"$numberDecimal", b"$numberDouble", b"$numberInt", b"$numberLong",
    b"$oid", b"$options", b"$regex", b"$regularExpression", b"$scope", b"$symbol", b"$timestamp", b"$type", b"$undefined", b"$uuid",
];

fn type_name(t: u8) -> &'static str {
    match t {
        0 => "end of document",
        1 => "double",
        2 => "utf-8",
        3 => "document",
        4 => "array",
        5 => "binary",
        6 => "undefined",
        7 => "objectid",
        8 => "bool",
        9 => "datetime",
        10 => "null",
        11 => "regex",
        12 => "dbpointer",
        13 => "code",
        14 => "symbol",
        15 => "code with scope",
        16 => "int32",
        17 => "timestamp",
        18 => "int64",
        19 => "decimal128",
        0x7f => "maxkey",
        0xff => "minkey",
        _ => "",
    }
}

fn lossy(b: &[u8]) -> String {
    let b = &b[..b.iter().position(|&c| c == 0).unwrap_or(b.len())];
    String::from_utf8_lossy(b).into_owned()
}

/// `bson_new_from_json`: the document, or the error message.
pub fn from_json(input: &[u8]) -> Result<Vec<u8>, String> {
    let mut p = P { s: input, i: 0 };
    p.ws();
    if p.i >= input.len() {
        return Err("Empty JSON string".into());
    }
    let root = match input[p.i] {
        b'{' | b'[' => p.value(0)?,
        _ => {
            let v = p.value(0)?;
            let what = match v {
                J::Str(_) => "string",
                J::Int(..) => "integer",
                J::Float(_) => "double",
                J::Bool(_) => "boolean",
                _ => "null",
            };
            return Err(format!("Invalid read of {} without key in state REGULAR", what));
        }
    };
    let mut w = Writer::with_capacity(input.len());
    match root {
        J::Obj(o) => doc(&mut w, &o)?,
        J::Arr(a) => arr(&mut w, &a)?,
        _ => unreachable!(),
    }
    Ok(w.finish())
}

fn doc(w: &mut Writer, o: &[(Vec<u8>, J)]) -> R<()> {
    for (k, v) in o {
        if !utf8::validate(k, false) {
            return Err("invalid bytes in UTF8 string".into());
        }
        value(w, k, v)?;
    }
    Ok(())
}

fn arr(w: &mut Writer, a: &[J]) -> R<()> {
    for (i, v) in a.iter().enumerate() {
        value(w, i.to_string().as_bytes(), v)?;
    }
    Ok(())
}

fn value(w: &mut Writer, k: &[u8], v: &J) -> R<()> {
    match v {
        J::Str(s) => {
            if !utf8::validate(s, true) {
                return Err("invalid bytes in UTF8 string".into());
            }
            w.utf8(k, s)
        }
        J::Int(neg, m, t) => match (neg, m) {
            (false, Some(m)) if *m <= i32::MAX as u64 => w.int32(k, *m as i32),
            (false, Some(m)) if *m <= i64::MAX as u64 => w.int64(k, *m as i64),
            (true, Some(m)) if *m <= i32::MAX as u64 + 1 => w.int32(k, (*m as i64).wrapping_neg() as i32),
            (true, Some(m)) if *m <= i64::MAX as u64 + 1 => w.int64(k, (*m as i64).wrapping_neg()),
            (_, Some(m)) => return Err(format!("Number \"{}{}\" is out of range", if *neg { "-" } else { "" }, m)),
            (_, None) => return Err(format!("Number \"{}\" is out of range", lossy(t))),
        },
        J::Float(t) => w.double(k, json_double(t)?),
        J::Bool(b) => w.bool(k, *b),
        J::Null => w.null(k),
        J::Arr(a) => {
            let p = w.begin(iter::T_ARRAY, k);
            arr(w, a)?;
            w.end(p);
        }
        J::Obj(o) => {
            if let Some((k0, _)) = o.first() {
                if KNOWN.contains(&k0.as_slice()) {
                    return special(w, k, o);
                }
            }
            let p = w.begin(iter::T_DOCUMENT, k);
            doc(w, o)?;
            w.end(p);
        }
    }
    Ok(())
}

/// A JSON float token, as jsonsl/strtod read it.
fn json_double(t: &[u8]) -> R<f64> {
    let s = std::str::from_utf8(t).unwrap_or("");
    match s.parse::<f64>() {
        Ok(d) if d.is_infinite() => Err(format!("Number \"{}\" is out of range", s)),
        Ok(d) => Ok(d),
        Err(_) => Err(format!("Number \"{}\" is invalid", s)),
    }
}

/// strtod on a $numberDouble string: the longest numeric prefix (glibc also reads inf/nan).
fn strtod(s: &[u8]) -> R<f64> {
    let txt = std::str::from_utf8(s).unwrap_or("");
    let t = txt.trim_start();
    let lower = t.to_ascii_lowercase();
    let (sign, rest) = match lower.as_bytes().first() {
        Some(b'-') => (-1.0, &lower[1..]),
        Some(b'+') => (1.0, &lower[1..]),
        _ => (1.0, &lower[..]),
    };
    if rest.starts_with("inf") {
        return Ok(sign * f64::INFINITY);
    }
    if rest.starts_with("nan") {
        return Ok(if sign < 0.0 { -f64::NAN } else { f64::NAN });
    }
    let b = t.as_bytes();
    let mut i = if matches!(b.first(), Some(b'-') | Some(b'+')) { 1 } else { 0 };
    let ds = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    let mut nd = i - ds;
    if i < b.len() && b[i] == b'.' {
        i += 1;
        let f = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        nd += i - f;
    }
    if nd == 0 {
        return Ok(0.0);
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        let mut j = i + 1;
        if j < b.len() && (b[j] == b'-' || b[j] == b'+') {
            j += 1;
        }
        let es = j;
        while j < b.len() && b[j].is_ascii_digit() {
            j += 1;
        }
        if j > es {
            i = j;
        }
    }
    let d: f64 = t[..i].parse().unwrap_or(0.0);
    if d.is_infinite() {
        return Err(format!("Number \"{}\" is out of range", txt));
    }
    Ok(d)
}

/// `bson_ascii_strtoll(val, &end, 10)` with the whole-string and range checks.
fn strtoll(s: &[u8]) -> R<i64> {
    let txt = lossy(s);
    let t = txt.trim_start();
    let b = t.as_bytes();
    let (neg, mut i) = match b.first() {
        Some(b'-') => (true, 1),
        Some(b'+') => (false, 1),
        _ => (false, 0),
    };
    let st = i;
    let mut v: i128 = 0;
    while i < b.len() && b[i].is_ascii_digit() {
        v = (v * 10 + (b[i] - b'0') as i128).min(i64::MAX as i128 + 2);
        i += 1;
    }
    if i == st || i != b.len() || s.contains(&0) {
        return Err(format!("Number \"{}\" is invalid", txt));
    }
    let v = if neg { -v } else { v };
    if v > i64::MAX as i128 || v < i64::MIN as i128 {
        return Err(format!("Number \"{}\" is out of range", txt));
    }
    Ok(v as i64)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// `_bson_iso8601_date_parse`.
pub fn iso8601_parse(s: &[u8]) -> R<i64> {
    let shown = lossy(s);
    let e = |m: &str| Err(format!("Could not parse \"{}\" as date: {}", shown, m));
    let n = s.len();
    let mut ptr = 0usize;
    // get_tok: (start, len, found)
    let tok = |ptr: &mut usize, terms: &[u8]| -> (usize, usize, bool) {
        let st = *ptr;
        while *ptr < n {
            let c = s[*ptr];
            *ptr += 1;
            if terms.contains(&c) {
                return (st, *ptr - st - 1, true);
            }
        }
        (st, *ptr - st, false)
    };
    let at = |i: usize| -> u8 { if i < n { s[i] } else { b'"' } };
    let (y, m, d, h, mi);
    let mut ok = true;
    y = tok(&mut ptr, b"-");
    ok &= y.2;
    m = if ok { tok(&mut ptr, b"-") } else { (0, 0, false) };
    ok &= m.2;
    d = if ok { tok(&mut ptr, b"T") } else { (0, 0, false) };
    ok &= d.2;
    h = if ok { tok(&mut ptr, b":") } else { (0, 0, false) };
    ok &= h.2;
    mi = if ok { tok(&mut ptr, b":+-Z") } else { (0, 0, false) };
    ok &= mi.2;
    if !ok {
        return e("use ISO8601 format yyyy-mm-ddThh:mm plus timezone, either \"Z\" or like \"+0500\" or like \"+05:00\"");
    }
    let mut sec = (0usize, 0usize);
    let mut ms = (0usize, 0usize);
    if at(mi.0 + mi.1) == b':' {
        if n - ptr < 2 {
            return e("reached end of date while looking for seconds");
        }
        let t = tok(&mut ptr, b".+-Z");
        sec = (t.0, t.1);
        if sec.1 == 0 {
            return e("minute ends in \":\" seconds is required");
        }
    }
    if sec.1 > 0 && at(sec.0 + sec.1) == b'.' {
        if n - ptr < 2 {
            return e("reached end of date while looking for milliseconds");
        }
        let t = tok(&mut ptr, b"+-Z");
        ms = (t.0, t.1);
        if ms.1 == 0 {
            return e("seconds ends in \".\", milliseconds is required");
        }
    }
    ptr -= 1;
    let tz = (ptr, n - ptr);
    let digits = |st: usize, l: usize| st + l <= n && s[st..st + l].iter().all(u8::is_ascii_digit);
    let num = |st: usize, l: usize, want: i32, min: i64, max: i64| -> Option<i64> {
        if (want >= 0 && l != want as usize) || !digits(st, l) {
            return None;
        }
        let v = s[st..st + l].iter().fold(0i64, |a, &c| a * 10 + (c - b'0') as i64);
        (min..=max).contains(&v).then_some(v)
    };
    let Some(year) = num(y.0, y.1, 4, -9999, 9999) else { return e("year must be an integer") };
    let Some(month) = num(m.0, m.1, 2, 1, 12) else { return e("month must be an integer") };
    let Some(day) = num(d.0, d.1, 2, 1, 31) else { return e("day must be an integer") };
    let Some(hour) = num(h.0, h.1, 2, 0, 23) else { return e("hour must be an integer") };
    let Some(min) = num(mi.0, mi.1, 2, 0, 59) else { return e("minute must be an integer") };
    let mut secs = 0;
    if sec.1 > 0 {
        match num(sec.0, sec.1, 2, 0, 60) {
            Some(v) => secs = v,
            None => return e("seconds must be an integer"),
        }
    }
    let mut tz_adj = 0i64;
    if tz.1 > 0 {
        let t0 = s[tz.0];
        if t0 == b'Z' && tz.1 == 1 {
        } else if t0 == b'+' || t0 == b'-' {
            if (tz.1 != 5 || !digits(tz.0 + 1, 4)) && (tz.1 != 6 || !digits(tz.0 + 1, 2) || s[tz.0 + 3] != b':' || !digits(tz.0 + 4, 2)) {
                return e("could not parse timezone");
            }
            let Some(tzh) = num(tz.0 + 1, 2, -1, -23, 23) else { return e("timezone hour must be at most 23") };
            let off = if s[tz.0 + 3] == b':' { 1 } else { 0 };
            let Some(tzm) = num(tz.0 + 3 + off, 2, -1, 0, 59) else { return e("timezone minute must be at most 59") };
            tz_adj = (if t0 == b'-' { 1 } else { -1 }) * (tzm * 60 + tzh * 3600);
            if !(tz_adj > -86400 && tz_adj < 86400) {
                return e("timezone offset must be less than 24 hours");
            }
        } else {
            return e("timezone is required");
        }
    }
    let mut millis = 0i64;
    if ms.1 > 0 {
        if ms.1 > 3 || !digits(ms.0, ms.1) {
            return e("milliseconds must be an integer");
        }
        millis = s[ms.0..ms.0 + ms.1].iter().fold(0i64, |a, &c| a * 10 + (c - b'0') as i64);
        millis *= [0, 100, 10, 1][ms.1];
    }
    let days = days_from_civil(year, month, day);
    let t = days * 86400 + hour * 3600 + min * 60 + secs;
    Ok(1000 * t + millis + tz_adj * 1000)
}

#[derive(Default)]
struct Spec {
    ty: u8,
    // binary
    legacy: bool,
    has_binary: bool,
    has_subtype: bool,
    subtype: u8,
    bin: Vec<u8>,
    // regex
    pattern: Option<Vec<u8>>,
    options: Option<Vec<u8>>,
    oid: [u8; 12],
    date: i64,
    i32v: i32,
    i64v: i64,
    dbl: f64,
    dec: Option<Decimal128>,
    code: Option<Vec<u8>>,
    scope: Option<Vec<u8>>,
    ts: (Option<u32>, Option<u32>),
    dbptr: Option<(Vec<u8>, [u8; 12])>,
    symbol: Option<Vec<u8>>,
    /// a wrapper that must end after its object value ($timestamp, $regularExpression, ...)
    closed: bool,
}

fn state_name(k: &[u8]) -> &'static str {
    match k {
        b"$regex" => "REGEX",
        b"$options" => "OPTIONS",
        b"pattern" => "REGULAR_EXPRESSION_PATTERN",
        b"options" => "REGULAR_EXPRESSION_OPTIONS",
        b"$code" => "CODE",
        b"$scope" => "SCOPE",
        b"$oid" => "OID",
        b"$binary" | b"base64" => "BINARY",
        b"$type" | b"subType" => "TYPE",
        b"$date" => "DATE",
        b"t" => "TIMESTAMP_T",
        b"i" => "TIMESTAMP_I",
        b"$undefined" => "UNDEFINED",
        b"$minKey" => "MINKEY",
        b"$maxKey" => "MAXKEY",
        b"$numberInt" => "INT32",
        b"$numberLong" => "INT64",
        b"$numberDouble" => "DOUBLE",
        b"$numberDecimal" => "DECIMAL128",
        b"$dbPointer" => "DBPOINTER",
        b"$symbol" => "SYMBOL",
        _ => "UUID",
    }
}

fn kind_of(v: &J) -> &'static str {
    match v {
        J::Str(_) => "string",
        J::Int(..) => "integer",
        J::Float(_) => "double",
        J::Bool(_) => "boolean",
        J::Null => "null",
        J::Arr(_) => "[",
        J::Obj(_) => "{",
    }
}

/// The error for a value of the wrong JSON kind for wrapper key `k` (state `rs`).
fn wrong(k: &[u8], v: &J, ty: u8, rs: &str) -> String {
    match v {
        J::Int(neg, m, _) => {
            let n = format!("{}{}", if *neg { "-" } else { "" }, m.unwrap_or(u64::MAX));
            match k {
                b"$numberInt" | b"$numberLong" => format!("Invalid state for integer read: {}, expected number as quoted string like \"123\"", state_name(k)),
                _ if rs == "IN_BSON_TYPE" || rs == "IN_BSON_TYPE_TIMESTAMP_VALUES" => format!("Unexpected integer {} in type \"{}\"", n, type_name(ty)),
                _ => format!("Unexpected integer {} in state \"{}\"", n, rs),
            }
        }
        J::Obj(_) => format!("Unexpected nested object value for \"{}\" key", lossy(k)),
        J::Arr(_) => format!("Invalid read of \"[\" in state \"{}\"", rs),
        J::Str(s) => format!("Invalid input string \"{}\", looking for {}", lossy(s), state_name(k)),
        _ => format!("Invalid read of {} in state {}", kind_of(v), rs),
    }
}

fn str_of<'v>(k: &[u8], v: &'v J, ty: u8, rs: &str, allow_null: bool) -> R<&'v [u8]> {
    match v {
        J::Str(s) => {
            if !utf8::validate(s, allow_null) {
                return Err("invalid bytes in UTF8 string".into());
            }
            Ok(s)
        }
        _ => Err(wrong(k, v, ty, rs)),
    }
}

fn special(w: &mut Writer, key: &[u8], o: &[(Vec<u8>, J)]) -> R<()> {
    let mut sp = Spec::default();
    let check = |sp: &mut Spec, k: &[u8], t: u8| -> R<()> {
        if sp.ty != 0 && sp.ty != t {
            return Err(format!("Invalid key \"{}\".  Looking for values for type \"{}\", got \"{}\"", lossy(k), type_name(sp.ty), type_name(t)));
        }
        sp.ty = t;
        Ok(())
    };
    const RS: &str = "IN_BSON_TYPE";
    for (idx, (k, v)) in o.iter().enumerate() {
        if !utf8::validate(k, false) {
            return Err("invalid bytes in UTF8 string".into());
        }
        if sp.closed {
            return Err(format!("Invalid read of {} in state {}", kind_of(v), "IN_BSON_TYPE_TIMESTAMP_ENDMAP"));
        }
        let ks = k.as_slice();
        match ks {
            b"$regex" | b"$options" => {
                check(&mut sp, k, iter::T_REGEX)?;
                let s = str_of(k, v, sp.ty, RS, false)?.to_vec();
                sp.legacy = true;
                if ks == b"$regex" {
                    sp.pattern = Some(s);
                } else {
                    sp.options = Some(s);
                }
            }
            b"$oid" => {
                check(&mut sp, k, iter::T_OID)?;
                let s = str_of(k, v, sp.ty, RS, true)?;
                sp.oid = match (s.len() == 24).then(|| oid::from_hex(s)).flatten() {
                    Some(o) => o,
                    None if s.len() == 24 => oid_lenient(s),
                    None => return Err(format!("Invalid input string \"{}\", looking for OID", lossy(s))),
                };
            }
            b"$binary" => {
                check(&mut sp, k, iter::T_BINARY)?;
                match v {
                    J::Obj(inner) => {
                        sp.legacy = false;
                        binary_values(&mut sp, inner)?;
                        sp.closed = true;
                    }
                    _ => {
                        let s = str_of(k, v, sp.ty, RS, true)?;
                        sp.legacy = true;
                        sp.has_binary = true;
                        sp.bin = base64_decode(s).ok_or_else(|| format!("Invalid input string \"{}\", looking for base64-encoded binary", lossy(s)))?;
                    }
                }
            }
            b"$type" => {
                check(&mut sp, k, iter::T_BINARY)?;
                match v {
                    J::Obj(_) if idx == 0 => return regular_doc(w, key, o),
                    J::Str(s) => {
                        if !utf8::validate(s, true) {
                            return Err("invalid bytes in UTF8 string".into());
                        }
                        sp.legacy = true;
                        sp.has_subtype = true;
                        match scan_hex2(s) {
                            Some(t) => sp.subtype = t,
                            None if !sp.has_binary => return regular_doc(w, key, o),
                            None => return Err(format!("Invalid input string \"{}\", looking for binary subtype", lossy(s))),
                        }
                    }
                    _ => return Err(wrong(k, v, sp.ty, RS)),
                }
            }
            b"$uuid" => {
                check(&mut sp, k, iter::T_BINARY)?;
                let s = str_of(k, v, sp.ty, RS, true)?;
                sp.has_binary = true;
                sp.has_subtype = true;
                sp.subtype = 4;
                sp.bin = parse_uuid(s).ok_or_else(|| format!("Invalid input string \"{}\", looking for a dash-separated UUID string", lossy(s)))?;
            }
            b"$date" => {
                check(&mut sp, k, iter::T_DATE_TIME)?;
                match v {
                    J::Int(neg, Some(m), _) if *m <= i64::MAX as u64 + 1 => {
                        sp.date = if *neg { (*m as i64).wrapping_neg() } else { *m as i64 };
                    }
                    J::Int(neg, m, t) => {
                        return Err(match m {
                            Some(m) => format!("Number \"{}{}\" is out of range", if *neg { "-" } else { "" }, m),
                            None => format!("Number \"{}\" is out of range", lossy(t)),
                        })
                    }
                    J::Str(s) => {
                        if !utf8::validate(s, true) {
                            return Err("invalid bytes in UTF8 string".into());
                        }
                        sp.date = iso8601_parse(s)?;
                    }
                    J::Obj(inner) => {
                        for (ik, iv) in inner {
                            if ik.as_slice() != b"$numberLong" {
                                return Err(format!("Invalid key \"{}\".  Looking for values for type \"{}\"", lossy(ik), type_name(sp.ty)));
                            }
                            let s = str_of(ik, iv, sp.ty, "IN_BSON_TYPE_DATE_NUMBERLONG", true)?;
                            sp.date = strtoll(s)?;
                        }
                        if inner.is_empty() {
                            return Err("Invalid MongoDB extended JSON".into());
                        }
                        sp.closed = true;
                    }
                    _ => return Err(wrong(k, v, sp.ty, RS)),
                }
            }
            b"$undefined" => {
                check(&mut sp, k, iter::T_UNDEFINED)?;
                if !matches!(v, J::Bool(_)) {
                    return Err(wrong(k, v, sp.ty, RS));
                }
            }
            b"$minKey" | b"$maxKey" => {
                let mk = ks == b"$minKey";
                check(&mut sp, k, if mk { iter::T_MINKEY } else { iter::T_MAXKEY })?;
                match v {
                    J::Int(true, m, _) => return Err(format!("Invalid MinKey value: \"-{}\"", m.unwrap_or(0))),
                    J::Int(false, Some(1), _) => {}
                    J::Int(false, m, _) => return Err(format!("Invalid MinKey value: \"{}\"", m.unwrap_or(0))),
                    _ => return Err(wrong(k, v, sp.ty, RS)),
                }
            }
            b"$numberInt" => {
                check(&mut sp, k, iter::T_INT32)?;
                let s = str_of(k, v, sp.ty, RS, true)?;
                let n = strtoll(s)?;
                if n < i32::MIN as i64 || n > i32::MAX as i64 {
                    return Err(format!("Invalid input string \"{}\", looking for INT32", lossy(s)));
                }
                sp.i32v = n as i32;
            }
            b"$numberLong" => {
                check(&mut sp, k, iter::T_INT64)?;
                sp.i64v = strtoll(str_of(k, v, sp.ty, RS, true)?)?;
            }
            b"$numberDouble" => {
                check(&mut sp, k, iter::T_DOUBLE)?;
                sp.dbl = strtod(str_of(k, v, sp.ty, RS, true)?)?;
            }
            b"$numberDecimal" => {
                check(&mut sp, k, iter::T_DECIMAL128)?;
                let s = str_of(k, v, sp.ty, RS, true)?;
                let nul = s.iter().position(|&c| c == 0).unwrap_or(s.len());
                sp.dec = Some(Decimal128::parse(&s[..nul]).ok_or_else(|| format!("Invalid input string \"{}\", looking for DECIMAL128", lossy(s)))?);
            }
            b"$symbol" => {
                check(&mut sp, k, iter::T_SYMBOL)?;
                sp.symbol = Some(str_of(k, v, sp.ty, RS, true)?.to_vec());
            }
            b"$timestamp" => {
                check(&mut sp, k, iter::T_TIMESTAMP)?;
                let J::Obj(inner) = v else { return Err(timestamp_wrong(v)) };
                for (ik, iv) in inner {
                    let which = match ik.as_slice() {
                        b"t" => 0,
                        b"i" => 1,
                        _ => return Err(format!("Invalid key \"{}\".  Looking for values for type \"{}\"", lossy(ik), type_name(sp.ty))),
                    };
                    let n = match iv {
                        J::Int(true, m, _) => return Err(format!("Invalid timestamp value: \"-{}\"", m.unwrap_or(0))),
                        J::Int(false, Some(m), _) if *m <= i64::MAX as u64 => *m as u32,
                        J::Int(false, m, t) => {
                            return Err(match m {
                                Some(m) => format!("Number \"{}\" is out of range", m),
                                None => format!("Number \"{}\" is out of range", lossy(t)),
                            })
                        }
                        _ => return Err(wrong(ik, iv, sp.ty, "IN_BSON_TYPE_TIMESTAMP_VALUES")),
                    };
                    if which == 0 {
                        sp.ts.0 = Some(n);
                    } else {
                        sp.ts.1 = Some(n);
                    }
                }
                if inner.is_empty() {
                    return Err("Invalid MongoDB extended JSON".into());
                }
                sp.closed = true;
            }
            b"$regularExpression" => {
                check(&mut sp, k, iter::T_REGEX)?;
                let J::Obj(inner) = v else { return Err(wrong(k, v, sp.ty, "IN_BSON_TYPE_REGEX_STARTMAP")) };
                for (ik, iv) in inner {
                    match ik.as_slice() {
                        b"pattern" => sp.pattern = Some(str_of(ik, iv, sp.ty, "IN_BSON_TYPE_REGEX_VALUES", false)?.to_vec()),
                        b"options" => sp.options = Some(str_of(ik, iv, sp.ty, "IN_BSON_TYPE_REGEX_VALUES", false)?.to_vec()),
                        _ => return Err(format!("Invalid key \"{}\".  Looking for values for type \"{}\"", lossy(ik), type_name(sp.ty))),
                    }
                }
                if inner.is_empty() {
                    return Err("Invalid MongoDB extended JSON".into());
                }
                sp.closed = true;
            }
            b"$dbPointer" => {
                check(&mut sp, k, iter::T_DBPOINTER)?;
                let J::Obj(inner) = v else { return Err(wrong(k, v, sp.ty, "IN_BSON_TYPE_DBPOINTER_STARTMAP")) };
                if inner.is_empty() {
                    return Err("Empty $dbPointer".into());
                }
                let mut sub = Writer::new();
                doc(&mut sub, inner)?;
                let d = sub.finish();
                let mut it = Iter::new(&d).unwrap();
                let (mut ns, mut id) = (None, None);
                while it.next() {
                    match (it.key(), it.value()) {
                        (b"$id", Value::Oid(o)) => id = Some(*o),
                        (b"$id", _) => return Err("$dbPointer.$id must be like {\"$oid\": ...\"}".into()),
                        (b"$ref", Value::Utf8(s)) => ns = Some(s.to_vec()),
                        (b"$ref", _) => return Err("$dbPointer.$ref must be a string like \"db.collection\"".into()),
                        (k2, _) => return Err(format!("$dbPointer contains invalid key: \"{}\"", lossy(k2))),
                    }
                }
                match (ns, id) {
                    (Some(n), Some(i)) => sp.dbptr = Some((n, i)),
                    _ => return Err("$dbPointer requires both $id and $ref".into()),
                }
                sp.closed = true;
            }
            b"$code" => {
                if sp.ty != iter::T_CODEWSCOPE {
                    check(&mut sp, k, iter::T_CODE)?;
                }
                sp.code = Some(str_of(k, v, sp.ty, RS, true)?.to_vec());
            }
            b"$scope" => {
                if sp.ty != iter::T_CODE {
                    check(&mut sp, k, iter::T_CODEWSCOPE)?;
                }
                sp.ty = iter::T_CODEWSCOPE;
                let J::Obj(inner) = v else { return Err(wrong(k, v, sp.ty, "IN_BSON_TYPE_SCOPE_STARTMAP")) };
                let mut sub = Writer::new();
                doc(&mut sub, inner)?;
                sp.scope = Some(sub.finish());
            }
            _ => return Err(format!("Invalid key \"{}\".  Looking for values for type \"{}\"", lossy(k), type_name(sp.ty))),
        }
    }
    // end of the wrapper object
    match sp.ty {
        iter::T_REGEX => {
            if sp.legacy {
                if sp.pattern.is_none() {
                    return Err("Missing \"$regex\" after \"$options\"".into());
                }
            } else if sp.pattern.is_none() {
                return Err("Missing \"pattern\" after \"options\" in regular expression".into());
            } else if sp.options.is_none() {
                return Err("Missing \"options\" after \"pattern\" in regular expression".into());
            }
            w.regex(key, sp.pattern.as_deref().unwrap_or(b""), sp.options.as_deref().unwrap_or(b""));
        }
        iter::T_CODE | iter::T_CODEWSCOPE => {
            let Some(code) = sp.code else { return Err("Missing $code after $scope".into()) };
            match sp.scope {
                Some(s) => w.code_with_scope(key, &code, &s),
                None => w.code(key, &code),
            }
        }
        iter::T_OID => w.oid(key, &sp.oid),
        iter::T_BINARY => {
            if !sp.legacy && !sp.closed && !(sp.has_binary && sp.has_subtype) {
                // $uuid only
            }
            if sp.legacy {
                if !sp.has_binary {
                    return Err("Missing \"$binary\" after \"$type\" reading type \"binary\"".into());
                } else if !sp.has_subtype {
                    return Err("Missing \"$type\" after \"$binary\" reading type \"binary\"".into());
                }
            }
            w.binary(key, sp.subtype, &sp.bin);
        }
        iter::T_DATE_TIME => w.date_time(key, sp.date),
        iter::T_UNDEFINED => w.undefined(key),
        iter::T_MINKEY => w.minkey(key),
        iter::T_MAXKEY => w.maxkey(key),
        iter::T_INT32 => w.int32(key, sp.i32v),
        iter::T_INT64 => w.int64(key, sp.i64v),
        iter::T_DOUBLE => w.double(key, sp.dbl),
        iter::T_DECIMAL128 => w.decimal128(key, &sp.dec.unwrap()),
        iter::T_SYMBOL => w.symbol(key, sp.symbol.as_deref().unwrap_or(b"")),
        iter::T_TIMESTAMP => match sp.ts {
            (None, _) => return Err("Missing t after $timestamp in BSON_TYPE_TIMESTAMP".into()),
            (_, None) => return Err("Missing i after $timestamp in BSON_TYPE_TIMESTAMP".into()),
            (Some(t), Some(i)) => w.timestamp(key, t, i),
        },
        iter::T_DBPOINTER => {
            let (n, i) = sp.dbptr.unwrap();
            w.dbpointer(key, &n, &i);
        }
        _ => return Err("Invalid MongoDB extended JSON".into()),
    }
    Ok(())
}

fn timestamp_wrong(v: &J) -> String {
    match v {
        J::Int(neg, m, _) => format!("Unexpected integer {}{} in state \"IN_BSON_TYPE_TIMESTAMP_STARTMAP\"", if *neg { "-" } else { "" }, m.unwrap_or(0)),
        J::Arr(_) => "Invalid read of \"[\" in state \"IN_BSON_TYPE_TIMESTAMP_STARTMAP\"".into(),
        J::Str(s) => format!("Invalid input string \"{}\", looking for TIMESTAMP_T", lossy(s)),
        _ => format!("Invalid read of {} in state IN_BSON_TYPE_TIMESTAMP_STARTMAP", kind_of(v)),
    }
}

/// libbson's `bson_oid_init_from_string` on 24 chars that are not all hex (it does not check).
fn oid_lenient(s: &[u8]) -> [u8; 12] {
    fn h(c: u8) -> u8 {
        match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            b'A'..=b'F' => c - b'A' + 10,
            _ => 0,
        }
    }
    let mut o = [0u8; 12];
    for i in 0..12 {
        o[i] = h(s[2 * i]) << 4 | h(s[2 * i + 1]);
    }
    o
}

/// The canonical `"$binary": {"base64": ..., "subType": ...}` object.
fn binary_values(sp: &mut Spec, inner: &[(Vec<u8>, J)]) -> R<()> {
    const RS: &str = "IN_BSON_TYPE_BINARY_VALUES";
    for (k, v) in inner {
        match k.as_slice() {
            b"base64" => {
                let s = str_of(k, v, sp.ty, RS, true)?;
                sp.has_binary = true;
                sp.bin = base64_decode(s).ok_or_else(|| format!("Invalid input string \"{}\", looking for base64-encoded binary", lossy(s)))?;
            }
            b"subType" => {
                let s = str_of(k, v, sp.ty, RS, true)?;
                sp.has_subtype = true;
                sp.subtype = scan_hex2(s).ok_or_else(|| format!("Invalid input string \"{}\", looking for binary subtype", lossy(s)))?;
            }
            _ => return Err(format!("Invalid key \"{}\".  Looking for values for type \"{}\"", lossy(k), type_name(sp.ty))),
        }
    }
    if inner.is_empty() {
        return Err("Invalid MongoDB extended JSON".into());
    }
    if !sp.has_binary {
        return Err("Missing \"base64\" after \"subType\" reading type \"binary\"".into());
    } else if !sp.has_subtype {
        return Err("Missing \"subType\" after \"base64\" reading type \"binary\"".into());
    }
    Ok(())
}

/// `sscanf(s, "%02x", &t) == 1`.
fn scan_hex2(s: &[u8]) -> Option<u8> {
    let mut i = 0;
    while i < s.len() && s[i].is_ascii_whitespace() {
        i += 1;
    }
    let mut v = 0u32;
    let mut n = 0;
    let mut w = 0;
    if i < s.len() && (s[i] == b'+' || s[i] == b'-') {
        return None;
    }
    while i < s.len() && w < 2 {
        let c = s[i];
        let d = match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            b'A'..=b'F' => c - b'A' + 10,
            _ => break,
        };
        v = v * 16 + d as u32;
        n += 1;
        w += 1;
        i += 1;
    }
    (n > 0).then_some(v as u8)
}

fn parse_uuid(s: &[u8]) -> Option<Vec<u8>> {
    if s.len() != 36 || s[8] != b'-' || s[13] != b'-' || s[18] != b'-' || s[23] != b'-' {
        return None;
    }
    let hex: Vec<u8> = s.iter().enumerate().filter(|(i, _)| ![8, 13, 18, 23].contains(i)).map(|(_, &c)| c).collect();
    let mut out = Vec::with_capacity(16);
    for i in 0..16 {
        let h = std::str::from_utf8(&hex[2 * i..2 * i + 2]).ok()?;
        if !h.bytes().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        out.push(u8::from_str_radix(h, 16).ok()?);
    }
    Some(out)
}

/// A wrapper-looking object that turns out to be a regular document (a `$type` query operator).
fn regular_doc(w: &mut Writer, key: &[u8], o: &[(Vec<u8>, J)]) -> R<()> {
    let p = w.begin(iter::T_DOCUMENT, key);
    doc(w, o)?;
    w.end(p);
    Ok(())
}
