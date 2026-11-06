//! TLS on rustls with the graviola crypto provider (no C): the settings of
//! mongoc-ssl.c / mongoc-openssl.c (_mongoc_openssl_ctx_new) and phongo's driver options.
//!
//! Verification is on unless an explicit opt-in turns part of it off:
//! - the certificate chain against tlsCAFile / the `ca_dir` driver option, else the system's
//!   trust store (SSL_CERT_FILE / SSL_CERT_DIR, else the usual bundle files), with the
//!   `crl_file` driver option's CRLs checked for the server certificate (X509_V_FLAG_CRL_CHECK);
//! - the host name (or IP address) against the certificate.
//! tlsAllowInvalidCertificates skips the certificate checks, the host name included (OpenSSL
//! checks the name as part of the verification it then ignores); tlsAllowInvalidHostnames skips
//! the name check only; tlsInsecure both. The handshake signatures are always verified.

use crate::error::{code, domain, Error};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::WebPkiServerVerifier;
use rustls::crypto::CryptoProvider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, CertificateRevocationListDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{CertificateError, ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};
use std::sync::Arc;

/// The TLS settings of a client (mongoc_ssl_opt_t plus the internal OCSP options).
#[derive(Clone, Default, PartialEq, Eq)]
pub struct TlsOptions {
    pub ca_file: Option<String>,
    pub ca_dir: Option<String>,
    pub crl_file: Option<String>,
    pub pem_file: Option<String>,
    pub pem_pwd: Option<String>,
    pub allow_invalid_certificates: bool,
    pub allow_invalid_hostname: bool,
    pub disable_ocsp_endpoint_check: bool,
    pub disable_certificate_revocation_check: bool,
}

impl std::fmt::Debug for TlsOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.debug_struct("TlsOptions")
            .field("ca_file", &self.ca_file)
            .field("ca_dir", &self.ca_dir)
            .field("crl_file", &self.crl_file)
            .field("pem_file", &self.pem_file)
            .field("pem_pwd", &self.pem_pwd.as_ref().map(|_| "******"))
            .field("allow_invalid_certificates", &self.allow_invalid_certificates)
            .field("allow_invalid_hostname", &self.allow_invalid_hostname)
            .finish_non_exhaustive()
    }
}

impl TlsOptions {
    /// _mongoc_ssl_opts_from_uri (+ phongo_mongoc_ssl_opts_from_uri): the URI's TLS options.
    pub fn from_uri(uri: &crate::uri::Uri) -> TlsOptions {
        let insecure = uri.get_bool("tlsinsecure", false);
        let s = |k: &str| uri.options.str(k).map(str::to_owned);
        TlsOptions {
            ca_file: s("tlscafile"),
            ca_dir: None,
            crl_file: None,
            pem_file: s("tlscertificatekeyfile"),
            pem_pwd: s("tlscertificatekeyfilepassword"),
            allow_invalid_certificates: uri.get_bool("tlsallowinvalidcertificates", insecure),
            allow_invalid_hostname: uri.get_bool("tlsallowinvalidhostnames", insecure),
            disable_ocsp_endpoint_check: uri.get_bool("tlsdisableocspendpointcheck", insecure),
            disable_certificate_revocation_check: uri.get_bool("tlsdisablecertificaterevocationcheck", false),
        }
    }
}

/// The CPU instructions graviola requires (it asserts them at every entry point: checked here
/// first so that a CPU without them gets an error, never a panic inside a handshake).
#[cfg(target_arch = "x86_64")]
const CPU_FEATURES: &str = "AES-NI, PCLMULQDQ, AVX, AVX2, BMI1 and ADX";
#[cfg(target_arch = "aarch64")]
const CPU_FEATURES: &str = "NEON, AES, PMULL and SHA2";

/// The crypto provider, or the error every TLS connection gets on a CPU graviola cannot use.
fn provider() -> Result<Arc<CryptoProvider>, Error> {
    static P: std::sync::OnceLock<Option<Arc<CryptoProvider>>> = std::sync::OnceLock::new();
    provider_if(P.get_or_init(|| graviola::util::is_cpu_supported().then(|| Arc::new(rustls_graviola::default_provider()))))
}

fn provider_if(p: &Option<Arc<CryptoProvider>>) -> Result<Arc<CryptoProvider>, Error> {
    p.clone().ok_or_else(|| handshake_error(format!("TLS is not available on this CPU: the TLS library needs the {CPU_FEATURES} instructions")))
}


/// The error of a TLS setup or handshake failure (libmongoc reports both at connection time).
pub fn handshake_error(detail: impl std::fmt::Display) -> Error {
    Error::new(domain::STREAM, code::STREAM_SOCKET, format!("TLS handshake failed: {detail}"))
}

/// Files are read whole: certificate, key, CA and CRL files above this size are refused.
const MAX_FILE: u64 = 16 * 1024 * 1024;
/// Files of a `ca_dir` read at most.
const MAX_DIR_FILES: usize = 10_000;

