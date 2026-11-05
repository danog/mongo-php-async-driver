//! Authentication (mongoc-cluster.c, mongoc-topology-scanner.c): the SCRAM conversation over
//! saslStart/saslContinue, speculative authentication in the handshake and mechanism
//! negotiation. Only SCRAM-SHA-1 and SCRAM-SHA-256 are implemented; every other mechanism fails
//! closed (a connection that should be authenticated is never used unauthenticated).

pub mod saslprep;
pub mod scram;
mod tables;

use crate::bson::iter::{Iter, Value};
use crate::bson::writer::Writer;
use crate::error::{code, domain, Error};
use crate::uri::Uri;
use scram::{Algo, Scram};

/// The server's saslSupportedMechs (mongoc_handshake_sasl_supported_mechs_t).
#[derive(Clone, Copy, Debug, Default)]
pub struct SaslMechs {
    pub scram_sha_1: bool,
    pub scram_sha_256: bool,
}

/// _mongoc_handshake_parse_sasl_supported_mechs
pub fn parse_sasl_mechs(hello: &[u8]) -> SaslMechs {
    let mut m = SaslMechs::default();
    let Some(mut it) = Iter::new(hello) else { return m };
    if it.find(b"saslSupportedMechs") {
        if let Value::Array(a) = it.value() {
            if let Some(mut i) = Iter::new(a) {
                while i.next() {
                    match i.value() {
                        Value::Utf8(b"SCRAM-SHA-256") => m.scram_sha_256 = true,
                        Value::Utf8(b"SCRAM-SHA-1") => m.scram_sha_1 = true,
                        _ => {}
                    }
                }
            }
        }
    }
    m
}

/// cluster->requires_auth
pub fn requires_auth(uri: &Uri) -> bool {
    uri.username.is_some() || uri.auth_mechanism().is_some()
}

/// _mongoc_uri_requires_auth_negotiation: a username and no mechanism.
pub fn requires_negotiation(uri: &Uri) -> bool {
    uri.username.is_some() && uri.auth_mechanism().is_none()
}

/// The saslSupportedMechs value of the handshake ("<authSource>.<username>").
pub fn sasl_user(uri: &Uri) -> Option<String> {
    requires_negotiation(uri).then(|| format!("{}.{}", uri.auth_source(), uri.username.as_deref().unwrap_or("")))
}

/// _mongoc_topology_scanner_get_speculative_auth_mechanism
pub fn speculative_mechanism(uri: &Uri) -> Option<&str> {
    if !requires_auth(uri) {
        return None;
    }
    Some(uri.auth_mechanism().unwrap_or("SCRAM-SHA-256"))
}

fn scram_algo(mechanism: &str) -> Option<Algo> {
    if mechanism.eq_ignore_ascii_case("SCRAM-SHA-1") {
        Some(Algo::Sha1)
    } else if mechanism.eq_ignore_ascii_case("SCRAM-SHA-256") {
        Some(Algo::Sha256)
    } else {
        None
    }
}

fn new_scram(uri: &Uri, algo: Algo) -> Scram {
    Scram::new(algo, uri.username.as_deref().unwrap_or(""), uri.password.as_deref())
}

/// _mongoc_cluster_get_auth_cmd_scram: the saslStart command (step 1 of `scram`).
fn sasl_start_cmd(scram: &mut Scram, w: &mut Writer) -> Result<(), Error> {
    let payload = scram.step(&[])?;
    w.int32(b"saslStart", 1);
    w.utf8(b"mechanism", scram.algo().mechanism().as_bytes());
    w.binary(b"payload", 0, &payload);
    w.int32(b"autoAuthorize", 1);
    let o = w.begin(3, b"options");
    w.bool(b"skipEmptyExchange", true);
    w.end(o);
    Ok(())
}

/// The handshake's speculativeAuthenticate document and the conversation it starts
/// (_mongoc_topology_scanner_add_speculative_authentication). Only SCRAM is speculated: X509
/// needs TLS and OIDC a cached token, neither of which exists here.
pub fn speculative_start(uri: &Uri) -> Option<(Vec<u8>, Scram)> {
    let algo = scram_algo(speculative_mechanism(uri)?)?;
    let mut s = new_scram(uri, algo);
    let mut w = Writer::with_capacity(160);
    // errors are ignored: the handshake proceeds and the explicit authentication reports them
    sasl_start_cmd(&mut s, &mut w).ok()?;
    w.utf8(b"db", uri.auth_source().as_bytes());
    Some((w.finish(), s))
}

/// The handshake reply's speculativeAuthenticate document (empty: none).
pub fn speculative_reply(hello: &[u8]) -> Vec<u8> {
    let Some(mut it) = Iter::new(hello) else { return Vec::new() };
    if it.find(b"speculativeAuthenticate") {
        if let Value::Document(d) = it.value() {
            return d.to_vec();
        }
    }
    Vec::new()
}

/// A hello reply without its speculativeAuthenticate field: what the server description,
/// the monitoring events and getInfo() see (mongoc_server_description_handle_hello).
pub fn redact_hello(hello: &[u8]) -> std::borrow::Cow<'_, [u8]> {
    let Some(mut it) = Iter::new(hello) else { return hello.into() };
    if !it.find(b"speculativeAuthenticate") {
        return hello.into();
    }
    let mut w = Writer::with_capacity(hello.len());
    let Some(mut it) = Iter::new(hello) else { return hello.into() };
    while it.next() {
        if it.key() != b"speculativeAuthenticate" {
            w.element_from(None, &it);
        }
    }
    w.finish().into()
}

