//! The key broker (`mongocrypt-key-broker.c`) and the decrypted-key cache (`mongocrypt-cache-key.c`):
//! collects key requests (by id or keyAltName), builds the key vault filter, takes the returned key
//! documents, decrypts their key material through the KMS providers and hands out decrypted keys.

use crate::bson::raw_element;
use crate::key::{AltName, KeyDoc};
use crate::opts::{Creds, KmsProviders, KmsType};
use mongo_core::bson::iter::T_UTF8;
use mongo_core::bson::writer::Writer;
use std::sync::Mutex;
use std::time::Instant;

pub type Result<T> = std::result::Result<T, String>;

pub const CACHE_EXPIRATION_MS_DEFAULT: u64 = 60_000;

#[derive(Clone)]
struct CacheEntry {
    doc: KeyDoc,
    material: Vec<u8>,
    last_updated: Instant,
}

/// The decrypted keys of one libmongocrypt handle (per client or ClientEncryption)
pub struct KeyCache {
    entries: Mutex<Vec<CacheEntry>>,
    /// 0: never expires
    pub expiration_ms: u64,
}

impl KeyCache {
    pub fn new(expiration_ms: u64) -> KeyCache {
        KeyCache { entries: Mutex::new(Vec::new()), expiration_ms }
    }

    fn expired(&self, e: &CacheEntry) -> bool {
        self.expiration_ms > 0 && e.last_updated.elapsed().as_millis() as u64 > self.expiration_ms
    }

    fn get(&self, id: Option<&[u8; 16]>, alt: &[AltName]) -> Option<CacheEntry> {
        let mut g = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        g.retain(|e| !self.expired(e));
        g.iter()
            .find(|e| id.is_some_and(|i| *i == e.doc.id) || alt.iter().any(|a| e.doc.alt_names.contains(a)))
            .cloned()
    }

    fn put(&self, doc: &KeyDoc, material: &[u8]) {
        let mut g = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        g.retain(|e| e.doc.id != doc.id);
        g.push(CacheEntry { doc: doc.clone(), material: material.to_vec(), last_updated: Instant::now() });
    }
}

struct Request {
    id: Option<[u8; 16]>,
    alt_names: Vec<AltName>,
    satisfied: bool,
}

struct Returned {
    doc: KeyDoc,
    material: Option<Vec<u8>>,
}

/// One key broker per operation
pub struct KeyBroker<'a> {
    kms: &'a KmsProviders,
    cache: &'a KeyCache,
    requests: Vec<Request>,
    returned: Vec<Returned>,
    cached: Vec<Returned>,
    any: bool,
}

fn intersects(a: &[AltName], b: &[AltName]) -> bool {
    a.iter().any(|x| b.contains(x))
}

impl<'a> KeyBroker<'a> {
    pub fn new(kms: &'a KmsProviders, cache: &'a KeyCache) -> Self {
        KeyBroker { kms, cache, requests: Vec::new(), returned: Vec::new(), cached: Vec::new(), any: false }
    }

    fn find_request(&self, id: Option<&[u8; 16]>, alt: &[AltName]) -> Option<usize> {
        self.requests.iter().position(|r| (id.is_some() && r.id.as_ref() == id) || intersects(alt, &r.alt_names))
    }

    fn try_cache(&mut self, i: usize) {
        let r = &self.requests[i];
        if let Some(e) = self.cache.get(r.id.as_ref(), &r.alt_names) {
            self.requests[i].satisfied = true;
            self.cached.push(Returned { doc: e.doc, material: Some(e.material) });
        }
    }

    /// `_mongocrypt_key_broker_request_id`
    pub fn request_id(&mut self, id: &[u8; 16]) {
        if self.find_request(Some(id), &[]).is_some() {
            return;
        }
        self.requests.push(Request { id: Some(*id), alt_names: Vec::new(), satisfied: false });
        self.try_cache(self.requests.len() - 1);
    }

    /// `_mongocrypt_key_broker_request_name`
    pub fn request_name(&mut self, name: &[u8]) {
        let alt = vec![name.to_vec()];
        if self.find_request(None, &alt).is_some() {
            return;
        }
        self.requests.push(Request { id: None, alt_names: alt, satisfied: false });
        self.try_cache(self.requests.len() - 1);
    }

    /// `_mongocrypt_key_broker_request_any` (rewrapManyDataKey)
    pub fn request_any(&mut self) {
        self.any = true;
    }

    /// Whether the key vault must be queried
    pub fn needs_keys(&self) -> bool {
        self.any || self.requests.iter().any(|r| !r.satisfied)
    }

