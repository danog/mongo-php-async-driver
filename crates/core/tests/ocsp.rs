//! OCSP checks of the TLS client (crates/core/src/ocsp.rs): stapled responses and the responder
//! fetch. A rustls server staples responses that this test builds and signs; a small HTTP
//! responder on 127.0.0.1:28041 (the URL in the fixture certificates) answers OCSP requests.
//! Fixtures: tools/ocsp-fixtures.sh. One test function, the cases run in order (the status cache
//! and the responder port are process-wide).

use der::asn1::{Any, BitString, GeneralizedTime, ObjectIdentifier, OctetString};
use der::{Decode, Encode};
use mongo_core::tls::{client_config, describe, TlsOptions};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::SignatureScheme;
use sha1::{Digest, Sha1};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use x509_cert::spki::AlgorithmIdentifierOwned;
use x509_cert::Certificate;
use x509_ocsp::{BasicOcspResponse, CertId, CertStatus, OcspGeneralizedTime, OcspRequest, OcspResponse, OcspResponseStatus, ResponderId, ResponseBytes, ResponseData, RevokedInfo, SingleResponse, Version};

const RESPONDER: &str = "127.0.0.1:28041";
const ID_SHA1: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.3.14.3.2.26");
const SHA256_RSA: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113549.1.1.11");
const ID_PKIX_OCSP_BASIC: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.3.6.1.5.5.7.48.1.1");
const ID_PKIX_OCSP_NONCE: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.3.6.1.5.5.7.48.1.2");

fn path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/ocsp").join(name)
}

fn cert_der(name: &str) -> CertificateDer<'static> {
    CertificateDer::from_pem_file(path(&format!("{name}.pem"))).unwrap()
}

fn cert(name: &str) -> Certificate {
    Certificate::from_der(&cert_der(name)).unwrap()
}

fn key(name: &str) -> PrivateKeyDer<'static> {
    PrivateKeyDer::from_pem_file(path(&format!("{name}.key"))).unwrap()
}

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64
}

fn gt(at: i64) -> OcspGeneralizedTime {
    OcspGeneralizedTime(GeneralizedTime::from_unix_duration(Duration::from_secs(at as u64)).unwrap())
}

fn cert_id(leaf: &Certificate, issuer: &Certificate) -> CertId {
    let t = &issuer.tbs_certificate;
    CertId {
        hash_algorithm: AlgorithmIdentifierOwned { oid: ID_SHA1, parameters: Some(Any::null()) },
        issuer_name_hash: OctetString::new(Sha1::digest(t.subject.to_der().unwrap()).to_vec()).unwrap(),
        issuer_key_hash: OctetString::new(Sha1::digest(t.subject_public_key_info.subject_public_key.raw_bytes()).to_vec()).unwrap(),
        serial_number: leaf.tbs_certificate.serial_number.clone(),
    }
}

/// What a test response says and who signs it.
struct Resp {
    leaf: &'static str,
    status: CertStatus,
    this: i64,
    next: Option<i64>,
    /// the key that signs; `by_name` names that certificate as the responder (else by key hash)
    signer: &'static str,
    by_name: bool,
    /// certificates included in the response
    certs: &'static [&'static str],
    /// the responder claims to be the CA (signed with `signer`'s key anyway)
    claim_ca: bool,
}

