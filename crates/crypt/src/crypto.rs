//! The value-encryption algorithms of libmongocrypt (`mongocrypt-crypto.c`), byte for byte:
//! FLE1 (AEAD_AES_256_CBC_HMAC_SHA_512), FLE2AEAD (AES-256-CTR + HMAC-SHA-256), FLE2 (AES-256-CTR) and
//! FLE2v2AEAD (AES-256-CBC + HMAC-SHA-256), the deterministic IV, key wrapping and the random helpers.
//! Error texts are libmongocrypt's.

use aes::cipher::{BlockDecryptMut, BlockEncryptMut, KeyIvInit, StreamCipher};
use hmac::{Hmac, KeyInit, Mac};
use sha2::{Sha256, Sha512};
use subtle::ConstantTimeEq;

pub const KEY_LEN: usize = 96;
pub const IV_KEY_LEN: usize = 32;
pub const MAC_KEY_LEN: usize = 32;
pub const ENC_KEY_LEN: usize = 32;
pub const IV_LEN: usize = 16;
pub const HMAC_LEN: usize = 32;
pub const BLOCK_SIZE: usize = 16;
pub const TOKEN_KEY_LEN: usize = 32;

pub type Result<T> = std::result::Result<T, String>;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Cbc,
    Ctr,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum HmacType {
    None,
    Sha512_256,
    Sha256,
}

