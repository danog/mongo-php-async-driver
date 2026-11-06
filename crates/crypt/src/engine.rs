//! The operations of a libmongocrypt handle, driven to completion synchronously: explicit
//! encryption and decryption, document decryption and data key creation. Key vault reads and KMS
//! round trips go through `Io`.

use crate::bson::{append_wrapped, value_bytes, wrap_value, SUBTYPE_ENCRYPTED};
use crate::broker::{KeyBroker, KeyCache, CACHE_EXPIRATION_MS_DEFAULT};
use crate::crypto::{self, Algorithm};
use crate::key::{Ciphertext, FLE1_DETERMINISTIC, FLE1_RANDOM};
use crate::opts::{Kek, KmsProviders};
use mongo_core::bson::iter::{Iter, Value, T_ARRAY, T_BINARY, T_DOCUMENT};
use mongo_core::bson::writer::Writer;

/// A libmongocrypt failure (`MONGOC_ERROR_CLIENT_SIDE_ENCRYPTION`, code 1) or an I/O error of the driver
#[derive(Debug)]
pub enum Error<E> {
    Crypt(String),
    Io(E),
}

impl<E> From<String> for Error<E> {
    fn from(s: String) -> Self {
        Error::Crypt(s)
    }
}
impl<E> From<&str> for Error<E> {
    fn from(s: &str) -> Self {
        Error::Crypt(s.to_string())
    }
}

pub type Result<T, E> = std::result::Result<T, Error<E>>;

/// The driver side of a libmongocrypt state machine
pub trait Io {
    type Error;
    /// `MONGOCRYPT_CTX_NEED_MONGO_KEYS`: the key vault documents matching `filter` (readConcern majority)
    fn find_keys(&mut self, filter: &[u8]) -> std::result::Result<Vec<Vec<u8>>, Self::Error>;
    /// `MONGOCRYPT_CTX_NEED_KMS`: one TLS round trip, reading until `done` accepts the bytes read
    /// `feed` gets each read's bytes and returns true once no more are wanted
    fn kms(&mut self, req: &crate::kms::KmsRequest, feed: &mut dyn FnMut(&[u8]) -> bool) -> std::result::Result<(), KmsFailure<Self::Error>>;
    /// A stream error (MONGOC_ERROR_STREAM_SOCKET) with this message
    fn stream_error(&self, msg: String) -> Self::Error;
    /// On-demand AWS credentials (`_mongoc_aws_credentials_obtain`): key id, secret, session token
    fn aws_credentials(&mut self) -> std::result::Result<(String, String, Option<String>), Self::Error> {
        Err(self.stream_error("on-demand AWS credentials are not available".into()))
    }
    /// An Azure IMDS access token for Key Vault and its lifetime
    fn azure_imds_token(&mut self) -> std::result::Result<(String, std::time::Duration), Self::Error> {
        Err(self.stream_error("on-demand Azure credentials are not available".into()))
    }
    /// A GCP metadata server access token
    fn gcp_metadata_token(&mut self) -> std::result::Result<String, Self::Error> {
        Err(self.stream_error("on-demand GCP credentials are not available".into()))
    }
    /// `MONGOCRYPT_CTX_NEED_MONGO_COLLINFO`: listCollections on `db` with `filter`, every result
    fn list_collections(&mut self, db: &str, filter: &[u8]) -> std::result::Result<Vec<Vec<u8>>, Self::Error> {
        let _ = (db, filter);
        Err(self.stream_error("listCollections is not available".into()))
    }
}

/// The options of automatic encryption that shape commands
#[derive(Clone, Debug, Default)]
pub struct AutoOpts {
    pub schema_map: Option<Vec<u8>>,
    pub encrypted_fields_map: Option<Vec<u8>>,
}

/// Clears the per-operation on-demand credentials when the operation ends
struct OnDemand<'a>(&'a std::sync::Mutex<Option<KmsProviders>>);

impl Drop for OnDemand<'_> {
    fn drop(&mut self) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

/// Where a KMS round trip failed
pub enum KmsFailure<E> {
    /// connecting or the TLS handshake (the error is reported as is)
    Connect(E),
    Write(E),
    Read(E),
}

/// `_mongocrypt_apply_default_port`
fn with_default_port(e: &crate::opts::Endpoint, port: u16) -> String {
    match e.port {
        Some(_) => e.host_and_port.clone(),
        None => format!("{}:{port}", e.host_and_port),
    }
}

/// `should_retry_http` for Azure requests
fn azure_retry(status: i32) -> bool {
    [408, 429, 500, 502, 503, 504].contains(&status)
}

/// `should_retry_http` for GCP requests
fn gcp_retry(status: i32) -> bool {
    status == 408 || status == 429 || status / 500 == 1
}

/// libmongocrypt's `kms_max_attempts`
const KMS_MAX_ATTEMPTS: u32 = 3;

/// `backoff_time_usec`: exponential backoff with full jitter
fn backoff(attempts: u32) -> std::time::Duration {
    let base = 200_000u64;
    let b = (base << (attempts - 1).min(20)).min(20_000_000);
    let r = crypto::random_uint64(b).unwrap_or(0);
    std::time::Duration::from_micros(r + 1)
}

/// `mongocrypt_t`
pub struct Crypt {
    pub kms: KmsProviders,
    pub cache: KeyCache,
    /// the TLS options per KMS provider id ("tlsOptions")
    pub tls: Vec<(String, mongo_core::tls::TlsOptions)>,
    /// `cache_oauth`: OAuth access tokens per KMS provider id, with when they expire
    oauth: std::sync::Mutex<Vec<(String, String, std::time::Instant)>>,
    /// the on-demand credentials of the running operation (`per_ctx_kms_providers`)
    ondemand: std::sync::Mutex<Option<KmsProviders>>,
    /// libmongoc's `azure_token`: the IMDS token and when it was issued plus its lifetime
    azure_imds: std::sync::Mutex<Option<(String, std::time::Instant)>>,
    /// `cache_collinfo`
    pub collinfo: crate::autoenc::CollinfoCache,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fle1Algorithm {
    Deterministic,
    Random,
}

/// The key of an explicit encryption: `keyId` or `keyAltName`
#[derive(Clone, Debug)]
pub enum KeySel {
    Id([u8; 16]),
    AltName(Vec<u8>),
}

impl Crypt {
    pub fn new(kms_providers: &[u8], key_expiration_ms: Option<u64>) -> std::result::Result<Crypt, String> {
        let kms = KmsProviders::parse(kms_providers)?;
        Ok(Crypt { kms, cache: KeyCache::new(key_expiration_ms.unwrap_or(CACHE_EXPIRATION_MS_DEFAULT)), tls: Vec::new(), oauth: Default::default(), ondemand: Default::default(), azure_imds: Default::default(), collinfo: Default::default() })
    }

    /// The credentials of a KMS provider: configured, or provided on demand for this operation
    fn creds(&self, kmsid: &str) -> Option<crate::opts::Creds> {
        if let Some(c) = self.kms.get(kmsid) {
            return Some(c.clone());
        }
        let od = self.ondemand.lock().unwrap_or_else(|e| e.into_inner());
        od.as_ref().and_then(|k| k.get(kmsid).cloned())
    }

    /// Whether `kmsid` is a provider configured as `{}` (credentials on demand)
    fn on_demand(&self, kmsid: &str) -> bool {
        !kmsid.contains(':') && self.kms.get(kmsid).is_none() && crate::opts::parse_kmsid(kmsid).is_ok_and(|(t, _)| self.kms.need_credentials.contains(&t))
    }