impl Resp {
    fn good(leaf: &'static str) -> Resp {
        Resp { leaf, status: CertStatus::good(), this: now() - 60, next: Some(now() + 3600), signer: "ca", by_name: true, certs: &[], claim_ca: false }
    }
    fn revoked(leaf: &'static str) -> Resp {
        Resp { status: CertStatus::revoked(RevokedInfo { revocation_time: gt(now() - 3600), revocation_reason: Some(x509_cert::ext::pkix::CrlReason::KeyCompromise) }), ..Resp::good(leaf) }
    }
    fn der(&self) -> Vec<u8> {
        let ca = cert("ca");
        let signer = cert(self.signer);
        let named = if self.claim_ca { &ca } else { &signer };
        let responder_id = if self.by_name {
            ResponderId::ByName(named.tbs_certificate.subject.clone())
        } else {
            ResponderId::ByKey(OctetString::new(Sha1::digest(named.tbs_certificate.subject_public_key_info.subject_public_key.raw_bytes()).to_vec()).unwrap())
        };
        let tbs = ResponseData {
            version: Version::V1,
            responder_id,
            produced_at: gt(now()),
            responses: vec![SingleResponse { cert_id: cert_id(&cert(self.leaf), &ca), cert_status: self.status, this_update: gt(self.this), next_update: self.next.map(gt), single_extensions: None }],
            response_extensions: None,
        };
        let msg = tbs.to_der().unwrap();
        let sk = rustls_graviola::default_provider().key_provider.load_private_key(key(self.signer)).unwrap();
        let sig = sk.choose_scheme(&[SignatureScheme::RSA_PKCS1_SHA256]).unwrap().sign(&msg).unwrap();
        let basic = BasicOcspResponse {
            tbs_response_data: tbs,
            signature_algorithm: AlgorithmIdentifierOwned { oid: SHA256_RSA, parameters: Some(Any::null()) },
            signature: BitString::from_bytes(&sig).unwrap(),
            certs: if self.certs.is_empty() { None } else { Some(self.certs.iter().map(|c| cert(c)).collect()) },
        };
        response(OcspResponseStatus::Successful, Some(basic.to_der().unwrap()))
    }
}

fn response(status: OcspResponseStatus, basic: Option<Vec<u8>>) -> Vec<u8> {
    OcspResponse { response_status: status, response_bytes: basic.map(|b| ResponseBytes { response_type: ID_PKIX_OCSP_BASIC, response: OctetString::new(b).unwrap() }) }.to_der().unwrap()
}

/// One TLS connection to a server presenting `leaf` (and stapling `staple` unless empty).
/// Returns the client's handshake error text, if any.
fn connect(leaf: &str, staple: Vec<u8>, opts: &TlsOptions) -> Result<(), String> {
    let provider = Arc::new(rustls_graviola::default_provider());
    let server = rustls::ServerConfig::builder_with_provider(provider).with_safe_default_protocol_versions().unwrap().with_no_client_auth().with_single_cert_with_ocsp(vec![cert_der(leaf)], key(leaf), staple).unwrap();
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    let t = std::thread::spawn(move || {
        let (mut s, _) = l.accept().unwrap();
        s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        let mut c = rustls::ServerConnection::new(Arc::new(server)).unwrap();
        while c.is_handshaking() {
            if c.complete_io(&mut s).is_err() {
                return;
            }
        }
        let _ = c.complete_io(&mut s);
    });
    let cfg = client_config(opts).map_err(|e| e.message)?;
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let mut c = rustls::ClientConnection::new(cfg, ServerName::try_from("localhost").unwrap()).unwrap();
    let mut r = Ok(());
    while c.is_handshaking() {
        if let Err(e) = c.complete_io(&mut s) {
            r = Err(match e.get_ref().and_then(|x| x.downcast_ref::<rustls::Error>()) {
                Some(te) => describe(te),
                None => e.to_string(),
            });
            break;
        }
    }
    drop(s);
    t.join().unwrap();
    r
}

/// An OCSP responder on 127.0.0.1:28041 answering every request with `body`; it checks that
/// each request is a well-formed OCSPRequest for `leaf` with a 16-byte nonce.
struct Responder {
    hits: Arc<AtomicUsize>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    t: Option<std::thread::JoinHandle<()>>,
}

