//! Write and read concerns (mongoc-write-concern.c, mongoc-read-concern.c).

use crate::bson::iter::{Iter, Value};
use crate::bson::writer::Writer;
use crate::error::{code, domain, Error};
use crate::uri::{W_DEFAULT, W_MAJORITY, W_TAG, W_UNACKNOWLEDGED};

/// mongoc_write_concern_t
#[derive(Clone, Debug, PartialEq)]
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
    pub fn set_w(&mut self, w: i32) {
        self.w = w;
        if w != W_DEFAULT {
            self.is_default = false;
        }
    }
    pub fn set_wtag(&mut self, t: &[u8]) {
        self.wtag = Some(t.to_vec());
        self.w = W_TAG;
        self.is_default = false;
    }
    pub fn set_wtimeout(&mut self, v: i64) {
        if v >= 0 {
            self.wtimeout = v;
            self.is_default = false;
        }
    }
    pub fn set_journal(&mut self, j: bool) {
        self.journal = Some(j);
        self.is_default = false;
    }
    /// mongoc_write_concern_is_valid
    pub fn is_valid(&self) -> bool {
        !(self.journal == Some(true) && self.w == W_UNACKNOWLEDGED) && self.wtimeout >= 0
    }
    /// mongoc_write_concern_is_acknowledged
    pub fn is_acknowledged(&self) -> bool {
        self.w != W_UNACKNOWLEDGED || self.journal == Some(true)
    }
    /// The client's write concern (from the URI).
    pub fn from_uri(u: &crate::uri::Uri) -> Self {
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
    /// _mongoc_write_concern_freeze: the document sent as "writeConcern" (may be empty).
    pub fn to_bson(&self) -> Vec<u8> {
        let mut w = Writer::with_capacity(48);
        if self.w == W_TAG {
            w.utf8(b"w", self.wtag.as_deref().unwrap_or_default());
        } else if self.w == W_MAJORITY {
            w.utf8(b"w", b"majority");
        } else if self.w != W_DEFAULT {
            w.int32(b"w", self.w);
        }
        if let Some(j) = self.journal {
            w.bool(b"j", j);
        }
        if self.wtimeout != 0 {
            w.int64(b"wtimeout", self.wtimeout);
        }
        w.finish()
    }
    /// _mongoc_write_concern_new_from_iter (the value of a "writeConcern" option).
    pub fn from_value(v: &Value) -> Result<Self, Error> {
        let fail = || Error::new(domain::COMMAND, code::COMMAND_INVALID_ARG, "Invalid writeConcern");
        let Value::Document(d) = v else { return Err(fail()) };
        let mut wc = WriteConcern::default();
        let mut it = Iter::new(d).ok_or_else(fail)?;
        while it.next() {
            match it.key() {
                b"w" => match it.value() {
                    Value::Int32(w) => {
                        if w < W_UNACKNOWLEDGED {
                            return Err(fail());
                        }
                        wc.set_w(w);
                    }
                    Value::Utf8(s) => {
                        let s = s.split(|&b| b == 0).next().unwrap_or(s);
                        if s == b"majority" {
                            wc.set_w(W_MAJORITY);
                        } else {
                            wc.set_wtag(s);
                        }
                    }
                    _ => return Err(fail()),
                },
                b"fsync" => return Err(fail()),
                b"j" => match it.value() {
                    Value::Bool(b) => wc.set_journal(b),
                    _ => return Err(fail()),
                },
                b"wtimeout" => {
                    let v = it.value();
                    if !matches!(v, Value::Int32(_) | Value::Int64(_)) || crate::sdam::as_i64(&v) < 0 {
                        return Err(fail());
                    }
                    wc.set_wtimeout(crate::sdam::as_i64(&v));
                }
                _ => {}
            }
        }
        if !wc.is_valid() {
            return Err(Error::new(domain::COMMAND, code::COMMAND_INVALID_ARG, "Invalid writeConcern"));
        }
        Ok(wc)
    }
}

/// mongoc_read_concern_t
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ReadConcern {
    pub level: Option<Vec<u8>>,
}

impl ReadConcern {
    /// mongoc_read_concern_is_default
    pub fn is_default(&self) -> bool {
        self.level.is_none()
    }
    /// The document sent as "readConcern" (empty when default).
    pub fn to_bson(&self) -> Vec<u8> {
        let mut w = Writer::with_capacity(32);
        if let Some(l) = &self.level {
            w.utf8(b"level", l);
        }
        w.finish()
    }
    pub fn from_uri(u: &crate::uri::Uri) -> Self {
        ReadConcern { level: u.read_concern_level.as_ref().map(|s| s.as_bytes().to_vec()) }
    }
}
