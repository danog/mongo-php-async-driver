//! Queryable Encryption (FLE2 v2) payloads of libmongocrypt: the token derivations
//! (`mc-tokens.c`), FLE2UnindexedEncryptedValueV2 (`mc-fle2-payload-uev*.c`),
//! FLE2InsertUpdatePayloadV2 (`mc-fle2-insert-update-payload-v2.c`), FLE2FindEqualityPayloadV2
//! (`mc-fle2-find-equality-payload-v2.c`) and the decryption of FLE2IndexedEncryptedValueV2
//! (`mc-fle2-payload-iev-v2.c`). Messages are libmongocrypt's.

use crate::bson::validate;
use crate::crypto::{self, Algorithm, IV_LEN, KEY_LEN, TOKEN_KEY_LEN};
use mongo_core::bson::iter::{Iter, Value};
use mongo_core::bson::writer::Writer;

pub type Result<T> = std::result::Result<T, String>;

pub const SUBTYPE_IUP_V2: u8 = 11;
pub const SUBTYPE_FIND_EQUALITY_V2: u8 = 12;
pub const SUBTYPE_IEV_EQUALITY_V2: u8 = 14;
pub const SUBTYPE_IEV_RANGE_V2: u8 = 15;
pub const SUBTYPE_UEV_V2: u8 = 16;
pub const SUBTYPE_IEV_TEXT: u8 = 17;

pub type Token = [u8; 32];

/// HMAC-SHA-256(key, data)
pub fn hmac(key: &[u8], data: &[u8]) -> Token {
    crypto::hmac_sha256(key, &[data])
}

/// HMAC-SHA-256(key, u as uint64 little-endian)
pub fn hmac_u64(key: &[u8], u: u64) -> Token {
    crypto::hmac_sha256(key, &[&u.to_le_bytes()])
}

/// The token key: the last 32 bytes of a 96-byte data key ([Ke][Km][TokenKey])
pub fn token_key(index_key: &[u8]) -> Result<&[u8]> {
    if index_key.len() != KEY_LEN {
        return Err(format!("invalid indexKey, expected len={KEY_LEN}, got len={}", index_key.len()));
    }
    Ok(&index_key[2 * TOKEN_KEY_LEN..])
}

/// `_FLE2EncryptedPayloadCommon_t`: the tokens shared by insert and find payloads
pub struct Common {
    pub collections_level1: Token,
    pub server_data_encryption_level1: Token,
    pub edc_derived: Token,
    pub esc_derived: Token,
    pub server_derived_from_data: Token,
}

/// `_mongocrypt_fle2_placeholder_common`
pub fn common(token_key: &[u8], value: &[u8], contention: Option<u64>) -> Common {
    let cl1 = hmac_u64(token_key, 1);
    let sde1 = hmac_u64(token_key, 3);
    let edc = hmac_u64(&cl1, 1);
    let esc = hmac_u64(&cl1, 2);
    let mut edc_d = hmac(&edc, value);
    let mut esc_d = hmac(&esc, value);
    if let Some(u) = contention {
        edc_d = hmac_u64(&edc_d, u);
        esc_d = hmac_u64(&esc_d, u);
    }
    let std1 = hmac_u64(token_key, 2);
    let sdfd = hmac(&std1, value);
    Common { collections_level1: cl1, server_data_encryption_level1: sde1, edc_derived: edc_d, esc_derived: esc_d, server_derived_from_data: sdfd }
}

/// `_fle2_placeholder_aes_ctr_encrypt`: FLE2 (CTR, no MAC) with a random IV
fn ctr_encrypt(key: &[u8], data: &[u8]) -> Result<Vec<u8>> {
    let iv = crypto::random(IV_LEN)?;
    Algorithm::Fle2.encrypt(&iv, None, key, data)
}

/// `_fle2_derive_encrypted_token` (v2): p = EncryptCTR(ECOCToken, ESCDerived [|| suffix])
pub fn encrypted_tokens(collections_level1: &Token, esc_derived: &Token, suffix: &[u8]) -> Result<Vec<u8>> {
    let ecoc = hmac_u64(collections_level1, 4);
    let mut p = esc_derived.to_vec();
    p.extend_from_slice(suffix);
    ctr_encrypt(&ecoc, &p)
}

