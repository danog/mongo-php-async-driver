//! A minimal blocking HTTP client for OCSP requests (mongoc-http.c's _mongoc_http_send): one
//! request per connection, `HTTP/1.0` with `Connection: close`, the response read until the
//! server closes the connection. Plain `http://` only (OCSP responders are reached without TLS).
//!
//! Bounds: one deadline for the whole exchange (name resolution excepted, as in libmongoc);
//! the response is refused above 8 MiB (libmongoc's limit); URLs, hosts and paths are limited
//! in length and to visible ASCII, so a URL cannot add header lines.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

/// libmongoc refuses HTTP responses above this size.
const MAX_RESPONSE: usize = 8 * 1024 * 1024;
/// Longest URL accepted.
const MAX_URL: usize = 2048;
/// Addresses of one host tried at most.
const MAX_ADDRS: usize = 8;

/// The parts of an `http://host[:port][/path]` URL (OCSP_parse_url).
#[derive(Debug, PartialEq, Eq)]
pub struct Url {
    pub host: String,
    pub port: u16,
    pub path: String,
}

/// Parses an OCSP responder URL. `https://` is refused (plain HTTP only).
pub fn parse_url(url: &str) -> Result<Url, String> {
    if url.len() > MAX_URL || !url.bytes().all(|b| b.is_ascii_graphic()) {
        return Err("Could not parse URL".into());
    }
    let rest = match url.get(..7) {
        Some(p) if p.eq_ignore_ascii_case("http://") => &url[7..],
        _ => return Err(if url.get(..8).is_some_and(|p| p.eq_ignore_ascii_case("https://")) { "https responders are not supported".into() } else { "Could not parse URL".into() }),
    };
    let (authority, path) = match rest.find(['/', '?', '#']) {
        Some(i) if rest.as_bytes()[i] == b'/' => (&rest[..i], &rest[i..]),
        Some(i) => (&rest[..i], "/"),
        None => (rest, "/"),
    };
    if authority.contains('@') {
        return Err("Could not parse URL".into());
    }
    let (host, port) = if let Some(h) = authority.strip_prefix('[') {
        let end = h.find(']').ok_or("Could not parse URL")?;
        let after = &h[end + 1..];
        let port = match after.strip_prefix(':') {
            Some(p) => p,
            None if after.is_empty() => "",
            None => return Err("Could not parse URL".into()),
        };
        (&authority[..end + 2], port)
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) => (h, p),
            None => (authority, ""),
        }
    };
    let port = if port.is_empty() { 80 } else { port.parse::<u16>().ok().filter(|p| *p != 0).ok_or("Could not parse URL")? };
    if host.is_empty() || host.trim_start_matches('[').trim_end_matches(']').is_empty() {
        return Err("Could not parse URL".into());
    }
    let path = path.split('#').next().unwrap_or("/");
    Ok(Url { host: host.to_owned(), port, path: if path.is_empty() { "/".into() } else { path.to_owned() } })
}

/// The request head (_mongoc_http_render_request_head).
fn request_head(method: &str, url: &Url, extra_headers: &str, body_len: usize) -> String {
    let mut h = format!("{method} {} HTTP/1.0\r\nHost: {}:{}\r\nConnection: close\r\n", url.path, url.host, url.port);
    if body_len > 0 {
        h.push_str(&format!("Content-Length: {body_len}\r\n"));
    }
    h.push_str(extra_headers);
    h.push_str("\r\n");
    h
}

fn remaining(deadline: Instant) -> Result<Duration, String> {
    deadline.checked_duration_since(Instant::now()).filter(|d| !d.is_zero()).ok_or_else(|| "Timeout reading from stream".to_owned())
}

fn connect(url: &Url, deadline: Instant) -> Result<TcpStream, String> {
    let host = url.host.trim_start_matches('[').trim_end_matches(']');
    let addrs: Vec<SocketAddr> = (host, url.port).to_socket_addrs().map_err(|_| format!("Failed to connect to: {}", url.host))?.take(MAX_ADDRS).collect();
    for a in addrs {
        let Ok(t) = remaining(deadline) else { break };
        if let Ok(s) = TcpStream::connect_timeout(&a, t) {
            return Ok(s);
        }
    }
    Err(format!("Failed to connect to: {}", url.host))
}

/// The status code and body of a response (_mongoc_http_send's parsing).
pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}

