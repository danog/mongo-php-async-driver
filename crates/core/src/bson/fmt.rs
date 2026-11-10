//! printf-compatible number formatting that libbson/phongo output depends on.

/// C's `printf("%.<prec>g", v)` for finite and non-finite doubles (glibc spelling).
pub fn fmt_g(v: f64, prec: usize) -> String {
    let mut out = Vec::with_capacity(prec + 8);
    fmt_g_into(&mut out, v, prec);
    // only ASCII is written
    String::from_utf8(out).unwrap()
}

/// `fmt_g`, appended to `out` (no allocation besides `out`'s growth).
pub fn fmt_g_into(out: &mut Vec<u8>, v: f64, prec: usize) {
    if v.is_nan() {
        out.extend_from_slice(if v.is_sign_negative() { b"-nan" } else { b"nan" });
        return;
    }
    if v.is_infinite() {
        out.extend_from_slice(if v < 0.0 { b"-inf" } else { b"inf" });
        return;
    }
    let p = if prec == 0 { 1 } else { prec };
    if v == 0.0 {
        out.extend_from_slice(if v.is_sign_negative() { b"-0" } else { b"0" });
        return;
    }
    // exact decimal digits, correctly rounded to p significant digits (one conversion; the
    // fixed notation is laid out from the same digits, as printf's %g does)
    let neg = v.is_sign_negative();
    let mut buf = [0u8; 48];
    let fallback: Vec<u8>;
    let (mut digits, x): (&[u8], i32) = match exact_digits(v.abs(), p, &mut buf) {
        Some((n, x)) => (&buf[..n], x),
        None => {
            let e = format!("{:.*e}", p - 1, v.abs());
            let (mant, exp) = e.split_once('e').unwrap();
            fallback = mant.bytes().filter(|&c| c != b'.').collect();
            (&fallback, exp.parse().unwrap())
        }
    };
    while digits.len() > 1 && digits.last() == Some(&b'0') {
        digits = &digits[..digits.len() - 1];
    }
    out.reserve(p + 8);
    if neg {
        out.push(b'-');
    }
    if x < -4 || x >= p as i32 {
        out.push(digits[0]);
        if digits.len() > 1 {
            out.push(b'.');
            out.extend_from_slice(&digits[1..]);
        }
        out.push(b'e');
        out.push(if x < 0 { b'-' } else { b'+' });
        if x.abs() < 10 {
            out.push(b'0');
        }
        push_u64(out, x.unsigned_abs() as u64);
    } else if x >= 0 {
        let int_len = x as usize + 1;
        if digits.len() <= int_len {
            out.extend_from_slice(digits);
            for _ in digits.len()..int_len {
                out.push(b'0');
            }
        } else {
            out.extend_from_slice(&digits[..int_len]);
            out.push(b'.');
            out.extend_from_slice(&digits[int_len..]);
        }
    } else {
        out.extend_from_slice(b"0.");
        for _ in 0..(-x - 1) {
            out.push(b'0');
        }
        out.extend_from_slice(digits);
    }
}