/// `_fle2_placeholder_aes_aead_encrypt` with FLE2v2AEAD: the associated data is the key id
fn aead_encrypt(key: &[u8], key_id: &[u8], plaintext: &[u8]) -> Result<Vec<u8>> {
    let iv = crypto::random(IV_LEN)?;
    Algorithm::Fle2v2Aead.encrypt(&iv, Some(key_id), key, plaintext)
}

/// FLE2InsertUpdatePayloadV2 of an equality-indexed insert: the BSON payload (without the
/// subtype byte)
#[allow(clippy::too_many_arguments)]
pub fn insert_update_payload(
    index_key_id: &[u8; 16],
    index_key: &[u8],
    user_key_id: &[u8; 16],
    user_key: &[u8],
    value_type: u8,
    value: &[u8],
    contention: i64,
) -> Result<Vec<u8>> {
    let c = common(token_key(index_key)?, value, Some(contention as u64));
    let p = encrypted_tokens(&c.collections_level1, &c.esc_derived, &[])?;
    let ct = aead_encrypt(user_key, user_key_id, value)?;
    let mut v = user_key_id.to_vec();
    v.extend_from_slice(&ct);
    let mut w = Writer::with_capacity(512);
    w.binary(b"d", 0, &c.edc_derived);
    w.binary(b"s", 0, &c.esc_derived);
    w.binary(b"p", 0, &p);
    w.binary(b"u", 4, index_key_id);
    w.int32(b"t", value_type as i32);
    w.binary(b"v", 0, &v);
    w.binary(b"e", 0, &c.server_data_encryption_level1);
    w.binary(b"l", 0, &c.server_derived_from_data);
    w.int64(b"k", contention);
    Ok(w.finish())
}

/// FLE2FindEqualityPayloadV2: {d, s, l, cm} (tokens without the contention factor)
pub fn find_equality_payload(index_key: &[u8], value: &[u8], max_contention: i64) -> Result<Vec<u8>> {
    let c = common(token_key(index_key)?, value, None);
    let mut w = Writer::with_capacity(160);
    w.binary(b"d", 0, &c.edc_derived);
    w.binary(b"s", 0, &c.esc_derived);
    w.binary(b"l", 0, &c.server_derived_from_data);
    w.int64(b"cm", max_contention);
    Ok(w.finish())
}

/// `mc_FLE2UnindexedEncryptedValueV2_encrypt` + `_mongocrypt_serialize_ciphertext`: the whole blob
pub fn uev_encrypt(key_id: &[u8; 16], value_type: u8, plaintext: &[u8], key: &[u8]) -> Result<Vec<u8>> {
    let mut ad = Vec::with_capacity(18);
    ad.push(SUBTYPE_UEV_V2);
    ad.extend_from_slice(key_id);
    ad.push(value_type);
    let iv = crypto::random(IV_LEN)?;
    let ct = Algorithm::Fle2v2Aead.encrypt(&iv, Some(&ad), key, plaintext)?;
    let mut out = ad;
    out.extend_from_slice(&ct);
    Ok(out)
}

/// A parsed FLE2UnindexedEncryptedValueV2
pub struct Uev<'a> {
    pub key_id: [u8; 16],
    pub value_type: u8,
    ciphertext: &'a [u8],
}

fn reader_err(name: &str, need: usize, len: usize) -> String {
    format!("{name} expected byte length >= {need} got: {len}")
}