/// Sends one request and reads the response until the server closes the connection, all
/// within `timeout`.
pub fn send(method: &str, url: &Url, extra_headers: &str, body: &[u8], timeout: Duration) -> Result<Response, String> {
    let deadline = Instant::now() + timeout;
    let mut s = connect(url, deadline)?;
    let mut req = request_head(method, url, extra_headers, body.len()).into_bytes();
    req.extend_from_slice(body);
    let mut off = 0;
    while off < req.len() {
        s.set_write_timeout(Some(remaining(deadline)?)).map_err(|e| e.to_string())?;
        match s.write(&req[off..]) {
            Ok(0) => return Err("Failed to write the HTTP request".into()),
            Ok(n) => off += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(format!("Failed to write the HTTP request: {e}")),
        }
    }
    let mut buf = Vec::new();
    let mut chunk = [0u8; 32 * 1024];
    loop {
        s.set_read_timeout(Some(remaining(deadline)?)).map_err(|e| e.to_string())?;
        match s.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.len() > MAX_RESPONSE {
                    return Err("HTTP response message is too large".into());
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => return Err("Timeout reading from stream".into()),
            // libmongoc stops reading at an error and parses what it has
            Err(_) => break,
        }
    }
    parse_response(&buf)
}

/// Parses `HTTP/1.x NNN ...\r\n<headers>\r\n\r\n<body>`.
pub fn parse_response(buf: &[u8]) -> Result<Response, String> {
    if buf.is_empty() {
        return Err("No response received".into());
    }
    let lead = if buf.starts_with(b"HTTP/1.0 ") || buf.starts_with(b"HTTP/1.1 ") { 9 } else { return Err("No HTTP version leader in HTTP response. Expected 'HTTP/1.0 ' or 'HTTP/1.1 '".into()) };
    let code = buf.get(lead..lead + 3).filter(|c| c.iter().all(u8::is_ascii_digit)).ok_or("Invalid HTTP response status string")?;
    let status = code.iter().fold(0u16, |a, d| a * 10 + u16::from(d - b'0'));
    let end = buf.windows(4).position(|w| w == b"\r\n\r\n").ok_or("Error occurred reading response: end of headers not found")?;
    Ok(Response { status, body: buf[end + 4..].to_vec() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        assert_eq!(parse_url("http://ocsp.example.com").unwrap(), Url { host: "ocsp.example.com".into(), port: 80, path: "/".into() });
        assert_eq!(parse_url("HTTP://127.0.0.1:28041/x/y?z").unwrap(), Url { host: "127.0.0.1".into(), port: 28041, path: "/x/y?z".into() });
        assert_eq!(parse_url("http://[::1]:8080").unwrap(), Url { host: "[::1]".into(), port: 8080, path: "/".into() });
        assert_eq!(parse_url("http://h?q").unwrap().path, "/");
        for bad in ["https://ocsp.example.com/", "ftp://x/", "http://", "http://:80/", "http://h:0/", "http://h:99999/", "http://u@h/", "http://h/a b", "http://h/\r\nX: y", "http://[::1/"] {
            assert!(parse_url(bad).is_err(), "{bad}");
        }
        assert!(parse_url(&format!("http://h/{}", "a".repeat(MAX_URL))).is_err());
        assert_eq!(parse_url("https://x/").unwrap_err(), "https responders are not supported");
    }

    #[test]
    fn head() {
        let u = parse_url("http://127.0.0.1:28041/ocsp").unwrap();
        assert_eq!(request_head("POST", &u, "Content-Type: application/ocsp-request\r\n", 5), "POST /ocsp HTTP/1.0\r\nHost: 127.0.0.1:28041\r\nConnection: close\r\nContent-Length: 5\r\nContent-Type: application/ocsp-request\r\n\r\n");
    }

    #[test]
    fn responses() {
        let r = parse_response(b"HTTP/1.1 200 OK\r\nContent-Type: x\r\n\r\nbody").unwrap();
        assert_eq!((r.status, r.body.as_slice()), (200, &b"body"[..]));
        assert!(parse_response(b"").is_err());
        assert!(parse_response(b"HTTP/2 200\r\n\r\n").is_err());
        assert!(parse_response(b"HTTP/1.0 2x0\r\n\r\n").is_err());
        assert!(parse_response(b"HTTP/1.0 200 OK\r\n").is_err());
    }

    #[test]
    fn deadline_and_size_bounds() {
        // a server that accepts and never answers: the deadline ends the exchange
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let t = std::thread::spawn(move || {
            let (s, _) = l.accept().unwrap();
            std::thread::sleep(Duration::from_millis(700));
            drop(s);
            // then one that sends more than the limit
            let (mut s, _) = l.accept().unwrap();
            let mut req = [0u8; 512];
            let _ = s.read(&mut req);
            let junk = vec![b'a'; 1024 * 1024];
            let _ = s.write_all(b"HTTP/1.0 200 OK\r\n\r\n");
            for _ in 0..9 {
                if s.write_all(&junk).is_err() {
                    break;
                }
            }
        });
        let u = Url { host: "127.0.0.1".into(), port, path: "/".into() };
        let t0 = Instant::now();
        assert_eq!(send("POST", &u, "", b"x", Duration::from_millis(300)).err().as_deref(), Some("Timeout reading from stream"));
        assert!(t0.elapsed() < Duration::from_millis(650));
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(send("POST", &u, "", b"x", Duration::from_secs(10)).err().as_deref(), Some("HTTP response message is too large"));
        t.join().unwrap();
    }
}
