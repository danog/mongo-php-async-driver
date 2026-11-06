//! Remote KMS requests and responses (libmongocrypt's `mongocrypt-kms-ctx.c` and kms-message):
//! AWS KMS with Signature Version 4 (`kms_request.c`), the HTTP/1.1 response parser
//! (`kms_response_parser.c`) and the JSON response handling. Messages are libmongocrypt's.

use crate::crypto::sha256;
use mongo_core::bson::fmt::{base64_decode, base64_encode};

pub type Result<T> = std::result::Result<T, String>;

/// One KMS round trip for the driver: where to connect and what to send
#[derive(Clone, Debug)]
pub struct KmsRequest {
    /// the KMS provider id (`aws`, `aws:name`, ...)
    pub kmsid: String,
    /// "host:port"
    pub endpoint: String,
    pub message: Vec<u8>,
    /// the provider's TLS options
    pub tls: mongo_core::tls::TlsOptions,
}

/// `kms_request_str_append_stripped`: leading spaces removed, runs of spaces collapsed, newlines
/// as commas, trailing spaces removed
fn stripped(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    let mut space = false;
    let mut comma = false;
    for c in v.trim_start().chars() {
        if c == '\n' {
            comma = true;
            space = false;
        } else if c.is_ascii_whitespace() {
            space = true;
        } else {
            if comma {
                out.push(',');
                comma = false;
                space = false;
            } else if space {
                out.push(' ');
                space = false;
            }
            out.push(c);
        }
    }
    out
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// An AWS KMS request (`kms_request_t` for provider AWS, method POST, path "/")
pub struct AwsRequest {
    pub region: String,
    pub service: String,
    pub access_key_id: String,
    pub secret_key: String,
    /// "YYYYmmDDTHHMMSSZ"
    pub datetime: String,
    /// in insertion order
    pub headers: Vec<(String, String)>,
    pub payload: String,
}

impl AwsRequest {
    pub fn new(datetime: &str, target: &str, payload: String) -> AwsRequest {
        AwsRequest {
            region: String::new(),
            service: String::new(),
            access_key_id: String::new(),
            secret_key: String::new(),
            datetime: datetime.to_string(),
            headers: vec![
                ("X-Amz-Date".into(), datetime.into()),
                ("Content-Type".into(), "application/x-amz-json-1.1".into()),
                ("X-Amz-Target".into(), format!("TrentService.{target}")),
            ],
            payload,
        }
    }

    /// `finalize`: the default Host header and Content-Length
    fn finalize(&mut self) {
        if !self.headers.iter().any(|(k, _)| k == "Host") {
            self.headers.push(("Host".into(), format!("{}.{}.amazonaws.com", self.service, self.region)));
        }
        if !self.headers.iter().any(|(k, _)| k == "Content-Length") && !self.payload.is_empty() {
            self.headers.push(("Content-Length".into(), self.payload.len().to_string()));
        }
    }

    /// Headers sorted by name, case-insensitively (stable for duplicates)
    fn sorted(&self) -> Vec<(String, String)> {
        let mut h = self.headers.clone();
        h.sort_by_key(|(k, _)| k.to_ascii_lowercase());
        h
    }

    fn signed_headers(&self) -> String {
        let mut names: Vec<String> = Vec::new();
        for (k, _) in self.sorted() {
            let l = k.to_ascii_lowercase();
            if l == "connection" || names.last() == Some(&l) {
                continue;
            }
            names.push(l);
        }
        names.join(";")
    }

    /// `kms_request_get_canonical`
    pub fn canonical(&mut self) -> String {
        self.finalize();
        let mut s = String::from("POST\n/\n\n");
        let mut prev: Option<String> = None;
        let mut first = true;
        for (k, v) in self.sorted().into_iter().filter(|(k, _)| !k.eq_ignore_ascii_case("connection")) {
            let l = k.to_ascii_lowercase();
            if prev.as_deref() == Some(&l) {
                s.push(',');
                s.push_str(&stripped(&v));
                continue;
            }
            if !first {
                s.push('\n');
            }
            first = false;
            s.push_str(&l);
            s.push(':');
            s.push_str(&stripped(&v));
            prev = Some(l);
        }
        s.push('\n');
        s.push('\n');
        s.push_str(&self.signed_headers());
        s.push('\n');
        s.push_str(&hex(&sha256(self.payload.as_bytes())));
        s
    }

    fn signature(&mut self) -> String {
        let creq = self.canonical();
        let date = &self.datetime[..8];
        let sts = format!(
            "AWS4-HMAC-SHA256\n{}\n{date}/{}/{}/aws4_request\n{}",
            self.datetime,
            self.region,
            self.service,
            hex(&sha256(creq.as_bytes()))
        );
        let h = |k: &[u8], d: &str| crate::crypto::hmac_sha256(k, &[d.as_bytes()]);
        let k_date = h(format!("AWS4{}", self.secret_key).as_bytes(), date);
        let k_region = h(&k_date, &self.region);
        let k_service = h(&k_region, &self.service);
        let k_signing = h(&k_service, "aws4_request");
        let sig = h(&k_signing, &sts);
        format!(
            "AWS4-HMAC-SHA256 Credential={}/{date}/{}/{}/aws4_request, SignedHeaders={}, Signature={}",
            self.access_key_id,
            self.region,
            self.service,
            self.signed_headers(),
            hex(&sig)
        )
    }

    /// `kms_request_get_signed`: the HTTP request
    pub fn signed(&mut self) -> Result<Vec<u8>> {
        if self.region.is_empty() {
            return Err("Region not set".into());
        }
        if self.access_key_id.is_empty() {
            return Err("Access key ID not set".into());
        }
        if self.secret_key.is_empty() {
            return Err("Secret key not set".into());
        }
        self.finalize();
        let mut s = String::from("POST / HTTP/1.1\r\n");
        for (k, v) in self.sorted() {
            s.push_str(&k);
            s.push(':');
            s.push_str(&v);
            s.push_str("\r\n");
        }
        let auth = self.signature();
        s.push_str("Authorization: ");
        s.push_str(&auth);
        if !self.payload.is_empty() {
            s.push_str("\r\n\r\n");
            s.push_str(&self.payload);
        }
        Ok(s.into_bytes())
    }
}

/// The current time as "YYYYmmDDTHHMMSSZ"
pub fn amz_datetime() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let (y, mo, d) = mongo_core::bson::fmt::civil_from_days(secs.div_euclid(86400));
    let t = secs.rem_euclid(86400);
    format!("{y:04}{mo:02}{d:02}T{:02}{:02}{:02}Z", t / 3600, t % 3600 / 60, t % 60)
}

