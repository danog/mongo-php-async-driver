//! A minimal DNS stub resolver for the SRV and TXT lookups of `mongodb+srv://` URIs: what
//! libmongoc gets from libresolv's res_nsearch (search list, UDP with a TCP retry on truncation,
//! RES_USEVC when TCP is preferred), in Rust, blocking. Only the resolver configuration of
//! /etc/resolv.conf is read (nameserver, search/domain, options ndots/timeout/attempts/use-vc).
//!
//! Everything a server sends is bounded: a response is at most 65,535 bytes (the TCP length
//! prefix), names are decompressed with backward-only pointers into at most 255 octets, every
//! record must lie inside the message, and at most `MAX_RECORDS` answers are accepted.

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream, UdpSocket};
use std::time::{Duration, Instant};

pub const T_TXT: u16 = 16;
pub const T_SRV: u16 = 33;
const C_IN: u16 = 1;
/// More answer records than any real deployment has (a 64 KiB response holds about 3,000
/// SRV records): an answer above it is refused.
pub const MAX_RECORDS: usize = 1024;
const MAXNS: usize = 3;
const MAXDNSRCH: usize = 6;
const MAX_CONF: u64 = 64 * 1024;

/// The h_errno values res_nsearch reports (and _mongoc_hstrerror's texts).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LookupError {
    HostNotFound,
    NoData,
    TryAgain,
    NoRecovery,
}

impl LookupError {
    pub fn message(self) -> &'static str {
        match self {
            LookupError::HostNotFound => "The specified host is unknown.",
            LookupError::NoData => "The requested name is valid but does not have an IP address.",
            LookupError::NoRecovery => "A nonrecoverable name server error occurred.",
            LookupError::TryAgain => "A temporary error occurred on an authoritative name server. Try again later.",
        }
    }
}

/// The resolver configuration (res_ninit).
#[derive(Clone, Debug)]
pub struct Config {
    pub nameservers: Vec<SocketAddr>,
    pub search: Vec<String>,
    pub ndots: usize,
    pub timeout: Duration,
    pub attempts: u32,
    pub use_vc: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config { nameservers: vec![SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 53)], search: Vec::new(), ndots: 1, timeout: Duration::from_secs(5), attempts: 2, use_vc: false }
    }
}

impl Config {
    /// /etc/resolv.conf (res_ninit's rules for the keys it reads; the limits of glibc).
    pub fn system() -> Config {
        let mut text = String::new();
        if let Ok(f) = std::fs::File::open("/etc/resolv.conf") {
            let _ = f.take(MAX_CONF).read_to_string(&mut text);
        }
        let mut c = Config::parse(&text);
        if c.search.is_empty() {
            // res_ninit: without search/domain, the domain part of the host name
            if let Some(d) = hostname_domain() {
                c.search.push(d);
            }
        }
        c
    }

    pub fn parse(text: &str) -> Config {
        let mut c = Config { nameservers: Vec::new(), ..Config::default() };
        for line in text.lines() {
            let mut w = line.split(|ch: char| ch == ' ' || ch == '\t').filter(|s| !s.is_empty());
            match w.next() {
                Some("nameserver") if c.nameservers.len() < MAXNS => {
                    // a scoped IPv6 address (fe80::1%en0) is skipped: std cannot parse the scope name
                    if let Some(ip) = w.next().and_then(|a| a.parse::<IpAddr>().ok()) {
                        c.nameservers.push(SocketAddr::new(ip, 53));
                    }
                }
                Some("domain") => {
                    if let Some(d) = w.next() {
                        c.search = vec![d.trim_end_matches('.').to_owned()];
                    }
                }
                Some("search") => {
                    c.search = w.take(MAXDNSRCH).map(|d| d.trim_end_matches('.').to_owned()).filter(|d| !d.is_empty()).collect();
                }
                Some("options") => {
                    for o in w {
                        let num = |p: &str| o.strip_prefix(p).and_then(|v| v.parse::<u32>().ok());
                        if let Some(n) = num("ndots:") {
                            c.ndots = n.min(15) as usize;
                        } else if let Some(n) = num("timeout:") {
                            c.timeout = Duration::from_secs(u64::from(n.clamp(1, 30)));
                        } else if let Some(n) = num("attempts:") {
                            c.attempts = n.clamp(1, 5);
                        } else if o == "use-vc" || o == "usevc" || o == "tcp" {
                            c.use_vc = true;
                        }
                    }
                }
                _ => {}
            }
        }
        if c.nameservers.is_empty() {
            c.nameservers = Config::default().nameservers;
        }
        c
    }
}