impl Responder {
    fn start(leaf: &'static str, body: Vec<u8>) -> Responder {
        let l = TcpListener::bind(RESPONDER).unwrap();
        l.set_nonblocking(true).unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (h, st) = (hits.clone(), stop.clone());
        let want = cert_id(&cert(leaf), &cert("ca"));
        let t = std::thread::spawn(move || {
            while !st.load(Ordering::SeqCst) {
                let Ok((mut s, _)) = l.accept() else {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                };
                s.set_nonblocking(false).unwrap();
                s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let (head_end, len) = loop {
                    let n = s.read(&mut chunk).unwrap();
                    assert!(n > 0, "request cut short");
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..i]).to_string();
                        assert!(head.starts_with("POST / HTTP/1.0\r\n"), "{head}");
                        assert!(head.contains("\r\nHost: 127.0.0.1:28041\r\n") && head.contains("\r\nConnection: close\r\n") && head.contains("\r\nContent-Type: application/ocsp-request"), "{head}");
                        let len: usize = head.lines().find_map(|l| l.strip_prefix("Content-Length: ")).unwrap().parse().unwrap();
                        break (i + 4, len);
                    }
                };
                while buf.len() < head_end + len {
                    let n = s.read(&mut chunk).unwrap();
                    assert!(n > 0);
                    buf.extend_from_slice(&chunk[..n]);
                }
                let req = OcspRequest::from_der(&buf[head_end..head_end + len]).unwrap();
                assert_eq!(req.tbs_request.request_list.len(), 1);
                assert_eq!(req.tbs_request.request_list[0].req_cert, want);
                let ext = req.tbs_request.request_extensions.as_ref().unwrap();
                assert!(ext.iter().any(|e| e.extn_id == ID_PKIX_OCSP_NONCE && OctetString::from_der(e.extn_value.as_bytes()).unwrap().as_bytes().len() == 16));
                h.fetch_add(1, Ordering::SeqCst);
                let _ = s.write_all(format!("HTTP/1.0 200 OK\r\nContent-Type: application/ocsp-response\r\nContent-Length: {}\r\n\r\n", body.len()).as_bytes());
                let _ = s.write_all(&body);
            }
        });
        Responder { hits, stop, t: Some(t) }
    }
    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
}

impl Drop for Responder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.t.take() {
            t.join().unwrap();
        }
    }
}

const FAILED: &str = "Failed OCSP verification";