/// `kms_decrypt_request_new` / `kms_encrypt_request_new` + the credentials (`_mongocrypt_kms_ctx_init_aws_*`)
#[allow(clippy::too_many_arguments)]
pub fn aws_request(
    decrypt: bool,
    data: &[u8],
    cmk: Option<&str>,
    region: &str,
    access_key_id: &str,
    secret_access_key: &str,
    session_token: Option<&str>,
    host_header: Option<&str>,
    datetime: &str,
) -> Result<Vec<u8>> {
    let b64 = base64_encode(data);
    let (target, payload) = if decrypt {
        ("Decrypt", format!("{{\"CiphertextBlob\": \"{b64}\"}}"))
    } else {
        ("Encrypt", format!("{{\"Plaintext\": \"{b64}\", \"KeyId\": \"{}\"}}", cmk.unwrap_or_default()))
    };
    let mut r = AwsRequest::new(datetime, target, payload);
    // kms_request_opt_set_connection_close (not signed)
    r.headers.insert(1, ("Connection".into(), "close".into()));
    r.service = "kms".into();
    if let Some(t) = session_token {
        r.headers.push(("X-Amz-Security-Token".into(), t.into()));
    }
    if let Some(h) = host_header {
        r.headers.push(("Host".into(), h.into()));
    }
    r.region = region.into();
    r.access_key_id = access_key_id.into();
    r.secret_key = secret_access_key.into();
    r.signed().map_err(|e| format!("failed to create KMS message: {e}"))
}

/// A complete HTTP response
pub struct Response {
    pub status: i32,
    pub body: Vec<u8>,
}

/// The most response bytes accepted (libmongocrypt bounds the JSON body it parses)
pub const MAX_RESPONSE: usize = 16 * 1024 * 1024;

