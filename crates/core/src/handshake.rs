//! The connection handshake (mongoc-handshake.c, mongoc-topology-scanner.c): the initial hello
//! command with client metadata, and running it on a fresh connection.

use std::time::{Duration, Instant};

use crate::bson::writer::Writer;
use crate::net::{Conn, NetError};
use crate::sdam::ServerDescription;
use crate::wire::{self, MsgBuilder};

/// Driver metadata (mongoc_handshake_data_append: libmongoc's values, then the wrapping
/// driver's, separated by " / ").
#[derive(Clone, Debug, Default)]
pub struct Metadata {
    pub appname: Option<String>,
    /// e.g. "ext-mongodb:PHP"
    pub driver_name: String,
    pub driver_version: String,
    /// e.g. "PHP 8.4.13"
    pub platform: String,
}

/// What the hello asks for besides the metadata.
#[derive(Clone, Debug, Default)]
pub struct HelloOpts {
    /// serverApi declared: use `hello` (else legacy `isMaster` with helloOk)
    pub api_version: Option<String>,
    pub api_strict: Option<bool>,
    pub api_deprecation_errors: Option<bool>,
    pub load_balanced: bool,
    pub compressors: Vec<String>,
    /// zlibCompressionLevel (-1: zlib's default)
    pub zlib_level: i32,
    /// "db.user" for saslSupportedMechs
    pub sasl_user: Option<String>,
}

fn os_type() -> &'static str {
    match std::env::consts::OS {
        "linux" => "Linux",
        "macos" => "Darwin",
        "windows" => "Windows",
        "freebsd" => "FreeBSD",
        o => o,
    }
}

/// The initial hello command body (with $db), as mongoc builds it for a handshake.
pub fn hello_command(md: &Metadata, o: &HelloOpts, hello_ok_known: bool) -> Vec<u8> {
    hello_command_auth(md, o, hello_ok_known, true, None)
}

/// `hello_command` with authentication parts: saslSupportedMechs (when `negotiate` and
/// `o.sasl_user` is set) and a speculativeAuthenticate document.
pub fn hello_command_auth(md: &Metadata, o: &HelloOpts, hello_ok_known: bool, negotiate: bool, speculative: Option<&[u8]>) -> Vec<u8> {
    let mut w = Writer::with_capacity(512);
    if o.api_version.is_some() || o.load_balanced || hello_ok_known {
        w.int32(b"hello", 1);
    } else {
        w.int32(b"isMaster", 1);
    }
    w.bool(b"helloOk", true);
    let c = w.begin(3, b"client");
    if let Some(a) = &md.appname {
        let a0 = w.begin(3, b"application");
        w.utf8(b"name", a.as_bytes());
        w.end(a0);
    }
    let d = w.begin(3, b"driver");
    let name = if md.driver_name.is_empty() { "mongoc".to_owned() } else { format!("mongoc / {}", md.driver_name) };
    let version = if md.driver_version.is_empty() { crate::LIBMONGOC_VERSION.to_owned() } else { format!("{} / {}", crate::LIBMONGOC_VERSION, md.driver_version) };
    w.utf8(b"name", name.as_bytes());
    w.utf8(b"version", version.as_bytes());
    w.end(d);
    let os = w.begin(3, b"os");
    w.utf8(b"type", os_type().as_bytes());
    w.utf8(b"architecture", std::env::consts::ARCH.as_bytes());
    w.end(os);
    if !md.platform.is_empty() {
        w.utf8(b"platform", md.platform.as_bytes());
    }
    w.end(c);
    if o.load_balanced {
        w.bool(b"loadBalanced", true);
    }
    if !o.compressors.is_empty() {
        let a = w.begin(4, b"compression");
        for (i, c) in o.compressors.iter().enumerate() {
            w.utf8(i.to_string().as_bytes(), c.as_bytes());
        }
        w.end(a);
    }
    if let Some(u) = o.sasl_user.as_ref().filter(|_| negotiate) {
        w.utf8(b"saslSupportedMechs", u.as_bytes());
    }
    if let Some(d) = speculative {
        w.document(b"speculativeAuthenticate", d);
    }
    if let Some(v) = &o.api_version {
        w.utf8(b"apiVersion", v.as_bytes());
        if let Some(s) = o.api_strict {
            w.bool(b"apiStrict", s);
        }
        if let Some(d) = o.api_deprecation_errors {
            w.bool(b"apiDeprecationErrors", d);
        }
    }
    w.utf8(b"$db", b"admin");
    w.finish()
}