fn hostname_domain() -> Option<String> {
    let mut buf = [0u8; 256];
    // SAFETY: the buffer is valid for its length; gethostname NUL-terminates within it or fails
    if unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len() - 1) } != 0 {
        return None;
    }
    let n = buf.iter().position(|&b| b == 0)?;
    let h = std::str::from_utf8(&buf[..n]).ok()?;
    h.split_once('.').map(|(_, d)| d.to_owned()).filter(|d| !d.is_empty())
}

/// res_nsearch: the response message for `name` (type `qtype`, class IN), with the search
/// list applied as libresolv does (the name as given first when it has at least ndots dots).
pub fn search(cfg: &Config, name: &str, qtype: u16, prefer_tcp: bool) -> Result<Vec<u8>, LookupError> {
    let tcp = prefer_tcp || cfg.use_vc;
    if name.ends_with('.') {
        return query(cfg, name, qtype, tcp);
    }
    let dots = name.matches('.').count();
    let mut saved = None;
    if dots >= cfg.ndots {
        match query(cfg, name, qtype, tcp) {
            Ok(m) => return Ok(m),
            Err(e) => saved = Some(e),
        }
    }
    let mut got_nodata = false;
    let mut got_servfail = false;
    for d in &cfg.search {
        match query(cfg, &format!("{name}.{d}"), qtype, tcp) {
            Ok(m) => return Ok(m),
            Err(LookupError::NoData) => got_nodata = true,
            Err(LookupError::HostNotFound) => {}
            Err(LookupError::TryAgain) => got_servfail = true,
            Err(e @ LookupError::NoRecovery) => {
                saved.get_or_insert(e);
                break;
            }
        }
    }
    if saved.is_none() {
        match query(cfg, name, qtype, tcp) {
            Ok(m) => return Ok(m),
            Err(e) => saved = Some(e),
        }
    }
    Err(saved.unwrap_or(if got_nodata {
        LookupError::NoData
    } else if got_servfail {
        LookupError::TryAgain
    } else {
        LookupError::HostNotFound
    }))
}

/// res_nquery: one name, every server in turn for each attempt.
fn query(cfg: &Config, name: &str, qtype: u16, tcp: bool) -> Result<Vec<u8>, LookupError> {
    let (q, id) = build_query(name, qtype).ok_or(LookupError::NoRecovery)?;
    let mut last = LookupError::TryAgain;
    for _ in 0..cfg.attempts {
        for ns in &cfg.nameservers {
            let reply = if tcp { send_tcp(*ns, &q, cfg.timeout) } else { send_udp(*ns, &q, id, cfg.timeout) };
            let reply = match reply {
                Ok(r) if !tcp && r.len() >= 4 && r[2] & 0x02 != 0 => match send_tcp(*ns, &q, cfg.timeout) {
                    Ok(r) => r,
                    Err(()) => continue,
                },
                Ok(r) => r,
                Err(()) => continue,
            };
            if !answers(&reply, &q) {
                continue;
            }
            let ancount = u16::from_be_bytes([reply[6], reply[7]]);
            match reply[3] & 0x0f {
                0 if ancount == 0 => return Err(LookupError::NoData),
                0 => return Ok(reply),
                3 => return Err(LookupError::HostNotFound),
                // SERVFAIL, NOTIMP, REFUSED: the next server
                2 => last = LookupError::TryAgain,
                _ => last = LookupError::NoRecovery,
            }
        }
    }
    Err(last)
}