/// `kms_response_parser_feed` run over what was read so far: Ok(Some) once the response is
/// complete, Ok(None) when more bytes are needed
pub fn parse_response(buf: &[u8]) -> Result<Option<Response>> {
    if buf.len() > MAX_RESPONSE {
        return Err("KMS response exceeds the maximum supported length".into());
    }
    let Some(he) = buf.windows(4).position(|w| w == b"\r\n\r\n") else { return Ok(None) };
    let head = std::str::from_utf8(&buf[..he]).map_err(|_| "Could not parse HTTP status line")?;
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let mut parts = status_line.splitn(3, ' ');
    if parts.next() != Some("HTTP/1.1") {
        return Err(format!("Could not parse HTTP-version: {status_line}"));
    }
    let status: i32 = parts.next().and_then(|s| s.parse().ok()).ok_or("Could not parse Status-Code")?;
    let (mut len, mut chunked) = (None, false);
    for l in lines {
        let Some((k, v)) = l.split_once(':') else { return Err(format!("Could not parse header, no colon found: {l}")) };
        let v = v.trim();
        if k == "Content-Length" && len.is_none() {
            len = Some(v.parse::<usize>().map_err(|_| "Could not parse Content-Length header.")?);
        }
        if k == "Transfer-Encoding" {
            if v == "chunked" {
                chunked = true;
            } else {
                return Err(format!("Unsupported Transfer-Encoding: {v}"));
            }
        }
    }
    let rest = &buf[he + 4..];
    if chunked {
        let mut body = Vec::new();
        let mut p = 0;
        loop {
            let Some(e) = rest[p..].windows(2).position(|w| w == b"\r\n") else { return Ok(None) };
            let n = usize::from_str_radix(std::str::from_utf8(&rest[p..p + e]).unwrap_or("x").trim(), 16)
                .map_err(|_| "Could not parse chunk length")?;
            p += e + 2;
            if rest.len() < p + n + 2 {
                return Ok(None);
            }
            if n == 0 {
                return Ok(Some(Response { status, body }));
            }
            body.extend_from_slice(&rest[p..p + n]);
            p += n + 2;
        }
    }
    let n = len.unwrap_or(0);
    if n > MAX_RESPONSE {
        return Err("KMS response exceeds the maximum supported length".into());
    }
    if rest.len() < n {
        return Ok(None);
    }
    Ok(Some(Response { status, body: rest[..n].to_vec() }))
}

/// `_handle_non200_http_status`
pub fn non200(status: i32, body: &[u8]) -> String {
    if status <= 0 || status >= 1000 {
        return format!(
            "Unsupported HTTP code in KMS response. HTTP status={status}. Response body=\n{}",
            String::from_utf8_lossy(body)
        );
    }
    if body.is_empty() {
        return format!("Error in KMS response. HTTP status={status}. Empty body.");
    }
    format!("Error in KMS response. HTTP status={status}. Response body=\n{}", String::from_utf8_lossy(body))
}

/// `_ctx_done_aws`: the base64-decoded `field` of the JSON body
pub fn aws_done(r: &Response, field: &str) -> Result<Vec<u8>> {
    let body = &r.body;
    if r.status != 200 {
        return Err(non200(r.status, body));
    }
    let doc = mongo_core::bson::json_parse::from_json(body).map_err(|e| {
        format!(
            "Error parsing JSON in KMS response '{e}'. HTTP status={}. Response body=\n{}",
            r.status,
            String::from_utf8_lossy(body)
        )
    })?;
    match crate::opts::find(&doc, field) {
        Some(mongo_core::bson::iter::Value::Utf8(s)) => base64_decode(s).ok_or_else(|| {
            format!("Failed to base64 decode response. HTTP status={}. Response body=\n{}", r.status, String::from_utf8_lossy(body))
        }),
        _ => Err(format!(
            "KMS JSON response does not include field '{field}'. HTTP status={}. Response body=\n{}",
            r.status,
            String::from_utf8_lossy(body)
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(format!("{}/tests/data/kms/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
    }

    #[test]
    fn sigv4_like_kms_message() {
        // kms-message's test/decrypt and test/encrypt: date 20150830T123600Z, service "service"
        for (name, decrypt) in [("decrypt", true), ("encrypt", false)] {
            let want = fixture(&format!("{name}.sreq"));
            let want = String::from_utf8(want).unwrap().replace('\n', "\r\n");
            let body = want.split("\r\n\r\n").nth(1).unwrap().trim_end().to_string();
            let mut r = AwsRequest::new("20150830T123600Z", if decrypt { "Decrypt" } else { "Encrypt" }, body);
            r.service = "service".into();
            r.region = "us-east-1".into();
            r.access_key_id = "AKIDEXAMPLE".into();
            r.secret_key = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into();
            let got = String::from_utf8(r.signed().unwrap()).unwrap();
            assert_eq!(got.trim_end(), want.trim_end(), "{name}");
        }
    }

    #[test]
    fn responses() {
        let r = parse_response(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello").unwrap().unwrap();
        assert_eq!((r.status, &r.body[..]), (200, &b"hello"[..]));
        assert!(parse_response(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhel").unwrap().is_none());
        let r = parse_response(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n").unwrap().unwrap();
        assert_eq!(r.body, b"abcde");
        assert!(parse_response(b"HTTP/1.1 200 OK\r\nContent-Length: 99999999999\r\n\r\n").is_err());
    }
}