impl<'a> Uev<'a> {
    /// `mc_FLE2UnindexedEncryptedValueV2_parse`
    pub fn parse(b: &'a [u8]) -> Result<Uev<'a>> {
        const N: &str = "mc_FLE2UnindexedEncryptedValueV2_parse";
        if b.is_empty() {
            return Err(reader_err(N, 1, 0));
        }
        if b.len() < 17 {
            return Err(reader_err(N, 17, b.len()));
        }
        if b.len() < 18 {
            return Err(reader_err(N, 18, b.len()));
        }
        Ok(Uev { key_id: b[1..17].try_into().expect("16 bytes"), value_type: b[17], ciphertext: &b[18..] })
    }

    /// `_mc_FLE2UnindexedEncryptedValueCommon_decrypt` (v2)
    pub fn decrypt(&self, key: &[u8]) -> Result<Vec<u8>> {
        let mut ad = Vec::with_capacity(18);
        ad.push(SUBTYPE_UEV_V2);
        ad.extend_from_slice(&self.key_id);
        ad.push(self.value_type);
        Algorithm::Fle2v2Aead.decrypt(Some(&ad), key, self.ciphertext)
    }
}

/// The fields of a FLE2InsertUpdatePayloadV2 decryption needs
pub struct Iup {
    pub user_key_id: [u8; 16],
    pub value_type: u8,
    pub value: Vec<u8>,
}

impl Iup {
    /// `mc_FLE2InsertUpdatePayloadV2_parse` (the input starts with the subtype byte)
    pub fn parse(b: &[u8]) -> Result<Iup> {
        if b.is_empty() {
            return Err("FLE2InsertUpdatePayloadV2_parse got too short input".into());
        }
        let doc = &b[1..];
        if !mongo_core::bson::iter::is_valid_static(doc) {
            return Err("FLE2InsertUpdatePayloadV2_parse got invalid BSON".into());
        }
        if !validate(doc) {
            return Err("invalid BSON".into());
        }
        let mut it = Iter::new(doc).ok_or("invalid BSON")?;
        let mut seen: Vec<Vec<u8>> = Vec::new();
        let (mut t, mut v) = (None, None);
        while it.next() {
            let k = it.key().to_vec();
            let known: &[&[u8]] = &[b"d", b"s", b"p", b"u", b"t", b"k", b"v", b"e", b"l", b"sp", b"pn", b"tf", b"mn", b"mx", b"g", b"b"];
            if known.contains(&&k[..]) {
                if seen.contains(&k) {
                    return Err(format!("Duplicate field '{}' in payload bson", String::from_utf8_lossy(&k)));
                }
                seen.push(k.clone());
            }
            let val = it.value();
            match &k[..] {
                b"d" | b"s" | b"p" | b"v" | b"e" | b"l" | b"u" => {
                    let want = if k == b"u" { 4 } else { 0 };
                    match val {
                        Value::Binary(st, d) if st == want => {
                            if k == b"v" {
                                v = Some(d.to_vec());
                            }
                        }
                        Value::Binary(st, _) => {
                            return Err(format!("Field '{}' expected to be bindata subtype {want}, got: {st}", String::from_utf8_lossy(&k)))
                        }
                        o => return Err(format!("Field '{}' expected to be bindata, got: {}", String::from_utf8_lossy(&k), o.type_code())),
                    }
                }
                b"t" => match val {
                    Value::Int32(x) if (0..=0xff).contains(&x) => t = Some(x as u8),
                    Value::Int32(x) => return Err(format!("Field 't' must be a valid BSON type, got: {x}")),
                    _ => return Err("Field 't' expected to hold an int32".into()),
                },
                b"k" => match val {
                    Value::Int64(c) => crate::engine::validate_contention(c)?,
                    _ => return Err("Field 'k' expected to hold an int64".into()),
                },
                _ => {}
            }
        }
        for f in ["d", "s", "p", "u", "t", "v", "e", "l", "k"] {
            if !seen.iter().any(|s| s == f.as_bytes()) {
                return Err(format!("Missing field '{f}' in payload"));
            }
        }
        let v = v.expect("checked above");
        if v.len() < 16 {
            return Err(format!("FLE2InsertUpdatePayloadV2 value not long enough. Expected > 16, got: {}", v.len()));
        }
        Ok(Iup { user_key_id: v[..16].try_into().expect("16 bytes"), value_type: t.expect("checked above"), value: v })
    }

    /// `mc_FLE2InsertUpdatePayloadV2_decrypt`
    pub fn decrypt(&self, user_key: &[u8]) -> Result<Vec<u8>> {
        Algorithm::Fle2v2Aead.decrypt(Some(&self.user_key_id), user_key, &self.value[16..])
    }
}

/// A parsed FLE2IndexedEncryptedValueV2 (equality, range or text)
pub struct Iev<'a> {
    pub s_key_id: [u8; 16],
    pub value_type: u8,
    server_encrypted_value: &'a [u8],
}

pub struct IevInner {
    pub k_key_id: [u8; 16],
    client_encrypted_value: Vec<u8>,
}

