//! SCRAM-SHA-1 and SCRAM-SHA-256 (RFC 5802, RFC 7677), a port of mongoc-scram.c: the client side
//! of the conversation, with libmongoc's messages, buffer limits and process-wide cache of
//! salted passwords. Pure Rust (RustCrypto); the server signature is compared in constant time.

use std::sync::Mutex;

use hmac::{Hmac, KeyInit, Mac};
use sha1::Sha1;
use sha2::{Digest, Sha256};

use crate::error::{code, domain, Error};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Algo {
    Sha1,
    Sha256,
}

impl Algo {
    pub fn mechanism(self) -> &'static str {
        match self {
            Algo::Sha1 => "SCRAM-SHA-1",
            Algo::Sha256 => "SCRAM-SHA-256",
        }
    }
    fn hash_size(self) -> usize {
        match self {
            Algo::Sha1 => 20,
            Algo::Sha256 => 32,
        }
    }
}

/// libmongoc's conversation buffers (`uint8_t buf[4096]`, and an auth message as big): every
/// message and the auth message must stay below this size.
pub const BUF_MAX: usize = 4096;
/// MONGOC_SCRAM_B64_HASH_MAX_SIZE: the decoded salt buffer.
const SALT_MAX: usize = 64;
/// The auth specification's minimum iteration count (downgrade protection).
pub const MIN_ITERATIONS: i64 = 4096;
/// The largest iteration count accepted (safety; libmongoc has no upper bound): the count comes
/// from the server and PBKDF2 runs that many HMACs. MongoDB's defaults are 10000 (SCRAM-SHA-1)
/// and 15000 (SCRAM-SHA-256).
pub const MAX_ITERATIONS: i64 = 10_000_000;
/// MONGOC_SCRAM_CACHE_SIZE
const CACHE_SIZE: usize = 64;

const HASH_MAX: usize = 32;

fn err(m: impl Into<String>) -> Error {
    Error::new(domain::SCRAM, code::SCRAM_PROTOCOL_ERROR, m)
}

/// Overwrites `b` (secrets are cleared when they are dropped).
fn wipe(b: &mut [u8]) {
    for x in b.iter_mut() {
        // SAFETY: a valid, exclusive reference
        unsafe { std::ptr::write_volatile(x, 0) };
    }
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
}

fn wipe_string(s: &mut String) {
    // SAFETY: zero bytes are valid UTF-8
    wipe(unsafe { s.as_mut_vec() });
}

/// Equality in time that depends only on the lengths (mongoc_memcmp).
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut d = 0u8;
    for (x, y) in a.iter().zip(b) {
        d |= std::hint::black_box(x ^ y);
    }
    std::hint::black_box(d) == 0
}

fn hmac(algo: Algo, key: &[u8], data: &[u8], out: &mut [u8; HASH_MAX]) {
    match algo {
        Algo::Sha1 => {
            let mut m = <Hmac<Sha1> as KeyInit>::new_from_slice(key).expect("HMAC takes any key size");
            m.update(data);
            out[..20].copy_from_slice(&m.finalize().into_bytes());
        }
        Algo::Sha256 => {
            let mut m = <Hmac<Sha256> as KeyInit>::new_from_slice(key).expect("HMAC takes any key size");
            m.update(data);
            out[..32].copy_from_slice(&m.finalize().into_bytes());
        }
    }
}

fn hash(algo: Algo, data: &[u8], out: &mut [u8; HASH_MAX]) {
    match algo {
        Algo::Sha1 => out[..20].copy_from_slice(&Sha1::digest(data)),
        Algo::Sha256 => out[..32].copy_from_slice(&Sha256::digest(data)),
    }
}

fn pbkdf2(algo: Algo, password: &[u8], salt: &[u8], iterations: u32, out: &mut [u8; HASH_MAX]) {
    let n = algo.hash_size();
    match algo {
        Algo::Sha1 => pbkdf2::pbkdf2_hmac::<Sha1>(password, salt, iterations, &mut out[..n]),
        Algo::Sha256 => pbkdf2::pbkdf2_hmac::<Sha256>(password, salt, iterations, &mut out[..n]),
    }
}

