//! IEEE 754-2008 decimal128 (BID encoding) for Queryable Encryption range indexes on Decimal128
//! values: the operations libmongocrypt's range encoding uses (mc-dec128.h), with the semantics of
//! the Intel(R) Decimal Floating-Point Math Library (BSD-3-Clause, Copyright (c) 2007-2018 Intel
//! Corp.) that libmongocrypt builds with: comparisons, subtraction rounded to nearest-even,
//! scalbn, round-to-integral toward zero, and bid128_to_string's format.

use std::cmp::Ordering;

/// MC_DEC128_EXPONENT_BIAS
pub const EXPONENT_BIAS: i32 = 6176;
const MAX_BIASED: i32 = 12287;
const MAX_DIGITS: u32 = 34;
/// 10^34 - 1
const COEFF_MAX: u128 = 9_999_999_999_999_999_999_999_999_999_999_999;

/// A decoded decimal128
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dec {
    Nan { neg: bool, signaling: bool },
    Inf { neg: bool },
    /// value = (-1)^neg * coeff * 10^(biased_exp - 6176), coeff <= 10^34 - 1
    Finite { neg: bool, coeff: u128, biased_exp: i32 },
}

pub fn pow10(n: u32) -> u128 {
    10u128.pow(n)
}

/// 10^n with wrapping (mlib_int128_pow10)
pub fn pow10_wrapping(n: u32) -> u128 {
    let mut r: u128 = 1;
    for _ in 0..n {
        r = r.wrapping_mul(10);
    }
    r
}

fn digits(c: u128) -> u32 {
    if c == 0 {
        return 1;
    }
    let mut n = 0;
    let mut v = c;
    while v > 0 {
        v /= 10;
        n += 1;
    }
    n
}

impl Dec {
    pub fn finite(neg: bool, coeff: u128, exp: i32) -> Dec {
        Dec::Finite { neg, coeff, biased_exp: exp + EXPONENT_BIAS }
    }

    pub fn from_i64(v: i64) -> Dec {
        Dec::finite(v < 0, v.unsigned_abs() as u128, 0)
    }

    /// From the 16 little-endian bytes of a BSON Decimal128 (non-canonical coefficients are 0)
    pub fn from_bytes(b: &[u8; 16]) -> Dec {
        let lo = u64::from_le_bytes(b[..8].try_into().expect("8 bytes"));
        let hi = u64::from_le_bytes(b[8..].try_into().expect("8 bytes"));
        let neg = hi >> 63 == 1;
        if (hi >> 58) & 0x1f == 0x1f {
            return Dec::Nan { neg, signaling: (hi >> 57) & 1 == 1 };
        }
        if (hi >> 58) & 0x1f == 0x1e {
            return Dec::Inf { neg };
        }
        if (hi >> 61) & 3 == 3 {
            // G0G1 = 11: the coefficient would exceed 113 bits, non-canonical: zero
            let biased_exp = ((hi >> 47) & 0x3fff) as i32;
            return Dec::Finite { neg, coeff: 0, biased_exp };
        }
        let biased_exp = ((hi >> 49) & 0x3fff) as i32;
        let coeff = (((hi & 0x1_ffff_ffff_ffff) as u128) << 64) | lo as u128;
        Dec::Finite { neg, coeff: if coeff > COEFF_MAX { 0 } else { coeff }, biased_exp }
    }

    /// The canonical BID bytes (little-endian)
    pub fn to_bytes(&self) -> [u8; 16] {
        let (hi, lo): (u64, u64) = match *self {
            Dec::Nan { neg, signaling } => (((neg as u64) << 63) | (0x1f << 58) | ((signaling as u64) << 57), 0),
            Dec::Inf { neg } => (((neg as u64) << 63) | (0x1e << 58), 0),
            Dec::Finite { neg, coeff, biased_exp } => {
                (((neg as u64) << 63) | ((biased_exp as u64) << 49) | ((coeff >> 64) as u64), coeff as u64)
            }
        };
        let mut out = [0u8; 16];
        out[..8].copy_from_slice(&lo.to_le_bytes());
        out[8..].copy_from_slice(&hi.to_le_bytes());
        out
    }

