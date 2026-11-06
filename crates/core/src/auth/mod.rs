//! Authentication (mongoc-cluster.c, mongoc-topology-scanner.c): the SCRAM conversation over
//! saslStart/saslContinue, speculative authentication in the handshake and mechanism
//! negotiation. SCRAM-SHA-1, SCRAM-SHA-256, PLAIN and MONGODB-X509 are implemented; every other
//! mechanism fails closed (a connection that should be authenticated is never used
//! unauthenticated).

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

/// _mongoc_cluster_get_auth_cmd_x509: the authenticate command; `user` only when the URI has
/// a username (else the server takes it from the client certificate's subject).
fn x509_cmd(uri: &Uri, w: &mut Writer) {
    w.int32(b"authenticate", 1);
    w.utf8(b"mechanism", b"MONGODB-X509");
    if let Some(u) = &uri.username {
        w.utf8(b"user", u.as_bytes());
    }
}

fn is_x509(mechanism: &str) -> bool {
    mechanism.eq_ignore_ascii_case("MONGODB-X509")
}

/// The handshake's speculativeAuthenticate document and the SCRAM conversation it starts
/// (_mongoc_topology_scanner_add_speculative_authentication). SCRAM and X509 are speculated
/// (OIDC needs a cached token, which does not exist here).
pub fn speculative_start(uri: &Uri) -> Option<(Vec<u8>, Option<Scram>)> {
    let mechanism = speculative_mechanism(uri)?;
    let mut w = Writer::with_capacity(160);
    if is_x509(mechanism) {
        x509_cmd(uri, &mut w);
        w.utf8(b"db", b"$external");
        return Some((w.finish(), None));
    }
    let algo = scram_algo(mechanism)?;
    let mut s = new_scram(uri, algo);
    // errors are ignored: the handshake proceeds and the explicit authentication reports them
    sasl_start_cmd(&mut s, &mut w).ok()?;
    w.utf8(b"db", uri.auth_source().as_bytes());
    Some((w.finish(), Some(s)))
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

/// Runs one authentication command on the connection being authenticated, with the given $db
/// (the auth source for SCRAM, "$external" for PLAIN and X509): the reply, or the error (the
/// callers convert it to an authentication error, as libmongoc does).
pub type RunCmd<'a> = dyn FnMut(&str, &[u8]) -> Result<Vec<u8>, Error> + 'a;

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
        crate::log::debug(crate::log::CLUSTER, || "SCRAM: authentication failed".to_owned());
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
pub fn scram_continue(scram: &mut Scram, start_reply: &[u8], db: &str, run: &mut RunCmd) -> Result<(), Error> {
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
        let reply = run(db, &w.finish()).map_err(auth_error)?;
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
    let db = scram_db(uri);
    let reply = run(db, &w.finish()).map_err(auth_error)?;
    scram_continue(&mut s, &reply, db, run)
}

/// The $db of SCRAM commands (_mongoc_cluster_run_scram_command): the auth source, "admin" when
/// it is empty.
fn scram_db(uri: &Uri) -> &str {
    match uri.auth_source() {
        "" => "admin",
        s => s,
    }
}

/// libmongoc's buffer for the base64 PLAIN payload (`char buf[4096]`, NUL included).
const PLAIN_B64_MAX: usize = 4096;

/// The saslStart command of _mongoc_cluster_auth_node_plain: one message, "\0user\0password"
/// in base64 as a string payload (strings end at a NUL as in C).
fn plain_cmd(uri: &Uri) -> Result<Vec<u8>, Error> {
    let cstr = |s: Option<&str>| -> Vec<u8> { s.unwrap_or("").bytes().take_while(|&b| b != 0).collect() };
    let mut msg = Vec::new();
    msg.push(0);
    msg.extend(cstr(uri.username.as_deref()));
    msg.push(0);
    msg.extend(cstr(uri.password.as_deref()));
    if msg.len().div_ceil(3) * 4 >= PLAIN_B64_MAX {
        return Err(Error::new(domain::CLIENT, code::CLIENT_AUTHENTICATE, "failed base64 encoding message"));
    }
    let mut payload = scram::b64_encode(&msg).into_bytes();
    scram::wipe(&mut msg);
    let mut w = Writer::with_capacity(payload.len() + 80);
    w.int32(b"saslStart", 1);
    w.utf8(b"mechanism", b"PLAIN");
    w.utf8(b"payload", &payload);
    w.int32(b"autoAuthorize", 1);
    scram::wipe(&mut payload);
    Ok(w.finish())
}

/// _mongoc_cluster_auth_node_plain / _mongoc_cluster_auth_node_x509: one command on $external
/// (whatever the auth source), its error converted to an authentication error.
fn auth_external(cmd: &[u8], run: &mut RunCmd) -> Result<(), Error> {
    run("$external", cmd).map(drop).map_err(auth_error)
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
    let Some(mechanism) = speculative_mechanism(uri) else { return Ok(false) };
    if speculative.is_empty() {
        return Ok(false);
    }
    // X509: a hello reply with a speculativeAuthenticate document is a successful authentication
    if is_x509(mechanism) {
        return Ok(true);
    }
    let Some(s) = scram else { return Ok(false) };
    if scram_algo(mechanism).is_none() || s.step != 1 {
        return Ok(false);
    }
    scram_continue(s, speculative, scram_db(uri), run).map(|()| true).inspect_err(|e| {
        crate::log::debug(crate::log::CLUSTER, || format!("Speculative authentication failed: {}", e.message));
    })
}

/// _mongoc_cluster_auth_node: authenticates the connection with the URI's mechanism (or the
/// negotiated default: SCRAM-SHA-256 when the server lists it, else SCRAM-SHA-1).
pub fn authenticate(uri: &Uri, mechs: SaslMechs, run: &mut RunCmd) -> Result<(), Error> {
    auth_node(uri, mechs, run).inspect_err(|e| crate::log::error(crate::log::CLUSTER, || format!("Authentication failed: {}", e.message)))
}

fn auth_node(uri: &Uri, mechs: SaslMechs, run: &mut RunCmd) -> Result<(), Error> {
    let mechanism = match uri.auth_mechanism() {
        Some(m) => m,
        None if mechs.scram_sha_256 => "SCRAM-SHA-256",
        None => "SCRAM-SHA-1",
    };
    if let Some(algo) = scram_algo(mechanism) {
        return auth_scram(uri, algo, run);
    }
    if is_x509(mechanism) {
        let mut w = Writer::with_capacity(80);
        x509_cmd(uri, &mut w);
        return auth_external(&w.finish(), run);
    }
    if mechanism.eq_ignore_ascii_case("PLAIN") {
        // the credentials are cleared from the command once it has run
        let mut cmd = plain_cmd(uri)?;
        let r = auth_external(&cmd, run);
        scram::wipe(&mut cmd);
        return r;
    }
    const KNOWN: &[&str] = &["GSSAPI", "MONGODB-AWS", "MONGODB-OIDC"];
    if KNOWN.iter().any(|k| k.eq_ignore_ascii_case(mechanism)) {
        return Err(unavailable(mechanism));
    }
    Err(Error::new(domain::CLIENT, code::CLIENT_AUTHENTICATE, format!("Unknown authentication mechanism \"{mechanism}\".")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uri(s: &str) -> Uri {
        Uri::parse(s).unwrap()
    }

    fn field<'a>(doc: &'a [u8], k: &[u8]) -> Option<Value<'a>> {
        let mut it = Iter::new(doc)?;
        it.find(k).then(|| it.value())
    }

    /// Runs `auth_node` with a fake server: records (db, command) and answers with `reply`.
    fn run_auth(u: &Uri, reply: Result<Vec<u8>, Error>) -> (Result<(), Error>, Vec<(String, Vec<u8>)>) {
        let mut sent = Vec::new();
        let r = auth_node(u, SaslMechs::default(), &mut |db: &str, body: &[u8]| {
            sent.push((db.to_owned(), body.to_vec()));
            reply.clone()
        });
        (r, sent)
    }

    fn ok_reply() -> Vec<u8> {
        let mut w = Writer::with_capacity(32);
        w.double(b"ok", 1.0);
        w.finish()
    }

    #[test]
    fn plain_conversation() {
        // the auth source does not change where PLAIN authenticates (libmongoc: "$external")
        for s in ["mongodb://us%C3%A9r:p%40ss@h/?authMechanism=PLAIN", "mongodb://us%C3%A9r:p%40ss@h/db?authMechanism=PLAIN&authSource=admin"] {
            let (r, sent) = run_auth(&uri(s), Ok(ok_reply()));
            r.unwrap();
            assert_eq!(sent.len(), 1);
            let (db, cmd) = &sent[0];
            assert_eq!(db, "$external");
            assert!(matches!(field(cmd, b"saslStart"), Some(Value::Int32(1))));
            assert!(matches!(field(cmd, b"mechanism"), Some(Value::Utf8(b"PLAIN"))));
            assert!(matches!(field(cmd, b"autoAuthorize"), Some(Value::Int32(1))));
            let Some(Value::Utf8(p)) = field(cmd, b"payload") else { panic!("payload is a string") };
            assert_eq!(scram::b64_decode(p, 64).unwrap(), "\0usér\0p@ss".as_bytes());
        }
        // an empty password
        let (_, sent) = run_auth(&uri("mongodb://u:@h/?authMechanism=PLAIN"), Ok(ok_reply()));
        let Some(Value::Utf8(p)) = field(&sent[0].1, b"payload") else { panic!() };
        assert_eq!(scram::b64_decode(p, 64).unwrap(), b"\0u\0");
    }

    #[test]
    fn plain_payload_bound() {
        // libmongoc's 4096-byte base64 buffer (NUL included): at most 3069 bytes of message
        let make = |n: usize| {
            let mut u = uri("mongodb://u:p@h/?authMechanism=PLAIN");
            u.password = Some("x".repeat(n - 3));
            u
        };
        assert!(plain_cmd(&make(3069)).is_ok());
        let (r, sent) = run_auth(&make(3070), Ok(ok_reply()));
        let e = r.unwrap_err();
        assert_eq!((e.domain, e.code, e.message.as_str()), (domain::CLIENT, code::CLIENT_AUTHENTICATE, "failed base64 encoding message"));
        assert!(sent.is_empty());
    }

    #[test]
    fn x509_conversation() {
        let (r, sent) = run_auth(&uri("mongodb://CN%3Dclient%2COU%3Dx@h/?authMechanism=MONGODB-X509"), Ok(ok_reply()));
        r.unwrap();
        let (db, cmd) = &sent[0];
        assert_eq!(db, "$external");
        assert!(matches!(field(cmd, b"authenticate"), Some(Value::Int32(1))));
        assert!(matches!(field(cmd, b"mechanism"), Some(Value::Utf8(b"MONGODB-X509"))));
        assert!(matches!(field(cmd, b"user"), Some(Value::Utf8(b"CN=client,OU=x"))));
        // without a username the server takes it from the certificate: no "user"
        let (_, sent) = run_auth(&uri("mongodb://h/?authMechanism=MONGODB-X509"), Ok(ok_reply()));
        assert!(field(&sent[0].1, b"user").is_none());
    }

    #[test]
    fn external_errors_are_authentication_errors() {
        let server = Error::new(domain::SERVER, 18, "Authentication failed.");
        for s in ["mongodb://u:p@h/?authMechanism=PLAIN", "mongodb://h/?authMechanism=MONGODB-X509"] {
            let (r, _) = run_auth(&uri(s), Err(server.clone()));
            let e = r.unwrap_err();
            assert_eq!((e.domain, e.code, e.message.as_str()), (domain::CLIENT, code::CLIENT_AUTHENTICATE, "Authentication failed."));
        }
    }

    #[test]
    fn x509_speculative() {
        let u = uri("mongodb://h/?authMechanism=MONGODB-X509");
        let (doc, scram) = speculative_start(&u).unwrap();
        assert!(scram.is_none());
        assert!(matches!(field(&doc, b"authenticate"), Some(Value::Int32(1))));
        assert!(matches!(field(&doc, b"db"), Some(Value::Utf8(b"$external"))));
        let mut never = |_: &str, _: &[u8]| -> Result<Vec<u8>, Error> { panic!("no command") };
        // a reply with speculativeAuthenticate completes it; none means explicit authentication
        assert!(finish_speculative(&u, &ok_reply(), None, &mut never).unwrap());
        assert!(!finish_speculative(&u, &[], None, &mut never).unwrap());
        // PLAIN is never speculated
        assert!(speculative_start(&uri("mongodb://u:p@h/?authMechanism=PLAIN")).is_none());
    }

    #[test]
    fn other_mechanisms_fail_closed() {
        for (m, s) in [("GSSAPI", "mongodb://u@h/?authMechanism=GSSAPI"), ("MONGODB-AWS", "mongodb://h/?authMechanism=MONGODB-AWS")] {
            let (r, sent) = run_auth(&uri(s), Ok(ok_reply()));
            assert_eq!(r.unwrap_err().message, format!("The {m} authentication mechanism is not supported by this build yet"));
            assert!(sent.is_empty());
        }
    }
}