/// SHA-256 of `data` (also the URI digest of phongo's client hash).
pub fn sha256(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// mcommon_b64_ntop
pub fn b64_encode(src: &[u8]) -> String {
    let mut o = String::with_capacity(src.len().div_ceil(3) * 4);
    for c in src.chunks(3) {
        let n = (u32::from(c[0]) << 16) | (u32::from(*c.get(1).unwrap_or(&0)) << 8) | u32::from(*c.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= c.len() {
                o.push(B64[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                o.push('=');
            }
        }
    }
    o
}

fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// mcommon_b64_pton into a buffer of `targsize` bytes: whitespace is skipped, the input ends
/// at a NUL or at the padding; None for invalid input or when the result does not fit.
pub fn b64_decode(src: &[u8], targsize: usize) -> Option<Vec<u8>> {
    let src = &src[..src.iter().position(|&c| c == 0).unwrap_or(src.len())];
    let mut t: Vec<u8> = Vec::new();
    let mut state = 0;
    let mut i = 0;
    let mut pad = false;
    while i < src.len() {
        let ch = src[i];
        i += 1;
        if is_space(ch) {
            continue;
        }
        if ch == b'=' {
            pad = true;
            break;
        }
        let ofs = B64.iter().position(|&c| c == ch)? as u8;
        match state {
            0 => {
                if t.len() >= targsize {
                    return None;
                }
                t.push(ofs << 2);
                state = 1;
            }
            1 => {
                if t.len() >= targsize {
                    return None;
                }
                *t.last_mut()? |= ofs >> 4;
                t.push((ofs & 0x0f) << 4);
                state = 2;
            }
            2 => {
                if t.len() >= targsize {
                    return None;
                }
                *t.last_mut()? |= ofs >> 2;
                t.push((ofs & 0x03) << 6);
                state = 3;
            }
            _ => {
                *t.last_mut()? |= ofs;
                state = 0;
            }
        }
    }
    // t holds one partial byte past the decoded ones in states 2 and 3
    if pad {
        let rest = &src[i..];
        let mut j = 0;
        match state {
            0 | 1 => return None,
            2 => {
                while j < rest.len() && is_space(rest[j]) {
                    j += 1;
                }
                if rest.get(j) != Some(&b'=') {
                    return None;
                }
                j += 1;
            }
            _ => {}
        }
        if rest[j..].iter().any(|&c| !is_space(c)) {
            return None;
        }
        // the bits past the last full byte must be zero
        if t.pop() != Some(0) {
            return None;
        }
        Some(t)
    } else if state != 0 {
        None
    } else {
        Some(t)
    }
}

struct CacheEntry {
    algo: Algo,
    hashed_password: Vec<u8>,
    salt: Vec<u8>,
    iterations: u32,
    client_key: [u8; HASH_MAX],
    server_key: [u8; HASH_MAX],
    salted_password: [u8; HASH_MAX],
}

/// The process-wide cache of derived secrets (g_scram_cache). Keyed like libmongoc's (hashed
/// password, salt, iteration count), plus the mechanism (as the auth specification asks).
static CACHE: Mutex<Vec<CacheEntry>> = Mutex::new(Vec::new());

/// The client side of one SCRAM conversation (mongoc_scram_t).
pub struct Scram {
    algo: Algo,
    pub step: u8,
    user: String,
    pass: String,
    hashed_password: Vec<u8>,
    salt: Vec<u8>,
    iterations: u32,
    client_key: Option<[u8; HASH_MAX]>,
    server_key: Option<[u8; HASH_MAX]>,
    salted_password: Option<[u8; HASH_MAX]>,
    nonce: String,
    auth_message: Vec<u8>,
    /// the salt must be hash size - 4 bytes long (as MongoDB's); the RFC 7677 example salt is
    /// 16 bytes, so its test turns this off
    check_salt_len: bool,
}

impl Drop for Scram {
    fn drop(&mut self) {
        wipe_string(&mut self.pass);
        wipe(&mut self.hashed_password);
        for k in [&mut self.client_key, &mut self.server_key, &mut self.salted_password].into_iter().flatten() {
            wipe(k);
        }
    }
}

impl std::fmt::Debug for Scram {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Scram").field("algo", &self.algo).field("step", &self.step).finish_non_exhaustive()
    }
}

/// Appends `s` to `buf` if the result stays below BUF_MAX (_mongoc_scram_buf_write).
fn buf_write(buf: &mut Vec<u8>, s: &[u8]) -> bool {
    if buf.len() + s.len() >= BUF_MAX {
        return false;
    }
    buf.extend_from_slice(s);
    true
}

/// Splits a SCRAM message into its `k=v` attributes, accepting only the keys in `keys`
/// (libmongoc's step 2/3 parsers: a later value replaces an earlier one).
fn parse_attrs<'a>(input: &'a [u8], keys: &[u8], step: u8) -> Result<Vec<(u8, &'a [u8])>, Error> {
    let mut out: Vec<(u8, &[u8])> = Vec::new();
    let mut p = 0;
    while p < input.len() {
        let k = input[p];
        if !keys.contains(&k) {
            return Err(err(format!("SCRAM Failure: unknown key ({}) in sasl step {step}", String::from_utf8_lossy(&[k]))));
        }
        p += 1;
        if p >= input.len() || input[p] != b'=' {
            return Err(err(format!("SCRAM Failure: invalid parse state in sasl step {step}")));
        }
        p += 1;
        let end = input[p..].iter().position(|&c| c == b',').map(|e| p + e);
        let v = &input[p..end.unwrap_or(input.len())];
        // a C string: the value ends at a NUL
        let v = &v[..v.iter().position(|&c| c == 0).unwrap_or(v.len())];
        out.retain(|(x, _)| *x != k);
        out.push((k, v));
        match end {
            Some(e) => p = e + 1,
            None => break,
        }
    }
    Ok(out)
}

fn attr<'a>(a: &[(u8, &'a [u8])], k: u8) -> Option<&'a [u8]> {
    a.iter().find(|(x, _)| *x == k).map(|(_, v)| *v)
}