    /// libmongoc's `_state_need_kms_credentials`: credentials for every on-demand provider, in
    /// the order aws, azure, gcp; they hold until the returned guard is dropped
    fn obtain_credentials<I: Io>(&self, io: &mut I) -> Result<OnDemand<'_>, I::Error> {
        use crate::opts::KmsType;
        let mut w = Writer::with_capacity(256);
        let need = &self.kms.need_credentials;
        if need.contains(&KmsType::Aws) {
            let (akid, secret, token) = io.aws_credentials().map_err(Error::Io)?;
            let p = w.begin(T_DOCUMENT, b"aws");
            w.utf8(b"accessKeyId", akid.as_bytes());
            w.utf8(b"secretAccessKey", secret.as_bytes());
            if let Some(t) = &token {
                w.utf8(b"sessionToken", t.as_bytes());
            }
            w.end(p);
        }
        if need.contains(&KmsType::Azure) {
            // reused while it is valid for at least one more minute
            let mut cache = self.azure_imds.lock().unwrap_or_else(|e| e.into_inner());
            let fresh = cache.as_ref().filter(|(_, exp)| *exp >= std::time::Instant::now() + std::time::Duration::from_secs(60)).map(|(t, _)| t.clone());
            let token = match fresh {
                Some(t) => t,
                None => {
                    let issued = std::time::Instant::now();
                    let (t, life) = io.azure_imds_token().map_err(Error::Io)?;
                    *cache = Some((t.clone(), issued.checked_add(life).unwrap_or(issued)));
                    t
                }
            };
            let p = w.begin(T_DOCUMENT, b"azure");
            w.utf8(b"accessToken", token.as_bytes());
            w.end(p);
        }
        if need.contains(&KmsType::Gcp) {
            let t = io.gcp_metadata_token().map_err(Error::Io)?;
            let p = w.begin(T_DOCUMENT, b"gcp");
            w.utf8(b"accessToken", t.as_bytes());
            w.end(p);
        }
        let provided = KmsProviders::parse(&w.finish())?;
        *self.ondemand.lock().unwrap_or_else(|e| e.into_inner()) = Some(provided);
        Ok(OnDemand(&self.ondemand))
    }

    /// The TLS options of a KMS provider (libmongoc's per-kmsid `mongoc_ssl_opt_t`)
    fn tls_for(&self, kmsid: &str) -> mongo_core::tls::TlsOptions {
        self.tls.iter().find(|(k, _)| k == kmsid).map(|(_, o)| o.clone()).unwrap_or_default()
    }

    /// One KMS operation with libmongocrypt's retries: `make` builds the request, `parse` reads the
    /// complete HTTP response (Err(Some(status)) when the status is retryable)
    fn kms_call<I: Io, T>(
        &self,
        io: &mut I,
        req: &crate::kms::KmsRequest,
        retryable: bool,
        http_retry: fn(i32) -> bool,
        parse: &dyn Fn(&crate::kms::Response) -> std::result::Result<T, String>,
    ) -> Result<T, I::Error> {
        let mut attempts = 0u32;
        loop {
            if attempts > 0 {
                std::thread::sleep(backoff(attempts));
            }
            let mut parser = crate::kms::ResponseParser::new();
            let res = io.kms(req, &mut |b: &[u8]| parser.feed(b));
            match res {
                Ok(()) => {
                    let r = parser.finish()?;
                    if retryable && http_retry(r.status) {
                        if attempts >= KMS_MAX_ATTEMPTS {
                            return Err(format!("KMS request failed after maximum of {KMS_MAX_ATTEMPTS} retries: {}", crate::kms::non200(r.status, &r.body)).into());
                        }
                        attempts += 1;
                        continue;
                    }
                    return Ok(parse(&r)?);
                }
                Err(f) => {
                    // mongocrypt_kms_ctx_fail
                    let msg = if !retryable {
                        "KMS request failed due to network error".to_string()
                    } else if attempts >= KMS_MAX_ATTEMPTS {
                        format!("KMS request failed after {KMS_MAX_ATTEMPTS} retries due to a network error")
                    } else {
                        attempts += 1;
                        continue;
                    };
                    return Err(Error::Io(match f {
                        KmsFailure::Connect(e) => e,
                        KmsFailure::Write(_) => io.stream_error(format!("{msg}. Failed to write to KMS stream: {}", req.endpoint)),
                        KmsFailure::Read(_) => io.stream_error(format!("{msg}. Failed to read from KMS stream to: {}", req.endpoint)),
                    }));
                }
            }
        }
    }

    /// AWS KMS Encrypt/Decrypt (`_mongocrypt_kms_ctx_init_aws_*` + `_ctx_done_aws`)
    fn aws<I: Io>(&self, io: &mut I, kek: &Kek, decrypt: bool, data: &[u8]) -> Result<Vec<u8>, I::Error> {
        let crate::opts::KekProvider::Aws { cmk, region, endpoint } = &kek.provider else {
            return Err("expected aws kms provider".into());
        };
        let creds = self.creds(&kek.kmsid);
        let Some(crate::opts::Creds::Aws { access_key_id, secret_access_key, session_token }) = &creds else {
            return Err(format!("KMS provider `{}` is not configured", kek.kmsid).into());
        };
        // the Host header: decryption uses host:port, encryption the host only (as libmongocrypt)
        let host = endpoint.as_ref().map(|e| if decrypt { e.host_and_port.clone() } else { e.host.clone() });
        let msg = crate::kms::aws_request(decrypt, data, Some(cmk), region, access_key_id, secret_access_key, session_token.as_deref(), host.as_deref(), &crate::kms::amz_datetime())?;
        let ep = endpoint.as_ref().map_or_else(|| format!("kms.{region}.amazonaws.com:443"), |e| with_default_port(e, 443));
        let req = crate::kms::KmsRequest { kmsid: kek.kmsid.clone(), endpoint: ep, message: msg, tls: self.tls_for(&kek.kmsid) };
        let field = if decrypt { "Plaintext" } else { "CiphertextBlob" };
        self.kms_call(io, &req, true, |s| [408, 429, 500, 502, 503, 509].contains(&s), &|r| crate::kms::aws_done(r, field))
    }

    /// `mc_mapof_kmsid_to_token_get_token`
    fn cached_token(&self, kmsid: &str) -> Option<String> {
        let c = self.oauth.lock().unwrap_or_else(|e| e.into_inner());
        c.iter().find(|(k, _, exp)| k == kmsid && std::time::Instant::now() < *exp).map(|(_, t, _)| t.clone())
    }

    /// `mc_mapof_kmsid_to_token_add_response`
    fn cache_token(&self, kmsid: &str, token: String, ttl: std::time::Duration) {
        let exp = std::time::Instant::now().checked_add(ttl).unwrap_or_else(|| std::time::Instant::now() + std::time::Duration::from_secs(86400 * 365));
        let mut c = self.oauth.lock().unwrap_or_else(|e| e.into_inner());
        match c.iter_mut().find(|(k, _, _)| k == kmsid) {
            Some(e) => {
                e.1 = token;
                e.2 = exp;
            }
            None => c.push((kmsid.to_string(), token, exp)),
        }
    }

    /// An Azure access token: configured, cached, or from an OAuth request
    /// (`_mongocrypt_kms_ctx_init_azure_auth`); `key_vault` sets the requested scope
    fn azure_token<I: Io>(&self, io: &mut I, kmsid: &str, key_vault: &crate::opts::Endpoint) -> Result<String, I::Error> {
        let found = self.creds(kmsid);
        let Some(crate::opts::Creds::Azure(creds)) = &found else {
            return Err(format!("KMS provider `{kmsid}` is not configured").into());
        };
        let (tenant_id, client_id, client_secret, ipe) = match creds {
            crate::opts::AzureCreds::AccessToken(t) => return Ok(t.clone()),
            crate::opts::AzureCreds::Client { tenant_id, client_id, client_secret, identity_platform_endpoint } => {
                (tenant_id, client_id, client_secret, identity_platform_endpoint)
            }
        };
        // cached per provider and scope: a token is only sent to the Key Vault domain it was
        // requested for (libmongocrypt caches per provider only; safety divergence)
        let scope = format!("https%3A%2F%2F{}%2F.default", key_vault.domain);
        let cache_key = format!("{kmsid}\n{scope}");
        if let Some(t) = self.cached_token(&cache_key) {
            return Ok(t);
        }
        let (endpoint, host) = match ipe {
            Some(e) => (with_default_port(e, 443), e.host.clone()),
            None => ("login.microsoftonline.com:443".to_string(), "login.microsoftonline.com".to_string()),
        };
        let msg = crate::kms::azure_oauth_request(&host, &scope, tenant_id, client_id, client_secret)?;
        let req = crate::kms::KmsRequest { kmsid: kmsid.to_string(), endpoint, message: msg, tls: self.tls_for(kmsid) };
        let (token, ttl) = self.kms_call(io, &req, true, azure_retry, &crate::kms::oauth_done)?;
        self.cache_token(&cache_key, token.clone(), ttl);
        Ok(token)
    }