thread_local! {
    static REQUEST_ID: std::cell::Cell<i32> = const { std::cell::Cell::new(1) };
}

/// The next OP_MSG requestID (process-wide on this thread, never 0).
pub fn next_request_id() -> i32 {
    REQUEST_ID.with(|r| {
        let v = r.get();
        r.set(v.checked_add(1).unwrap_or(1));
        v
    })
}

/// Runs `body` on `conn` (blocking) and returns the reply's body document.
pub fn run_command(conn: &mut Conn, body: &[u8], timeout: Option<Duration>) -> Result<Vec<u8>, NetError> {
    Ok(run_msg(conn, body, &[], false, timeout)?.unwrap_or_default())
}

/// The OP_MSG for `body` (+ document sequences), compressed with the connection's compressor
/// when the command may be.
pub fn build_msg(compressor: Option<(u8, i32)>, body: &[u8], payload: crate::wire::Payloads<'_>, more_to_come: bool) -> Result<Vec<u8>, NetError> {
    let mut m = MsgBuilder::new(next_request_id(), if more_to_come { wire::FLAG_MORE_TO_COME } else { 0 });
    m.body(body);
    for (id, docs) in payload {
        m.sequence_raw(id, docs);
    }
    let msg = m.finish();
    if let Some((id, level)) = compressor {
        if crate::compress::is_compressible(crate::compress::command_name(body)) {
            return wire::compress_message(&msg, id, level).map_err(|_| NetError::Protocol("could not compress the message"));
        }
    }
    Ok(msg)
}

/// The body document of a reply (decompressed first when OP_COMPRESSED, bounded by `max`).
pub fn reply_body(reply: &[u8], max: usize) -> Result<Vec<u8>, NetError> {
    if wire::op_code(reply) == wire::OP_COMPRESSED {
        let full = wire::decompress_message(reply, max).map_err(NetError::Protocol)?;
        return Ok(wire::parse_reply(&full).map_err(NetError::Protocol)?.body.to_vec());
    }
    Ok(wire::parse_reply(reply).map_err(NetError::Protocol)?.body.to_vec())
}

/// Sends an OP_MSG (body + optional document sequence); with `more_to_come` no reply is
/// read (unacknowledged writes) and None is returned.
pub fn run_msg(conn: &mut Conn, body: &[u8], payload: crate::wire::Payloads<'_>, more_to_come: bool, timeout: Option<Duration>) -> Result<Option<Vec<u8>>, NetError> {
    let msg = build_msg(conn.compressor, body, payload, more_to_come)?;
    if more_to_come {
        conn.send(&msg, timeout)?;
        return Ok(None);
    }
    let max = conn.max_message_size();
    let reply = conn.roundtrip(&msg, timeout)?;
    reply_body(reply, max).map(Some)
}

/// The compressor of a handshake reply's `compression` list (as a server description holds it).
pub fn negotiated(sd: &ServerDescription, o: &HelloOpts) -> Option<(u8, i32)> {
    crate::compress::negotiate(&sd.compressors).map(|id| (id, o.zlib_level))
}

/// Connects to `host:port` and runs the handshake; the server description it yields.
pub fn connect_and_hello(host: &str, port: u16, connect_timeout: Option<Duration>, md: &Metadata, o: &HelloOpts) -> Result<(Conn, ServerDescription), NetError> {
    let mut conn = Conn::connect(host, port, connect_timeout)?;
    let body = hello_command(md, o, false);
    let t0 = Instant::now();
    // the handshake is bounded by connectTimeoutMS, as libmongoc does
    let reply = run_command(&mut conn, &body, connect_timeout)?;
    let rtt = t0.elapsed().as_millis() as i64;
    let sd = ServerDescription::from_hello(&conn.addr, &reply, rtt);
    conn.set_max_message_size(sd.max_msg_size);
    conn.compressor = negotiated(&sd, o);
    Ok((conn, sd))
}