/// bson_ascii_strtoll(s, &end, 10) with `*end == 0` required: None when the value does not
/// parse; out-of-range values saturate (libmongoc's strtoll reports ERANGE with the limit).
fn parse_iterations(s: &[u8]) -> Option<i64> {
    let mut i = 0;
    while i < s.len() && is_space(s[i]) {
        i += 1;
    }
    let neg = match s.get(i) {
        Some(b'-') => {
            i += 1;
            true
        }
        Some(b'+') => {
            i += 1;
            false
        }
        _ => false,
    };
    let digits = &s[i..];
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let mut n: i64 = 0;
    for &d in digits {
        n = n.saturating_mul(10).saturating_add(i64::from(d - b'0'));
    }
    Some(if neg { -n } else { n })
}

impl Scram {
    /// _mongoc_scram_init + set_user/set_pass (a missing password is "")
    pub fn new(algo: Algo, user: &str, pass: Option<&str>) -> Self {
        Scram {
            algo,
            step: 0,
            user: user.to_owned(),
            pass: pass.unwrap_or("").to_owned(),
            hashed_password: Vec::new(),
            salt: Vec::new(),
            iterations: 0,
            client_key: None,
            server_key: None,
            salted_password: None,
            nonce: String::new(),
            auth_message: Vec::new(),
            check_salt_len: true,
        }
    }

    pub fn algo(&self) -> Algo {
        self.algo
    }

    /// _mongoc_scram_step: the next client message from the server's last one.
    pub fn step(&mut self, input: &[u8]) -> Result<Vec<u8>, Error> {
        self.step += 1;
        match self.step {
            1 => {
                let mut nonce = [0u8; 24];
                if getrandom::fill(&mut nonce).is_err() {
                    return Err(err("SCRAM Failure: could not generate a cryptographically secure nonce in sasl step 1"));
                }
                self.start(&b64_encode(&nonce))
            }
            2 => self.step2(input),
            3 => self.step3(input).map(|()| Vec::new()),
            _ => Err(Error::new(domain::SCRAM, code::SCRAM_NOT_DONE, "SCRAM Failure: maximum steps detected")),
        }
    }