    /// Azure Key Vault wrapkey/unwrapkey
    fn azure<I: Io>(&self, io: &mut I, kek: &Kek, wrap: bool, data: &[u8]) -> Result<Vec<u8>, I::Error> {
        let crate::opts::KekProvider::Azure { key_vault_endpoint, key_name, key_version } = &kek.provider else {
            return Err("expected azure kms provider".into());
        };
        let token = self.azure_token(io, &kek.kmsid, key_vault_endpoint)?;
        let msg = crate::kms::azure_wrap_request(wrap, &key_vault_endpoint.host, &token, key_name, key_version.as_deref(), data)?;
        let req = crate::kms::KmsRequest { kmsid: kek.kmsid.clone(), endpoint: with_default_port(key_vault_endpoint, 443), message: msg, tls: self.tls_for(&kek.kmsid) };
        self.kms_call(io, &req, true, azure_retry, &|r| crate::kms::azure_done(r))
    }

    /// A GCP access token: configured, cached, or from a JWT bearer OAuth request
    /// (`_mongocrypt_kms_ctx_init_gcp_auth`); `kms_endpoint` (the key's) sets the requested scope
    fn gcp_token<I: Io>(&self, io: &mut I, kmsid: &str, kms_endpoint: Option<&crate::opts::Endpoint>) -> Result<String, I::Error> {
        let found = self.creds(kmsid);
        let Some(crate::opts::Creds::Gcp(creds)) = &found else {
            return Err(format!("KMS provider `{kmsid}` is not configured").into());
        };
        let (email, private_key, auth) = match creds {
            crate::opts::GcpCreds::AccessToken(t) => return Ok(t.clone()),
            crate::opts::GcpCreds::ServiceAccount { email, private_key, endpoint } => (email, private_key, endpoint),
        };
        let scope = match kms_endpoint {
            Some(e) => format!("https://www.{}/auth/cloudkms", e.domain),
            None => "https://www.googleapis.com/auth/cloudkms".to_string(),
        };
        // cached per provider and scope, as for Azure (safety divergence)
        let cache_key = format!("{kmsid}\n{scope}");
        if let Some(t) = self.cached_token(&cache_key) {
            return Ok(t);
        }
        let (endpoint, host) = match auth {
            Some(e) => (with_default_port(e, 443), e.host.clone()),
            None => ("oauth2.googleapis.com:443".to_string(), "oauth2.googleapis.com".to_string()),
        };
        let audience = format!("https://{host}/token");
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let msg = crate::kms::gcp_oauth_request(&host, email, &audience, &scope, private_key, now)?;
        let req = crate::kms::KmsRequest { kmsid: kmsid.to_string(), endpoint, message: msg, tls: self.tls_for(kmsid) };
        let (token, ttl) = self.kms_call(io, &req, true, gcp_retry, &crate::kms::oauth_done)?;
        self.cache_token(&cache_key, token.clone(), ttl);
        Ok(token)
    }

    /// Google Cloud KMS encrypt/decrypt
    fn gcp<I: Io>(&self, io: &mut I, kek: &Kek, encrypt: bool, data: &[u8]) -> Result<Vec<u8>, I::Error> {
        let crate::opts::KekProvider::Gcp { endpoint, project_id, location, key_ring, key_name, key_version } = &kek.provider else {
            return Err("expected gcp kms provider".into());
        };
        let token = self.gcp_token(io, &kek.kmsid, endpoint.as_ref())?;
        let (ep, host) = match endpoint {
            Some(e) => (with_default_port(e, 443), e.host.clone()),
            None => ("cloudkms.googleapis.com:443".to_string(), "cloudkms.googleapis.com".to_string()),
        };
        // decryption names no key version (the ciphertext carries it), as libmongocrypt
        let key_version = if encrypt { key_version.as_deref() } else { None };
        let key = crate::kms::GcpKey { project_id, location, key_ring, key_name, key_version };
        let msg = crate::kms::gcp_crypt_request(encrypt, &host, &token, &key, data)?;
        let req = crate::kms::KmsRequest { kmsid: kek.kmsid.clone(), endpoint: ep, message: msg, tls: self.tls_for(&kek.kmsid) };
        let field = if encrypt { "ciphertext" } else { "plaintext" };
        self.kms_call(io, &req, true, gcp_retry, &|r| crate::kms::aws_done(r, field))
    }

    /// One KMIP round trip (never retried: KMIP requests are not idempotent)
    fn kmip_call<I: Io, T>(&self, io: &mut I, req: &crate::kms::KmsRequest, parse: &dyn Fn(&[u8]) -> std::result::Result<T, String>) -> Result<T, I::Error> {
        let mut parser = crate::kmip::Parser::default();
        let res = io.kms(req, &mut |b: &[u8]| parser.feed(b));
        match res {
            Ok(()) => Ok(parse(&parser.finish()?)?),
            Err(f) => {
                let msg = "KMS request failed due to network error";
                Err(Error::Io(match f {
                    KmsFailure::Connect(e) => e,
                    KmsFailure::Write(_) => io.stream_error(format!("{msg}. Failed to write to KMS stream: {}", req.endpoint)),
                    KmsFailure::Read(_) => io.stream_error(format!("{msg}. Failed to read from KMS stream to: {}", req.endpoint)),
                }))
            }
        }
    }

    /// The KMIP endpoint: the KEK's, else the provider's (default port 5696)
    fn kmip_request(&self, kek: &Kek, message: Vec<u8>) -> std::result::Result<crate::kms::KmsRequest, String> {
        let crate::opts::KekProvider::Kmip { endpoint, .. } = &kek.provider else {
            return Err("KMS provider is not KMIP".into());
        };
        let configured = match self.creds(&kek.kmsid) {
            Some(crate::opts::Creds::Kmip { endpoint }) => Some(endpoint),
            _ => None,
        };
        let e = endpoint.as_ref().or(configured.as_ref()).ok_or("endpoint not set for KMIP request")?;
        Ok(crate::kms::KmsRequest { kmsid: kek.kmsid.clone(), endpoint: with_default_port(e, 5696), message, tls: self.tls_for(&kek.kmsid) })
    }

    /// The datakey context's KMIP steps (`_kms_kmip_start`)
    fn kmip_wrap<I: Io>(&self, io: &mut I, kek: &Kek, material: &[u8]) -> Result<(Vec<u8>, Kek), I::Error> {
        let crate::opts::KekProvider::Kmip { endpoint, key_id, delegated } = &kek.provider else {
            return Err("KMS provider is not KMIP".into());
        };
        let uid_of = |what: &'static str| move |m: &[u8]| crate::kmip::unique_identifier(m).map_err(|e| format!("Error getting UniqueIdentifer from KMIP {what} response: {e}"));
        let uid = match key_id {
            Some(k) => k.clone(),
            None => {
                let msg = if *delegated {
                    crate::kmip::create()
                } else {
                    let secret = crypto::random(crate::kmip::SECRETDATA_LENGTH)?;
                    crate::kmip::register_secretdata(&secret).map_err(|e| format!("Error creating KMIP register request: {e}"))?
                };
                let req = self.kmip_request(kek, msg)?;
                let uid = self.kmip_call(io, &req, &uid_of(if *delegated { "Create" } else { "Register" }))?;
                // _ctx_done_kmip_activate reports errors as a Register response's (as libmongocrypt)
                let req = self.kmip_request(kek, crate::kmip::activate(&uid))?;
                self.kmip_call(io, &req, &uid_of("Register"))?;
                uid
            }
        };
        let encrypted = if *delegated {
            let req = self.kmip_request(kek, crate::kmip::encrypt_decrypt(true, &uid, material, None))?;
            self.kmip_call(io, &req, &|m| {
                let data = crate::kmip::data(m).map_err(|e| format!("Error getting data from KMIP Encrypt response: {e}"))?;
                let iv = crate::kmip::iv(m).map_err(|e| format!("Error getting IV from KMIP Encrypt response: {e}"))?;
                if iv.len() != 16 {
                    return Err(format!("KMIP IV response has unexpected length: {}", iv.len()));
                }
                Ok([iv, data].concat())
            })?
        } else {
            let req = self.kmip_request(kek, crate::kmip::get(&uid))?;
            let secret = self.kmip_call(io, &req, &|m| crate::kmip::secretdata(m).map_err(|e| format!("Error getting SecretData from KMIP Get response: {e}")))?;
            crypto::wrap_key(&secret, material)?
        };
        let kek = Kek { kmsid: kek.kmsid.clone(), provider: crate::opts::KekProvider::Kmip { endpoint: endpoint.clone(), key_id: Some(uid), delegated: *delegated } };
        Ok((encrypted, kek))
    }

