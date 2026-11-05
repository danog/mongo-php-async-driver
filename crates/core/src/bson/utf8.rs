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
pub fn validate(s: &[u8], allow_null: bool) -> bool {
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