    pub fn is_nan(&self) -> bool {
        matches!(self, Dec::Nan { .. })
    }
    pub fn is_inf(&self) -> bool {
        matches!(self, Dec::Inf { .. })
    }
    pub fn is_finite(&self) -> bool {
        matches!(self, Dec::Finite { .. })
    }
    pub fn is_negative(&self) -> bool {
        match *self {
            Dec::Nan { neg, .. } | Dec::Inf { neg } | Dec::Finite { neg, .. } => neg,
        }
    }
    pub fn is_zero(&self) -> bool {
        matches!(self, Dec::Finite { coeff: 0, .. })
    }
    pub fn coeff(&self) -> u128 {
        match *self {
            Dec::Finite { coeff, .. } => coeff,
            _ => 0,
        }
    }
    pub fn biased_exp(&self) -> i32 {
        match *self {
            Dec::Finite { biased_exp, .. } => biased_exp,
            _ => 0,
        }
    }

    pub fn abs(&self) -> Dec {
        match *self {
            Dec::Nan { signaling, .. } => Dec::Nan { neg: false, signaling },
            Dec::Inf { .. } => Dec::Inf { neg: false },
            Dec::Finite { coeff, biased_exp, .. } => Dec::Finite { neg: false, coeff, biased_exp },
        }
    }

    pub fn negate(&self) -> Dec {
        match *self {
            Dec::Nan { neg, signaling } => Dec::Nan { neg: !neg, signaling },
            Dec::Inf { neg } => Dec::Inf { neg: !neg },
            Dec::Finite { neg, coeff, biased_exp } => Dec::Finite { neg: !neg, coeff, biased_exp },
        }
    }

    /// The order of two values (None with a NaN), as bid128_quiet_* compare
    pub fn cmp_value(&self, o: &Dec) -> Option<Ordering> {
        let key = |d: &Dec| -> Option<(i8, Option<(u128, i32)>)> {
            match *d {
                Dec::Nan { .. } => None,
                Dec::Inf { neg } => Some((if neg { -2 } else { 2 }, None)),
                Dec::Finite { coeff: 0, .. } => Some((0, None)),
                Dec::Finite { neg, coeff, biased_exp } => Some((if neg { -1 } else { 1 }, Some((coeff, biased_exp)))),
            }
        };
        let (a, b) = (key(self)?, key(o)?);
        if a.0 != b.0 {
            return Some(a.0.cmp(&b.0));
        }
        let (Some(x), Some(y)) = (a.1, b.1) else { return Some(Ordering::Equal) };
        let mag = cmp_magnitude(x, y);
        Some(if a.0 < 0 { mag.reverse() } else { mag })
    }

    pub fn less(&self, o: &Dec) -> bool {
        self.cmp_value(o) == Some(Ordering::Less)
    }
    pub fn greater(&self, o: &Dec) -> bool {
        self.cmp_value(o) == Some(Ordering::Greater)
    }
    pub fn greater_equal(&self, o: &Dec) -> bool {
        matches!(self.cmp_value(o), Some(Ordering::Greater | Ordering::Equal))
    }
    /// bid128_quiet_not_equal (true with a NaN)
    pub fn not_equal(&self, o: &Dec) -> bool {
        self.cmp_value(o) != Some(Ordering::Equal)
    }

    /// bid128_scalbn: the exponent moved by `n`
    pub fn scale(&self, n: i64) -> Dec {
        let Dec::Finite { neg, mut coeff, biased_exp } = *self else {
            return match *self {
                Dec::Nan { neg, .. } => Dec::Nan { neg, signaling: false },
                o => o,
            };
        };
        let mut e = biased_exp as i64 + n;
        if coeff == 0 {
            return Dec::Finite { neg, coeff: 0, biased_exp: e.clamp(0, MAX_BIASED as i64) as i32 };
        }
        if (0..=MAX_BIASED as i64).contains(&e) {
            return Dec::Finite { neg, coeff, biased_exp: e as i32 };
        }
        if e > MAX_BIASED as i64 {
            // normalize the coefficient while it stays under Intel's bound
            while (coeff >> 64) < 0x314d_c644_8d93 && e > MAX_BIASED as i64 {
                coeff *= 10;
                e -= 1;
            }
            if e <= MAX_BIASED as i64 {
                return Dec::Finite { neg, coeff, biased_exp: e as i32 };
            }
            return Dec::Inf { neg };
        }
        // underflow: the coefficient is rounded (nearest-even) to exponent 0
        let k = (-e) as u64;
        Dec::Finite { neg, coeff: round_div(coeff, k, false), biased_exp: 0 }
    }

