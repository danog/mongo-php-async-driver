//! OCSP checks of the server certificate, as libmongoc's OpenSSL path does them
//! (mongoc-openssl.c _mongoc_ocsp_tlsext_status and _mongoc_contact_ocsp_responder,
//! mongoc-ocsp-cache.c), after the certificate chain and the host name are verified:
//!
//! - the stapled response (rustls always asks for one) is parsed and verified: response status,
//!   signature, responder authorisation, the response for this certificate's CertID, freshness.
//!   Any failure of a stapled response fails the handshake; a "revoked" status fails it.
//! - without a staple: a certificate marked "must staple" (TLS feature status_request) fails;
//!   otherwise, unless tlsDisableOCSPEndpointCheck, the OCSP responders named in the
//!   certificate are asked over plain HTTP (POST, 5 s each). Only a verified "revoked" answer
//!   fails the handshake (soft-fail: unreachable or broken responders are ignored).
//! - verified "good" and "revoked" statuses are cached per CertID until their nextUpdate.
//!
//! Not applied at all with tlsAllowInvalidCertificates (and tlsInsecure, which implies it) or
//! tlsDisableCertificateRevocationCheck.
//!
//! Safety differences from libmongoc (DIVERGENCES.md): the response must be signed by the
//! certificate's issuer or by a responder certificate the issuer issued for OCSP signing
//! (RFC 6960 4.2.2.2; OpenSSL with OCSP_TRUSTOTHER also accepts any certificate of the verified
//! chain, the server's own included); responses without nextUpdate are not cached; the cache,
//! the responder URLs tried and every size are bounded.

use crate::log;
use der::asn1::{Any, ObjectIdentifier, OctetString};
use der::{Decode, Encode};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{CertificateError, DigitallySignedStruct, OtherError, RootCertStore, SignatureScheme};
use sha1::{Digest, Sha1};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use x509_cert::ext::pkix::name::GeneralName;
use x509_cert::ext::pkix::{AuthorityInfoAccessSyntax, CrlReason, ExtendedKeyUsage};
use x509_cert::spki::{AlgorithmIdentifierOwned, SubjectPublicKeyInfoOwned};
use x509_cert::Certificate;
use x509_ocsp::{BasicOcspResponse, CertId, CertStatus, OcspRequest, OcspResponse, OcspResponseStatus, Request, ResponderId, TbsRequest, Version};

const ID_SHA1: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.3.14.3.2.26");
const ID_PE_AUTHORITY_INFO_ACCESS: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.3.6.1.5.5.7.1.1");
const ID_PE_TLSFEATURE: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.3.6.1.5.5.7.1.24");
const ID_AD_OCSP: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.3.6.1.5.5.7.48.1");
const ID_PKIX_OCSP_BASIC: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.3.6.1.5.5.7.48.1.1");
const ID_PKIX_OCSP_NONCE: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.3.6.1.5.5.7.48.1.2");
const ID_CE_EXT_KEY_USAGE: ObjectIdentifier = ObjectIdentifier::new_unwrap("2.5.29.37");
const ID_KP_OCSP_SIGNING: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.3.6.1.5.5.7.3.9");

/// MONGOC_OCSP_REQUEST_TIMEOUT, per responder.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// Responder URLs of one certificate tried at most (libmongoc: all).
const MAX_URLS: usize = 4;
/// Certificates of the chain or of a response looked at, at most.
const MAX_CERTS: usize = 16;
/// Cached statuses at most (libmongoc: unbounded).
const MAX_CACHE: usize = 1024;

/// The handshake error of a failed OCSP check ("TLS handshake failed: Failed OCSP verification").
#[derive(Debug)]
pub struct OcspFailed;

impl std::fmt::Display for OcspFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("Failed OCSP verification")
    }
}

impl std::error::Error for OcspFailed {}

/// _mongoc_ocsp_tlsext_status's outcomes.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Success,
    Revoked,
    /// fails the handshake for a stapled response only
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Status {
    Good,
    /// the CRL reason (-1: none given)
    Revoked(i32),
    Unknown,
}

