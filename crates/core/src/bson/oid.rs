//! ObjectId: generation as libbson's default context does (4-byte big-endian time, 5 bytes of
//! per-process randomness, 3-byte big-endian counter) and hex conversions.

use std::cell::Cell;
use std::time::{SystemTime, UNIX_EPOCH};

thread_local! {
    static CTX: Cell<Option<(u32, [u8; 5], u32)>> = const { Cell::new(None) };
}

pub fn random_bytes(n: usize) -> Vec<u8> {
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

/// Bumped in every child process (a pthread_atfork child handler, registered at the first
/// ObjectId): the random part is re-drawn there, so parent and child never generate the same
/// ids. No per-call system call.
static FORK_EPOCH: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

#[cfg(unix)]
fn fork_epoch() -> u32 {
    extern "C" fn child() {
        FORK_EPOCH.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    static ONCE: std::sync::Once = std::sync::Once::new();
    // SAFETY: registers a handler that only touches an atomic
    ONCE.call_once(|| unsafe {
        libc::pthread_atfork(None, None, Some(child));
    });
    FORK_EPOCH.load(std::sync::atomic::Ordering::Relaxed)
}

#[cfg(not(unix))]
fn fork_epoch() -> u32 {
    FORK_EPOCH.load(std::sync::atomic::Ordering::Relaxed)
}

/// `bson_oid_init(oid, NULL)`. The random part is re-drawn after fork (see FORK_EPOCH).
pub fn generate() -> [u8; 12] {
    let epoch = fork_epoch();
    let (rand, seq) = CTX.with(|c| {
        let (p, rand, seq) = match c.get() {
            Some(s) if s.0 == epoch => s,
            _ => {
                let r = random_bytes(8);
                let seq = u32::from_le_bytes([r[5], r[6], r[7], 0]) & !0xf0000f;
                (epoch, [r[0], r[1], r[2], r[3], r[4]], seq)
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

#[cfg(test)]
mod tests {
    /// A child process draws a new random part: parent and child never share ObjectIds.
    #[cfg(unix)]
    #[test]
    fn fork_redraws_the_random_part() {
        let parent = super::generate();
        let mut fds = [0; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0);
        if pid == 0 {
            let child = super::generate();
            unsafe {
                libc::write(fds[1], child.as_ptr().cast(), 12);
                libc::_exit(0);
            }
        }
        let mut child = [0u8; 12];
        assert_eq!(unsafe { libc::read(fds[0], child.as_mut_ptr().cast(), 12) }, 12);
        unsafe { libc::waitpid(pid, std::ptr::null_mut(), 0) };
        assert_ne!(parent[4..9], child[4..9]);
        // the parent keeps its own
        assert_eq!(super::generate()[4..9], parent[4..9]);
    }
}