/// Which part of the key is which: FLE1 keys are Km || Ke || Kiv (96 bytes); FLE2 keys are Ke (32 bytes);
/// FLE2AEAD and FLE2v2AEAD keys are Ke || Km || ... (96 bytes).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum KeyFormat {
    Fle1,
    Fle2,
    Fle2Aead,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum MacFormat {
    /// HMAC(AAD || IV || S || AL)
    Fle1,
    /// HMAC(AAD || IV || S)
    Fle2,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Algorithm {
    /// AES-256-CBC, HMAC-SHA-512 truncated to 256 bits
    Fle1,
    /// AES-256-CTR, HMAC-SHA-256
    Fle2Aead,
    /// AES-256-CTR, no MAC (ESC/ECOC tokens)
    Fle2,
    /// AES-256-CBC, HMAC-SHA-256
    Fle2v2Aead,
}

impl Algorithm {
    fn params(self) -> (KeyFormat, MacFormat, Mode, HmacType) {
        match self {
            Algorithm::Fle1 => (KeyFormat::Fle1, MacFormat::Fle1, Mode::Cbc, HmacType::Sha512_256),
            Algorithm::Fle2Aead => (KeyFormat::Fle2Aead, MacFormat::Fle2, Mode::Ctr, HmacType::Sha256),
            Algorithm::Fle2 => (KeyFormat::Fle2, MacFormat::Fle2, Mode::Ctr, HmacType::None),
            Algorithm::Fle2v2Aead => (KeyFormat::Fle2Aead, MacFormat::Fle2, Mode::Cbc, HmacType::Sha256),
        }
    }

    /// `_mongocrypt_calculate_ciphertext_len`
    pub fn ciphertext_len(self, inlen: usize) -> Result<usize> {
        let (_, _, mode, hmac) = self.params();
        ciphertext_len(inlen, mode, hmac)
    }

    /// `_mongocrypt_calculate_plaintext_len`
    pub fn plaintext_len(self, inlen: usize) -> Result<usize> {
        let (_, _, mode, hmac) = self.params();
        let hmaclen = if hmac == HmacType::None { 0 } else { HMAC_LEN };
        let mincipher = if mode == Mode::Ctr { 1 } else { BLOCK_SIZE };
        if inlen < IV_LEN + mincipher + hmaclen {
            return Err(format!("input ciphertext too small. Must be more than {} bytes", IV_LEN + mincipher + hmaclen));
        }
        Ok(inlen - (IV_LEN + hmaclen))
    }

    /// `_mongocrypt_do_encryption`: IV || S || T
    pub fn encrypt(self, iv: &[u8], aad: Option<&[u8]>, key: &[u8], plaintext: &[u8]) -> Result<Vec<u8>> {
        let (key_format, mac_format, mode, hmac) = self.params();
        if plaintext.is_empty() {
            return Err("input plaintext too small. Must be more than zero bytes.".into());
        }
        let total = ciphertext_len(plaintext.len(), mode, hmac)?;
        if iv.len() != IV_LEN {
            return Err(format!("IV should have length {IV_LEN}, but has length {}", iv.len()));
        }
        let expected_key_len = if key_format == KeyFormat::Fle2 { ENC_KEY_LEN } else { KEY_LEN };
        if key.len() != expected_key_len {
            return Err(format!("key should have length {expected_key_len}, but has length {}", key.len()));
        }
        let ke_off = if key_format == KeyFormat::Fle1 { MAC_KEY_LEN } else { 0 };
        let ke = &key[ke_off..ke_off + ENC_KEY_LEN];
        let mut out = Vec::with_capacity(total);
        out.extend_from_slice(iv);
        match mode {
            Mode::Ctr => {
                let mut s = plaintext.to_vec();
                aes256_ctr(ke, iv, &mut s);
                out.extend_from_slice(&s);
            }
            Mode::Cbc => {
                // PKCS#7: a full padding block for block-aligned input
                let pad = BLOCK_SIZE - plaintext.len() % BLOCK_SIZE;
                let mut s = Vec::with_capacity(plaintext.len() + pad);
                s.extend_from_slice(plaintext);
                s.resize(plaintext.len() + pad, pad as u8);
                aes256_cbc_encrypt(ke, iv, &mut s);
                out.extend_from_slice(&s);
            }
        }
        if hmac != HmacType::None {
            let km_off = if key_format == KeyFormat::Fle1 { 0 } else { ENC_KEY_LEN };
            let km = &key[km_off..km_off + MAC_KEY_LEN];
            let tag = hmac_step(mac_format, hmac, km, aad, &out);
            out.extend_from_slice(&tag);
        }
        debug_assert_eq!(out.len(), total);
        Ok(out)
    }

    /// `_mongocrypt_do_decryption`
    pub fn decrypt(self, aad: Option<&[u8]>, key: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>> {
        let (key_format, mac_format, mode, hmac) = self.params();
        let expect_plaintext_len = self.plaintext_len(ciphertext.len())?;
        if expect_plaintext_len == 0 {
            return Err(format!(
                "input ciphertext too small. Must be more than {} bytes",
                ciphertext_len(0, mode, hmac).unwrap_or(0)
            ));
        }
        let expected_key_len = if key_format == KeyFormat::Fle2 { ENC_KEY_LEN } else { KEY_LEN };
        if key.len() != expected_key_len {
            return Err(format!("key should have length {expected_key_len}, but has length {}", key.len()));
        }
        let min = ciphertext_len(0, mode, hmac).unwrap_or(0);
        if ciphertext.len() < min {
            return Err(format!("corrupt ciphertext - must be >= {min} bytes"));
        }
        let ke_off = if key_format == KeyFormat::Fle1 { MAC_KEY_LEN } else { 0 };
        let ke = &key[ke_off..ke_off + ENC_KEY_LEN];
        let iv = &ciphertext[..IV_LEN];
        let hmac_len = if hmac == HmacType::None { 0 } else { HMAC_LEN };
        if hmac != HmacType::None {
            let km_off = if key_format == KeyFormat::Fle1 { 0 } else { ENC_KEY_LEN };
            let km = &key[km_off..km_off + MAC_KEY_LEN];
            let body = &ciphertext[..ciphertext.len() - HMAC_LEN];
            let tag = hmac_step(mac_format, hmac, km, aad, body);
            let t = &ciphertext[ciphertext.len() - HMAC_LEN..];
            if !bool::from(tag.as_slice().ct_eq(t)) {
                return Err("HMAC validation failure".into());
            }
        }
        let s = &ciphertext[IV_LEN..ciphertext.len() - hmac_len];
        match mode {
            Mode::Cbc => {
                if s.len() % BLOCK_SIZE != 0 {
                    return Err("error, ciphertext length is not a multiple of block size".into());
                }
                let mut p = s.to_vec();
                aes256_cbc_decrypt(ke, iv, &mut p);
                // libmongocrypt only checks the last byte; the HMAC (if any) already authenticated the data
                let pad = *p.last().expect("at least one block") as usize;
                if pad > 16 {
                    return Err("error, ciphertext malformed padding".into());
                }
                p.truncate(p.len() - pad);
                Ok(p)
            }
            Mode::Ctr => {
                let mut p = s.to_vec();
                aes256_ctr(ke, iv, &mut p);
                Ok(p)
            }
        }
    }
}

fn ciphertext_len(inlen: usize, mode: Mode, hmac: HmacType) -> Result<usize> {
    let hmaclen = if hmac == HmacType::None { 0 } else { HMAC_LEN };
    let maxinlen = u32::MAX as usize - (IV_LEN + BLOCK_SIZE + hmaclen);
    if inlen > maxinlen {
        return Err("plaintext too long".into());
    }
    let fill = if mode == Mode::Cbc { BLOCK_SIZE - inlen % BLOCK_SIZE } else { 0 };
    Ok(IV_LEN + inlen + fill + hmaclen)
}

fn hmac_step(fmt: MacFormat, hmac: HmacType, km: &[u8], aad: Option<&[u8]>, iv_and_s: &[u8]) -> [u8; HMAC_LEN] {
    let al;
    let mut parts: [&[u8]; 3] = [aad.unwrap_or(&[]), iv_and_s, &[]];
    if fmt == MacFormat::Fle1 {
        al = (8 * aad.map_or(0, |a| a.len()) as u64).to_be_bytes();
        parts[2] = &al;
    }
    let mut out = [0u8; HMAC_LEN];
    match hmac {
        HmacType::Sha512_256 => out.copy_from_slice(&hmac_sha512(km, &parts)[..HMAC_LEN]),
        HmacType::Sha256 => out = hmac_sha256(km, &parts),
        HmacType::None => unreachable!("no MAC"),
    }
    out
}

pub fn hmac_sha512(key: &[u8], parts: &[&[u8]]) -> [u8; 64] {
    let mut m = <Hmac<Sha512> as KeyInit>::new_from_slice(key).expect("HMAC takes any key length");
    for p in parts {
        m.update(p);
    }
    m.finalize().into_bytes().into()
}

/// `_mongocrypt_hmac_sha_256`
pub fn hmac_sha256(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut m = <Hmac<Sha256> as KeyInit>::new_from_slice(key).expect("HMAC takes any key length");
    for p in parts {
        m.update(p);
    }
    m.finalize().into_bytes().into()
}

pub fn sha256(data: &[u8]) -> [u8; 32] {
    use sha2::Digest;
    Sha256::digest(data).into()
}

fn aes256_cbc_encrypt(key: &[u8], iv: &[u8], buf: &mut [u8]) {
    let mut c = cbc::Encryptor::<aes::Aes256>::new_from_slices(key, iv).expect("32-byte key, 16-byte IV");
    for block in buf.chunks_exact_mut(BLOCK_SIZE) {
        c.encrypt_block_mut(block.into());
    }
}

fn aes256_cbc_decrypt(key: &[u8], iv: &[u8], buf: &mut [u8]) {
    let mut c = cbc::Decryptor::<aes::Aes256>::new_from_slices(key, iv).expect("32-byte key, 16-byte IV");
    for block in buf.chunks_exact_mut(BLOCK_SIZE) {
        c.decrypt_block_mut(block.into());
    }
}

/// AES-256-CTR with the whole 16-byte IV as a big-endian counter (OpenSSL's EVP_aes_256_ctr)
pub fn aes256_ctr(key: &[u8], iv: &[u8], buf: &mut [u8]) {
    let mut c = ctr::Ctr128BE::<aes::Aes256>::new_from_slices(key, iv).expect("32-byte key, 16-byte IV");
    c.apply_keystream(buf);
}

/// `_mongocrypt_calculate_deterministic_iv`: HMAC-SHA-512(Kiv, AAD || AL || P) truncated to the IV length
pub fn deterministic_iv(key: &[u8], plaintext: &[u8], aad: &[u8]) -> Result<[u8; IV_LEN]> {
    if key.len() != KEY_LEN {
        return Err(format!("key should have length {KEY_LEN}, but has length {}\n", key.len()));
    }
    let iv_key = &key[ENC_KEY_LEN + MAC_KEY_LEN..];
    let al = (8 * aad.len() as u64).to_be_bytes();
    let tag = hmac_sha512(iv_key, &[aad, &al, plaintext]);
    let mut iv = [0u8; IV_LEN];
    iv.copy_from_slice(&tag[..IV_LEN]);
    Ok(iv)
}

pub fn random(n: usize) -> Result<Vec<u8>> {
    let mut v = vec![0u8; n];
    getrandom::fill(&mut v).map_err(|e| format!("failed to generate random bytes: {e}"))?;
    Ok(v)
}

/// `_mongocrypt_wrap_key`: FLE1 with a random IV and no associated data
pub fn wrap_key(kek: &[u8], dek: &[u8]) -> Result<Vec<u8>> {
    if dek.len() != KEY_LEN {
        return Err(format!("data encryption key is incorrect length, expected: {KEY_LEN}, got: {}", dek.len()));
    }
    let iv = random(IV_LEN)?;
    Algorithm::Fle1.encrypt(&iv, None, kek, dek)
}

/// `_mongocrypt_unwrap_key`
pub fn unwrap_key(kek: &[u8], encrypted_dek: &[u8]) -> Result<Vec<u8>> {
    let dek = Algorithm::Fle1.decrypt(None, kek, encrypted_dek)?;
    if dek.len() != KEY_LEN {
        return Err(format!("decrypted key is incorrect length, expected: {KEY_LEN}, got: {}", dek.len()));
    }
    Ok(dek)
}

/// `_mongocrypt_random_uint64`: uniform in [0, bound) without modulo bias (arc4random_uniform)
pub fn random_uint64(exclusive_upper_bound: u64) -> Result<u64> {
    if exclusive_upper_bound < 2 {
        return Ok(0);
    }
    let min = exclusive_upper_bound.wrapping_neg() % exclusive_upper_bound;
    loop {
        let b = random(8)?;
        let r = u64::from_le_bytes(b.try_into().expect("8 bytes"));
        if r >= min {
            return Ok(r % exclusive_upper_bound);
        }
    }
}

/// `_mongocrypt_random_int64`
pub fn random_int64(exclusive_upper_bound: i64) -> Result<i64> {
    if exclusive_upper_bound <= 0 {
        return Err("Expected exclusive_upper_bound > 0".into());
    }
    Ok((random_uint64(exclusive_upper_bound as u64)? & !(1u64 << 63)) as i64)
}