/// The verifier: the chain and name checks of `inner`, then OCSP.
#[derive(Debug)]
pub struct OcspVerifier {
    pub inner: Arc<dyn ServerCertVerifier>,
    pub roots: Arc<RootCertStore>,
    pub provider: Arc<CryptoProvider>,
    /// !tlsDisableOCSPEndpointCheck
    pub endpoint_check: bool,
}

impl ServerCertVerifier for OcspVerifier {
    fn verify_server_cert(&self, end_entity: &CertificateDer<'_>, intermediates: &[CertificateDer<'_>], server_name: &ServerName<'_>, ocsp: &[u8], now: UnixTime) -> Result<ServerCertVerified, rustls::Error> {
        let verified = self.inner.verify_server_cert(end_entity, intermediates, server_name, ocsp, now)?;
        let stapled = !ocsp.is_empty();
        match self.status(end_entity, intermediates, ocsp, now.as_secs()) {
            Outcome::Success => Ok(verified),
            Outcome::Error if !stapled => Ok(verified),
            _ => Err(rustls::Error::InvalidCertificate(CertificateError::Other(OtherError(Arc::new(OcspFailed))))),
        }
    }
    fn verify_tls12_signature(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(m, c, d)
    }
    fn verify_tls13_signature(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(m, c, d)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

/// The issuer of the server certificate: its subject (DER) and key.
struct Issuer {
    name: Vec<u8>,
    spki: SubjectPublicKeyInfoOwned,
}

/// A DER SEQUENCE around `contents` (webpki keeps a trust anchor's fields without the tag).
fn sequence(contents: &[u8]) -> Vec<u8> {
    let mut v = vec![0x30];
    let n = contents.len();
    if n < 0x80 {
        v.push(n as u8);
    } else {
        let b = n.to_be_bytes();
        let skip = b.iter().take_while(|x| **x == 0).count();
        v.push(0x80 | (b.len() - skip) as u8);
        v.extend_from_slice(&b[skip..]);
    }
    v.extend_from_slice(contents);
    v
}

/// The DER contents of an AlgorithmIdentifier (how rustls names algorithms).
fn alg_contents(a: &AlgorithmIdentifierOwned) -> Option<Vec<u8>> {
    let mut v = a.oid.to_der().ok()?;
    if let Some(p) = &a.parameters {
        v.extend_from_slice(&p.to_der().ok()?);
    }
    Some(v)
}

fn sha1(b: &[u8]) -> Vec<u8> {
    Sha1::digest(b).to_vec()
}

fn extension<'a>(c: &'a Certificate, oid: &ObjectIdentifier) -> Option<&'a [u8]> {
    c.tbs_certificate.extensions.as_ref()?.iter().find(|e| e.extn_id == *oid).map(|e| e.extn_value.as_bytes())
}

/// X509_get1_ocsp: the OCSP URIs of the Authority Information Access extension.
fn ocsp_urls(c: &Certificate) -> Vec<String> {
    let Some(v) = extension(c, &ID_PE_AUTHORITY_INFO_ACCESS) else { return Vec::new() };
    let Ok(aia) = AuthorityInfoAccessSyntax::from_der(v) else { return Vec::new() };
    aia.0
        .iter()
        .filter(|d| d.access_method == ID_AD_OCSP)
        .filter_map(|d| match &d.access_location {
            GeneralName::UniformResourceIdentifier(u) => Some(u.as_str().to_owned()),
            _ => None,
        })
        .take(MAX_URLS)
        .collect()
}

/// _get_must_staple: a TLS feature extension listing status_request (5).
fn must_staple(c: &Certificate) -> bool {
    extension(c, &ID_PE_TLSFEATURE).and_then(|v| Vec::<u32>::from_der(v).ok()).is_some_and(|f| f.contains(&5))
}

fn time_ok(not_before: u64, not_after: u64, now: u64) -> bool {
    not_before <= now && now <= not_after
}

/// OCSP_check_validity(thisUpdate, nextUpdate, 0, -1).
fn validity_error(this: u64, next: Option<u64>, now: u64) -> Option<&'static str> {
    if this > now {
        return Some("status not yet valid");
    }
    match next {
        Some(n) if n < now => Some("status expired"),
        Some(n) if n < this => Some("status next update before this update"),
        _ => None,
    }
}