/// res_nmkquery: a recursive query with a random ID; None for a name that cannot be encoded.
fn build_query(name: &str, qtype: u16) -> Option<(Vec<u8>, u16)> {
    let mut idb = [0u8; 2];
    getrandom::fill(&mut idb).ok()?;
    let mut q = Vec::with_capacity(name.len() + 18);
    q.extend_from_slice(&idb);
    q.extend_from_slice(&[0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0]);
    let name = name.strip_suffix('.').unwrap_or(name);
    for label in name.split('.') {
        if label.is_empty() || label.len() > 63 {
            return None;
        }
        q.push(label.len() as u8);
        q.extend_from_slice(label.as_bytes());
    }
    q.push(0);
    if q.len() - 12 > 255 {
        return None;
    }
    q.extend_from_slice(&qtype.to_be_bytes());
    q.extend_from_slice(&C_IN.to_be_bytes());
    Some((q, u16::from_be_bytes(idb)))
}

/// Whether `r` is the response to `q`: same ID, a response, the same single question (the
/// name compared without case).
fn answers(r: &[u8], q: &[u8]) -> bool {
    if r.len() < q.len() || r[..2] != q[..2] || r[2] & 0x80 == 0 || r[4..6] != [0, 1] {
        return false;
    }
    r[12..q.len()].eq_ignore_ascii_case(&q[12..])
}

fn send_udp(ns: SocketAddr, q: &[u8], id: u16, timeout: Duration) -> Result<Vec<u8>, ()> {
    let local: SocketAddr = if ns.is_ipv4() { (Ipv4Addr::UNSPECIFIED, 0).into() } else { (std::net::Ipv6Addr::UNSPECIFIED, 0).into() };
    let s = UdpSocket::bind(local).map_err(drop)?;
    // connected: datagrams from other addresses are dropped by the kernel
    s.connect(ns).map_err(drop)?;
    s.send(q).map_err(drop)?;
    let deadline = Instant::now() + timeout;
    let mut buf = vec![0u8; 65535];
    loop {
        let left = deadline.checked_duration_since(Instant::now()).filter(|d| !d.is_zero()).ok_or(())?;
        s.set_read_timeout(Some(left)).map_err(drop)?;
        let n = s.recv(&mut buf).map_err(drop)?;
        // a datagram that is not the answer (an old or forged one) is ignored
        if n >= 12 && u16::from_be_bytes([buf[0], buf[1]]) == id && answers(&buf[..n], q) {
            buf.truncate(n);
            return Ok(buf);
        }
    }
}

fn send_tcp(ns: SocketAddr, q: &[u8], timeout: Duration) -> Result<Vec<u8>, ()> {
    let deadline = Instant::now() + timeout;
    let mut s = TcpStream::connect_timeout(&ns, timeout).map_err(drop)?;
    let left = || deadline.checked_duration_since(Instant::now()).filter(|d| !d.is_zero()).ok_or(());
    s.set_write_timeout(Some(left()?)).map_err(drop)?;
    let mut m = Vec::with_capacity(q.len() + 2);
    m.extend_from_slice(&(q.len() as u16).to_be_bytes());
    m.extend_from_slice(q);
    s.write_all(&m).map_err(drop)?;
    let mut read = |buf: &mut [u8]| -> Result<(), ()> {
        let mut at = 0;
        while at < buf.len() {
            s.set_read_timeout(Some(left()?)).map_err(drop)?;
            match s.read(&mut buf[at..]) {
                Ok(0) | Err(_) => return Err(()),
                Ok(n) => at += n,
            }
        }
        Ok(())
    };
    let mut len = [0u8; 2];
    read(&mut len)?;
    let mut r = vec![0u8; u16::from_be_bytes(len) as usize];
    read(&mut r)?;
    Ok(r)
}