    /// bid128_round_integral_zero
    pub fn trunc(&self) -> Dec {
        let Dec::Finite { neg, coeff, biased_exp } = *self else {
            return match *self {
                Dec::Nan { neg, .. } => Dec::Nan { neg, signaling: false },
                o => o,
            };
        };
        if coeff == 0 {
            return Dec::Finite { neg, coeff: 0, biased_exp: biased_exp.max(EXPONENT_BIAS) };
        }
        let exp = biased_exp - EXPONENT_BIAS;
        if exp >= 0 {
            return *self;
        }
        if exp <= -(MAX_DIGITS as i32) || digits(coeff) as i32 + exp <= 0 {
            return Dec::finite(neg, 0, 0);
        }
        Dec::finite(neg, coeff / pow10((-exp) as u32), 0)
    }

    /// bid128_sub, rounded to nearest-even
    pub fn sub(&self, o: &Dec) -> Dec {
        self.add(&o.negate())
    }

    /// bid128_add, rounded to nearest-even
    pub fn add(&self, o: &Dec) -> Dec {
        match (*self, *o) {
            (Dec::Nan { neg, .. }, _) | (_, Dec::Nan { neg, .. }) => return Dec::Nan { neg, signaling: false },
            (Dec::Inf { neg: a }, Dec::Inf { neg: b }) if a != b => return Dec::Nan { neg: false, signaling: false },
            (Dec::Inf { neg }, _) | (_, Dec::Inf { neg }) => return Dec::Inf { neg },
            _ => {}
        }
        let (Dec::Finite { neg: na, coeff: ca, biased_exp: ea }, Dec::Finite { neg: nb, coeff: cb, biased_exp: eb }) = (*self, *o) else {
            unreachable!("specials handled above")
        };
        if ca == 0 && cb == 0 {
            // +0 unless both are -0 (round to nearest)
            return Dec::Finite { neg: na && nb, coeff: 0, biased_exp: ea.min(eb) };
        }
        if ca == 0 {
            return rescale_to(*o, ea.min(eb));
        }
        if cb == 0 {
            return rescale_to(*self, ea.min(eb));
        }
        // big has the larger exponent
        let ((nbig, cbig, ebig), (nsmall, csmall, esmall)) = if ea >= eb { ((na, ca, ea), (nb, cb, eb)) } else { ((nb, cb, eb), (na, ca, ea)) };
        let shift = (ebig - esmall) as u32;
        let db = digits(cbig);
        // exact when it fits 38 digits; otherwise the small operand is cut with a sticky bit
        let (big, small, sticky, e) = if db + shift <= 38 {
            (cbig * pow10(shift), csmall, false, esmall)
        } else {
            let k = 37 - db;
            let cut = shift - k;
            let (q, r) = if cut > 38 { (0, csmall) } else { (csmall / pow10(cut), csmall % pow10(cut)) };
            (cbig * pow10(k), q, r != 0, ebig - k as i32)
        };
        let (neg, mag, sticky) = if nbig == nsmall {
            (nbig, big + small, sticky)
        } else if big >= small {
            // big - (small + fraction) = (big - small - 1) + (1 - fraction)
            if sticky { (nbig, big - small - 1, true) } else { (nbig, big - small, false) }
        } else {
            (nsmall, small - big, sticky)
        };
        if mag == 0 && !sticky {
            return Dec::Finite { neg: false, coeff: 0, biased_exp: e };
        }
        normalize(neg, mag, e, sticky)
    }

    /// Whether this finite value is an integer (`modf` with a zero fraction)
    pub fn is_integral(&self) -> bool {
        match *self {
            Dec::Finite { coeff, biased_exp, .. } => {
                let exp = biased_exp - EXPONENT_BIAS;
                exp >= 0 || coeff == 0 || ((-exp) as u32 <= 38 && coeff % pow10((-exp) as u32) == 0)
            }
            _ => false,
        }
    }

    /// floor(log10(self)) as bid128_log10 rounded to 34 digits then truncated: the decimal
    /// exponent of the leading digit, one more when log10 rounds up to that integer
    pub fn floor_log10(&self) -> Option<i64> {
        let Dec::Finite { neg: false, coeff, biased_exp } = *self else { return None };
        if coeff == 0 {
            return None;
        }
        let d = digits(coeff) as i64;
        let k = biased_exp as i64 - EXPONENT_BIAS as i64 + d; // value in [10^(k-1), 10^k)
        // log10(value) = k - eps; rounds to k when eps is under half an ulp of k at 34 digits
        let top = pow10(d as u32);
        let gap = top - coeff;
        if gap == 0 {
            return Some(k);
        }
        let delta = gap as f64 / top as f64;
        let eps = -(1.0 - delta).ln() / std::f64::consts::LN_10;
        let int_digits = if k == 0 { 1 } else { digits(k.unsigned_abs() as u128) as i32 };
        let half_ulp = 0.5 * 10f64.powi(int_digits - MAX_DIGITS as i32);
        Some(if eps < half_ulp { k } else { k - 1 })
    }

