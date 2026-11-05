//! printf-compatible number formatting that libbson/phongo output depends on.

/// C's `printf("%.<prec>g", v)` for finite and non-finite doubles (glibc spelling).
pub fn fmt_g(v: f64, prec: usize) -> String {
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
    let (mut digits, x): (Vec<u8>, i32) = match exact_digits(v.abs(), p, &mut buf) {
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
    let mut t = ip;
    while t > 0 {
        idig[ni] = (t % 10) as u8;
        t /= 10;
        ni += 1;
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
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        out.push(T[(n >> 18) as usize & 63]);
        out.push(T[(n >> 12) as usize & 63]);
        out.push(if c.len() > 1 { T[(n >> 6) as usize & 63] } else { b'=' });
        out.push(if c.len() > 2 { T[n as usize & 63] } else { b'=' });
    }
    String::from_utf8(out).unwrap()
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
