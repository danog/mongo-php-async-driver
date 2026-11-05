//! Command replies: error detection as libmongoc does it (mongoc-rpc.c `_mongoc_cmd_check_ok`).

use crate::bson::iter::{Iter, Value};
use crate::sdam::as_i64;

/// mongoc_error_code_t values that surface as exception codes.
pub mod code {
    pub const STREAM_INVALID_TYPE: u32 = 1;
    pub const STREAM_NAME_RESOLUTION: u32 = 3;
    pub const STREAM_SOCKET: u32 = 4;
    pub const STREAM_CONNECT: u32 = 5;
    pub const CLIENT_AUTHENTICATE: u32 = 11;
    pub const PROTOCOL_INVALID_REPLY: u32 = 14;
    pub const PROTOCOL_BAD_WIRE_VERSION: u32 = 15;
    pub const QUERY_FAILURE: u32 = 17;
    /// protocolError (17) is mapped to commandNotFound (DRIVERS-192)
    pub const PROTOCOL_ERROR: u32 = 17;
    pub const QUERY_COMMAND_NOT_FOUND: u32 = 59;
    pub const WRITE_CONCERN_ERROR: u32 = 64;
    pub const SERVER_SELECTION_FAILURE: u32 = 13053;
}

/// A command error from a server reply.
#[derive(Clone, Debug, PartialEq)]
pub struct CmdError {
    pub code: u32,
    /// true: the code is the server's (MONGOC_ERROR_CATEGORY_SERVER)
    pub server: bool,
    pub message: String,
}

fn find<'a>(doc: &'a [u8], key: &[u8]) -> Option<Value<'a>> {
    let mut it = Iter::new(doc)?;
    if it.find(key) {
        Some(it.value())
    } else {
        None
    }
}

fn is_number(v: &Value) -> bool {
    matches!(v, Value::Double(_) | Value::Int32(_) | Value::Int64(_))
}

/// _parse_error_reply: (code, message) when the reply carries an error.
fn parse_error_reply(doc: &[u8], check_wce: bool) -> Option<(u32, String)> {
    let mut code = 0u32;
    let mut msg: Option<&[u8]> = None;
    let mut found = false;
    if let Some(v) = find(doc, b"code").filter(is_number) {
        code = as_i64(&v) as u32;
        found = true;
    }
    if let Some(Value::Utf8(s)) = find(doc, b"errmsg") {
        msg = Some(s);
        found = true;
    } else if let Some(Value::Utf8(s)) = find(doc, b"$err") {
        msg = Some(s);
        found = true;
    }
    if !found && check_wce {
        if let Some(Value::Document(w)) = find(doc, b"writeConcernError") {
            if let Some(v) = find(w, b"code").filter(is_number) {
                code = as_i64(&v) as u32;
                found = true;
            }
            if let Some(Value::Utf8(s)) = find(w, b"errmsg") {
                msg = Some(s);
                found = true;
            }
        }
    }
    if !found {
        return None;
    }
    let msg = msg.map(|m| String::from_utf8_lossy(m).into_owned()).unwrap_or_else(|| "Unknown command error".to_owned());
    Some((code, msg))
}

/// _mongoc_cmd_set_error's code/category mapping.
fn to_error(code: u32, message: String) -> CmdError {
    if code == code::PROTOCOL_ERROR {
        CmdError { code: code::QUERY_COMMAND_NOT_FOUND, server: true, message }
    } else if code == 0 {
        CmdError { code: code::QUERY_FAILURE, server: false, message }
    } else {
        CmdError { code, server: true, message }
    }
}

/// _mongoc_cmd_check_ok: Err when `doc` is an error reply (writeConcernError not checked).
pub fn check_ok(doc: &[u8]) -> Result<(), CmdError> {
    if find(doc, b"ok").is_some_and(|v| crate::sdam::as_bool(&v)) {
        return Ok(());
    }
    match parse_error_reply(doc, false) {
        Some((c, m)) => Err(to_error(c, m)),
        None => Ok(()),
    }
}

/// _mongoc_cmd_check_ok_no_wce: a writeConcernError is an error too.
pub fn check_ok_no_wce(doc: &[u8]) -> Result<(), CmdError> {
    match parse_error_reply(doc, true) {
        Some((c, m)) => Err(to_error(c, m)),
        None => Ok(()),
    }
}

/// The message of an error reply ("Unknown command error" when it has none).
pub fn error_message(doc: &[u8]) -> String {
    parse_error_reply(doc, false).map(|e| e.1).unwrap_or_else(|| "Unknown command error".to_owned())
}

impl CmdError {
    /// The bson_error_t libmongoc builds with error API version 2 (domain SERVER).
    pub fn into_error(self) -> crate::error::Error {
        let category = if self.server { crate::error::Category::Server } else { crate::error::Category::Driver };
        crate::error::Error { domain: crate::error::domain::SERVER, code: self.code, category, message: self.message }
    }
}

/// mongoc_error_has_label: `reply.errorLabels` contains `label`.
pub fn has_label(reply: &[u8], label: &str) -> bool {
    let Some(Value::Array(a)) = find(reply, b"errorLabels") else { return false };
    let Some(mut it) = Iter::new(a) else { return false };
    while it.next() {
        if matches!(it.value(), Value::Utf8(s) if s == label.as_bytes()) {
            return true;
        }
    }
    false
}

/// _mongoc_parse_wc_err: the error of a reply's writeConcernError.
pub fn parse_wc_error(reply: &[u8]) -> Option<crate::error::Error> {
    let Some(Value::Document(w)) = find(reply, b"writeConcernError") else { return None };
    let (mut code, mut msg) = (0u32, String::new());
    let mut i = Iter::new(w)?;
    while i.next() {
        match i.key() {
            b"code" => code = crate::sdam::as_i64(&i.value()) as u32,
            b"errmsg" => {
                if let Value::Utf8(s) = i.value() {
                    msg = String::from_utf8_lossy(s).into_owned();
                }
            }
            _ => {}
        }
    }
    Some(crate::error::Error::new(crate::error::domain::WRITE_CONCERN, code, format!("Write Concern error: {msg}")))
}
