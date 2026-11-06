//! A TLS round trip to a KMS endpoint (libmongoc's `_state_need_kms`: `_get_stream`, write, read
//! until the response is complete). Blocking, with the socket timeout as libmongoc's default.

use crate::error::{code, domain, Error};
use crate::tls::{client_config, server_name, TlsOptions};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// Where a KMS round trip failed: connecting (TLS errors included), writing or reading
#[derive(Debug)]
pub enum Phase {
    Connect,
    Write,
    Read,
}

/// MONGOC_DEFAULT_SOCKETTIMEOUTMS
pub const SOCKET_TIMEOUT: Duration = Duration::from_secs(300);

/// The most response bytes read (the response parser refuses more anyway)
pub const MAX_RESPONSE: usize = 16 * 1024 * 1024;

fn stream_err(m: String) -> Error {
    Error::new(domain::STREAM, code::STREAM_SOCKET, m)
}

/// Sends `msg` to `endpoint` ("host:port") over TLS and reads until `done` accepts what was read
pub fn exchange(endpoint: &str, tls: &TlsOptions, msg: &[u8], done: &mut dyn FnMut(&[u8]) -> bool) -> Result<Vec<u8>, (Phase, Error)> {
    let (host, port) = crate::client::split_host_port(endpoint);
    let connect = || -> Result<rustls::StreamOwned<rustls::ClientConnection, TcpStream>, Error> {
        let addrs = crate::net::resolve(&host, port).map_err(|_| stream_err(format!("Failed to resolve '{host}'")))?;
        let mut last = None;
        let mut tcp = None;
        for a in addrs {
            match TcpStream::connect_timeout(&a, SOCKET_TIMEOUT) {
                Ok(s) => {
                    tcp = Some(s);
                    break;
                }
                Err(e) => last = Some(e),
            }
        }
        let _ = last;
        let tcp = tcp.ok_or_else(|| Error::new(domain::STREAM, code::STREAM_CONNECT, format!("Failed to connect to target host: {endpoint}")))?;
        tcp.set_read_timeout(Some(SOCKET_TIMEOUT)).ok();
        tcp.set_write_timeout(Some(SOCKET_TIMEOUT)).ok();
        let cfg = client_config(tls)?;
        let conn = rustls::ClientConnection::new(cfg, server_name(&host)?).map_err(|_| stream_err(format!("Failed to create TLS stream to: {endpoint}")))?;
        let mut s = rustls::StreamOwned::new(conn, tcp);
        while s.conn.is_handshaking() {
            s.conn.complete_io(&mut s.sock).map_err(|e| {
                let reason = match e.get_ref().and_then(|x| x.downcast_ref::<rustls::Error>()) {
                    Some(t) => crate::tls::describe(t),
                    None => e.to_string(),
                };
                Error::new(domain::STREAM, code::STREAM_SOCKET, format!("TLS handshake failed: {reason}"))
            })?;
        }
        Ok(s)
    };
    let mut s = connect().map_err(|e| (Phase::Connect, e))?;
    s.write_all(msg).and_then(|_| s.flush()).map_err(|e| (Phase::Write, stream_err(e.to_string())))?;
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 1024];
    loop {
        if done(&buf) {
            return Ok(buf);
        }
        match s.read(&mut chunk) {
            Ok(0) => return Err((Phase::Read, stream_err("connection closed".into()))),
            Ok(n) => {
                if buf.len() + n > MAX_RESPONSE {
                    return Err((Phase::Read, stream_err("KMS response too large".into())));
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            Err(e) => return Err((Phase::Read, stream_err(e.to_string()))),
        }
    }
}