    /// The key broker's KMIP step: Get the secret and unwrap locally, or have the server decrypt
    fn kmip_unwrap<I: Io>(&self, io: &mut I, key: &crate::key::KeyDoc) -> Result<Vec<u8>, I::Error> {
        let crate::opts::KekProvider::Kmip { key_id, delegated, .. } = &key.kek.provider else {
            return Err("KMS provider is not KMIP".into());
        };
        let uid = key_id.as_deref().ok_or("KMIP key malformed, no keyId present")?;
        if *delegated {
            if key.key_material.len() < 16 {
                return Err("Error getting IV from key material".into());
            }
            let (iv, ct) = key.key_material.split_at(16);
            let req = self.kmip_request(&key.kek, crate::kmip::encrypt_decrypt(false, uid, ct, Some(iv)))?;
            self.kmip_call(io, &req, &|m| crate::kmip::data(m).map_err(|e| format!("Error getting data from KMIP Decrypt response: {e}")))
        } else {
            let req = self.kmip_request(&key.kek, crate::kmip::get(uid))?;
            let secret = self.kmip_call(io, &req, &|m| crate::kmip::secretdata(m).map_err(|e| format!("Error getting SecretData from KMIP Get response: {e}")))?;
            Ok(crypto::unwrap_key(&secret, &key.key_material)?)
        }
    }

    /// The key material of a key document whose KEK is remote
    fn kms_unwrap<I: Io>(&self, io: &mut I, key: &crate::key::KeyDoc) -> Result<Vec<u8>, I::Error> {
        match key.kek.kms_type() {
            crate::opts::KmsType::Aws => self.aws(io, &key.kek, true, &key.key_material),
            crate::opts::KmsType::Azure => self.azure(io, &key.kek, false, &key.key_material),
            crate::opts::KmsType::Gcp => self.gcp(io, &key.kek, false, &key.key_material),
            crate::opts::KmsType::Kmip => self.kmip_unwrap(io, key),
            t => Err(format!("the {} KMS provider is not supported by this build yet", t.name()).into()),
        }
    }

    /// Key material encrypted by a remote KMS (at least as long as the plaintext)
    fn kms_wrap<I: Io>(&self, io: &mut I, kek: &Kek, material: &[u8]) -> Result<Vec<u8>, I::Error> {
        let e = match kek.kms_type() {
            crate::opts::KmsType::Aws => self.aws(io, kek, false, material)?,
            crate::opts::KmsType::Azure => self.azure(io, kek, true, material)?,
            crate::opts::KmsType::Gcp => self.gcp(io, kek, true, material)?,
            t => return Err(format!("the {} KMS provider is not supported by this build yet", t.name()).into()),
        };
        if e.len() < crypto::KEY_LEN {
            return Err("key material not expected length".into());
        }
        Ok(e)
    }