impl<'a> Iev<'a> {
    /// `mc_FLE2IndexedEncryptedValueV2_parse`
    pub fn parse(b: &'a [u8]) -> Result<Iev<'a>> {
        const N: &str = "mc_FLE2IndexedEncryptedValueV2_parse";
        if b.is_empty() {
            return Err("Empty buffer passed to mc_FLE2IndexedEncryptedValueV2_parse".into());
        }
        let st = b[0];
        if st != SUBTYPE_IEV_EQUALITY_V2 && st != SUBTYPE_IEV_RANGE_V2 && st != SUBTYPE_IEV_TEXT {
            return Err(format!(
                "mc_FLE2IndexedEncryptedValueV2_parse expected fle_blob_subtype MC_SUBTYPE_FLE2Indexed(Equality|Range|Text)EncryptedValue[V2] got: {st}"
            ));
        }
        let need = |n: usize| if b.len() < n { Err(reader_err(N, n, b.len())) } else { Ok(()) };
        need(17)?;
        let s_key_id: [u8; 16] = b[1..17].try_into().expect("16 bytes");
        need(18)?;
        let value_type = b[17];
        let mut pos = 18usize;
        let edge_count: u64 = match st {
            SUBTYPE_IEV_EQUALITY_V2 => 1,
            SUBTYPE_IEV_RANGE_V2 => {
                need(pos + 1)?;
                let ec = b[pos];
                pos += 1;
                if ec == 0 {
                    return Err(
                        "mc_FLE2IndexedEncryptedValueV2_parse edge count must not be 0 for type range, but found edge count is 0."
                            .into(),
                    );
                }
                ec as u64
            }
            _ => {
                let rd = |pos: &mut usize| -> Result<u32> {
                    need(*pos + 4)?;
                    let v = u32::from_le_bytes(b[*pos..*pos + 4].try_into().expect("4 bytes"));
                    *pos += 4;
                    Ok(v)
                };
                let ec = rd(&mut pos)?;
                let sub = rd(&mut pos)?;
                let suf = rd(&mut pos)?;
                if (ec as u64) < sub as u64 + suf as u64 + 1 {
                    return Err(format!(
                        "mc_FLE2IndexedEncryptedValueV2_parse for text type expected edge count to be at least substr_tag_count + suffix_count + 1, but: {ec} < {sub} + {suf} + 1"
                    ));
                }
                ec as u64
            }
        };
        let metadata_len = edge_count * 96;
        let rest = (b.len() - pos) as u64;
        let min = 17 + metadata_len;
        if rest < min {
            return Err(format!("Invalid payload size {rest}, smaller than minimum length {min}"));
        }
        let sev_len = (rest - metadata_len) as usize;
        Ok(Iev { s_key_id, value_type, server_encrypted_value: &b[pos..pos + sev_len] })
    }

    /// `mc_FLE2IndexedEncryptedValueV2_add_S_Key`: the K key id and the client ciphertext
    pub fn add_s_key(&self, s_key: &[u8]) -> Result<IevInner> {
        if s_key.len() != KEY_LEN {
            return Err(format!("mc_FLE2IndexedEncryptedValueV2_add_S_Key expected S_Key to be {KEY_LEN} bytes, got: {}", s_key.len()));
        }
        let token = hmac_u64(&s_key[KEY_LEN - TOKEN_KEY_LEN..], 3);
        let n = Algorithm::Fle2.plaintext_len(self.server_encrypted_value.len())?;
        if n <= 16 {
            return Err(format!("Invalid ServerEncryptedValue length, got {n}, expected more than 16"));
        }
        let d = Algorithm::Fle2.decrypt(None, &token, self.server_encrypted_value)?;
        Ok(IevInner { k_key_id: d[..16].try_into().expect("16 bytes"), client_encrypted_value: d[16..].to_vec() })
    }
}

impl IevInner {
    /// `mc_FLE2IndexedEncryptedValueV2_add_K_Key`: the client value
    pub fn add_k_key(&self, k_key: &[u8]) -> Result<Vec<u8>> {
        Algorithm::Fle2v2Aead.decrypt(Some(&self.k_key_id), k_key, &self.client_encrypted_value)
    }
}