/// One answer record: its type, TTL and where its RDATA lies in the message.
#[derive(Clone, Copy, Debug)]
pub struct Record {
    pub rtype: u16,
    pub ttl: u32,
    pub rdata: (usize, usize),
}

/// ns_initparse + ns_parserr over the answer section: the records, every length checked.
pub fn parse_answers(m: &[u8]) -> Result<Vec<Record>, &'static str> {
    if m.len() < 12 {
        return Err("short message");
    }
    let qd = u16::from_be_bytes([m[4], m[5]]) as usize;
    let an = u16::from_be_bytes([m[6], m[7]]) as usize;
    if an > MAX_RECORDS {
        return Err("too many records");
    }
    let mut at = 12;
    for _ in 0..qd {
        at = skip_name(m, at)?;
        at = at.checked_add(4).filter(|&a| a <= m.len()).ok_or("truncated question")?;
    }
    let mut out = Vec::with_capacity(an);
    for _ in 0..an {
        at = skip_name(m, at)?;
        let h = m.get(at..at + 10).ok_or("truncated record")?;
        let rtype = u16::from_be_bytes([h[0], h[1]]);
        let ttl = u32::from_be_bytes([h[4], h[5], h[6], h[7]]);
        let len = u16::from_be_bytes([h[8], h[9]]) as usize;
        let start = at + 10;
        let end = start.checked_add(len).filter(|&e| e <= m.len()).ok_or("truncated record data")?;
        out.push(Record { rtype, ttl, rdata: (start, end) });
        at = end;
    }
    Ok(out)
}

/// The offset after the (possibly compressed) name at `at`.
fn skip_name(m: &[u8], mut at: usize) -> Result<usize, &'static str> {
    loop {
        let l = *m.get(at).ok_or("truncated name")?;
        match l & 0xc0 {
            0x00 if l == 0 => return Ok(at + 1),
            0x00 => at += 1 + l as usize,
            0xc0 => return if at + 2 <= m.len() { Ok(at + 2) } else { Err("truncated name") },
            _ => return Err("invalid label"),
        }
    }
}

