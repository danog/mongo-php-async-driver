//! ObjectId: generation as libbson's default context does (4-byte big-endian time, 5 bytes of
//! per-process randomness, 3-byte big-endian counter) and hex conversions.

use std::cell::Cell;
use std::time::{SystemTime, UNIX_EPOCH};

thread_local! {
    static CTX: Cell<Option<(u32, [u8; 5], u32)>> = const { Cell::new(None) };
}

fn random_bytes(n: usize) -> Vec<u8> {
    let mut v = vec![0u8; n];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        use std::io::Read;
        if f.read_exact(&mut v).is_ok() {
            return v;
        }
    }
    let mut x = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(1) ^ (std::process::id() as u64) << 32;
    for b in v.iter_mut() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *b = x as u8;
    }
    v
}

pub fn now_secs() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// `bson_oid_init(oid, NULL)`. The random part is re-drawn after fork (pid change).
pub fn generate() -> [u8; 12] {
    let pid = std::process::id();
    let (rand, seq) = CTX.with(|c| {
        let (p, rand, seq) = match c.get() {
            Some(s) if s.0 == pid => s,
            _ => {
                let r = random_bytes(8);
                let seq = u32::from_le_bytes([r[5], r[6], r[7], 0]) & !0xf0000f;
                (pid, [r[0], r[1], r[2], r[3], r[4]], seq)
            }
        };
        let seq = seq.wrapping_add(1);
        c.set(Some((p, rand, seq)));
        (rand, seq)
    });
    let t = now_secs() as u32;
    let mut o = [0u8; 12];
    o[0..4].copy_from_slice(&t.to_be_bytes());
    o[4..9].copy_from_slice(&rand);
    o[9] = (seq >> 16) as u8;
    o[10] = (seq >> 8) as u8;
    o[11] = seq as u8;
    o
}

const HEX: &[u8; 16] = b"0123456789abcdef";

pub fn to_hex(o: &[u8; 12]) -> [u8; 24] {
    let mut h = [0u8; 24];
    for (i, b) in o.iter().enumerate() {
        h[2 * i] = HEX[(b >> 4) as usize];
        h[2 * i + 1] = HEX[(b & 15) as usize];
    }
    h
}

fn nib(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// `bson_oid_is_valid` + `bson_oid_init_from_string`: exactly 24 hex digits (either case).
pub fn from_hex(s: &[u8]) -> Option<[u8; 12]> {
    if s.len() != 24 {
        return None;
    }
    let mut o = [0u8; 12];
    for i in 0..12 {
        o[i] = (nib(s[2 * i])? << 4) | nib(s[2 * i + 1])?;
    }
    Some(o)
}
