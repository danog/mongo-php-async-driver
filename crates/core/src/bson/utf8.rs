//! UTF-8 validation with libbson's rules (bson-utf8.c `bson_utf8_validate`): no overlong forms
//! (except the two-byte NUL `C0 80` when NULs are allowed), no surrogates, nothing past U+10FFFF.

#[inline]
fn sequence(b: u8) -> (usize, u8) {
    if b & 0x80 == 0 {
        (1, 0x7f)
    } else if b & 0xe0 == 0xc0 {
        (2, 0x1f)
    } else if b & 0xf0 == 0xe0 {
        (3, 0x0f)
    } else if b & 0xf8 == 0xf0 {
        (4, 0x07)
    } else {
        (0, 0)
    }
}

/// `bson_utf8_validate(s, len, allow_null)`.
///
/// Well-formed UTF-8 as `core::str::from_utf8` checks it (word-at-a-time over ASCII runs) is
/// exactly libbson's rule set apart from NUL: libbson rejects a 0x00 byte unless `allow_null`,
/// and with `allow_null` it also accepts the overlong two-byte NUL `C0 80`, which `from_utf8`
/// rejects; only that case falls back to the byte-wise port.
#[inline]
pub fn validate(s: &[u8], allow_null: bool) -> bool {
    // the common short ASCII string (a key, a name) without a call: ASCII bytes other than NUL
    // are valid either way, and a prefix of them never changes the verdict on the rest
    let mut i = 0;
    while i + 8 <= s.len() {
        let w = super::swar::load(s, i);
        if (w & 0x8080_8080_8080_8080) | super::swar::zero_bytes(w) != 0 {
            return validate_general(&s[i..], allow_null);
        }
        i += 8;
    }
    while i < s.len() {
        if s[i].wrapping_sub(1) >= 0x7f {
            return validate_general(&s[i..], allow_null);
        }
        i += 1;
    }
    true
}

fn validate_general(s: &[u8], allow_null: bool) -> bool {
    match std::str::from_utf8(s) {
        Ok(_) => allow_null || !s.contains(&0),
        Err(_) => allow_null && validate_bytewise(s, allow_null),
    }
}

/// The byte-wise port of `bson_utf8_validate`.
fn validate_bytewise(s: &[u8], allow_null: bool) -> bool {
    let mut i = 0;
    let n = s.len();
    while i < n {
        let b = s[i];
        if b < 0x80 {
            // fast path: ASCII
            if b == 0 && !allow_null {
                return false;
            }
            i += 1;
            continue;
        }
        let (len, mask) = sequence(b);
        if len == 0 || n - i < len {
            return false;
        }
        let mut c = (b & mask) as u32;
        for j in i + 1..i + len {
            if s[j] & 0xc0 != 0x80 {
                return false;
            }
            c = (c << 6) | (s[j] & 0x3f) as u32;
        }
        if c > 0x10ffff || (c & 0xffff_f800) == 0xd800 {
            return false;
        }
        let ok = match len {
            2 => (0x80..=0x7ff).contains(&c) || (c == 0 && allow_null),
            3 => (0x800..=0xffff).contains(&c),
            _ => (0x10000..=0x10ffff).contains(&c),
        };
        if !ok {
            return false;
        }
        i += len;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::{validate, validate_bytewise};

    /// The fast path agrees with the byte-wise port on every 1-3 byte input built from
    /// boundary bytes, and on random 1-12 byte inputs (with and without NULs allowed).
    #[test]
    fn fast_path_matches_bytewise() {
        let interesting: Vec<u8> = vec![
            0x00, 0x01, 0x41, 0x7f, 0x80, 0x81, 0x8f, 0x90, 0x9f, 0xa0, 0xbf, 0xc0, 0xc1, 0xc2, 0xdf, 0xe0, 0xe1,
            0xec, 0xed, 0xee, 0xef, 0xf0, 0xf1, 0xf3, 0xf4, 0xf5, 0xf7, 0xf8, 0xfb, 0xfc, 0xfe, 0xff,
        ];
        let check = |b: &[u8]| {
            for allow in [false, true] {
                assert_eq!(validate(b, allow), validate_bytewise(b, allow), "{:02x?} allow_null={}", b, allow);
            }
        };
        check(&[]);
        for &a in &interesting {
            check(&[a]);
            for &b in &interesting {
                check(&[a, b]);
                for &c in &interesting {
                    check(&[a, b, c]);
                    check(&[b'x', a, b, c]);
                }
            }
        }
        let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
        for _ in 0..2_000_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let n = (x % 12) as usize + 1;
            let mut b = Vec::with_capacity(n);
            let mut y = x;
            for _ in 0..n {
                y = y.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                b.push(interesting[(y >> 59) as usize % interesting.len()] ^ if y & 1 == 0 { 0 } else { (y >> 40) as u8 & 0x0f });
            }
            check(&b);
        }
        // the cases that differ between from_utf8 and libbson
        assert!(validate(b"a\xc0\x80b", true) && !validate(b"a\xc0\x80b", false));
        assert!(validate(b"a\0b", true) && !validate(b"a\0b", false));
        assert!(!validate(b"\xed\xa0\x80", true) && !validate(b"\xf4\x90\x80\x80", true));
    }
}
