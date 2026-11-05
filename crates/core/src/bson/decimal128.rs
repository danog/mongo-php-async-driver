//! IEEE 754-2008 decimal128 (BID) <-> string, ported from libbson's bson-decimal128.c so that
//! the results, including rounding refusals and the string forms, are identical.

const EXPONENT_MAX: i32 = 6111;
const EXPONENT_MIN: i32 = -6176;
const EXPONENT_BIAS: i32 = 6176;
const MAX_DIGITS: usize = 34;
/// BSON_DECIMAL128_STRING: the buffer size libbson formats into (including the NUL).
const STRING_SIZE: usize = 43;

/// A decimal128 value as its two 64-bit halves (BSON stores `low` first, little-endian).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Decimal128 {
    pub high: u64,
    pub low: u64,
}

impl Decimal128 {
    pub const NAN: Decimal128 = Decimal128 { high: 0x7c00_0000_0000_0000, low: 0 };

    fn inf(negative: bool) -> Self {
        Decimal128 { high: 0x7800_0000_0000_0000 + if negative { 0x8000_0000_0000_0000 } else { 0 }, low: 0 }
    }

    /// The 16 bytes as stored in BSON.
    pub fn to_le_bytes(self) -> [u8; 16] {
        let mut b = [0; 16];
        b[..8].copy_from_slice(&self.low.to_le_bytes());
        b[8..].copy_from_slice(&self.high.to_le_bytes());
        b
    }

    pub fn from_le_bytes(b: [u8; 16]) -> Self {
        Decimal128 { low: u64::from_le_bytes(b[..8].try_into().unwrap()), high: u64::from_le_bytes(b[8..].try_into().unwrap()) }
    }

    /// bson_decimal128_to_string.
    #[allow(clippy::inherent_to_string)]
    pub fn to_string(&self) -> String {
        let mut out: Vec<u8> = Vec::with_capacity(STRING_SIZE);
        if (self.high as i64) < 0 {
            out.push(b'-');
        }
        let low = self.low as u32;
        let midl = (self.low >> 32) as u32;
        let midh = self.high as u32;
        let high = (self.high >> 32) as u32;
        let combination = (high >> 26) & 0x1f;
        let biased_exponent: i32;
        let significand_msb: u32;
        if (combination >> 3) == 3 {
            if combination == 30 {
                out.extend_from_slice(b"Infinity");
                return String::from_utf8(out).unwrap();
            } else if combination == 31 {
                // the sign is erased
                return "NaN".to_owned();
            }
            biased_exponent = ((high >> 15) & 0x3fff) as i32;
            significand_msb = 0x8 + ((high >> 14) & 0x1);
        } else {
            significand_msb = (high >> 14) & 0x7;
            biased_exponent = ((high >> 17) & 0x3fff) as i32;
        }
        let exponent = biased_exponent - EXPONENT_BIAS;

        let mut significand = [0u32; 36];
        let top = (high & 0x3fff) + ((significand_msb & 0xf) << 14);
        let mut s128: u128 = ((top as u128) << 96) | ((midh as u128) << 64) | ((midl as u128) << 32) | low as u128;
        let is_zero = if s128 == 0 {
            true
        } else if top >= (1 << 17) {
            // non-canonical significand (> 1e34 - 1): treated as zero
            true
        } else {
            for k in (0..4).rev() {
                let mut least = (s128 % 1_000_000_000) as u32;
                s128 /= 1_000_000_000;
                if least == 0 {
                    continue;
                }
                for j in (0..9).rev() {
                    significand[k * 9 + j] = least % 10;
                    least /= 10;
                }
            }
            false
        };

        let mut read = 0usize;
        let mut num_significant_digits: i32;
        if is_zero {
            num_significant_digits = 1;
            significand[0] = 0;
        } else {
            num_significant_digits = 36;
            while significand[read] == 0 {
                num_significant_digits -= 1;
                read += 1;
            }
        }
        let scientific_exponent = num_significant_digits - 1 + exponent;
        let digit = |i: usize| b'0' + significand[i] as u8;

        if scientific_exponent < -6 || exponent > 0 {
            out.push(digit(read));
            read += 1;
            num_significant_digits -= 1;
            if num_significant_digits != 0 {
                out.push(b'.');
            }
            let mut i = 0;
            while i < num_significant_digits && out.len() < 36 {
                out.push(digit(read));
                read += 1;
                i += 1;
            }
            out.push(b'E');
            // bson_snprintf(str_out, 6, "%+d"): at most 5 characters
            let e = format!("{scientific_exponent:+}");
            out.extend_from_slice(&e.as_bytes()[..e.len().min(5)]);
        } else if exponent >= 0 {
            let mut i = 0;
            while i < num_significant_digits && out.len() < 36 {
                out.push(digit(read));
                read += 1;
                i += 1;
            }
        } else {
            let mut radix_position = num_significant_digits + exponent;
            let available = STRING_SIZE - 1;
            if radix_position > 0 {
                let mut i = 0;
                while i < radix_position && out.len() != available {
                    out.push(digit(read));
                    read += 1;
                    i += 1;
                }
            } else {
                out.push(b'0');
            }
            out.push(b'.');
            loop {
                let r = radix_position;
                radix_position += 1;
                if !(r < 0 && out.len() != available) {
                    break;
                }
                out.push(b'0');
            }
            let dot_pos = (radix_position - 1).max(0) as u32;
            let n_trailing = (num_significant_digits as u32).wrapping_sub(dot_pos);
            let n_to_write = n_trailing.min((available - out.len()) as u32);
            for _ in 0..n_to_write {
                out.push(digit(read));
                read += 1;
            }
        }
        String::from_utf8(out).unwrap()
    }