    /// bid128_to_string: sign, the coefficient, 'E', the signed exponent
    pub fn to_intel_string(&self) -> String {
        match *self {
            Dec::Nan { neg, signaling } => format!("{}{}", if neg { '-' } else { '+' }, if signaling { "SNaN" } else { "NaN" }),
            Dec::Inf { neg } => format!("{}Inf", if neg { '-' } else { '+' }),
            Dec::Finite { neg, coeff, biased_exp } => {
                let e = biased_exp - EXPONENT_BIAS;
                format!("{}{coeff}E{}{e}", if neg { '-' } else { '+' }, if e >= 0 { "+" } else { "" })
            }
        }
    }
}

/// |a| vs |b| for finite non-zero (coeff, biased exponent)
fn cmp_magnitude(a: (u128, i32), b: (u128, i32)) -> Ordering {
    // compare by order of magnitude first, then align exactly
    let ma = digits(a.0) as i64 + a.1 as i64;
    let mb = digits(b.0) as i64 + b.1 as i64;
    if ma != mb {
        return ma.cmp(&mb);
    }
    // same magnitude: the exponent difference is below 34 digits
    let (x, y) = if a.1 >= b.1 { (a.0 * pow10((a.1 - b.1) as u32), b.0) } else { (a.0, b.0 * pow10((b.1 - a.1) as u32)) };
    x.cmp(&y)
}

/// coeff / 10^k rounded to nearest-even (`sticky`: a non-zero fraction below the last digit)
fn round_div(coeff: u128, k: u64, sticky: bool) -> u128 {
    if k == 0 {
        return coeff;
    }
    if k > 38 {
        return 0;
    }
    let p = pow10(k as u32);
    let (q, r) = (coeff / p, coeff % p);
    let half = p / 2;
    let up = r > half || (r == half && (sticky || q % 2 == 1));
    if r == half && !sticky && q % 2 == 0 {
        return q;
    }
    q + up as u128
}

/// A value with an exact (or sticky) coefficient, rounded to 34 digits (nearest-even)
fn normalize(neg: bool, mag: u128, e: i32, sticky: bool) -> Dec {
    let d = digits(mag);
    let (mut c, mut e) = if d > MAX_DIGITS {
        let k = d - MAX_DIGITS;
        (round_div(mag, k as u64, sticky), e + k as i32)
    } else {
        (mag, e)
    };
    if c > COEFF_MAX {
        c /= 10;
        e += 1;
    }
    if e > MAX_BIASED {
        return Dec::Inf { neg };
    }
    Dec::Finite { neg, coeff: c, biased_exp: e }
}

/// `d` with its coefficient at exponent `e` (<= its own), exactly when it fits 34 digits
fn rescale_to(d: Dec, e: i32) -> Dec {
    let Dec::Finite { neg, coeff, biased_exp } = d else { return d };
    let shift = (biased_exp - e) as u32;
    if shift == 0 || digits(coeff) + shift > MAX_DIGITS {
        return d;
    }
    Dec::Finite { neg, coeff: coeff * pow10(shift), biased_exp: e }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(c: i64, e: i32) -> Dec {
        Dec::finite(c < 0, c.unsigned_abs() as u128, e)
    }

    #[test]
    fn arithmetic() {
        // 3.1415 - 1 = 2.1415
        assert_eq!(d(31415, -4).sub(&d(1, 0)), d(21415, -4));
        assert!(d(1, 0).less(&d(11, -1)));
        assert!(!d(10, -1).not_equal(&d(1, 0)));
        assert_eq!(d(31419, -1).trunc(), d(3141, 0));
        assert_eq!(d(-31419, -1).trunc(), d(-3141, 0));
        assert_eq!(d(31415, -4).scale(2), d(31415, -2));
        assert_eq!(d(5, -40).trunc(), Dec::finite(false, 0, 0));
        assert_eq!(d(31415, -4).to_intel_string(), "+31415E-4");
        assert_eq!(d(0, 0).to_intel_string(), "+0E+0");
        // rounding of a 39-digit difference to 34 digits (half-even)
        let big = Dec::finite(false, COEFF_MAX, 4);
        let r = big.sub(&d(1, 0));
        assert_eq!(r, Dec::finite(false, COEFF_MAX, 4));
        assert_eq!(Dec::from_bytes(&big.to_bytes()), big);
        assert_eq!(d(1000, 0).floor_log10(), Some(3));
        assert_eq!(d(999, 0).floor_log10(), Some(2));
    }
}