/// OCSP_crl_reason_str
fn reason_str(r: i32) -> &'static str {
    match r {
        -1 => "noStatus",
        0 => "unspecified",
        1 => "keyCompromise",
        2 => "cACompromise",
        3 => "affiliationChanged",
        4 => "superseded",
        5 => "cessationOfOperation",
        6 => "certificateHold",
        8 => "removeFromCRL",
        9 => "privilegeWithdrawn",
        10 => "aACompromise",
        _ => "(UNKNOWN)",
    }
}

/// OCSP_response_status_str
fn response_status_str(s: OcspResponseStatus) -> (i32, &'static str) {
    match s {
        OcspResponseStatus::Successful => (0, "successful"),
        OcspResponseStatus::MalformedRequest => (1, "malformedrequest"),
        OcspResponseStatus::InternalError => (2, "internalerror"),
        OcspResponseStatus::TryLater => (3, "trylater"),
        OcspResponseStatus::SigRequired => (5, "sigrequired"),
        OcspResponseStatus::Unauthorized => (6, "unauthorized"),
    }
}

impl OcspVerifier {
    /// A signature by `key` (an algorithm of the crypto provider, as for the handshake).
    fn verify(&self, key: &SubjectPublicKeyInfoOwned, alg: &AlgorithmIdentifierOwned, msg: &[u8], sig: &[u8]) -> bool {
        let (Some(pk), Some(sa)) = (alg_contents(&key.algorithm), alg_contents(alg)) else { return false };
        let Some(raw) = key.subject_public_key.as_bytes() else { return false };
        self.provider
            .signature_verification_algorithms
            .all
            .iter()
            .find(|a| a.public_key_alg_id().as_ref() == pk.as_slice() && a.signature_alg_id().as_ref() == sa.as_slice())
            .is_some_and(|a| a.verify_signature(raw, msg, sig).is_ok())
    }

    /// Whether `issuer` signed `c`.
    fn signed_by(&self, c: &Certificate, issuer: &SubjectPublicKeyInfoOwned) -> bool {
        let (Ok(tbs), Some(sig)) = (c.tbs_certificate.to_der(), c.signature.as_bytes()) else { return false };
        self.verify(issuer, &c.signature_algorithm, &tbs, sig)
    }

