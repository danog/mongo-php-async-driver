//! Eight bytes at a time ("SIMD within a register") scans for the byte loops of the BSON code.
//! The bit tricks are the classic ones (Sean Eron Anderson, "Bit Twiddling Hacks": `haszero`,
//! `hasless`): a non-zero result means at least one matching byte, and the lowest set 0x80 bit
//! marks the first match (a borrow can only add false marks above a true one).

const LO: u64 = 0x0101_0101_0101_0101;
const HI: u64 = 0x8080_8080_8080_8080;

/// 0x80 in every byte of `w` that is zero (and possibly in bytes above the first such byte).
#[inline(always)]
pub fn zero_bytes(w: u64) -> u64 {
    w.wrapping_sub(LO) & !w & HI
}

/// 0x80 marks for the bytes of `w` equal to `b`.
#[inline(always)]
pub fn eq_bytes(w: u64, b: u8) -> u64 {
    zero_bytes(w ^ (LO * b as u64))
}

/// Non-zero when a byte of `w` is below `n` (`n` <= 128).
#[inline(always)]
pub fn lt_bytes(w: u64, n: u8) -> u64 {
    w.wrapping_sub(LO * n as u64) & !w & HI
}

#[inline(always)]
pub fn load(s: &[u8], i: usize) -> u64 {
    u64::from_le_bytes(s[i..i + 8].try_into().unwrap())
}

/// The index of the first NUL in `s[from..]` (as an index into `s`).
#[inline]
pub fn find_nul(s: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i + 8 <= s.len() {
        let m = zero_bytes(load(s, i));
        if m != 0 {
            return Some(i + (m.trailing_zeros() / 8) as usize);
        }
        i += 8;
    }
    while i < s.len() {
        if s[i] == 0 {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// The index of the first `a` or `b` in `s[from..]`, or `s.len()`.
#[inline]
pub fn find2(s: &[u8], from: usize, a: u8, b: u8) -> usize {
    let mut i = from;
    while i + 8 <= s.len() {
        let w = load(s, i);
        let m = eq_bytes(w, a) | eq_bytes(w, b);
        if m != 0 {
            // the lowest mark of either mask is exact (each mask's lowest mark is)
            return i + (m.trailing_zeros() / 8) as usize;
        }
        i += 8;
    }
    while i < s.len() && s[i] != a && s[i] != b {
        i += 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_match_bytewise() {
        let alphabet = [0u8, 1, b'"', b'\\', 0x7f, 0x80, 0xff, b'a', 0x22 ^ 0x80, 0x5c ^ 0x01];
        let mut x: u64 = 0xdead_beef_1234_5678;
        for _ in 0..200_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let n = (x % 40) as usize;
            let mut y = x;
            let s: Vec<u8> = (0..n)
                .map(|_| {
                    y = y.wrapping_mul(6364136223846793005).wrapping_add(1);
                    alphabet[(y >> 60) as usize % alphabet.len()]
                })
                .collect();
            for from in 0..=n {
                assert_eq!(find_nul(&s, from), s[from..].iter().position(|&c| c == 0).map(|p| p + from));
                assert_eq!(find2(&s, from, b'"', b'\\'), s[from..].iter().position(|&c| c == b'"' || c == b'\\').map_or(n, |p| p + from));
            }
        }
    }
}