fn read_file(path: &str) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let f = std::fs::File::open(path)?;
    let mut out = Vec::new();
    f.take(MAX_FILE + 1).read_to_end(&mut out)?;
    if out.len() as u64 > MAX_FILE {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "file too large"));
    }
    Ok(out)
}

fn certs_of(pem: &[u8]) -> Vec<CertificateDer<'static>> {
    CertificateDer::pem_slice_iter(pem).filter_map(Result::ok).collect()
}

/// The system trust store as OpenSSL's default verify paths find it.
fn system_roots(store: &mut RootCertStore) {
    let env_file = std::env::var("SSL_CERT_FILE").ok();
    let env_dir = std::env::var("SSL_CERT_DIR").ok();
    if env_file.is_some() || env_dir.is_some() {
        if let Some(f) = env_file {
            add_file(store, &f);
        }
        if let Some(d) = env_dir {
            for d in d.split(':') {
                add_dir(store, d);
            }
        }
        return;
    }
    // the bundle locations of the common distributions and of macOS (openssl-probe's list)
    const FILES: &[&str] = &[
        "/etc/ssl/certs/ca-certificates.crt",
        "/etc/pki/tls/certs/ca-bundle.crt",
        "/etc/ssl/ca-bundle.pem",
        "/etc/pki/ca-trust/extracted/pem/tls-ca-bundle.pem",
        "/etc/pki/tls/cacert.pem",
        "/etc/ssl/cert.pem",
        "/opt/homebrew/etc/openssl@3/cert.pem",
        "/usr/local/etc/openssl@3/cert.pem",
        "/usr/local/etc/openssl/cert.pem",
    ];
    for f in FILES {
        if std::path::Path::new(f).is_file() {
            add_file(store, f);
            return;
        }
    }
}

fn add_file(store: &mut RootCertStore, path: &str) -> usize {
    let Ok(pem) = read_file(path) else { return 0 };
    let (added, _) = store.add_parsable_certificates(certs_of(&pem));
    added
}

/// The certificates of a CA directory, as OpenSSL's hashed-directory lookup finds them: the
/// files named `<8 hex digits>.<n>` (c_rehash / openssl rehash names; CRLs are `.r<n>`).
fn add_dir(store: &mut RootCertStore, dir: &str) -> usize {
    let Ok(rd) = std::fs::read_dir(dir) else { return 0 };
    let mut added = 0;
    for e in rd.flatten().take(MAX_DIR_FILES) {
        let name = e.file_name();
        let n = name.to_string_lossy();
        let hashed = n.len() > 9 && n.as_bytes()[8] == b'.' && n[..8].bytes().all(|b| b.is_ascii_hexdigit()) && n[9..].bytes().all(|b| b.is_ascii_digit());
        let p = e.path();
        if hashed && p.is_file() {
            added += add_file(store, &p.to_string_lossy());
        }
    }
    added
}

/// Loads the client certificate chain and key of `pem_file` (_mongoc_openssl_setup_pem_file).
fn client_identity(pem_file: &str, password: Option<&str>) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>), String> {
    let pem = read_file(pem_file).map_err(|_| format!("Cannot find certificate in '{pem_file}'"))?;
    let chain = certs_of(&pem);
    if chain.is_empty() {
        return Err(format!("Cannot find certificate in '{pem_file}'"));
    }
    let key = match crate::tls_key::private_key(&pem, password) {
        Ok(Some(k)) => k,
        Ok(None) | Err(_) => return Err(format!("Cannot find private key in: '{pem_file}'")),
    };
    Ok((chain, key))
}

/// Builds the rustls configuration for `o` (_mongoc_openssl_ctx_new). Errors carry libmongoc's
/// messages (it logs them and fails the connection).
pub fn client_config(o: &TlsOptions) -> Result<Arc<ClientConfig>, Error> {
    let fail = |m: String| {
        crate::log::error(crate::log::MONGOC, || m.clone());
        handshake_error(m)
    };
    let p = provider().inspect_err(|e| crate::log::error(crate::log::MONGOC, || e.message.clone()))?;
    let builder = ClientConfig::builder_with_provider(p.clone()).with_safe_default_protocol_versions().map_err(|e| handshake_error(e))?;
    let mut roots = RootCertStore::empty();
    if o.ca_file.is_some() || o.ca_dir.is_some() {
        let mut n = 0;
        if let Some(f) = &o.ca_file {
            n += add_file(&mut roots, f);
        }
        if let Some(d) = &o.ca_dir {
            n += add_dir(&mut roots, d);
        }
        if n == 0 {
            return Err(fail(format!("Cannot load Certificate Authorities from '{}' and '{}'", o.ca_file.as_deref().unwrap_or("(null)"), o.ca_dir.as_deref().unwrap_or("(null)"))));
        }
    } else {
        system_roots(&mut roots);
    }
    let roots = Arc::new(roots);
    let mut crls = Vec::new();
    if let Some(f) = &o.crl_file {
        let pem = read_file(f).map_err(|_| fail(format!("Cannot load CRL file '{f}'")))?;
        crls = CertificateRevocationListDer::pem_slice_iter(&pem).filter_map(Result::ok).collect();
        if crls.is_empty() {
            return Err(fail(format!("Cannot load CRL file '{f}'")));
        }
    }
    let verifier: Arc<dyn ServerCertVerifier> = if o.allow_invalid_certificates {
        Arc::new(NoChainCheck { provider: p.clone() })
    } else {
        let mut b = WebPkiServerVerifier::builder_with_provider(roots.clone(), p.clone());
        if !crls.is_empty() {
            // an expired CRL fails the handshake (OpenSSL: "CRL has expired")
            b = b.with_crls(crls).only_check_end_entity_revocation().enforce_revocation_expiration();
        }
        let inner = b.build().map_err(|e| fail(format!("Cannot set up certificate verification: {e}")))?;
        if o.allow_invalid_hostname {
            Arc::new(AnyName(inner))
        } else {
            inner
        }
    };
    let builder = builder.dangerous().with_custom_certificate_verifier(verifier);
    let mut cfg = match &o.pem_file {
        Some(f) => {
            let (chain, key) = client_identity(f, o.pem_pwd.as_deref()).map_err(fail)?;
            builder.with_client_auth_cert(chain, key).map_err(|_| fail(format!("Cannot load private key: '{f}'")))?
        }
        None => builder.with_no_client_auth(),
    };
    cfg.enable_sni = true;
    Ok(Arc::new(cfg))
}

