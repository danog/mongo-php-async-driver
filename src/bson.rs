//! Minimal raw BSON reader/writer: only what the wire protocol needs (replies, hello, cursors).
//! Documents returned to PHP are decoded by ext-mongodb (see abi.rs), never here.

use crate::error::{Error, R};

#[derive(Clone, Copy)]
pub struct Doc<'a>(pub &'a [u8]);

#[derive(Clone, Copy)]
pub enum Val<'a> {
    Double(f64),
    Str(&'a str),
    Doc(Doc<'a>),
    Arr(Doc<'a>),
    Bool(bool),
    I32(i32),
    I64(i64),
    Other,
}

fn i32_at(b: &[u8], at: usize) -> R<i32> {
    b.get(at..at + 4)
        .map(|s| i32::from_le_bytes(s.try_into().unwrap()))
        .ok_or_else(|| Error::bson("truncated int32"))
}

/// A non-negative int32 length at `at`, plus `extra` bytes (overflow-checked).
fn len_at(b: &[u8], at: usize, extra: usize) -> R<usize> {
    let n = i32_at(b, at)?;
    if n < 0 {
        return Err(Error::bson("negative length"));
    }
    (n as usize).checked_add(extra).ok_or_else(|| Error::bson("length overflow"))
}

impl<'a> Doc<'a> {
    /// The document at the start of `b` (which may continue past it).
    pub fn at(b: &'a [u8]) -> R<Self> {
        let len = i32_at(b, 0)?;
        if len < 5 || len as usize > b.len() || b[len as usize - 1] != 0 {
            return Err(Error::bson("invalid document length"));
        }
        Ok(Doc(&b[..len as usize]))
    }

    pub fn iter(&self) -> Iter<'a> {
        Iter { b: self.0, pos: 4 }
    }

    pub fn get(&self, key: &str) -> R<Option<Val<'a>>> {
        for e in self.iter() {
            let (k, v) = e?;
            if k == key.as_bytes() {
                return Ok(Some(v));
            }
        }
        Ok(None)
    }

    pub fn str(&self, key: &str) -> R<Option<&'a str>> {
        Ok(match self.get(key)? {
            Some(Val::Str(s)) => Some(s),
            _ => None,
        })
    }

    pub fn bool(&self, key: &str) -> R<bool> {
        Ok(matches!(self.get(key)?, Some(Val::Bool(true))))
    }

    pub fn doc(&self, key: &str) -> R<Option<Doc<'a>>> {
        Ok(match self.get(key)? {
            Some(Val::Doc(d)) | Some(Val::Arr(d)) => Some(d),
            _ => None,
        })
    }

    pub fn num(&self, key: &str) -> R<Option<f64>> {
        Ok(self.get(key)?.and_then(|v| v.num()))
    }
}

impl<'a> Val<'a> {
    pub fn num(&self) -> Option<f64> {
        match *self {
            Val::Double(d) => Some(d),
            Val::I32(i) => Some(i as f64),
            Val::I64(i) => Some(i as f64),
            Val::Bool(b) => Some(b as i32 as f64),
            _ => None,
        }
    }
}

pub struct Iter<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> Iterator for Iter<'a> {
    type Item = R<(&'a [u8], Val<'a>)>;

    fn next(&mut self) -> Option<Self::Item> {
        let b = self.b;
        let t = *b.get(self.pos)?;
        if t == 0 {
            return None;
        }
        Some(self.element(t))
    }
}

impl<'a> Iter<'a> {
    fn element(&mut self, t: u8) -> R<(&'a [u8], Val<'a>)> {
        let b = self.b;
        let ks = self.pos + 1;
        let ke = ks + b[ks..].iter().position(|&c| c == 0).ok_or_else(|| Error::bson("unterminated key"))?;
        let key = &b[ks..ke];
        let v = ke + 1;
        let (len, val) = match t {
            0x01 => (8, Val::Double(f64::from_le_bytes(b.get(v..v + 8).ok_or_else(|| Error::bson("truncated"))?.try_into().unwrap()))),
            0x02 | 0x0D | 0x0E => {
                let n = i32_at(b, v)?;
                if n < 1 || v + 4 + n as usize > b.len() {
                    return Err(Error::bson("invalid string length"));
                }
                let s = std::str::from_utf8(&b[v + 4..v + 4 + n as usize - 1]).map_err(|_| Error::bson("invalid UTF-8"))?;
                (4 + n as usize, if t == 0x02 { Val::Str(s) } else { Val::Other })
            }
            0x03 | 0x04 => {
                let d = Doc::at(&b[v..])?;
                (d.0.len(), if t == 0x03 { Val::Doc(d) } else { Val::Arr(d) })
            }
            0x05 => (len_at(b, v, 5)?, Val::Other),
            0x06 | 0x0A | 0xFF | 0x7F => (0, Val::Other),
            0x07 => (12, Val::Other),
            0x08 => (1, Val::Bool(*b.get(v).ok_or_else(|| Error::bson("truncated"))? != 0)),
            0x09 | 0x11 => (8, Val::Other),
            0x0B => {
                let a = b[v..].iter().position(|&c| c == 0).ok_or_else(|| Error::bson("bad regex"))?;
                let c = b[v + a + 1..].iter().position(|&c| c == 0).ok_or_else(|| Error::bson("bad regex"))?;
                (a + c + 2, Val::Other)
            }
            0x0C => (len_at(b, v, 16)?, Val::Other),
            0x0F => (len_at(b, v, 0)?, Val::Other),
            0x10 => (4, Val::I32(i32_at(b, v)?)),
            0x12 => (8, Val::I64(i64::from_le_bytes(b.get(v..v + 8).ok_or_else(|| Error::bson("truncated"))?.try_into().unwrap()))),
            0x13 => (16, Val::Other),
            _ => return Err(Error::bson("unknown element type")),
        };
        match v.checked_add(len) {
            Some(end) if end < b.len() => self.pos = end,
            _ => return Err(Error::bson("element overflows document")),
        }
        Ok((key, val))
    }
}

// ---- writer --------------------------------------------------------------------------------

pub fn begin(buf: &mut Vec<u8>) -> usize {
    let at = buf.len();
    buf.extend_from_slice(&[0; 4]);
    at
}

pub fn end(buf: &mut Vec<u8>, at: usize) {
    buf.push(0);
    let len = (buf.len() - at) as i32;
    buf[at..at + 4].copy_from_slice(&len.to_le_bytes());
}

fn key(buf: &mut Vec<u8>, t: u8, k: &str) {
    buf.push(t);
    buf.extend_from_slice(k.as_bytes());
    buf.push(0);
}

pub fn put_str(buf: &mut Vec<u8>, k: &str, s: &str) {
    key(buf, 0x02, k);
    buf.extend_from_slice(&(s.len() as i32 + 1).to_le_bytes());
    buf.extend_from_slice(s.as_bytes());
    buf.push(0);
}

pub fn put_i32(buf: &mut Vec<u8>, k: &str, v: i32) {
    key(buf, 0x10, k);
    buf.extend_from_slice(&v.to_le_bytes());
}

pub fn put_i64(buf: &mut Vec<u8>, k: &str, v: i64) {
    key(buf, 0x12, k);
    buf.extend_from_slice(&v.to_le_bytes());
}

pub fn put_bool(buf: &mut Vec<u8>, k: &str, v: bool) {
    key(buf, 0x08, k);
    buf.push(v as u8);
}

pub fn begin_sub(buf: &mut Vec<u8>, k: &str, array: bool) -> usize {
    key(buf, if array { 0x04 } else { 0x03 }, k);
    begin(buf)
}