    /// _get_issuer: the certificate (from the chain the server sent, or a trust anchor) whose
    /// subject is the server certificate's issuer and whose key signed it.
    fn issuer(&self, leaf: &Certificate, intermediates: &[CertificateDer<'_>]) -> Option<Issuer> {
        let want = leaf.tbs_certificate.issuer.to_der().ok()?;
        for c in intermediates.iter().take(MAX_CERTS) {
            let Ok(c) = Certificate::from_der(c) else { continue };
            if c.tbs_certificate.subject.to_der().ok().as_deref() == Some(want.as_slice()) && self.signed_by(leaf, &c.tbs_certificate.subject_public_key_info) {
                return Some(Issuer { name: want, spki: c.tbs_certificate.subject_public_key_info });
            }
        }
        for a in &self.roots.roots {
            if sequence(a.subject.as_ref()) != want {
                continue;
            }
            let Ok(spki) = SubjectPublicKeyInfoOwned::from_der(&sequence(a.subject_public_key_info.as_ref())) else { continue };
            if self.signed_by(leaf, &spki) {
                return Some(Issuer { name: want, spki });
            }
        }
        None
    }

    /// OCSP_basic_verify, with the responder authorisation of RFC 6960 4.2.2.2: signed by the
    /// issuer, or by a certificate (included in the response) that the issuer issued with the
    /// OCSPSigning extended key usage and that is valid now.
    fn verify_basic(&self, basic: &BasicOcspResponse, issuer: &Issuer, now: u64) -> bool {
        let (Ok(tbs), Some(sig)) = (basic.tbs_response_data.to_der(), basic.signature.as_bytes()) else { return false };
        let Some(issuer_key) = issuer.spki.subject_public_key.as_bytes() else { return false };
        let names = |rid: &ResponderId, name: &[u8], key: &[u8]| match rid {
            ResponderId::ByName(n) => n.to_der().ok().as_deref() == Some(name),
            ResponderId::ByKey(k) => k.as_bytes() == sha1(key).as_slice(),
        };
        let rid = &basic.tbs_response_data.responder_id;
        if names(rid, &issuer.name, issuer_key) {
            return self.verify(&issuer.spki, &basic.signature_algorithm, &tbs, sig);
        }
        for c in basic.certs.iter().flatten().take(MAX_CERTS) {
            let t = &c.tbs_certificate;
            let (Ok(subject), Some(key)) = (t.subject.to_der(), t.subject_public_key_info.subject_public_key.as_bytes()) else { continue };
            if !names(rid, &subject, key) {
                continue;
            }
            let delegated = t.issuer.to_der().ok().as_deref() == Some(issuer.name.as_slice())
                && self.signed_by(c, &issuer.spki)
                && extension(c, &ID_CE_EXT_KEY_USAGE).and_then(|v| ExtendedKeyUsage::from_der(v).ok()).is_some_and(|e| e.0.contains(&ID_KP_OCSP_SIGNING))
                && time_ok(t.validity.not_before.to_unix_duration().as_secs(), t.validity.not_after.to_unix_duration().as_secs(), now);
            return delegated && self.verify(&t.subject_public_key_info, &basic.signature_algorithm, &tbs, sig);
        }
        false
    }

    /// _mongoc_ocsp_tlsext_status
    fn status(&self, ee: &CertificateDer<'_>, intermediates: &[CertificateDer<'_>], staple: &[u8], now: u64) -> Outcome {
        let stapled = !staple.is_empty();
        let soft = |m: &str| {
            if stapled {
                log::error(log::MONGOC, || m.to_owned());
            } else {
                log::debug(log::MONGOC, || m.to_owned());
            }
        };
        let Ok(leaf) = Certificate::from_der(ee) else {
            log::error(log::MONGOC, || "Could not get issuer from peer cert".to_owned());
            return Outcome::Error;
        };
        let urls = if stapled || !self.endpoint_check { Vec::new() } else { ocsp_urls(&leaf) };
        let must = !stapled && must_staple(&leaf);
        // nothing to look at and nothing cached: the outcome cannot be other than success
        if !stapled && !must && urls.is_empty() && cache::is_empty() {
            return Outcome::Success;
        }
        let Some(issuer) = self.issuer(&leaf, intermediates) else {
            log::error(log::MONGOC, || "Could not get issuer from peer cert".to_owned());
            return Outcome::Error;
        };
        let Some(issuer_key) = issuer.spki.subject_public_key.as_bytes() else { return Outcome::Error };
        let id = CertId {
            hash_algorithm: AlgorithmIdentifierOwned { oid: ID_SHA1, parameters: Some(Any::null()) },
            issuer_name_hash: OctetString::new(sha1(&issuer.name)).expect("20 bytes"),
            issuer_key_hash: OctetString::new(sha1(issuer_key)).expect("20 bytes"),
            serial_number: leaf.tbs_certificate.serial_number.clone(),
        };
        let key = cache::Key::of(&id);
        if let Some(st) = cache::get(&key, now) {
            return validate(st, &key, None);
        }
        let resp = if stapled {
            match OcspResponse::from_der(staple) {
                Ok(r) => r,
                Err(_) => {
                    log::error(log::MONGOC, || "Failed to parse OCSP response".to_owned());
                    return Outcome::Error;
                }
            }
        } else {
            if must {
                log::error(log::MONGOC, || "Server must contain a stapled response".to_owned());
                return Outcome::Revoked;
            }
            match contact(&id, &urls) {
                Some(r) => r,
                None => {
                    if !urls.is_empty() {
                        log::debug(log::MONGOC, || "Soft-fail: No OCSP responder could be reached".to_owned());
                    }
                    return Outcome::Success;
                }
            }
        };
        if resp.response_status != OcspResponseStatus::Successful {
            let (n, s) = response_status_str(resp.response_status);
            soft(&format!("OCSP response error {n} {s}"));
            return Outcome::Error;
        }
        let basic = resp.response_bytes.as_ref().filter(|b| b.response_type == ID_PKIX_OCSP_BASIC).and_then(|b| BasicOcspResponse::from_der(b.response.as_bytes()).ok());
        let Some(basic) = basic else {
            soft("Could not find BasicOCSPResponse");
            return Outcome::Error;
        };
        if !self.verify_basic(&basic, &issuer, now) {
            soft("OCSP response failed verification");
            return Outcome::Error;
        }
        let single = basic.tbs_response_data.responses.iter().find(|r| {
            r.cert_id.hash_algorithm.oid == ID_SHA1 && r.cert_id.issuer_name_hash == id.issuer_name_hash && r.cert_id.issuer_key_hash == id.issuer_key_hash && r.cert_id.serial_number == id.serial_number
        });
        let Some(single) = single else {
            soft("No OCSP response found for the peer certificate");
            return Outcome::Error;
        };
        let this = single.this_update.0.to_unix_duration().as_secs();
        let next = single.next_update.map(|t| t.0.to_unix_duration().as_secs());
        if let Some(e) = validity_error(this, next, now) {
            soft(&format!("OCSP response has expired: {e}"));
            return Outcome::Error;
        }
        let st = match &single.cert_status {
            CertStatus::Good(_) => Status::Good,
            CertStatus::Revoked(info) => Status::Revoked(info.revocation_reason.map_or(-1, |r: CrlReason| r as i32)),
            CertStatus::Unknown(_) => Status::Unknown,
        };
        validate(st, &key, Some((this, next)))
    }
}