    /// The client-first message with the given nonce (step 1).
    pub fn start(&mut self, nonce: &str) -> Result<Vec<u8>, Error> {
        if self.step == 0 {
            self.step = 1;
        }
        self.nonce = nonce.to_owned();
        let buffer = || err("SCRAM Failure: could not buffer sasl step1");
        let mut out = Vec::with_capacity(64);
        if !buf_write(&mut out, b"n,,n=") {
            return Err(buffer());
        }
        // RFC 5802: ',' and '=' in the user name are sent as "=2C" and "=3D"
        for &c in self.user.as_bytes() {
            let ok = match c {
                b',' => buf_write(&mut out, b"=2C"),
                b'=' => buf_write(&mut out, b"=3D"),
                _ => buf_write(&mut out, &[c]),
            };
            if !ok {
                return Err(buffer());
            }
        }
        if !buf_write(&mut out, b",r=") || !buf_write(&mut out, nonce.as_bytes()) {
            return Err(buffer());
        }
        self.auth_message.clear();
        if !buf_write(&mut self.auth_message, &out[3..]) || !buf_write(&mut self.auth_message, b",") {
            return Err(err("SCRAM Failure: could not buffer auth message in sasl step1"));
        }
        Ok(out)
    }

    /// Parses the server-first message, derives the keys and returns the client-final message.
    fn step2(&mut self, input: &[u8]) -> Result<Vec<u8>, Error> {
        let mut hashed = match self.algo {
            // the MongoDB hashed variant: HEX(MD5(user + ":mongo:" + password))
            Algo::Sha1 => {
                let mut t = format!("{}:mongo:{}", self.user, self.pass).into_bytes();
                let h = md5::Md5::digest(&t);
                wipe(&mut t);
                h.iter().map(|b| format!("{b:02x}")).collect::<String>().into_bytes()
            }
            Algo::Sha256 => super::saslprep::prep(&self.pass)?.into_bytes(),
        };
        let r = self.step2_with(input, &hashed);
        wipe(&mut hashed);
        r
    }

    fn step2_with(&mut self, input: &[u8], hashed: &[u8]) -> Result<Vec<u8>, Error> {
        let buffer_auth = || err("SCRAM Failure: could not buffer auth message in sasl step2");
        if !buf_write(&mut self.auth_message, input) || !buf_write(&mut self.auth_message, b",") {
            return Err(buffer_auth());
        }
        let a = parse_attrs(input, b"rsi", 2)?;
        let Some(r) = attr(&a, b'r') else { return Err(err("SCRAM Failure: no r param in sasl step 2")) };
        let Some(s) = attr(&a, b's') else { return Err(err("SCRAM Failure: no s param in sasl step 2")) };
        let Some(i) = attr(&a, b'i') else { return Err(err("SCRAM Failure: no i param in sasl step 2")) };
        if r.len() < self.nonce.len() || !ct_eq(&r[..self.nonce.len()], self.nonce.as_bytes()) {
            return Err(err("SCRAM Failure: client nonce not repeated in sasl step 2"));
        }
        let mut out = Vec::with_capacity(128);
        if !buf_write(&mut out, b"c=biws,r=") || !buf_write(&mut out, r) {
            return Err(err("SCRAM Failure: could not buffer sasl step2"));
        }
        if !buf_write(&mut self.auth_message, &out) {
            return Err(buffer_auth());
        }
        if !buf_write(&mut out, b",p=") {
            return Err(err("SCRAM Failure: could not buffer sasl step2"));
        }
        let Some(salt) = b64_decode(s, SALT_MAX) else { return Err(err("SCRAM Failure: unable to decode salt in sasl step2")) };
        if self.check_salt_len && salt.len() != self.algo.hash_size() - 4 {
            return Err(err(format!("SCRAM Failure: invalid salt length of {} in sasl step2", salt.len())));
        }
        let Some(iterations) = parse_iterations(i) else { return Err(err("SCRAM Failure: unable to parse iterations in sasl step2")) };
        if iterations < 0 {
            return Err(err("SCRAM Failure: iterations is negative in sasl step2"));
        }
        if iterations < MIN_ITERATIONS {
            return Err(err("SCRAM Failure: iterations must be at least 4096"));
        }
        if iterations > MAX_ITERATIONS {
            return Err(err(format!("SCRAM Failure: iterations must be at most {MAX_ITERATIONS}")));
        }
        self.hashed_password = hashed.to_vec();
        self.salt = salt;
        self.iterations = iterations as u32;
        self.apply_cache();
        if self.salted_password.is_none() {
            let mut sp = [0u8; HASH_MAX];
            pbkdf2(self.algo, hashed, &self.salt, self.iterations, &mut sp);
            self.salted_password = Some(sp);
        }
        let proof = self.client_proof();
        if !buf_write(&mut out, proof.as_bytes()) {
            return Err(err("SCRAM Failure: could not buffer sasl step2"));
        }
        Ok(out)
    }