/// Runs one authentication command (with its $db: the auth source) on the connection being
/// authenticated: the reply, or the error (_mongoc_cluster_run_scram_command converts it to an
/// authentication error).
pub type RunCmd<'a> = dyn FnMut(&[u8]) -> Result<Vec<u8>, Error> + 'a;

fn auth_error(e: Error) -> Error {
    Error { domain: domain::CLIENT, code: code::CLIENT_AUTHENTICATE, ..e }
}

/// _mongoc_cluster_scram_handle_reply: (done, conversationId, payload).
fn handle_reply(scram: &Scram, reply: &[u8]) -> Result<(bool, i32, Vec<u8>), Error> {
    let find = |k: &[u8]| Iter::new(reply).and_then(|mut it| it.find(k).then(|| it.value()));
    let auth = |m: &str| Error::new(domain::CLIENT, code::CLIENT_AUTHENTICATE, m);
    let done = matches!(find(b"done"), Some(Value::Bool(true)));
    if done {
        if scram.step < 2 {
            return Err(auth("Incorrect step for 'done'"));
        }
        if scram.step >= 3 {
            return Ok((true, 0, Vec::new()));
        }
    }
    let conv = match find(b"conversationId") {
        Some(Value::Int32(c)) if c != 0 => Some(c),
        _ => None,
    };
    let payload = match find(b"payload") {
        Some(Value::Binary(_, p)) => Some(p),
        _ => None,
    };
    let (Some(conv), Some(payload)) = (conv, payload) else {
        let msg = match find(b"errmsg") {
            Some(Value::Utf8(m)) => String::from_utf8_lossy(m).into_owned(),
            _ => "Received invalid SCRAM reply from MongoDB server.".to_owned(),
        };
        return Err(auth(&msg));
    };
    if payload.len() > scram::BUF_MAX {
        return Err(auth("SCRAM reply from MongoDB is too large."));
    }
    Ok((done, conv, payload.to_vec()))
}

/// _mongoc_cluster_auth_scram_continue: the conversation after the saslStart reply (sent
/// explicitly or received in the handshake).
pub fn scram_continue(scram: &mut Scram, start_reply: &[u8], run: &mut RunCmd) -> Result<(), Error> {
    let (mut done, mut conv, mut buf) = handle_reply(scram, start_reply)?;
    loop {
        buf = scram.step(&buf)?;
        if done && scram.step >= 3 {
            return Ok(());
        }
        let mut w = Writer::with_capacity(buf.len() + 64);
        w.int32(b"saslContinue", 1);
        w.int32(b"conversationId", conv);
        w.binary(b"payload", 0, &buf);
        let reply = run(&w.finish()).map_err(auth_error)?;
        (done, conv, buf) = match handle_reply(scram, &reply)? {
            (d, 0, b) => (d, conv, b),
            r => r,
        };
        if done && scram.step >= 3 {
            return Ok(());
        }
    }
}

/// _mongoc_cluster_auth_node_scram
fn auth_scram(uri: &Uri, algo: Algo, run: &mut RunCmd) -> Result<(), Error> {
    let mut s = new_scram(uri, algo);
    let mut w = Writer::with_capacity(160);
    sasl_start_cmd(&mut s, &mut w).map_err(auth_error)?;
    let reply = run(&w.finish()).map_err(auth_error)?;
    scram_continue(&mut s, &reply, run)
}

/// The error for a mechanism this build does not implement (fail closed).
pub fn unavailable(mechanism: &str) -> Error {
    Error::new(domain::CLIENT, code::CLIENT_AUTHENTICATE, format!("The {mechanism} authentication mechanism is not supported by this build yet"))
}

/// _mongoc_cluster_finish_speculative_auth: Ok(true) when the handshake's speculative
/// conversation completed the authentication, Ok(false) when there was none to complete, the
/// conversation's error otherwise (the caller then authenticates explicitly, as libmongoc does,
/// unless the connection was lost).
pub fn finish_speculative(uri: &Uri, speculative: &[u8], scram: Option<&mut Scram>, run: &mut RunCmd) -> Result<bool, Error> {
    let (Some(mechanism), Some(s)) = (speculative_mechanism(uri), scram) else { return Ok(false) };
    if speculative.is_empty() || scram_algo(mechanism).is_none() || s.step != 1 {
        return Ok(false);
    }
    scram_continue(s, speculative, run).map(|()| true)
}

/// _mongoc_cluster_auth_node: authenticates the connection with the URI's mechanism (or the
/// negotiated default: SCRAM-SHA-256 when the server lists it, else SCRAM-SHA-1).
pub fn authenticate(uri: &Uri, mechs: SaslMechs, run: &mut RunCmd) -> Result<(), Error> {
    let mechanism = match uri.auth_mechanism() {
        Some(m) => m,
        None if mechs.scram_sha_256 => "SCRAM-SHA-256",
        None => "SCRAM-SHA-1",
    };
    if let Some(algo) = scram_algo(mechanism) {
        return auth_scram(uri, algo, run);
    }
    const KNOWN: &[&str] = &["MONGODB-X509", "GSSAPI", "PLAIN", "MONGODB-AWS", "MONGODB-OIDC"];
    if KNOWN.iter().any(|k| k.eq_ignore_ascii_case(mechanism)) {
        return Err(unavailable(mechanism));
    }
    Err(Error::new(domain::CLIENT, code::CLIENT_AUTHENTICATE, format!("Unknown authentication mechanism \"{mechanism}\".")))
}