/// The `validate:` part: cache and outcome of a status.
fn validate(st: Status, key: &cache::Key, times: Option<(u64, Option<u64>)>) -> Outcome {
    match st {
        Status::Good => {
            if let Some((this, next)) = times {
                cache::set(key, st, this, next);
            }
            Outcome::Success
        }
        Status::Revoked(r) => {
            log::error(log::MONGOC, || format!("OCSP Certificate Status: Revoked. Reason: {}", reason_str(r)));
            if let Some((this, next)) = times {
                cache::set(key, st, this, next);
            }
            Outcome::Revoked
        }
        Status::Unknown => {
            log::debug(log::MONGOC, || "OCSP Certificate Status: Unknown".to_owned());
            Outcome::Success
        }
    }
}

/// The DER OCSPRequest for `id`, with a random 16-byte nonce (OCSP_request_add1_nonce).
fn request_der(id: &CertId) -> Option<Vec<u8>> {
    let mut nonce = [0u8; 16];
    getrandom::fill(&mut nonce).ok()?;
    let ext = x509_cert::ext::Extension { extn_id: ID_PKIX_OCSP_NONCE, critical: false, extn_value: OctetString::new(OctetString::new(nonce.to_vec()).ok()?.to_der().ok()?).ok()? };
    let req = OcspRequest {
        tbs_request: TbsRequest { version: Version::V1, requestor_name: None, request_list: vec![Request { req_cert: id.clone(), single_request_extensions: None }], request_extensions: Some(vec![ext]) },
        optional_signature: None,
    };
    req.to_der().ok()
}

/// _mongoc_contact_ocsp_responder: the first parsable response of the certificate's responders.
fn contact(id: &CertId, urls: &[String]) -> Option<OcspResponse> {
    for url in urls {
        let u = match crate::http::parse_url(url) {
            Ok(u) => u,
            Err(e) => {
                log::debug(log::MONGOC, || e);
                continue;
            }
        };
        let Some(req) = request_der(id) else {
            log::debug(log::MONGOC, || "Could not encode OCSP request".to_owned());
            continue;
        };
        match crate::http::send("POST", &u, "Content-Type: application/ocsp-request\r\n", &req, REQUEST_TIMEOUT) {
            Err(e) => log::debug(log::MONGOC, || format!("Could not send HTTP request: {e}")),
            Ok(r) => match OcspResponse::from_der(&r.body) {
                Ok(resp) if !r.body.is_empty() => return Some(resp),
                _ => log::debug(log::MONGOC, || "Could not parse OCSP response from HTTP response".to_owned()),
            },
        }
    }
    None
}

/// The process-wide status cache (mongoc-ocsp-cache.c), keyed by CertID.
mod cache {
    use super::*;

    #[derive(Clone, PartialEq, Eq)]
    pub struct Key(pub Vec<u8>, pub Vec<u8>, pub Vec<u8>);