    fn client_proof(&mut self) -> String {
        let n = self.algo.hash_size();
        let sp = self.salted_password.expect("salted in step 2");
        let ck = *self.client_key.get_or_insert_with(|| {
            let mut k = [0u8; HASH_MAX];
            hmac(self.algo, &sp[..n], b"Client Key", &mut k);
            k
        });
        let mut stored = [0u8; HASH_MAX];
        hash(self.algo, &ck[..n], &mut stored);
        let mut sig = [0u8; HASH_MAX];
        hmac(self.algo, &stored[..n], &self.auth_message, &mut sig);
        let mut proof = [0u8; HASH_MAX];
        for j in 0..n {
            proof[j] = ck[j] ^ sig[j];
        }
        let r = b64_encode(&proof[..n]);
        wipe(&mut stored);
        wipe(&mut sig);
        wipe(&mut proof);
        r
    }

    /// Verifies the server-final message.
    fn step3(&mut self, input: &[u8]) -> Result<(), Error> {
        let a = parse_attrs(input, b"ev", 3)?;
        if let Some(e) = attr(&a, b'e') {
            return Err(err(format!("SCRAM Failure: authentication failure in sasl step 3 : {}", String::from_utf8_lossy(e))));
        }
        let Some(v) = attr(&a, b'v') else { return Err(err("SCRAM Failure: no v param in sasl step 3")) };
        if !self.verify_server_signature(v) {
            return Err(err("SCRAM Failure: could not verify server signature in sasl step 3"));
        }
        self.update_cache();
        Ok(())
    }

    fn verify_server_signature(&mut self, v: &[u8]) -> bool {
        let n = self.algo.hash_size();
        let Some(sp) = self.salted_password else { return false };
        let sk = *self.server_key.get_or_insert_with(|| {
            let mut k = [0u8; HASH_MAX];
            hmac(self.algo, &sp[..n], b"Server Key", &mut k);
            k
        });
        let mut sig = [0u8; HASH_MAX];
        hmac(self.algo, &sk[..n], &self.auth_message, &mut sig);
        let expected = b64_encode(&sig[..n]);
        wipe(&mut sig);
        ct_eq(v, expected.as_bytes())
    }

    fn cache_key_eq(&self, e: &CacheEntry) -> bool {
        e.algo == self.algo && e.iterations == self.iterations && e.salt == self.salt && ct_eq(&e.hashed_password, &self.hashed_password)
    }

    /// _mongoc_scram_cache_has_presecrets + apply_secrets
    fn apply_cache(&mut self) {
        let Ok(c) = CACHE.lock() else { return };
        if let Some(e) = c.iter().find(|e| self.cache_key_eq(e)) {
            self.client_key = Some(e.client_key);
            self.server_key = Some(e.server_key);
            self.salted_password = Some(e.salted_password);
        }
    }

    /// _mongoc_scram_update_cache: a full cache is emptied first.
    fn update_cache(&self) {
        let (Some(ck), Some(sk), Some(sp)) = (self.client_key, self.server_key, self.salted_password) else { return };
        let Ok(mut c) = CACHE.lock() else { return };
        if c.iter().any(|e| self.cache_key_eq(e)) {
            return;
        }
        if c.len() >= CACHE_SIZE {
            for mut e in c.drain(..) {
                wipe(&mut e.hashed_password);
                wipe(&mut e.salted_password);
                wipe(&mut e.client_key);
                wipe(&mut e.server_key);
            }
        }
        c.push(CacheEntry {
            algo: self.algo,
            hashed_password: self.hashed_password.clone(),
            salt: self.salt.clone(),
            iterations: self.iterations,
            client_key: ck,
            server_key: sk,
            salted_password: sp,
        });
    }
}

