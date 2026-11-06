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
}

/// `mongocrypt_t`
pub struct Crypt {
    pub kms: KmsProviders,
    pub cache: KeyCache,
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
        Ok(Crypt { kms, cache: KeyCache::new(key_expiration_ms.unwrap_or(CACHE_EXPIRATION_MS_DEFAULT)) })
    }

    fn fetch<I: Io>(&self, kb: &mut KeyBroker, io: &mut I) -> Result<(), I::Error> {
        if kb.needs_keys() {
            let docs = io.find_keys(&kb.filter()).map_err(Error::Io)?;
            for d in docs {
                kb.add_doc(&d)?;
            }
        }
        kb.docs_done()?;
        if kb.pending_kms().next().is_some() {
            return Err(Error::Crypt("remote KMS providers are not supported by this build yet".into()));
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
        Ok(transform(doc, &mut |b| decrypt_value(&kb, b))?)
    }

    /// `mongocrypt_ctx_datakey_init` for an already-encrypted key: the key document to insert
    pub fn data_key_doc(
        &self,
        kek: &Kek,
        key_material: Option<&[u8]>,
        alt_names: &[Vec<u8>],
    ) -> std::result::Result<Vec<u8>, String> {
        let material = match key_material {
            Some(m) => {
                if m.len() != crypto::KEY_LEN {
                    return Err(format!("keyMaterial should have length {}, but has length {}", crypto::KEY_LEN, m.len()));
                }
                m.to_vec()
            }
            None => crypto::random(crypto::KEY_LEN)?,
        };
        let encrypted = match self.kms.get(&kek.kmsid) {
            Some(crate::opts::Creds::Local { key }) => crypto::wrap_key(key, &material)?,
            Some(_) => return Err("remote KMS providers are not supported by this build yet".into()),
            None => return Err(format!("KMS provider `{}` is not configured", kek.kmsid)),
        };
        key_document(kek, &encrypted, alt_names)
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
        for (i, n) in alt_names.iter().enumerate() {
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
        _ => Err("Queryable Encryption payloads are not supported by this build yet".into()),
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
        _ => Err("Queryable Encryption payloads are not supported by this build yet".into()),
    }
}
