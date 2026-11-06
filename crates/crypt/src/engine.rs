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
}

/// Where a KMS round trip failed
pub enum KmsFailure<E> {
    /// connecting or the TLS handshake (the error is reported as is)
    Connect(E),
    Write(E),
    Read(E),
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
        Ok(Crypt { kms, cache: KeyCache::new(key_expiration_ms.unwrap_or(CACHE_EXPIRATION_MS_DEFAULT)), tls: Vec::new() })
    }

    /// The TLS options of a KMS provider (libmongoc's per-kmsid `mongoc_ssl_opt_t`)
    fn tls_for(&self, kmsid: &str) -> mongo_core::tls::TlsOptions {
        self.tls.iter().find(|(k, _)| k == kmsid).map(|(_, o)| o.clone()).unwrap_or_default()
    }

    /// One KMS operation with libmongocrypt's retries: `make` builds the request, `parse` reads the
    /// complete HTTP response (Err(Some(status)) when the status is retryable)
    fn kms_call<I: Io>(
        &self,
        io: &mut I,
        req: &crate::kms::KmsRequest,
        retryable: bool,
        http_retry: fn(i32) -> bool,
        parse: &dyn Fn(&crate::kms::Response) -> std::result::Result<Vec<u8>, String>,
    ) -> Result<Vec<u8>, I::Error> {
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
        let Some(crate::opts::Creds::Aws { access_key_id, secret_access_key, session_token }) = self.kms.get(&kek.kmsid) else {
            return Err(format!("KMS provider `{}` is not configured", kek.kmsid).into());
        };
        // the Host header: decryption uses host:port, encryption the host only (as libmongocrypt)
        let host = endpoint.as_ref().map(|e| if decrypt { e.host_and_port.clone() } else { e.host.clone() });
        let msg = crate::kms::aws_request(decrypt, data, Some(cmk), region, access_key_id, secret_access_key, session_token.as_deref(), host.as_deref(), &crate::kms::amz_datetime())?;
        let mut ep = endpoint.as_ref().map_or_else(|| format!("kms.{region}.amazonaws.com"), |e| e.host_and_port.clone());
        if endpoint.as_ref().is_none_or(|e| e.port.is_none()) {
            ep.push_str(":443");
        }
        let req = crate::kms::KmsRequest { kmsid: kek.kmsid.clone(), endpoint: ep, message: msg, tls: self.tls_for(&kek.kmsid) };
        let field = if decrypt { "Plaintext" } else { "CiphertextBlob" };
        self.kms_call(io, &req, true, |s| [408, 429, 500, 502, 503, 509].contains(&s), &|r| crate::kms::aws_done(r, field))
    }

    /// The key material of a key document whose KEK is remote
    fn kms_unwrap<I: Io>(&self, io: &mut I, key: &crate::key::KeyDoc) -> Result<Vec<u8>, I::Error> {
        match key.kek.kms_type() {
            crate::opts::KmsType::Aws => self.aws(io, &key.kek, true, &key.key_material),
            t => Err(format!("the {} KMS provider is not supported by this build yet", t.name()).into()),
        }
    }

    fn fetch<I: Io>(&self, kb: &mut KeyBroker, io: &mut I) -> Result<(), I::Error> {
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
        let encrypted = self.wrap(io, kek, &material)?;
        Ok(key_document(kek, &encrypted, alt_names)?)
    }

    /// The datakey context's KMS step: `material` encrypted with `kek`
    fn wrap<I: Io>(&self, io: &mut I, kek: &Kek, material: &[u8]) -> Result<Vec<u8>, I::Error> {
        match self.kms.get(&kek.kmsid) {
            Some(crate::opts::Creds::Local { key }) => Ok(crypto::wrap_key(key, material)?),
            Some(crate::opts::Creds::Aws { .. }) => {
                let e = self.aws(io, kek, false, material)?;
                if e.len() < crypto::KEY_LEN {
                    return Err("key material not expected length".into());
                }
                Ok(e)
            }
            Some(_) => Err(format!("the {} KMS provider is not supported by this build yet", kek.kms_type().name()).into()),
            None if self.kms.need_credentials.contains(&kek.kms_type()) && !kek.kmsid.contains(':') => {
                Err("on-demand KMS credentials are not supported by this build yet".into())
            }
            None => Err(format!("requested kms provider required by datakey is not configured: `{}`", kek.kmsid).into()),
        }
    }

    /// `mongocrypt_ctx_rewrap_many_datakey_init` for one key document: its id and the
    /// `{masterKey, keyMaterial}` to `$set` (re-encrypted with `new_kek`, or with its own KEK)
    pub fn rewrap<I: Io>(&self, io: &mut I, doc: &[u8], new_kek: Option<&Kek>) -> Result<([u8; 16], Vec<u8>), I::Error> {
        let key = crate::key::KeyDoc::parse(doc)?;
        let material = match self.kms.get(&key.kek.kmsid) {
            None => return Err(format!("KMS provider `{}` is not configured", key.kek.kmsid).into()),
            Some(crate::opts::Creds::Local { key: kek }) => crypto::unwrap_key(kek, &key.key_material)?,
            Some(_) => self.kms_unwrap(io, &key)?,
        };
        if material.len() != crypto::KEY_LEN {
            return Err("decrypted key is incorrect length".into());
        }
        let kek = new_kek.unwrap_or(&key.kek);
        let encrypted = self.wrap(io, kek, &material)?;
        let mut w = Writer::with_capacity(256);
        let p = w.begin(T_DOCUMENT, b"masterKey");
        kek.append(&mut w).map_err(Error::Crypt)?;
        w.end(p);
        w.binary(b"keyMaterial", 0, &encrypted);
        Ok((key.id, w.finish()))
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
    Ok(Checked { fle1, index_type, query_type, range, key })
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
            _ => Err("Queryable Encryption's range and string algorithms are not supported by this build yet".into()),
        }
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