    impl Key {
        pub fn of(id: &CertId) -> Key {
            Key(id.issuer_name_hash.as_bytes().to_vec(), id.issuer_key_hash.as_bytes().to_vec(), id.serial_number.as_bytes().to_vec())
        }
    }

    struct Entry {
        key: Key,
        status: Status,
        this: u64,
        next: u64,
    }

    static CACHE: Mutex<Vec<Entry>> = Mutex::new(Vec::new());

    fn lock() -> std::sync::MutexGuard<'static, Vec<Entry>> {
        CACHE.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn is_empty() -> bool {
        lock().is_empty()
    }

    /// _mongoc_ocsp_cache_get_status: an expired entry is dropped.
    pub fn get(k: &Key, now: u64) -> Option<Status> {
        let mut c = lock();
        let i = c.iter().position(|e| e.key == *k)?;
        if validity_error(c[i].this, Some(c[i].next), now).is_some() {
            c.remove(i);
            return None;
        }
        Some(c[i].status)
    }

    /// _mongoc_ocsp_cache_set_resp: a new entry, or a fresher one (later nextUpdate). A
    /// response without nextUpdate is not cached.
    pub fn set(k: &Key, status: Status, this: u64, next: Option<u64>) {
        let Some(next) = next else { return };
        let mut c = lock();
        if let Some(e) = c.iter_mut().find(|e| e.key == *k) {
            if next > e.next {
                *e = Entry { key: k.clone(), status, this, next };
            }
            return;
        }
        if c.len() >= MAX_CACHE {
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
            c.retain(|e| validity_error(e.this, Some(e.next), now).is_none());
            if c.len() >= MAX_CACHE {
                c.remove(0);
            }
        }
        c.push(Entry { key: k.clone(), status, this, next });
    }

    pub fn clear() {
        lock().clear();
    }

    pub fn len() -> usize {
        lock().len()
    }
}

/// Empties the status cache (tests).
#[doc(hidden)]
pub fn clear_cache() {
    cache::clear();
}

/// Cached statuses (tests).
#[doc(hidden)]
pub fn cache_len() -> usize {
    cache::len()
}

/// Wraps `inner` with the OCSP checks.
pub fn verifier(inner: Arc<dyn ServerCertVerifier>, roots: Arc<RootCertStore>, provider: Arc<CryptoProvider>, endpoint_check: bool) -> Arc<dyn ServerCertVerifier> {
    Arc::new(OcspVerifier { inner, roots, provider, endpoint_check })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequence_header() {
        assert_eq!(sequence(&[1, 2]), vec![0x30, 2, 1, 2]);
        let big = vec![0u8; 300];
        assert_eq!(&sequence(&big)[..4], &[0x30, 0x82, 1, 44]);
    }

    #[test]
    fn validity() {
        assert_eq!(validity_error(10, Some(20), 15), None);
        assert_eq!(validity_error(10, None, 1000), None);
        assert_eq!(validity_error(16, Some(20), 15), Some("status not yet valid"));
        assert_eq!(validity_error(10, Some(14), 15), Some("status expired"));
    }

    #[test]
    fn cache_rules() {
        clear_cache();
        let k = cache::Key(vec![1], vec![2], vec![3]);
        cache::set(&k, Status::Good, 10, None);
        assert!(cache::get(&k, 15).is_none(), "no nextUpdate: not cached");
        cache::set(&k, Status::Good, 10, Some(20));
        cache::set(&k, Status::Revoked(1), 10, Some(19));
        assert_eq!(cache::get(&k, 15), Some(Status::Good), "an older nextUpdate does not replace");
        cache::set(&k, Status::Revoked(1), 12, Some(30));
        assert_eq!(cache::get(&k, 15), Some(Status::Revoked(1)));
        assert_eq!(cache::get(&k, 31), None, "expired entries are dropped");
        assert_eq!(cache_len(), 0);
        for i in 0..MAX_CACHE + 10 {
            cache::set(&cache::Key(vec![], vec![], i.to_be_bytes().to_vec()), Status::Good, 0, Some(u64::MAX));
        }
        assert_eq!(cache_len(), MAX_CACHE);
        clear_cache();
    }
}