    /// bson_decimal128_from_string_w_len(s, len): `None` = libbson's failure (it sets NaN and
    /// returns false). `s` is read like a C string: a NUL ends it.
    pub fn parse(s: &[u8]) -> Option<Self> {
        let s = match s.iter().position(|&b| b == 0) {
            Some(n) => &s[..n],
            None => s,
        };
        let at = |i: usize| s.get(i).copied().unwrap_or(0);
        let mut p = 0usize;
        let mut is_negative = false;
        let mut includes_sign = 0usize;
        if at(p) == b'+' || at(p) == b'-' {
            is_negative = at(p) == b'-';
            p += 1;
            includes_sign = 1;
        }
        if !at(p).is_ascii_digit() && at(p) != b'.' {
            let rest = &s[p..];
            if rest.eq_ignore_ascii_case(b"inf") || rest.eq_ignore_ascii_case(b"infinity") {
                return Some(Self::inf(is_negative));
            } else if rest.eq_ignore_ascii_case(b"nan") {
                return Some(Self::NAN);
            }
            return None;
        }

        let mut saw_radix = false;
        let mut found_nonzero = false;
        let mut digits = [0u16; MAX_DIGITS];
        let mut ndigits_stored = 0usize;
        let mut ndigits_read = 0usize;
        let mut ndigits = 0usize;
        let mut radix_position = 0usize;
        let mut first_nonzero = 0usize;
        while at(p).is_ascii_digit() || at(p) == b'.' {
            if at(p) == b'.' {
                if saw_radix {
                    return None;
                }
                saw_radix = true;
                p += 1;
                continue;
            }
            if ndigits_stored < MAX_DIGITS && (at(p) != b'0' || found_nonzero) {
                if !found_nonzero {
                    first_nonzero = ndigits_read;
                }
                found_nonzero = true;
                digits[ndigits_stored] = (at(p) - b'0') as u16;
                ndigits_stored += 1;
            }
            if found_nonzero {
                ndigits += 1;
            }
            if saw_radix {
                radix_position += 1;
            }
            ndigits_read += 1;
            p += 1;
        }
        if saw_radix && ndigits_read == 0 {
            return None;
        }

        let mut exponent: i32 = 0;
        if at(p) == b'e' || at(p) == b'E' {
            p += 1;
            // sscanf("%" SCNd64 "%n"): leading whitespace, an optional sign, digits
            let mut q = p;
            while matches!(at(q), b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r') {
                q += 1;
            }
            let neg = at(q) == b'-';
            if at(q) == b'+' || at(q) == b'-' {
                q += 1;
            }
            let d0 = q;
            let mut v: i128 = 0;
            while at(q).is_ascii_digit() {
                v = (v * 10 + (at(q) - b'0') as i128).min(i64::MAX as i128 + 1);
                q += 1;
            }
            if q == d0 {
                return None;
            }
            let v = if neg { -v } else { v }.clamp(i64::MIN as i128, i64::MAX as i128);
            if v < i32::MIN as i128 || v > i32::MAX as i128 {
                return None;
            }
            exponent = v as i32;
            p = q;
        }
        if p < s.len() {
            return None;
        }

        let first_digit = 0usize;
        let mut last_digit: usize;
        let mut significant_digits: usize;
        let byte = |i: usize| at(i);
        let sr = saw_radix as usize;
        if ndigits_stored == 0 {
            last_digit = 0;
            digits[0] = 0;
            ndigits = 1;
            ndigits_stored = 1;
            significant_digits = 0;
        } else {
            last_digit = ndigits_stored - 1;
            significant_digits = ndigits;
            while byte(first_nonzero + significant_digits - 1 + includes_sign + sr) == b'0' {
                significant_digits -= 1;
            }
        }

        if (exponent as i64) <= radix_position as i64 && radix_position as i64 > exponent as i64 + (1 << 14) {
            exponent = EXPONENT_MIN;
        } else {
            exponent -= radix_position as i32;
        }

        while exponent > EXPONENT_MAX {
            last_digit += 1;
            if last_digit - first_digit >= MAX_DIGITS {
                if significant_digits == 0 {
                    exponent = EXPONENT_MAX;
                    break;
                }
                return None;
            }
            exponent -= 1;
        }

        while exponent < EXPONENT_MIN || ndigits_stored < ndigits {
            if last_digit == 0 {
                if significant_digits == 0 {
                    exponent = EXPONENT_MIN;
                    break;
                }
                return None;
            }
            if ndigits_stored < ndigits {
                if byte(first_nonzero + ndigits - 1 + includes_sign + sr) != b'0' && significant_digits != 0 {
                    return None;
                }
                ndigits -= 1;
            } else {
                if digits[last_digit] != 0 {
                    return None;
                }
                last_digit -= 1;
            }
            if exponent < EXPONENT_MAX {
                exponent += 1;
            } else {
                return None;
            }
        }

        if last_digit - first_digit + 1 < significant_digits {
            let round_digit = byte(first_nonzero + last_digit + includes_sign + sr + 1).wrapping_sub(b'0');
            if round_digit != 0 {
                return None;
            }
        }

        let (mut sig_high, mut sig_low) = (0u64, 0u64);
        if significant_digits == 0 {
        } else if last_digit - first_digit < 17 {
            let mut d = first_digit;
            sig_low = digits[d] as u64;
            d += 1;
            while d <= last_digit {
                sig_low = sig_low * 10 + digits[d] as u64;
                d += 1;
            }
        } else {
            let mut d = first_digit;
            sig_high = digits[d] as u64;
            d += 1;
            while d <= last_digit - 17 {
                sig_high = sig_high * 10 + digits[d] as u64;
                d += 1;
            }
            sig_low = digits[d] as u64;
            d += 1;
            while d <= last_digit {
                sig_low = sig_low * 10 + digits[d] as u64;
                d += 1;
            }
        }
        let significand: u128 = (sig_high as u128) * 100_000_000_000_000_000u128 + sig_low as u128;
        let (s_high, s_low) = ((significand >> 64) as u64, significand as u64);
        let biased_exponent = (exponent + EXPONENT_BIAS) as u16 as u64;
        let mut dec = Decimal128::default();
        if (s_high >> 49) & 1 != 0 {
            dec.high |= 0x3 << 61;
            dec.high |= (biased_exponent & 0x3fff) << 47;
            dec.high |= s_high & 0x7fff_ffff_ffff;
        } else {
            dec.high |= (biased_exponent & 0x3fff) << 49;
            dec.high |= s_high & 0x1_ffff_ffff_ffff;
        }
        dec.low = s_low;
        if is_negative {
            dec.high |= 0x8000_0000_0000_0000;
        }
        Some(dec)
    }
}