/// dn_expand: the presentation form of the name at `at` (special characters escaped as
/// libresolv does). Pointers must go strictly backwards, so every expansion ends.
pub fn expand_name(m: &[u8], mut at: usize) -> Result<String, &'static str> {
    let mut out = String::new();
    let mut octets = 0usize;
    let mut limit = at;
    loop {
        let l = *m.get(at).ok_or("truncated name")?;
        match l & 0xc0 {
            0x00 if l == 0 => break,
            0x00 => {
                let label = m.get(at + 1..at + 1 + l as usize).ok_or("truncated name")?;
                octets += 1 + label.len();
                if octets > 255 {
                    return Err("name too long");
                }
                if !out.is_empty() {
                    out.push('.');
                }
                for &c in label {
                    match c {
                        b'.' | b';' | b'\\' | b'(' | b')' | b'@' | b'$' | b'"' => {
                            out.push('\\');
                            out.push(c as char);
                        }
                        0x21..=0x7e => out.push(c as char),
                        _ => out.push_str(&format!("\\{c:03}")),
                    }
                }
                at += 1 + label.len();
            }
            0xc0 => {
                let p = (u16::from_be_bytes([l, *m.get(at + 1).ok_or("truncated name")?]) & 0x3fff) as usize;
                if p >= limit {
                    return Err("invalid compression pointer");
                }
                limit = p;
                at = p;
            }
            _ => return Err("invalid label"),
        }
    }
    if out.is_empty() {
        out.push('.');
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(q: &[u8], answers: &[(u16, u32, Vec<u8>)]) -> Vec<u8> {
        let mut r = q.to_vec();
        r[2] |= 0x80;
        r[6..8].copy_from_slice(&(answers.len() as u16).to_be_bytes());
        for (t, ttl, rdata) in answers {
            r.extend_from_slice(&[0xc0, 12]);
            r.extend_from_slice(&t.to_be_bytes());
            r.extend_from_slice(&C_IN.to_be_bytes());
            r.extend_from_slice(&ttl.to_be_bytes());
            r.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
            r.extend_from_slice(rdata);
        }
        r
    }

    #[test]
    fn query_and_answers() {
        let (q, id) = build_query("_mongodb._tcp.test1.example.com", T_SRV).unwrap();
        assert_eq!(u16::from_be_bytes([q[0], q[1]]), id);
        let mut srv = vec![0, 0, 0, 0, 0x6d, 0x89];
        srv.extend_from_slice(b"\x09localhost\xc0\x1a");
        let r = response(&q, &[(T_SRV, 300, srv)]);
        assert!(answers(&r, &q));
        let recs = parse_answers(&r).unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!((recs[0].rtype, recs[0].ttl), (T_SRV, 300));
        // the target: "localhost" + the suffix of the question name after "_mongodb._tcp."
        assert_eq!(expand_name(&r, recs[0].rdata.0 + 6).unwrap(), "localhost.test1.example.com");
        let mut other = r.clone();
        other[0] ^= 1;
        assert!(!answers(&other, &q));
        assert!(build_query(&"a".repeat(64), T_TXT).is_none());
        assert!(build_query(&["a"; 130].join("."), T_TXT).is_none());
        assert!(build_query("a..b", T_TXT).is_none());
    }

    #[test]
    fn hostile_names_and_lengths() {
        let (q, _) = build_query("x.example.com", T_SRV).unwrap();
        // a pointer to itself, a forward pointer, a pointer loop
        let mut r = response(&q, &[]);
        let at = r.len();
        r.extend_from_slice(&[0xc0, at as u8]);
        assert!(expand_name(&r, at).is_err());
        let mut r2 = response(&q, &[]);
        let at2 = r2.len();
        r2.extend_from_slice(&[0xc0, (at2 + 2) as u8, 1, b'a', 0]);
        assert!(expand_name(&r2, at2).is_err());
        // record data running past the end
        let mut bad = response(&q, &[(T_SRV, 1, vec![0; 8])]);
        bad.truncate(bad.len() - 1);
        assert!(parse_answers(&bad).is_err());
        // too many records
        let mut many = response(&q, &[]);
        many[6..8].copy_from_slice(&((MAX_RECORDS + 1) as u16).to_be_bytes());
        assert_eq!(parse_answers(&many).unwrap_err(), "too many records");
        // a name longer than 255 octets
        let mut long = response(&q, &[]);
        let at3 = long.len();
        for _ in 0..5 {
            long.push(63);
            long.extend_from_slice(&[b'a'; 63]);
        }
        long.push(0);
        assert_eq!(expand_name(&long, at3).unwrap_err(), "name too long");
        // escapes as dn_expand
        let mut esc = response(&q, &[]);
        let at4 = esc.len();
        esc.extend_from_slice(b"\x04a.b\x01\x00\x00");
        assert_eq!(expand_name(&esc, at4).unwrap(), "a\\.b\\001");
    }

    #[test]
    fn resolv_conf() {
        let c = Config::parse("# x\nnameserver 10.0.0.1\nnameserver ::1\nnameserver 10.0.0.2\nnameserver 10.0.0.3\nsearch a.example b.example.\noptions ndots:2 timeout:99 attempts:0 use-vc\n");
        assert_eq!(c.nameservers.len(), 3);
        assert_eq!(c.search, vec!["a.example", "b.example"]);
        assert_eq!((c.ndots, c.timeout, c.attempts, c.use_vc), (2, Duration::from_secs(30), 1, true));
        assert_eq!(Config::parse("").nameservers, Config::default().nameservers);
    }
}