/// The decimal digits of `v`, appended to `out` (`{}` without core::fmt).
#[inline]
pub fn push_u64(out: &mut Vec<u8>, mut v: u64) {
    let mut b = [0u8; 20];
    let mut i = b.len();
    loop {
        i -= 1;
        b[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    out.extend_from_slice(&b[i..]);
}

/// The decimal form of `v` as `{}` prints it, appended to `out`.
#[inline]
pub fn push_i64(out: &mut Vec<u8>, v: i64) {
    if v < 0 {
        out.push(b'-');
    }
    push_u64(out, v.unsigned_abs());
}

/// The first `p` significant decimal digits of the finite, positive `v`, correctly rounded
/// (ties to even, as printf), and the decimal exponent of the first one. Exact u128 arithmetic
/// for v = m * 2^e with -124 <= e <= 74 (None otherwise: the caller falls back to core::fmt).
fn exact_digits(v: f64, p: usize, out: &mut [u8; 48]) -> Option<(usize, i32)> {
    if p > 40 {
        return None;
    }
    let bits = v.to_bits();
    let be = ((bits >> 52) & 0x7ff) as i32;
    let mut m = bits & ((1u64 << 52) - 1);
    let e = if be == 0 {
        -1074
    } else {
        m |= 1u64 << 52;
        be - 1075
    };
    if !(-124..=74).contains(&e) || m == 0 {
        return None;
    }
    let (ip, mut frac, k): (u128, u128, u32) = if e >= 0 {
        ((m as u128) << e, 0, 0)
    } else {
        let k = (-e) as u32;
        ((m as u128) >> k, (m as u128) & ((1u128 << k) - 1), k)
    };
    let mask = if k == 0 { 0 } else { (1u128 << k) - 1 };
    // integer part digits
    let mut idig = [0u8; 40];
    let mut ni = 0;
    if ip <= u64::MAX as u128 {
        // u64 division: u128 `% 10` is a libcall per digit
        let mut t = ip as u64;
        while t > 0 {
            idig[ni] = (t % 10) as u8;
            t /= 10;
            ni += 1;
        }
    } else {
        let mut t = ip;
        while t > 0 {
            idig[ni] = (t % 10) as u8;
            t /= 10;
            ni += 1;
        }
    }
    idig[..ni].reverse();
    let mut n = 0; // significant digits produced
    let mut exp10: i32 = ni as i32 - 1;
    let mut next = 0u8; // the digit after the p-th
    let mut sticky = false;
    let mut have_next = false;
    for &d in &idig[..ni] {
        if n < p {
            out[n] = d;
            n += 1;
        } else if !have_next {
            next = d;
            have_next = true;
        } else if d != 0 {
            sticky = true;
        }
    }
    while !have_next && (frac != 0 || n < p) {
        frac *= 10;
        let d = (frac >> k) as u8;
        frac &= mask;
        if n == 0 && d == 0 {
            exp10 -= 1;
            continue;
        }
        if n < p {
            out[n] = d;
            n += 1;
        } else {
            next = d;
            have_next = true;
        }
    }
    if frac != 0 {
        sticky = true;
    }
    if next > 5 || (next == 5 && (sticky || out[n - 1] % 2 == 1)) {
        let mut i = n;
        loop {
            if i == 0 {
                out[0] = 1;
                for d in &mut out[1..n] {
                    *d = 0;
                }
                exp10 += 1;
                break;
            }
            i -= 1;
            if out[i] == 9 {
                out[i] = 0;
            } else {
                out[i] += 1;
                break;
            }
        }
    }
    for d in &mut out[..n] {
        *d += b'0';
    }
    Some((n, exp10))
}

/// Days since 1970-01-01 -> (year, month, day) (proleptic Gregorian).
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `mcommon_iso8601_string_append`: "%Y-%m-%dT%H:%M:%S" (gmtime) + ".%03d" when ms != 0, + "Z".
/// C semantics for negatives (truncating division) are kept.
pub fn iso8601(ms: i64) -> String {
    let mut out = Vec::with_capacity(24);
    iso8601_into(&mut out, ms);
    String::from_utf8(out).unwrap()
}

/// `iso8601`, appended to `out`.
pub fn iso8601_into(out: &mut Vec<u8>, ms: i64) {
    let msec = ms % 1000;
    let t = ms / 1000;
    let (y, m, d) = civil_from_days(t.div_euclid(86400));
    let secs = t.rem_euclid(86400);
    if !(0..=9999).contains(&y) || msec < 0 {
        out.extend_from_slice(iso8601_fmt(ms).as_bytes());
        return;
    }
    let two = |out: &mut Vec<u8>, v: u32| out.extend_from_slice(&[b'0' + (v / 10) as u8, b'0' + (v % 10) as u8]);
    let y = y as u32;
    two(out, y / 100);
    two(out, y % 100);
    out.push(b'-');
    two(out, m);
    out.push(b'-');
    two(out, d);
    out.push(b'T');
    two(out, (secs / 3600) as u32);
    out.push(b':');
    two(out, (secs / 60 % 60) as u32);
    out.push(b':');
    two(out, (secs % 60) as u32);
    if msec != 0 {
        let ms = msec as u32;
        out.extend_from_slice(&[b'.', b'0' + (ms / 100) as u8, b'0' + (ms / 10 % 10) as u8, b'0' + (ms % 10) as u8]);
    }
    out.push(b'Z');
}

/// `iso8601` through core::fmt (years outside 0..=9999, negative milliseconds).
fn iso8601_fmt(ms: i64) -> String {
    let msec = ms % 1000;
    let t = ms / 1000;
    let days = t.div_euclid(86400);
    let secs = t.rem_euclid(86400);
    let (y, m, d) = civil_from_days(days);
    let base = format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}", y, m, d, secs / 3600, secs / 60 % 60, secs % 60);
    if msec != 0 {
        format!("{}.{:03}Z", base, msec)
    } else {
        format!("{}Z", base)
    }
}

pub fn base64_encode(data: &[u8]) -> String {
    let mut out = Vec::with_capacity(data.len().div_ceil(3) * 4);
    base64_encode_into(&mut out, data);
    String::from_utf8(out).unwrap()
}

/// `base64_encode`, appended to `out`.
pub fn base64_encode_into(out: &mut Vec<u8>, data: &[u8]) {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    out.reserve(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        out.push(T[(n >> 18) as usize & 63]);
        out.push(T[(n >> 12) as usize & 63]);
        out.push(if c.len() > 1 { T[(n >> 6) as usize & 63] } else { b'=' });
        out.push(if c.len() > 2 { T[n as usize & 63] } else { b'=' });
    }
}

/// `mcommon_b64_pton`: decodes standard base64 (whitespace skipped, padding rules as in the
/// BIND-derived implementation). None on invalid input.
pub fn base64_decode(s: &[u8]) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32)
    }
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let mut acc = 0u32;
    let mut n = 0;
    let mut i = 0;
    while i < s.len() {
        let c = s[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if c == b'=' {
            break;
        }
        acc = acc << 6 | val(c)?;
        n += 1;
        if n == 4 {
            out.extend_from_slice(&[(acc >> 16) as u8, (acc >> 8) as u8, acc as u8]);
            acc = 0;
            n = 0;
        }
        i += 1;
    }
    if i < s.len() {
        // padding: valid only after 2 or 3 data chars of a quantum
        match n {
            2 => {
                let mut j = i + 1;
                while j < s.len() && s[j].is_ascii_whitespace() {
                    j += 1;
                }
                if j >= s.len() || s[j] != b'=' {
                    return None;
                }
                i = j + 1;
                out.push((acc >> 4) as u8);
                if acc & 0xf != 0 {
                    return None;
                }
            }
            3 => {
                i += 1;
                out.extend_from_slice(&[(acc >> 10) as u8, (acc >> 2) as u8]);
                if acc & 0x3 != 0 {
                    return None;
                }
            }
            _ => return None,
        }
        while i < s.len() {
            if !s[i].is_ascii_whitespace() {
                return None;
            }
            i += 1;
        }
    } else if n != 0 {
        return None;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pre-optimisation `fmt_g` (String pieces), kept as the reference.
    /// C's `printf("%.<prec>g", v)` for finite and non-finite doubles (glibc spelling).
    fn reference(v: f64, prec: usize) -> String {
        if v.is_nan() {
            return if v.is_sign_negative() { "-nan".into() } else { "nan".into() };
        }
        if v.is_infinite() {
            return if v < 0.0 { "-inf".into() } else { "inf".into() };
        }
        let p = if prec == 0 { 1 } else { prec };
        if v == 0.0 {
            return if v.is_sign_negative() { "-0".into() } else { "0".into() };
        }
        // exact decimal digits, correctly rounded to p significant digits (one conversion; the
        // fixed notation is laid out from the same digits, as printf's %g does)
        let neg = v.is_sign_negative();
        let mut buf = [0u8; 48];
        let (mut digits, x): (Vec<u8>, i32) = match super::exact_digits(v.abs(), p, &mut buf) {
            Some((n, x)) => (buf[..n].to_vec(), x),
            None => {
                let e = format!("{:.*e}", p - 1, v.abs());
                let (mant, exp) = e.split_once('e').unwrap();
                (mant.bytes().filter(|&c| c != b'.').collect(), exp.parse().unwrap())
            }
        };
        while digits.len() > 1 && digits.last() == Some(&b'0') {
            digits.pop();
        }
        let mut out = String::with_capacity(p + 8);
        if neg {
            out.push('-');
        }
        let ds = |d: &[u8]| unsafe { std::str::from_utf8_unchecked(d) }.to_owned();
        if x < -4 || x >= p as i32 {
            out.push(digits[0] as char);
            if digits.len() > 1 {
                out.push('.');
                out.push_str(&ds(&digits[1..]));
            }
            out.push('e');
            out.push(if x < 0 { '-' } else { '+' });
            if x.abs() < 10 {
                out.push('0');
            }
            out.push_str(&x.abs().to_string());
        } else if x >= 0 {
            let int_len = x as usize + 1;
            if digits.len() <= int_len {
                out.push_str(&ds(&digits));
                for _ in digits.len()..int_len {
                    out.push('0');
                }
            } else {
                out.push_str(&ds(&digits[..int_len]));
                out.push('.');
                out.push_str(&ds(&digits[int_len..]));
            }
        } else {
            out.push_str("0.");
            for _ in 0..(-x - 1) {
                out.push('0');
            }
            out.push_str(&ds(&digits));
        }
        out
    }

    #[test]
    fn fmt_g_into_matches_reference() {
        let mut x: u64 = 0x2545_f491_4f6c_dd1d;
        let check = |v: f64| {
            for p in [1, 6, 17, 20] {
                assert_eq!(fmt_g(v, p), reference(v, p), "{:?} p={}", v, p);
            }
        };
        for v in [0.0, -0.0, 1.0, -1.0, 0.1, 1e-5, 1e-4, 123456.0, 1e20, 1e21, 1e300, 5e-324, f64::MAX, f64::MIN_POSITIVE, f64::NAN, -f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 142.857142857142857, 2.5, 0.5] {
            check(v);
        }
        for _ in 0..300_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            check(f64::from_bits(x));
            check((x % 1_000_000) as f64 / 7.0);
            check((x >> 11) as f64);
        }
    }

    #[test]
    fn iso8601_into_matches_format() {
        let mut x: u64 = 0x1234_5678_9abc_def1;
        for _ in 0..200_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            for ms in [(x % 253402300800000) as i64, (x % 4_000_000_000_000) as i64 - 2_000_000_000_000, x as i64] {
                assert_eq!(iso8601(ms), iso8601_fmt(ms), "{}", ms);
            }
        }
    }

    #[test]
    fn push_i64_matches_display() {
        for v in [0, 1, -1, 9, 10, 99, 100, i64::MAX, i64::MIN, i32::MAX as i64, i32::MIN as i64, 1234567890123] {
            let mut o = Vec::new();
            push_i64(&mut o, v);
            assert_eq!(o, v.to_string().into_bytes());
        }
    }
}