    fn fetch<I: Io>(&self, kb: &mut KeyBroker, io: &mut I) -> Result<(), I::Error> {
        // KB_ADDING_DOCS: on-demand credentials are obtained before the key documents are read
        let _ondemand = if kb.needs_keys() && !self.kms.need_credentials.is_empty() {
            let g = self.obtain_credentials(io)?;
            if let Some(k) = self.ondemand.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
                kb.provide(k.clone());
            }
            Some(g)
        } else {
            None
        };
        if kb.needs_keys() {
            let docs = io.find_keys(&kb.filter()).map_err(Error::Io)?;
            for d in docs {
                kb.add_doc(&d)?;
            }
        }
        kb.docs_done()?;
        for (doc, slot) in kb.pending_kms() {
            *slot = Some(self.kms_unwrap(io, doc)?);
        }
        kb.kms_done()?;
        Ok(())
    }

    /// `mongocrypt_ctx_explicit_encrypt_init` + finalize for FLE1: `value` is `{v: <value>}`;
    /// returns the binary subtype 6 payload
    pub fn encrypt_fle1<I: Io>(&self, io: &mut I, value: &[u8], key: &KeySel, algo: Fle1Algorithm) -> Result<Vec<u8>, I::Error> {
        let mut it = Iter::new(value).ok_or("msg must be bson")?;
        if !it.find(b"v") {
            return Err("invalid msg, must contain 'v'".into());
        }
        permitted_for_encryption(&it.value(), algo)?;
        let mut kb = KeyBroker::new(&self.kms, &self.cache);
        match key {
            KeySel::Id(id) => kb.request_id(id),
            KeySel::AltName(n) => kb.request_name(n),
        }
        self.fetch(&mut kb, io)?;
        let (material, key_id) = match key {
            KeySel::Id(id) => (kb.key_by_id(id)?, *id),
            KeySel::AltName(n) => kb.key_by_name(n)?,
        };
        let subtype = if algo == Fle1Algorithm::Deterministic { FLE1_DETERMINISTIC } else { FLE1_RANDOM };
        let ct = Ciphertext { blob_subtype: subtype, key_id, original_bson_type: it.value().type_code(), data: &[] };
        let aad = ct.associated_data().ok_or("could not serialize associated data")?;
        let plaintext = value_bytes(&it);
        let iv = match algo {
            Fle1Algorithm::Deterministic => crypto::deterministic_iv(&material, plaintext, &aad)?.to_vec(),
            Fle1Algorithm::Random => crypto::random(crypto::IV_LEN)?,
        };
        let data = Algorithm::Fle1.encrypt(&iv, Some(&aad), &material, plaintext)?;
        Ok(Ciphertext { data: &data, ..ct }.serialize())
    }

    /// `mongocrypt_ctx_explicit_decrypt_init`: `msg` is `{v: <binary subtype 6>}`; returns `{v: <value>}`
    pub fn explicit_decrypt<I: Io>(&self, io: &mut I, msg: &[u8]) -> Result<Vec<u8>, I::Error> {
        let mut it = Iter::new(msg).ok_or("malformed bson")?;
        if !it.find(b"v") {
            return Err("invalid msg, must contain 'v'".into());
        }
        let Value::Binary(subtype, b) = it.value() else {
            return Err("invalid msg, 'v' must contain a binary".into());
        };
        if subtype != SUBTYPE_ENCRYPTED {
            return Err(format!("decryption expected BSON binary subtype {SUBTYPE_ENCRYPTED}, got {subtype}").into());
        }
        if b.is_empty() {
            return Err("unsupported FLE blob subtype: expected non-empty payload".into());
        }
        if !is_decryptable(b[0]) {
            return Err(format!("unsupported FLE blob subtype: {}", b[0]).into());
        }
        self.decrypt_doc(io, msg)
    }

    /// `mongocrypt_ctx_decrypt_init` + finalize: every ciphertext of `doc` replaced by its plaintext
    pub fn decrypt_doc<I: Io>(&self, io: &mut I, doc: &[u8]) -> Result<Vec<u8>, I::Error> {
        let mut kb = KeyBroker::new(&self.kms, &self.cache);
        let mut found = false;
        traverse(doc, &mut |b| {
            found = true;
            collect_key(&mut kb, b)
        })?;
        if !found {
            return Ok(doc.to_vec());
        }
        self.fetch(&mut kb, io)?;
        // _check_for_K_KeyId: indexed values name a second key inside their S_Key envelope
        let mut more = false;
        traverse(doc, &mut |b| {
            if matches!(b[0], crate::fle2::SUBTYPE_IEV_EQUALITY_V2 | crate::fle2::SUBTYPE_IEV_RANGE_V2 | crate::fle2::SUBTYPE_IEV_TEXT) {
                let iev = crate::fle2::Iev::parse(b)?;
                let s = kb.key_by_id(&iev.s_key_id)?;
                kb.request_id(&iev.add_s_key(&s)?.k_key_id);
                more = true;
            }
            Ok(())
        })?;
        if more {
            self.fetch(&mut kb, io)?;
        }
        Ok(transform(doc, &mut |b| decrypt_value(&kb, b))?)
    }

    /// `mongocrypt_ctx_datakey_init` for an already-encrypted key: the key document to insert
    pub fn data_key_doc<I: Io>(
        &self,
        io: &mut I,
        kek: &Kek,
        key_material: Option<&[u8]>,
        alt_names: &[Vec<u8>],
    ) -> Result<Vec<u8>, I::Error> {
        let material = match key_material {
            Some(m) => {
                if m.len() != crypto::KEY_LEN {
                    return Err(format!("keyMaterial should have length {}, but has length {}", crypto::KEY_LEN, m.len()).into());
                }
                m.to_vec()
            }
            None => crypto::random(crypto::KEY_LEN)?,
        };
        let _ondemand = if self.on_demand(&kek.kmsid) { Some(self.obtain_credentials(io)?) } else { None };
        let (encrypted, kek) = self.wrap(io, kek, &material)?;
        Ok(key_document(&kek, &encrypted, alt_names)?)
    }

    /// The datakey context's KMS step: `material` encrypted with `kek`, and the KEK to store (a
    /// KMIP KEK gains the keyId of the object it registered or created)
    fn wrap<I: Io>(&self, io: &mut I, kek: &Kek, material: &[u8]) -> Result<(Vec<u8>, Kek), I::Error> {
        match self.creds(&kek.kmsid).as_ref() {
            Some(crate::opts::Creds::Local { key }) => Ok((crypto::wrap_key(key, material)?, kek.clone())),
            Some(crate::opts::Creds::Kmip { .. }) => self.kmip_wrap(io, kek, material),
            Some(_) => Ok((self.kms_wrap(io, kek, material)?, kek.clone())),
            None => Err(format!("requested kms provider required by datakey is not configured: `{}`", kek.kmsid).into()),
        }
    }

    /// `mongocrypt_ctx_encrypt_init` + finalize with `bypass_query_analysis`: the command to send
    /// (None: send `cmd` as is). Explicit payloads are kept; Queryable Encryption collections get
    /// encryptionInformation (and compaction/cleanup tokens).
    pub fn auto_encrypt<I: Io>(&self, io: &mut I, db: &str, cmd: &[u8], opts: &AutoOpts) -> Result<Option<Vec<u8>>, I::Error> {
        use crate::autoenc as ae;
        if db.is_empty() || db.contains('\0') {
            return Err("invalid db".into());
        }
        if db.contains('.') {
            return Err("invalid db: must not contain a dot".into());
        }
        let name = {
            let mut it = Iter::new(cmd).ok_or("invalid BSON")?;
            if !it.next() {
                return Err("invalid empty BSON".into());
            }
            it.key().to_vec()
        };
        let mut sb = ae::SchemaBroker::default();
        let (target_db, target_coll) = if name == b"bulkWrite" {
            let (tdb, tc) = ae::check_bulk_write(cmd)?;
            sb.request(&tdb, &tc)?;
            (Some(tdb), tc)
        } else {
            let (bypass, tc) = ae::check_cmd(cmd)?;
            if bypass {
                return Ok(None);
            }
            let tc = tc.ok_or("unexpected error: did not bypass or error but no collection name")?;
            sb.request(db, &tc)?;
            (None, tc)
        };
        if name == b"aggregate" {
            ae::collections_in_agg(&mut sb, cmd, db)?;
        }
        // mongocrypt_ctx_encrypt_ismaster_done: the schema sources, in libmongocrypt's order
        if let Some(m) = &opts.encrypted_fields_map {
            sb.satisfy_from_encrypted_fields_map(m)?;
        }
        if sb.need_more() {
            sb.satisfy_from_create_or_coll_mod(cmd)?;
        }
        if sb.need_more() {
            if let Some(m) = &opts.schema_map {
                sb.satisfy_from_schema_map(m)?;
            }
        }
        if sb.need_more() {
            sb.satisfy_from_cache(&self.collinfo)?;
        }
        if sb.need_more() && name == b"create" {
            sb.satisfy_remaining_with_empty(None);
        }
        if sb.need_more() {
            let filter = sb.list_collections_filter()?;
            let docs = io.list_collections(target_db.as_deref().unwrap_or(db), &filter).map_err(Error::Io)?;
            for d in docs {
                sb.satisfy_from_collinfo(&d, &self.collinfo)?;
            }
            // _mongo_done_collinfo: collections without collinfo have no schema
            sb.satisfy_remaining_with_empty(Some(&self.collinfo));
        }
        // keys: compaction/cleanup tokens and the fields named by keyAltName
        let target_efc = sb.maybe_encrypted_fields(&target_coll).cloned();
        let mut kb = KeyBroker::new(&self.kms, &self.cache);
        let mut need_keys = false;
        if let Some(efc) = &target_efc {
            let compaction = name == b"compactStructuredEncryptionData" || name == b"cleanupStructuredEncryptionData";
            for f in &efc.fields {
                match (&f.key_alt_name, f.key_id) {
                    (Some(a), _) => {
                        kb.request_name(a);
                        need_keys = true;
                    }
                    (None, Some(id)) if compaction => {
                        kb.request_id(&id);
                        need_keys = true;
                    }
                    _ => {}
                }
            }
        }
        if need_keys {
            self.fetch(&mut kb, io)?;
        }
        if !sb.has_any_qe() {
            // nothing to do: the command is sent as is
            return Ok(None);
        }
        // _fle2_finalize
        let by_name = |n: &[u8]| -> std::result::Result<[u8; 16], String> { kb.key_by_name(n).map(|(_, id)| id) };
        let by_id = |id: &[u8; 16]| -> std::result::Result<Vec<u8>, String> { kb.key_by_id(id) };
        let mut converted = ae::strip_encryption_information(&name, cmd)?;
        let target_efc = match target_efc {
            Some(mut efc) => {
                for f in efc.fields.iter_mut() {
                    if f.key_id.is_none() {
                        if let Some(a) = &f.key_alt_name {
                            f.key_id = Some(by_name(a).map_err(|_| format!("Could not find key by keyAltName: {}", String::from_utf8_lossy(a)))?);
                        }
                    }
                }
                Some(efc)
            }
            None => None,
        };
        let omit = ae::must_omit_encryption_information(&name, &converted, target_efc.as_ref())?;
        converted = ae::fixup_str_encode_version(&name, &converted, target_efc.as_ref())?;
        if !omit {
            converted = sb.add_schemas_to_cmd(&converted, &by_name)?;
        }
        converted = ae::append_compaction_tokens(&name, target_efc.as_ref(), &converted, &by_id)?;
        // an input $db stays in the output
        let has_db = |d: &[u8]| Iter::new(d).is_some_and(|mut it| it.find(b"$db"));
        if has_db(cmd) && !has_db(&converted) {
            let mut w = Writer::with_capacity(converted.len() + db.len() + 8);
            w.append_raw_elements(&converted);
            w.utf8(b"$db", db.as_bytes());
            converted = w.finish();
        }
        Ok(Some(converted))
    }

    /// `mongocrypt_ctx_rewrap_many_datakey_init`: for each key document, its id and the
    /// `{masterKey, keyMaterial}` to `$set` (re-encrypted with `new_kek`, or with its own KEK).
    /// The key broker runs as libmongocrypt's: cached keys are not decrypted again, and come first.
    /// On-demand credentials are obtained first, then the key documents matching `filter` read.
    pub fn rewrap_many<I: Io>(&self, io: &mut I, filter: &[u8], new_kek: Option<&Kek>) -> Result<Vec<([u8; 16], Vec<u8>)>, I::Error> {
        let mut kb = KeyBroker::new(&self.kms, &self.cache);
        let _ondemand = if self.kms.need_credentials.is_empty() {
            None
        } else {
            let g = self.obtain_credentials(io)?;
            if let Some(k) = self.ondemand.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
                kb.provide(k.clone());
            }
            Some(g)
        };
        let docs = io.find_keys(filter).map_err(Error::Io)?;
        kb.request_any();
        for d in &docs {
            kb.add_doc(d)?;
        }
        kb.docs_done()?;
        for (doc, slot) in kb.pending_kms() {
            *slot = Some(self.kms_unwrap(io, doc)?);
        }
        kb.kms_done()?;
        let keys: Vec<(crate::key::KeyDoc, Vec<u8>)> = kb.all_keys().map(|(d, m)| (d.clone(), m.clone())).collect();
        let mut out = Vec::with_capacity(keys.len());
        for (key, material) in keys {
            let (encrypted, kek) = self.wrap(io, new_kek.unwrap_or(&key.kek), &material)?;
            let mut w = Writer::with_capacity(256);
            let p = w.begin(T_DOCUMENT, b"masterKey");
            kek.append(&mut w).map_err(Error::Crypt)?;
            w.end(p);
            w.binary(b"keyMaterial", 0, &encrypted);
            out.push((key.id, w.finish()));
        }
        Ok(out)
    }
}