    /// `_mongocrypt_key_broker_filter`: `{$or: [{_id: {$in: [ids]}}, {keyAltNames: {$in: [names]}}]}`
    pub fn filter(&self) -> Vec<u8> {
        let mut ids = Writer::new();
        let mut names = Writer::new();
        let (mut ni, mut ii) = (0usize, 0usize);
        for r in self.requests.iter().filter(|r| !r.satisfied) {
            if let Some(id) = &r.id {
                ids.binary(ii.to_string().as_bytes(), crate::bson::SUBTYPE_UUID, id);
                ii += 1;
            }
            for n in &r.alt_names {
                let mut b = Vec::with_capacity(n.len() + 5);
                b.extend_from_slice(&((n.len() + 1) as u32).to_le_bytes());
                b.extend_from_slice(n);
                b.push(0);
                raw_element(&mut names, T_UTF8, ni.to_string().as_bytes(), &b);
                ni += 1;
            }
        }
        let (ids, names) = (ids.finish(), names.finish());
        let mut w = Writer::new();
        let or = w.begin(mongo_core::bson::iter::T_ARRAY, b"$or");
        let a = w.begin(mongo_core::bson::iter::T_DOCUMENT, b"0");
        let b = w.begin(mongo_core::bson::iter::T_DOCUMENT, b"_id");
        w.array(b"$in", &ids);
        w.end(b);
        w.end(a);
        let a = w.begin(mongo_core::bson::iter::T_DOCUMENT, b"1");
        let b = w.begin(mongo_core::bson::iter::T_DOCUMENT, b"keyAltNames");
        w.array(b"$in", &names);
        w.end(b);
        w.end(a);
        w.end(or);
        w.finish()
    }

    /// `_mongocrypt_key_broker_add_doc`: local keys are decrypted here, the others wait for KMS
    pub fn add_doc(&mut self, doc: &[u8]) -> Result<()> {
        let key = KeyDoc::parse(doc)?;
        match self.find_request(Some(&key.id), &key.alt_names) {
            Some(_) => {}
            None if !self.any => return Err("unexpected key returned, does not match any requests".into()),
            None => {
                self.requests.push(Request { id: Some(key.id), alt_names: key.alt_names.clone(), satisfied: false });
                let i = self.requests.len() - 1;
                self.try_cache(i);
                if self.requests[i].satisfied {
                    return Ok(());
                }
            }
        }
        if self.returned.iter().any(|r| r.doc.id == key.id || intersects(&r.doc.alt_names, &key.alt_names)) {
            return Err("keys returned have duplicate keyAltNames or _id".into());
        }
        let creds = self.kms.get(&key.kek.kmsid);
        if creds.is_none() {
            return Err(format!("KMS provider `{}` is not configured", key.kek.kmsid));
        }
        let mut material = None;
        if key.kek.kms_type() == KmsType::Local {
            let Some(Creds::Local { key: kek }) = creds else { unreachable!("local kmsid has local creds") };
            let m = crate::crypto::unwrap_key(kek, &key.key_material)?;
            self.cache.put(&key, &m);
            material = Some(m);
        }
        for r in self.requests.iter_mut() {
            if r.id == Some(key.id) {
                r.satisfied = true;
            }
            if intersects(&key.alt_names, &r.alt_names) {
                r.satisfied = true;
                r.id = Some(key.id);
            }
        }
        self.returned.push(Returned { doc: key, material });
        Ok(())
    }

    /// `_mongocrypt_key_broker_docs_done`
    pub fn docs_done(&self) -> Result<()> {
        if self.requests.iter().any(|r| !r.satisfied) {
            return Err(
                "not all keys requested were satisfied. Verify that key vault DB/collection name was correctly specified."
                    .into(),
            );
        }
        Ok(())
    }

    /// The returned keys whose material still needs a KMS round trip
    pub fn pending_kms(&mut self) -> impl Iterator<Item = (&KeyDoc, &mut Option<Vec<u8>>)> {
        self.returned.iter_mut().filter(|r| r.material.is_none()).map(|r| (&r.doc, &mut r.material))
    }

    /// Stores a key decrypted by a KMS
    pub fn kms_done(&mut self) -> Result<()> {
        for r in &self.returned {
            match &r.material {
                Some(m) if m.len() != crate::crypto::KEY_LEN => return Err("decrypted key is incorrect length".into()),
                Some(m) => self.cache.put(&r.doc, m),
                None => return Err("unexpected, key not decrypted".into()),
            }
        }
        Ok(())
    }

    /// `_mongocrypt_key_broker_decrypted_key_by_id`
    pub fn key_by_id(&self, id: &[u8; 16]) -> Result<Vec<u8>> {
        let r = self
            .returned
            .iter()
            .chain(self.cached.iter())
            .find(|r| r.doc.id == *id)
            .ok_or("could not find key")?;
        r.material.clone().ok_or_else(|| "unexpected, key not decrypted".into())
    }

    /// `_mongocrypt_key_broker_decrypted_key_by_name`: (material, id)
    pub fn key_by_name(&self, name: &[u8]) -> Result<(Vec<u8>, [u8; 16])> {
        let r = self
            .returned
            .iter()
            .chain(self.cached.iter())
            .find(|r| r.doc.alt_names.iter().any(|a| a == name))
            .ok_or("could not find key")?;
        Ok((r.material.clone().ok_or("unexpected, key not decrypted")?, r.doc.id))
    }

    /// The key documents returned (rewrapManyDataKey)
    pub fn docs(&self) -> impl Iterator<Item = (&KeyDoc, Option<&Vec<u8>>)> {
        self.returned.iter().map(|r| (&r.doc, r.material.as_ref()))
    }
}