/// The server name to verify (and to send as SNI unless it is an address).
pub fn server_name(host: &str) -> Result<ServerName<'static>, Error> {
    let h = host.trim_start_matches('[').trim_end_matches(']');
    ServerName::try_from(h.to_owned()).map_err(|_| handshake_error(format!("invalid host name '{host}'")))
}

/// tlsAllowInvalidHostnames: the chain is verified, a name mismatch is accepted.
#[derive(Debug)]
struct AnyName(Arc<WebPkiServerVerifier>);

impl ServerCertVerifier for AnyName {
    fn verify_server_cert(&self, end_entity: &CertificateDer<'_>, intermediates: &[CertificateDer<'_>], server_name: &ServerName<'_>, ocsp: &[u8], now: UnixTime) -> Result<ServerCertVerified, rustls::Error> {
        match self.0.verify_server_cert(end_entity, intermediates, server_name, ocsp, now) {
            // webpki checks the chain (and revocation) before the name
            Err(rustls::Error::InvalidCertificate(CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. })) => Ok(ServerCertVerified::assertion()),
            r => r,
        }
    }
    fn verify_tls12_signature(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.0.verify_tls12_signature(m, c, d)
    }
    fn verify_tls13_signature(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.0.verify_tls13_signature(m, c, d)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.supported_verify_schemes()
    }
}

/// tlsAllowInvalidCertificates (SSL_VERIFY_NONE): no certificate check; the handshake
/// signatures are verified.
#[derive(Debug)]
struct NoChainCheck {
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for NoChainCheck {
    fn verify_server_cert(&self, _end_entity: &CertificateDer<'_>, _intermediates: &[CertificateDer<'_>], _server_name: &ServerName<'_>, _ocsp: &[u8], _now: UnixTime) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(m, c, d, &self.provider.signature_verification_algorithms)
    }
    fn verify_tls13_signature(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(m, c, d, &self.provider.signature_verification_algorithms)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

/// The handshake error text for a rustls error (OpenSSL's verify-error wording where one
/// exists, so that messages read as the reference's).
pub fn describe(e: &rustls::Error) -> String {
    match e {
        rustls::Error::InvalidCertificate(c) => {
            let (n, s) = match c {
                CertificateError::UnknownIssuer => (20, "unable to get local issuer certificate".to_owned()),
                CertificateError::Expired | CertificateError::ExpiredContext { .. } => (10, "certificate has expired".to_owned()),
                CertificateError::NotValidYet | CertificateError::NotValidYetContext { .. } => (9, "certificate is not yet valid".to_owned()),
                CertificateError::Revoked => (23, "certificate revoked".to_owned()),
                CertificateError::UnknownRevocationStatus => (3, "unable to get certificate CRL".to_owned()),
                CertificateError::ExpiredRevocationList | CertificateError::ExpiredRevocationListContext { .. } => (12, "CRL has expired".to_owned()),
                CertificateError::BadSignature => (7, "certificate signature failure".to_owned()),
                CertificateError::NotValidForNameContext { expected: ServerName::IpAddress(_), .. } => (64, "IP address mismatch".to_owned()),
                CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. } => (62, "hostname mismatch".to_owned()),
                other => (0, format!("{other:?}")),
            };
            if n == 0 {
                format!("certificate verify failed: {s}")
            } else {
                format!("certificate verify failed ({n}): {s}")
            }
        }
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn unsupported_cpu_is_an_error() {
        let e = super::provider_if(&None).unwrap_err();
        assert!(e.message.starts_with("TLS handshake failed: TLS is not available on this CPU: the TLS library needs the "), "{}", e.message);
        assert!(super::provider().is_ok(), "this CPU should be supported");
    }
}