/// `_finalize` of the datakey context: the new key document
pub fn key_document(kek: &Kek, encrypted_material: &[u8], alt_names: &[Vec<u8>]) -> std::result::Result<Vec<u8>, String> {
    let mut w = Writer::new();
    let mut uuid = crypto::random(16)?;
    uuid[6] = 0x40 | (uuid[6] & 0x0f);
    uuid[8] = 0x80 | (uuid[8] & 0x3f);
    w.binary(b"_id", crate::bson::SUBTYPE_UUID, &uuid);
    if !alt_names.is_empty() {
        let p = w.begin(T_ARRAY, b"keyAltNames");
        // libmongocrypt prepends each keyAltName to its list: the document has them reversed
        for (i, n) in alt_names.iter().rev().enumerate() {
            w.utf8(i.to_string().as_bytes(), n);
        }
        w.end(p);
    }
    w.binary(b"keyMaterial", 0, encrypted_material);
    let now = now_ms();
    w.date_time(b"creationDate", now);
    w.date_time(b"updateDate", now);
    w.int32(b"status", 0);
    let p = w.begin(T_DOCUMENT, b"masterKey");
    kek.append(&mut w)?;
    w.end(p);
    Ok(w.finish())
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// `_permitted_for_encryption`
fn permitted_for_encryption(v: &Value, algo: Fle1Algorithm) -> std::result::Result<(), String> {
    match v {
        Value::Null | Value::MinKey | Value::MaxKey | Value::Undefined => Err("BSON type invalid for encryption".into()),
        Value::Binary(SUBTYPE_ENCRYPTED, _) => Err("BSON binary subtype 6 is invalid for encryption".into()),
        Value::Binary(..) => Ok(()),
        Value::Double(_)
        | Value::Document(_)
        | Value::Array(_)
        | Value::CodeWScope(..)
        | Value::Bool(_)
        | Value::Decimal128(_) => {
            if algo == Fle1Algorithm::Deterministic {
                Err("BSON type invalid for deterministic encryption".into())
            } else {
                Ok(())
            }
        }
        _ => Ok(()),
    }
}

/// `_is_decryptable_fle_blob_subtype`
fn is_decryptable(b: u8) -> bool {
    matches!(b, 1 | 2 | 4 | 6 | 7 | 9 | 11 | 14 | 15 | 16 | 17)
}

/// TRAVERSE_MATCH_CIPHERTEXT
fn is_ciphertext(b: u8) -> bool {
    is_decryptable(b)
}

/// `_mongocrypt_traverse_binary_in_bson`
fn traverse(doc: &[u8], cb: &mut dyn FnMut(&[u8]) -> std::result::Result<(), String>) -> std::result::Result<(), String> {
    let mut it = Iter::new(doc).ok_or("malformed bson")?;
    while it.next() {
        match it.value() {
            Value::Binary(SUBTYPE_ENCRYPTED, b) if !b.is_empty() && is_ciphertext(b[0]) => cb(b)?,
            Value::Document(d) => traverse(d, cb).map_err(|e| if e == "malformed bson" { "error recursing into document".into() } else { e })?,
            Value::Array(d) => traverse(d, cb).map_err(|e| if e == "malformed bson" { "error recursing into array".into() } else { e })?,
            _ => {}
        }
    }
    Ok(())
}

/// `_mongocrypt_traverse_binary_in_bson` with TRAVERSE_MATCH_SUBTYPE6: every binary of subtype 6
pub fn traverse_subtype6(doc: &[u8], cb: &mut dyn FnMut(&[u8]) -> std::result::Result<(), String>) -> std::result::Result<(), String> {
    let mut it = Iter::new(doc).ok_or("malformed bson")?;
    while it.next() {
        match it.value() {
            Value::Binary(SUBTYPE_ENCRYPTED, b) => cb(b)?,
            Value::Document(d) => traverse_subtype6(d, cb).map_err(|e| if e == "malformed bson" { "error recursing into document".into() } else { e })?,
            Value::Array(d) => traverse_subtype6(d, cb).map_err(|e| if e == "malformed bson" { "error recursing into array".into() } else { e })?,
            _ => {}
        }
    }
    Ok(())
}

/// `_mongocrypt_transform_binary_in_bson`: a copy with every matching binary replaced by `cb`'s value
/// (a `wrap_value` document)
fn transform(doc: &[u8], cb: &mut dyn FnMut(&[u8]) -> std::result::Result<Vec<u8>, String>) -> std::result::Result<Vec<u8>, String> {
    let mut w = Writer::with_capacity(doc.len());
    transform_into(&mut w, doc, cb)?;
    Ok(w.finish())
}

fn transform_into(
    w: &mut Writer,
    doc: &[u8],
    cb: &mut dyn FnMut(&[u8]) -> std::result::Result<Vec<u8>, String>,
) -> std::result::Result<(), String> {
    let mut it = Iter::new(doc).ok_or("malformed bson")?;
    while it.next() {
        let key = it.key();
        match it.value() {
            Value::Binary(SUBTYPE_ENCRYPTED, b) if !b.is_empty() && is_ciphertext(b[0]) => {
                let v = cb(b)?;
                append_wrapped(w, key, &v);
            }
            Value::Document(d) => {
                let p = w.begin(T_DOCUMENT, key);
                transform_into(w, d, cb)?;
                w.end(p);
            }
            Value::Array(d) => {
                let p = w.begin(T_ARRAY, key);
                transform_into(w, d, cb)?;
                w.end(p);
            }
            _ => w.element_from(Some(key), &it),
        }
    }
    let _ = T_BINARY;
    Ok(())
}

/// `_collect_key_from_ciphertext`
fn collect_key(kb: &mut KeyBroker, b: &[u8]) -> std::result::Result<(), String> {
    match b[0] {
        1 | 2 => {
            let ct = Ciphertext::parse(b)?;
            kb.request_id(&ct.key_id);
            Ok(())
        }
        crate::fle2::SUBTYPE_IEV_EQUALITY_V2 | crate::fle2::SUBTYPE_IEV_RANGE_V2 | crate::fle2::SUBTYPE_IEV_TEXT => {
            kb.request_id(&crate::fle2::Iev::parse(b)?.s_key_id);
            Ok(())
        }
        crate::fle2::SUBTYPE_UEV_V2 => {
            kb.request_id(&crate::fle2::Uev::parse(b)?.key_id);
            Ok(())
        }
        crate::fle2::SUBTYPE_IUP_V2 => {
            kb.request_id(&crate::fle2::Iup::parse(b)?.user_key_id);
            Ok(())
        }
        _ => Err("Queryable Encryption v1 payloads are not supported by this build".into()),
    }
}

/// `_replace_ciphertext_with_plaintext`
fn decrypt_value(kb: &KeyBroker, b: &[u8]) -> std::result::Result<Vec<u8>, String> {
    match b[0] {
        1 | 2 => {
            let ct = Ciphertext::parse(b)?;
            let material = kb.key_by_id(&ct.key_id).map_err(|_| "key not found".to_string())?;
            let aad = ct.associated_data().ok_or("could not serialize associated data")?;
            let p = Algorithm::Fle1.decrypt(Some(&aad), &material, ct.data)?;
            wrap_value(ct.original_bson_type, &p).ok_or_else(|| "malformed encrypted bson".to_string())
        }
        crate::fle2::SUBTYPE_IEV_EQUALITY_V2 | crate::fle2::SUBTYPE_IEV_RANGE_V2 | crate::fle2::SUBTYPE_IEV_TEXT => {
            let iev = crate::fle2::Iev::parse(b)?;
            let inner = iev.add_s_key(&kb.key_by_id(&iev.s_key_id)?)?;
            let v = inner.add_k_key(&kb.key_by_id(&inner.k_key_id)?)?;
            wrap_value(iev.value_type, &v).ok_or_else(|| "decrypted clientValue is not valid BSON".to_string())
        }
        crate::fle2::SUBTYPE_UEV_V2 => {
            let u = crate::fle2::Uev::parse(b)?;
            let p = u.decrypt(&kb.key_by_id(&u.key_id)?)?;
            wrap_value(u.value_type, &p).ok_or_else(|| "decrypted plaintext is not valid BSON".to_string())
        }
        crate::fle2::SUBTYPE_IUP_V2 => {
            let u = crate::fle2::Iup::parse(b)?;
            let p = u.decrypt(&kb.key_by_id(&u.user_key_id)?)?;
            wrap_value(u.value_type, &p).ok_or_else(|| "decrypted plaintext is not valid BSON".to_string())
        }
        _ => Err("Queryable Encryption v1 payloads are not supported by this build".into()),
    }
}

/// The options of an explicit encryption, as libmongoc hands them to libmongocrypt
/// (`_create_explicit_state_machine`)
#[derive(Clone, Debug, Default)]
pub struct ExplicitOpts {
    /// None: "passed null algorithm"
    pub algorithm: Option<Vec<u8>>,
    /// the UUID binary's data (libmongoc checked the subtype)
    pub key_id: Option<Vec<u8>>,
    pub key_alt_name: Option<Vec<u8>>,
    pub query_type: Option<Vec<u8>>,
    pub contention_factor: Option<i64>,
    pub range_opts: Option<Vec<u8>>,
    pub string_opts: Option<Vec<u8>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IndexType {
    None,
    Equality,
    Range,
    String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum QueryType {
    Equality,
    Range,
    Prefix,
    Suffix,
    Substring,
}

impl QueryType {
    fn name(self) -> &'static str {
        match self {
            QueryType::Equality => "Equality",
            QueryType::Range => "Range",
            QueryType::Prefix => "Prefix",
            QueryType::Suffix => "Suffix",
            QueryType::Substring => "Substring",
        }
    }
}

impl IndexType {
    fn name(self) -> &'static str {
        match self {
            IndexType::None => "None",
            IndexType::Equality => "Equality",
            IndexType::Range => "Range",
            IndexType::String => "String",
        }
    }
}

/// `mc_validate_contention`
pub fn validate_contention(c: i64) -> std::result::Result<(), String> {
    if c < 0 {
        return Err(format!("Error validating contention: contention must be non-negative, got: {c}"));
    }
    if c == i64::MAX {
        return Err(format!("Error validating contention: contention must be < INT64_MAX, got: {c}"));
    }
    Ok(())
}

/// The checked options (`ctx->opts`)
struct Checked {
    fle1: Option<Fle1Algorithm>,
    index_type: Option<IndexType>,
    query_type: Option<QueryType>,
    range: Option<crate::range::RangeOpts>,
    text: Option<crate::text::TextOpts>,
    key: KeySel,
}

/// `mongocrypt_ctx_setopt_*` and `explicit_encrypt_init`'s checks, in libmongocrypt's order
fn check_explicit(o: &ExplicitOpts, expression: bool) -> std::result::Result<Checked, String> {
    // mongocrypt_ctx_setopt_algorithm
    let algo = o.algorithm.as_deref().ok_or("passed null algorithm")?;
    let a = String::from_utf8_lossy(algo).to_ascii_lowercase();
    let (mut fle1, mut index_type) = (None, None);
    match a.as_str() {
        "aead_aes_256_cbc_hmac_sha_512-deterministic" => fle1 = Some(Fle1Algorithm::Deterministic),
        "aead_aes_256_cbc_hmac_sha_512-random" => fle1 = Some(Fle1Algorithm::Random),
        "indexed" => index_type = Some(IndexType::Equality),
        "unindexed" => index_type = Some(IndexType::None),
        "range" => index_type = Some(IndexType::Range),
        "rangepreview" => return Err("Algorithm 'rangePreview' is deprecated, please use 'range'".into()),
        "string" => index_type = Some(IndexType::String),
        "textpreview" => return Err("Algorithm 'textPreview' is deprecated, please use 'string'".into()),
        _ => return Err(format!("unsupported algorithm string \"{}\"", String::from_utf8_lossy(algo))),
    }
    // mongocrypt_ctx_setopt_algorithm_range
    let range = match &o.range_opts {
        Some(r) => Some(crate::range::RangeOpts::parse(r)?),
        None => None,
    };
    // mongocrypt_ctx_setopt_algorithm_text
    let text = match &o.string_opts {
        Some(t) => Some(crate::text::TextOpts::parse(t)?),
        None => None,
    };
    // mongocrypt_ctx_setopt_query_type
    let query_type = match &o.query_type {
        None => None,
        Some(q) => Some(match String::from_utf8_lossy(q).to_ascii_lowercase().as_str() {
            "equality" => QueryType::Equality,
            "range" => QueryType::Range,
            "rangepreview" => return Err("Query type 'rangePreview' is deprecated, please use 'range'".into()),
            "prefix" => QueryType::Prefix,
            "suffix" => QueryType::Suffix,
            "substring" => QueryType::Substring,
            "prefixpreview" => return Err("Query type 'prefixPreview' is deprecated, please use 'prefix'".into()),
            "suffixpreview" => return Err("Query type 'suffixPreview' is deprecated, please use 'suffix'".into()),
            "substringpreview" => return Err("Query type 'substringPreview' is deprecated, please use 'substring'".into()),
            _ => return Err(format!("Unsupported query_type \"{}\"", String::from_utf8_lossy(q))),
        }),
    };
    // mongocrypt_ctx_setopt_key_id
    if let Some(id) = &o.key_id {
        if id.len() != 16 {
            return Err("expected 16 byte UUID".into());
        }
    }
    // _mongocrypt_ctx_init: the key descriptor is required
    let key = match (&o.key_id, &o.key_alt_name) {
        (None, None) => return Err("either key id or key alt name required".into()),
        (Some(_), Some(_)) => return Err("cannot have both key id and key alt name".into()),
        (Some(id), None) => KeySel::Id(id[..].try_into().expect("checked above")),
        (None, Some(n)) => KeySel::AltName(n.clone()),
    };
    let (cf, rng, txt) = (o.contention_factor.is_some(), o.range_opts.is_some(), o.string_opts.is_some());
    if o.key_alt_name.is_some() {
        if index_type.is_some() {
            return Err("cannot set both key alt name and index type".into());
        }
        if cf {
            return Err("cannot set both key alt name and contention factor".into());
        }
        if query_type.is_some() {
            return Err("cannot set both key alt name and query type".into());
        }
        if rng {
            return Err("cannot set both key alt name and range opts".into());
        }
    }
    if fle1.is_some() {
        if cf {
            return Err("cannot set both algorithm and contention factor".into());
        }
        if query_type.is_some() {
            return Err("cannot set both algorithm and query type".into());
        }
        if rng {
            return Err("cannot set both algorithm and range opts".into());
        }
    }
    if cf && index_type == Some(IndexType::None) {
        return Err("cannot set contention factor with no index type".into());
    }
    if query_type.is_some() && index_type == Some(IndexType::None) {
        return Err("cannot set query type with no index type".into());
    }
    if let Some(q) = query_type {
        let string = index_type == Some(IndexType::String);
        match q {
            QueryType::Prefix if !string => return Err("prefix query type requires string index type".into()),
            QueryType::Suffix if !string => return Err("suffix query type requires string index type".into()),
            QueryType::Substring if !string => return Err("substring query type requires string index type".into()),
            _ => {}
        }
    }
    if rng {
        match index_type {
            Some(IndexType::None) => return Err("cannot set range opts with no index type".into()),
            Some(IndexType::Equality) => return Err("cannot set range opts with equality index type".into()),
            Some(IndexType::String) => return Err("cannot set range opts with 'string' algorithm".into()),
            _ => {}
        }
    }
    if txt && index_type.is_some_and(|t| t != IndexType::String) {
        return Err("cannot set string opts without string index type".into());
    }
    if let Some(c) = o.contention_factor {
        validate_contention(c)?;
    }
    match index_type {
        Some(IndexType::Equality) if !cf => return Err("contention factor is required for indexed algorithm".into()),
        Some(IndexType::Range) if !cf => return Err("contention factor is required for range indexed algorithm".into()),
        Some(IndexType::Range) if !rng => return Err("range opts are required for range indexed algorithm".into()),
        Some(IndexType::String) if !cf => return Err("contention factor is required for string algorithm".into()),
        Some(IndexType::String) if !txt => return Err("string opts are required for string algorithm".into()),
        _ => {}
    }
    if let (Some(q), Some(t)) = (query_type, index_type) {
        let ok = match q {
            QueryType::Range => t == IndexType::Range,
            QueryType::Equality => t == IndexType::Equality,
            _ => t == IndexType::String,
        };
        if !ok {
            return Err(format!("query_type ({}) must match index_type ({})", q.name(), t.name()));
        }
    }
    let is_range_query = query_type == Some(QueryType::Range);
    if !expression && is_range_query {
        return Err("Encrypt may not be used for range queries. Use EncryptExpression.".into());
    }
    if expression && !is_range_query {
        return Err("EncryptExpression may only be used for range queries.".into());
    }
    Ok(Checked { fle1, index_type, query_type, range, text, key })
}

impl Crypt {
    /// `_mongoc_crypt_explicit_encrypt`: the ciphertext (a binary subtype 6 payload) of the `{v: ..}` document's value
    pub fn explicit_encrypt<I: Io>(&self, io: &mut I, value: &[u8], o: &ExplicitOpts) -> Result<Vec<u8>, I::Error> {
        let c = check_explicit(o, false)?;
        match (c.fle1, c.index_type) {
            (Some(a), _) => self.encrypt_fle1(io, value, &c.key, a),
            (None, Some(t @ (IndexType::None | IndexType::Equality))) => {
                let KeySel::Id(key_id) = c.key else { unreachable!("index types require a key id") };
                self.encrypt_fle2(io, value, &key_id, t, c.query_type, o.contention_factor.unwrap_or(0))
            }
            (None, Some(IndexType::Range)) if c.query_type.is_none() => {
                let KeySel::Id(key_id) = c.key else { unreachable!("index types require a key id") };
                self.encrypt_range(io, value, &key_id, c.range.as_ref().expect("checked: range opts are required"), o.contention_factor.unwrap_or(0))
            }
            (None, Some(IndexType::String)) => {
                let KeySel::Id(key_id) = c.key else { unreachable!("index types require a key id") };
                let text = c.text.as_ref().expect("checked: string opts are required");
                self.encrypt_text(io, value, &key_id, text, c.query_type, o.contention_factor.unwrap_or(0))
            }
            _ => Err("Queryable Encryption's range and string algorithms are not supported by this build yet".into()),
        }
    }

    /// `_fle2_finalize_explicit` for the string algorithm: FLE2InsertUpdatePayloadV2 with text
    /// search token sets, or FLE2FindTextPayload for prefix/suffix/substring queries
    fn encrypt_text<I: Io>(&self, io: &mut I, value: &[u8], key_id: &[u8; 16], text: &crate::text::TextOpts, q: Option<QueryType>, cf: i64) -> Result<Vec<u8>, I::Error> {
        use crate::text::Which;
        // explicit_encrypt_init: the TextOpts become a FLE2TextSearchInsertSpec
        let which = match q {
            None => None,
            Some(QueryType::Prefix) => Some(Which::Prefix),
            Some(QueryType::Suffix) => Some(Which::Suffix),
            Some(QueryType::Substring) => Some(Which::Substring),
            Some(q) => return Err(format!("Unexpected query type: {}\n", q.name()).into()),
        };
        let spec_doc = text.to_insert_spec(value, which)?;
        let mut kb = KeyBroker::new(&self.kms, &self.cache);
        kb.request_id(key_id);
        self.fetch(&mut kb, io)?;
        let key = kb.key_by_id(key_id).map_err(|_| "unable to retrieve key")?;
        // _mongocrypt_marking_to_ciphertext
        let mut it = Iter::new(&spec_doc).ok_or("invalid input BSON, must contain 'v'")?;
        if !it.find(b"v") {
            return Err("invalid input BSON, must contain 'v'".into());
        }
        let spec = crate::text::InsertSpec::parse(&it.value())?;
        let mut out = Vec::new();
        if q.is_some() {
            out.push(crate::text::SUBTYPE_FIND_TEXT);
            out.extend(crate::text::find_payload(&spec, &key, cf)?);
        } else {
            // the value as a BSON value: the spec's "v" (a string)
            let mut v = Vec::with_capacity(spec.v.len() + 5);
            v.extend_from_slice(&((spec.v.len() + 1) as i32).to_le_bytes());
            v.extend_from_slice(&spec.v);
            v.push(0);
            out.push(crate::fle2::SUBTYPE_IUP_V2);
            out.extend(crate::text::insert_payload(&spec, key_id, &key, key_id, &key, &v, cf)?);
        }
        Ok(out)
    }

    /// `_fle2_finalize_explicit` for the Unindexed and Indexed (equality) algorithms
    fn encrypt_fle2<I: Io>(&self, io: &mut I, value: &[u8], key_id: &[u8; 16], t: IndexType, q: Option<QueryType>, cf: i64) -> Result<Vec<u8>, I::Error> {
        let mut it = Iter::new(value).ok_or("unable to convert input to BSON")?;
        if !it.find(b"v") {
            return Err("invalid input BSON, must contain 'v'".into());
        }
        permitted_for_encryption(&it.value(), Fle1Algorithm::Random)?;
        let mut kb = KeyBroker::new(&self.kms, &self.cache);
        kb.request_id(key_id);
        self.fetch(&mut kb, io)?;
        let key = kb.key_by_id(key_id).map_err(|_| "unable to retrieve key")?;
        let value_type = it.value().type_code();
        let plaintext = value_bytes(&it);
        let mut out = Vec::new();
        match (t, q) {
            (IndexType::None, _) => {
                return Ok(crate::fle2::uev_encrypt(key_id, value_type, plaintext, &key)?);
            }
            (_, Some(_)) => {
                out.push(crate::fle2::SUBTYPE_FIND_EQUALITY_V2);
                out.extend(crate::fle2::find_equality_payload(&key, plaintext, cf)?);
            }
            (_, None) => {
                // _fle2_choose_contention_factor: uniform in [0, maxContentionFactor]
                let k = if cf > 0 { crypto::random_int64(cf + 1)? } else { 0 };
                out.push(crate::fle2::SUBTYPE_IUP_V2);
                out.extend(crate::fle2::insert_update_payload(key_id, &key, key_id, &key, value_type, plaintext, k)?);
            }
        }
        Ok(out)
    }

    /// `_fle2_finalize_explicit` for a range insert: FLE2InsertUpdatePayloadV2 with edges
    fn encrypt_range<I: Io>(&self, io: &mut I, value: &[u8], key_id: &[u8; 16], ro: &crate::range::RangeOpts, cf: i64) -> Result<Vec<u8>, I::Error> {
        let mut it = Iter::new(value).ok_or("unable to convert input to BSON")?;
        if !it.find(b"v") {
            return Err("Error making FLE2RangeInsertSpec: Unable to find 'v' in input".into());
        }
        permitted_for_encryption(&it.value(), Fle1Algorithm::Random)?;
        let v = {
            let mut w = Writer::with_capacity(32);
            w.element_from(Some(b""), &it);
            crate::range::Val { t: it.value().type_code(), elem: w.finish() }
        };
        let spec = ro.insert_spec(&v)?;
        let mut kb = KeyBroker::new(&self.kms, &self.cache);
        kb.request_id(key_id);
        self.fetch(&mut kb, io)?;
        let key = kb.key_by_id(key_id).map_err(|_| "unable to retrieve key")?;
        let k = if cf > 0 { crypto::random_int64(cf + 1)? } else { 0 };
        let mut out = vec![crate::fle2::SUBTYPE_IUP_V2];
        out.extend(crate::range::insert_payload(key_id, &key, &spec, value_bytes(&it), ro.sparsity, k)?);
        Ok(out)
    }

    /// `_mongoc_crypt_explicit_encrypt_expression`
    pub fn explicit_encrypt_expression<I: Io>(&self, io: &mut I, expr: &[u8], o: &ExplicitOpts) -> Result<Vec<u8>, I::Error> {
        let c = check_explicit(o, true)?;
        let KeySel::Id(key_id) = c.key else { unreachable!("index types require a key id") };
        // FLE2RangeFindDriverSpec_to_ciphertexts
        let ro = c.range.as_ref().ok_or("Expected RangeOpts to be set for Range Find")?;
        let cf = o.contention_factor.ok_or("Expected Contention Factor to be set for Range Find")?;
        let mut kb = KeyBroker::new(&self.kms, &self.cache);
        kb.request_id(&key_id);
        self.fetch(&mut kb, io)?;
        let key = kb.key_by_id(&key_id).map_err(|_| "unable to retrieve key")?;
        let mut w = Writer::with_capacity(expr.len() + 8);
        w.document(b"v", expr);
        let out = crate::range::encrypt_expression(&w.finish(), ro, cf, &key)?;
        // _mongoc_crypt_explicit_encrypt_expression: the document under "v"
        let mut it = Iter::new(&out).ok_or("encrypted result unexpected: no 'v' found")?;
        match (it.find(b"v"), it.value()) {
            (true, Value::Document(d)) => Ok(d.to_vec()),
            _ => Err("encrypted result unexpected: no 'v' found".into()),
        }
    }
}