#[cfg(test)]
pub fn cache_len() -> usize {
    CACHE.lock().map(|c| c.len()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The auth specification's SCRAM-SHA-1 example (RFC 5802 section 5).
    #[test]
    fn sha1_conversation() {
        let mut s = Scram::new(Algo::Sha1, "user", Some("pencil"));
        assert_eq!(s.start("fyko+d2lbbFgONRv9qkxdawL").unwrap(), b"n,,n=user,r=fyko+d2lbbFgONRv9qkxdawL");
        s.step = 1;
        let c2 = s.step(b"r=fyko+d2lbbFgONRv9qkxdawLHo+Vgk7qvUOKUwuWLIWg4l/9SraGMHEE,s=rQ9ZY3MntBeuP3E1TDVC4w==,i=10000").unwrap();
        assert_eq!(String::from_utf8(c2).unwrap(), "c=biws,r=fyko+d2lbbFgONRv9qkxdawLHo+Vgk7qvUOKUwuWLIWg4l/9SraGMHEE,p=MC2T8BvbmWRckDw8oWl5IVghwCY=");
        assert_eq!(s.step(b"v=UMWeI25JD1yNYZRMpZ4VHvhZ9e0=").unwrap(), b"");
        let e = s.step(b"").unwrap_err();
        assert_eq!((e.code, e.message.as_str()), (code::SCRAM_NOT_DONE, "SCRAM Failure: maximum steps detected"));
    }

    /// The auth specification's SCRAM-SHA-256 example (RFC 7677 section 3).
    #[test]
    fn sha256_conversation() {
        let mut s = Scram::new(Algo::Sha256, "user", Some("pencil"));
        s.check_salt_len = false;
        assert_eq!(s.start("rOprNGfwEbeRWgbNEkqO").unwrap(), b"n,,n=user,r=rOprNGfwEbeRWgbNEkqO");
        let c2 = s.step(b"r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096").unwrap();
        assert_eq!(String::from_utf8(c2).unwrap(), "c=biws,r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,p=dHzbZapWIk4jUhN+Ute9ytag9zjfMHgsqmmiz7AndVQ=");
        s.step(b"v=6rriTRBi23WpRR/wtup+mMhUZUn/dB5nLTJRsjl95G4=").unwrap();
        assert!(cache_len() >= 1);
        // a second conversation with the same pre-secrets uses the cache (same result)
        let mut s2 = Scram::new(Algo::Sha256, "user", Some("pencil"));
        s2.check_salt_len = false;
        s2.start("rOprNGfwEbeRWgbNEkqO").unwrap();
        let c2 = s2.step(b"r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096").unwrap();
        assert!(c2.ends_with(b"p=dHzbZapWIk4jUhN+Ute9ytag9zjfMHgsqmmiz7AndVQ="));
    }

    fn step2_err(server_first: &[u8]) -> String {
        let mut s = Scram::new(Algo::Sha256, "user", Some("pencil"));
        s.check_salt_len = false;
        s.start("rOprNGfwEbeRWgbNEkqO").unwrap();
        s.step(server_first).unwrap_err().message
    }

    #[test]
    fn step2_errors() {
        let n = "r=rOprNGfwEbeRWgbNEkqOserver,s=W22ZaJ0SNY7soEsUEjb6gQ==";
        assert_eq!(step2_err(format!("{n},i=4095").as_bytes()), "SCRAM Failure: iterations must be at least 4096");
        assert_eq!(step2_err(format!("{n},i=-1").as_bytes()), "SCRAM Failure: iterations is negative in sasl step2");
        assert_eq!(step2_err(format!("{n},i=12x").as_bytes()), "SCRAM Failure: unable to parse iterations in sasl step2");
        assert_eq!(step2_err(format!("{n},i=").as_bytes()), "SCRAM Failure: unable to parse iterations in sasl step2");
        assert_eq!(step2_err(format!("{n},i=10000001").as_bytes()), "SCRAM Failure: iterations must be at most 10000000");
        assert_eq!(step2_err(format!("{n},i=99999999999999999999999").as_bytes()), "SCRAM Failure: iterations must be at most 10000000");
        assert_eq!(step2_err(b"r=ZZZZZZZZserver-nonce,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096"), "SCRAM Failure: client nonce not repeated in sasl step 2");
        let mut s = Scram::new(Algo::Sha256, "user", Some("pencil"));
        s.start("N").unwrap();
        assert_eq!(s.step(b"r=Nserver,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096").unwrap_err().message, "SCRAM Failure: invalid salt length of 16 in sasl step2");
        assert_eq!(step2_err(b"r=rOprNGfwEbeRWgbNEkqOserver,s=*,i=4096"), "SCRAM Failure: unable to decode salt in sasl step2");
        assert_eq!(step2_err(b"x=1"), "SCRAM Failure: unknown key (x) in sasl step 2");
        assert_eq!(step2_err(b"r"), "SCRAM Failure: invalid parse state in sasl step 2");
        assert_eq!(step2_err(b"r=rOprNGfwEbeRWgbNEkqO,i=4096"), "SCRAM Failure: no s param in sasl step 2");
        assert_eq!(step2_err(b"s=AAAA,i=4096"), "SCRAM Failure: no r param in sasl step 2");
        assert_eq!(step2_err(b"r=rOprNGfwEbeRWgbNEkqO,s=AAAA"), "SCRAM Failure: no i param in sasl step 2");
        assert_eq!(step2_err(&[b'r'; BUF_MAX]), "SCRAM Failure: could not buffer auth message in sasl step2");
    }

    #[test]
    fn step3_errors() {
        let run = |last: &[u8]| {
            let mut s = Scram::new(Algo::Sha256, "user", Some("pencil"));
            s.check_salt_len = false;
            s.start("rOprNGfwEbeRWgbNEkqO").unwrap();
            s.step(b"r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096").unwrap();
            s.step(last).map(|_| ()).map_err(|e| e.message)
        };
        assert_eq!(run(b"v=6rriTRBi23WpRR/wtup+mMhUZUn/dB5nLTJRsjl95G4="), Ok(()));
        assert_eq!(run(b"v=6rriTRBi23WpRR/wtup+mMhUZUn/dB5nLTJRsjl95G5="), Err("SCRAM Failure: could not verify server signature in sasl step 3".into()));
        assert_eq!(run(b"v=6rriTRBi23WpRR/wtup+mMhUZUn/dB5nLTJRsjl95G4"), Err("SCRAM Failure: could not verify server signature in sasl step 3".into()));
        assert_eq!(run(b"e=other-error"), Err("SCRAM Failure: authentication failure in sasl step 3 : other-error".into()));
        assert_eq!(run(b"v=6rriTRBi23WpRR/wtup+mMhUZUn/dB5nLTJRsjl95G4=,x=1"), Err("SCRAM Failure: unknown key (x) in sasl step 3".into()));
        assert_eq!(run(b""), Err("SCRAM Failure: no v param in sasl step 3".into()));
    }

    #[test]
    fn user_escaping_and_limits() {
        let mut s = Scram::new(Algo::Sha1, "a,b=c", None);
        assert_eq!(s.start("N").unwrap(), b"n,,n=a=2Cb=3Dc,r=N");
        let mut s = Scram::new(Algo::Sha1, &"u".repeat(BUF_MAX), None);
        assert_eq!(s.start("N").unwrap_err().message, "SCRAM Failure: could not buffer sasl step1");
    }

    #[test]
    fn base64() {
        for (raw, enc) in [(&b""[..], ""), (b"f", "Zg=="), (b"fo", "Zm8="), (b"foo", "Zm9v"), (b"foob", "Zm9vYg==")] {
            assert_eq!(b64_encode(raw), enc);
            assert_eq!(b64_decode(enc.as_bytes(), 64).unwrap(), raw);
        }
        assert_eq!(b64_decode(b" Zm 9v\n", 64).unwrap(), b"foo");
        assert_eq!(b64_decode(b"Zm8=x", 64), None);
        assert_eq!(b64_decode(b"Zm9=", 64), None, "non-zero slop bits");
        assert_eq!(b64_decode(b"Zg", 64), None);
        assert_eq!(b64_decode(b"Zm9vYmFy", 4), None, "does not fit");
        assert_eq!(b64_decode(b"Zm9v\0garbage", 64).unwrap(), b"foo");
    }
}