#[test]
fn ocsp() {
    // libmongoc's messages, shown when a case fails
    mongo_core::log::set_handler(|l, d, m| eprintln!("{} {d}: {m}", l.as_str()));
    mongo_core::log::set_max_level(Some(mongo_core::log::Level::Debug));
    let ca = path("ca.pem").to_string_lossy().into_owned();
    let opts = TlsOptions { ca_file: Some(ca.clone()), ..Default::default() };
    let fresh = || mongo_core::ocsp::clear_cache();

    // stapled responses
    fresh();
    assert_eq!(connect("leaf", Resp::good("leaf").der(), &opts), Ok(()), "good staple");
    fresh();
    assert_eq!(connect("leaf", Resp::revoked("leaf").der(), &opts).unwrap_err(), FAILED, "revoked staple");
    fresh();
    assert_eq!(connect("leaf", Resp { signer: "responder", by_name: false, certs: &["responder"], ..Resp::good("leaf") }.der(), &opts), Ok(()), "delegated responder");
    fresh();
    assert_eq!(connect("leaf", Resp { signer: "responder", certs: &["responder"], ..Resp::revoked("leaf") }.der(), &opts).unwrap_err(), FAILED, "revoked by a delegated responder");
    for (what, r) in [
        ("responder without OCSPSigning", Resp { signer: "noeku", certs: &["noeku"], ..Resp::good("leaf") }),
        ("delegated responder not included", Resp { signer: "responder", ..Resp::good("leaf") }),
        ("signed by another CA's key as the CA", Resp { signer: "other", claim_ca: true, ..Resp::good("leaf") }),
        ("signed by another CA", Resp { signer: "other", certs: &["other"], ..Resp::good("leaf") }),
        ("signed by the server certificate itself", Resp { signer: "leaf", certs: &["leaf"], ..Resp::good("leaf") }),
        ("expired", Resp { this: now() - 7200, next: Some(now() - 3600), ..Resp::good("leaf") }),
        ("not yet valid", Resp { this: now() + 3600, next: Some(now() + 7200), ..Resp::good("leaf") }),
        ("for another certificate", Resp::good("noaia")),
    ] {
        fresh();
        assert_eq!(connect("leaf", r.der(), &opts).unwrap_err(), FAILED, "{what}");
    }
    fresh();
    assert_eq!(connect("leaf", b"not DER".to_vec(), &opts).unwrap_err(), FAILED, "malformed staple");
    fresh();
    assert_eq!(connect("leaf", response(OcspResponseStatus::TryLater, None), &opts).unwrap_err(), FAILED, "tryLater staple");
    fresh();
    assert_eq!(connect("leaf", Resp { next: None, ..Resp::good("leaf") }.der(), &opts), Ok(()), "no nextUpdate");
    assert_eq!(mongo_core::ocsp::cache_len(), 0, "a response without nextUpdate is not cached");

    // the opt-outs
    fresh();
    let no_rev = TlsOptions { disable_certificate_revocation_check: true, ..opts.clone() };
    assert_eq!(connect("leaf", Resp::revoked("leaf").der(), &no_rev), Ok(()), "tlsDisableCertificateRevocationCheck");
    let invalid = TlsOptions { allow_invalid_certificates: true, ..opts.clone() };
    assert_eq!(connect("leaf", Resp::revoked("leaf").der(), &invalid), Ok(()), "tlsAllowInvalidCertificates (tlsInsecure)");
    let hostnames = TlsOptions { allow_invalid_hostname: true, ..opts.clone() };
    assert_eq!(connect("leaf", Resp::revoked("leaf").der(), &hostnames).unwrap_err(), FAILED, "tlsAllowInvalidHostnames keeps OCSP");

    // no staple: must staple
    fresh();
    assert_eq!(connect("staple", Vec::new(), &opts).unwrap_err(), FAILED, "must staple without a staple");
    assert_eq!(connect("staple", Resp::good("staple").der(), &opts), Ok(()), "must staple with a staple");

    // no staple: the responder
    fresh();
    assert_eq!(connect("leaf", Vec::new(), &opts), Ok(()), "unreachable responder: soft-fail");
    assert_eq!(connect("noaia", Vec::new(), &opts), Ok(()), "no responder named");
    {
        let r = Responder::start("leaf", Resp::revoked("leaf").der());
        fresh();
        assert_eq!(connect("leaf", Vec::new(), &opts).unwrap_err(), FAILED, "revoked by the responder");
        assert_eq!(r.hits(), 1);
        fresh();
        let no_endpoint = TlsOptions { disable_ocsp_endpoint_check: true, ..opts.clone() };
        assert_eq!(connect("leaf", Vec::new(), &no_endpoint), Ok(()), "tlsDisableOCSPEndpointCheck");
        assert_eq!(r.hits(), 1, "no request with tlsDisableOCSPEndpointCheck");
    }
    {
        let r = Responder::start("leaf", Resp::good("leaf").der());
        fresh();
        assert_eq!(connect("leaf", Vec::new(), &opts), Ok(()), "good from the responder");
        assert_eq!(connect("leaf", Vec::new(), &opts), Ok(()), "cached");
        assert_eq!(r.hits(), 1, "the second connection uses the cache");
    }
    {
        // a cached revocation applies to later connections without a staple
        let r = Responder::start("leaf", Resp::revoked("leaf").der());
        fresh();
        assert_eq!(connect("leaf", Vec::new(), &opts).unwrap_err(), FAILED);
        drop(r);
        assert_eq!(connect("leaf", Vec::new(), &opts).unwrap_err(), FAILED, "cached revocation");
        assert_eq!(connect("leaf", Resp::good("leaf").der(), &opts).unwrap_err(), FAILED, "the cache is consulted before the staple");
    }
    for (what, body) in [
        ("garbage", b"garbage".to_vec()),
        ("tryLater", response(OcspResponseStatus::TryLater, None)),
        ("bad signature", Resp { signer: "other", claim_ca: true, ..Resp::revoked("leaf") }.der()),
        ("expired", Resp { this: now() - 7200, next: Some(now() - 3600), ..Resp::revoked("leaf") }.der()),
    ] {
        let r = Responder::start("leaf", body);
        fresh();
        assert_eq!(connect("leaf", Vec::new(), &opts), Ok(()), "broken responder answer ({what}): soft-fail");
        assert_eq!(r.hits(), 1, "{what}");
    }
    fresh();
}
