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

/// kms-message's `check_header_value`: CR and LF are refused (no header injection)
pub fn check_header_value(what: &str, v: &str) -> Result<()> {
    if v.contains(['\r', '\n']) {
        return Err(format!("HTTP {what} contains invalid characters"));
    }
    Ok(())
}

/// A JSON string body (without the quotes)
pub fn json_escape(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    for c in v.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
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
        // kms-message formats the key id in unescaped: here it is JSON-escaped, so a key id (from a
        // key document when rewrapping) cannot add fields to the request (safety divergence)
        ("Encrypt", format!("{{\"Plaintext\": \"{b64}\", \"KeyId\": \"{}\"}}", json_escape(cmk.unwrap_or_default())))
    };
    let mut r = AwsRequest::new(datetime, target, payload);
    // kms_request_opt_set_connection_close (not signed)
    r.headers.insert(1, ("Connection".into(), "close".into()));
    r.service = "kms".into();
    if let Some(t) = session_token {
        check_header_value("header value", t).map_err(|e| format!("failed to set session token: {e}"))?;
        r.headers.push(("X-Amz-Security-Token".into(), t.into()));
    }
    if let Some(h) = host_header {
        check_header_value("header value", h).map_err(|e| format!("error constructing KMS message: {e}"))?;
        r.headers.push(("Host".into(), h.into()));
    }
    check_header_value("region", region).map_err(|e| format!("failed to set region: {e}"))?;
    check_header_value("access key id", access_key_id).map_err(|e| format!("failed to set aws access key id: {e}"))?;
    r.region = region.into();
    r.access_key_id = access_key_id.into();
    r.secret_key = secret_access_key.into();
    r.signed().map_err(|e| format!("failed to create KMS message: {e}"))
}

/// `kms_request_to_string` for a request that is not signed (Azure, GCP): the request line, the
/// headers sorted by name (Content-Length added), the body. Every header is checked for CR/LF.
fn plain_request(method: &str, path_and_query: &str, mut headers: Vec<(String, String)>, payload: &str) -> Result<String> {
    if path_and_query.contains([' ', '\r', '\n']) {
        return Err("HTTP path contains invalid characters".into());
    }
    if !headers.iter().any(|(k, _)| k == "Host") {
        return Err("Required Host header not set".into());
    }
    if !payload.is_empty() {
        headers.push(("Content-Length".into(), payload.len().to_string()));
    }
    for (k, v) in &headers {
        check_header_value("header name", k)?;
        check_header_value("header value", v)?;
    }
    headers.sort_by_key(|(k, _)| k.to_ascii_lowercase());
    let mut s = format!("{method} {path_and_query} HTTP/1.1\r\n");
    for (k, v) in &headers {
        s.push_str(k);
        s.push(':');
        s.push_str(v);
        s.push_str("\r\n");
    }
    s.push_str("\r\n");
    s.push_str(payload);
    Ok(s)
}

/// kms-message's `_check_path_segment`: unreserved characters only, at least one alphanumeric
fn check_path_segment(name: &str, value: &str) -> Result<()> {
    if !value.bytes().all(|c| c.is_ascii_alphanumeric() || b"-._~".contains(&c)) {
        return Err(format!("Invalid character in Azure KMS identifier: {name}"));
    }
    if !value.bytes().any(|c| c.is_ascii_alphanumeric()) {
        return Err(format!("Invalid Azure KMS identifier: {name}"));
    }
    Ok(())
}

/// `kms_message_raw_to_b64url` (the padding is kept)
fn b64url(data: &[u8]) -> String {
    base64_encode(data).replace('+', "-").replace('/', "_")
}

/// `kms_azure_request_oauth_new` (`_mongocrypt_kms_ctx_init_azure_auth`): the client credentials
/// token request; `scope` is already %-encoded
pub fn azure_oauth_request(host: &str, scope: &str, tenant_id: &str, client_id: &str, client_secret: &str) -> Result<Vec<u8>> {
    let err = |e: String| format!("error constructing KMS message: {e}");
    check_path_segment("tenantId", tenant_id).map_err(err)?;
    let payload = format!("client_id={client_id}&scope={scope}&client_secret={client_secret}&grant_type=client_credentials");
    let headers = vec![
        ("Connection".into(), "close".into()),
        ("Content-Type".into(), "application/x-www-form-urlencoded".into()),
        ("Host".into(), host.into()),
        ("Accept".into(), "application/json".into()),
    ];
    Ok(plain_request("POST", &format!("/{tenant_id}/oauth2/v2.0/token"), headers, &payload).map_err(err)?.into_bytes())
}

/// `kms_azure_request_wrapkey_new` / `_unwrapkey_new`
pub fn azure_wrap_request(wrap: bool, host: &str, access_token: &str, key_name: &str, key_version: Option<&str>, value: &[u8]) -> Result<Vec<u8>> {
    let op = if wrap { "wrapkey" } else { "unwrapkey" };
    let err = |e: String| format!("error constructing KMS {op} message: {e}");
    check_path_segment("keyName", key_name).map_err(err)?;
    if let Some(v) = key_version.filter(|v| !v.is_empty()) {
        check_path_segment("keyVersion", v).map_err(err)?;
    }
    let path = format!("/keys/{key_name}/{}/{op}?api-version=7.1", key_version.unwrap_or_default());
    let payload = format!("{{\"alg\": \"RSA-OAEP-256\", \"value\": \"{}\"}}", b64url(value));
    let headers = vec![
        ("Connection".into(), "close".into()),
        ("Authorization".into(), format!("Bearer {access_token}")),
        ("Content-Type".into(), "application/json".into()),
        ("Host".into(), host.into()),
        ("Accept".into(), "application/json".into()),
    ];
    Ok(plain_request("POST", &path, headers, &payload).map_err(err)?.into_bytes())
}

/// The JSON body of an OAuth or Azure response (`_ctx_done_oauth`, `_ctx_done_azure_wrapkey_unwrapkey`)
fn json_body(r: &Response) -> Result<Vec<u8>> {
    let body = String::from_utf8_lossy(&r.body);
    if r.body.is_empty() {
        return Err(format!("Empty KMS response. HTTP status={}", r.status));
    }
    let doc = mongo_core::bson::json_parse::from_json(&r.body)
        .map_err(|e| format!("Error parsing JSON in KMS response '{e}'. HTTP status={}. Response body=\n{body}", r.status))?;
    if r.status != 200 {
        return Err(non200(r.status, &r.body));
    }
    Ok(doc)
}

/// `_ctx_done_oauth` + `mc_mapof_kmsid_to_token_add_response`: the access token and how long it
/// may be cached
pub fn oauth_done(r: &Response) -> Result<(String, std::time::Duration)> {
    use mongo_core::bson::iter::Value;
    let doc = json_body(r)?;
    if !matches!(crate::opts::find(&doc, "access_token"), Some(Value::Utf8(_))) {
        return Err(format!(
            "Invalid KMS response. KMS JSON response does not include field 'access_token'. HTTP status={}. Response body=\n{}",
            r.status,
            String::from_utf8_lossy(&r.body)
        ));
    }
    let expires_in = match crate::opts::find(&doc, "expires_in") {
        Some(Value::Int32(n)) => n as i64,
        Some(Value::Int64(n)) => n,
        _ => return Err("OAuth response invalid, no 'expires_in' field.".into()),
    };
    let Some(Value::Utf8(token)) = crate::opts::find(&doc, "access_token") else { unreachable!() };
    // MONGOCRYPT_OAUTH_CACHE_EVICTION_PERIOD_US: tokens are dropped 5 s before they expire
    // (libmongocrypt aborts on an expires_in that overflows; here it saturates)
    let ttl = std::time::Duration::from_secs(expires_in.max(0) as u64).saturating_sub(std::time::Duration::from_secs(5));
    Ok((String::from_utf8_lossy(token).into_owned(), ttl))
}

/// `_ctx_done_azure_wrapkey_unwrapkey`: the base64url-decoded `value`
pub fn azure_done(r: &Response) -> Result<Vec<u8>> {
    let doc = json_body(r)?;
    let body = String::from_utf8_lossy(&r.body);
    let Some(mongo_core::bson::iter::Value::Utf8(v)) = crate::opts::find(&doc, "value") else {
        return Err(format!("KMS JSON response does not include field 'value'. HTTP status={}. Response body=\n{body}", r.status));
    };
    let mut b64: Vec<u8> = v.iter().map(|&c| match c {
        b'-' => b'+',
        b'_' => b'/',
        c => c,
    }).collect();
    while b64.len() % 4 != 0 {
        b64.push(b'=');
    }
    base64_decode(&b64).ok_or_else(|| format!("Failed to base64 decode response. HTTP status={}. Response body=\n{body}", r.status))
}

/// A complete HTTP response
pub struct Response {
    pub status: i32,
    pub body: Vec<u8>,
}

/// The most response bytes accepted (libmongocrypt bounds the JSON body it parses)
pub const MAX_RESPONSE: usize = 16 * 1024 * 1024;

/// The longest chunk-size line accepted (a size and its extensions)
const MAX_CHUNK_LINE: usize = 1024;

enum Body {
    Length(usize),
    /// the next chunk-size line starts at `pos`
    Chunked { pos: usize, body: Vec<u8> },
}

/// `kms_response_parser_t`: an incremental HTTP/1.1 response parser. Each byte is scanned a bounded
/// number of times, and every length is checked against `MAX_RESPONSE`.
pub struct ResponseParser {
    buf: Vec<u8>,
    /// where the search for the end of the headers resumes
    scan: usize,
    /// (status, where the body starts, how it is delimited)
    head: Option<(i32, usize, Body)>,
    done: bool,
    error: Option<String>,
}

impl Default for ResponseParser {
    fn default() -> Self {
        Self::new()
    }
}

impl ResponseParser {
    pub fn new() -> ResponseParser {
        ResponseParser { buf: Vec::new(), scan: 0, head: None, done: false, error: None }
    }

    /// Adds bytes read; true once the response is complete or malformed (no more bytes are wanted)
    pub fn feed(&mut self, data: &[u8]) -> bool {
        if self.done || self.error.is_some() {
            return true;
        }
        match self.step(data) {
            Ok(d) => self.done = d,
            Err(e) => self.error = Some(e),
        }
        self.done || self.error.is_some()
    }

    fn step(&mut self, data: &[u8]) -> Result<bool> {
        if self.buf.len().saturating_add(data.len()) > MAX_RESPONSE {
            return Err("KMS response exceeds the maximum supported length".into());
        }
        self.buf.extend_from_slice(data);
        if self.head.is_none() {
            let from = self.scan.saturating_sub(3);
            let Some(he) = self.buf[from..].windows(4).position(|w| w == b"\r\n\r\n").map(|x| x + from) else {
                self.scan = self.buf.len();
                return Ok(false);
            };
            self.head = Some(Self::parse_head(&self.buf[..he], he + 4)?);
        }
        let len = self.buf.len();
        let Some((_, start, body)) = self.head.as_mut() else { return Ok(false) };
        match body {
            Body::Length(n) => Ok(len - *start >= *n),
            Body::Chunked { pos, body } => loop {
                let line = &self.buf[*pos..];
                let Some(e) = line.windows(2).take(MAX_CHUNK_LINE).position(|w| w == b"\r\n") else {
                    if line.len() > MAX_CHUNK_LINE {
                        return Err("Could not parse chunk length".into());
                    }
                    return Ok(false);
                };
                let size = std::str::from_utf8(&line[..e]).ok().map(|x| x.split(';').next().unwrap_or_default().trim());
                let n = size.and_then(|x| usize::from_str_radix(x, 16).ok()).ok_or("Could not parse chunk length")?;
                if n > MAX_RESPONSE - body.len() {
                    return Err("KMS response exceeds the maximum supported length".into());
                }
                // the chunk data starts after the size line and ends with CRLF
                let data_start = *pos + e + 2;
                let end = data_start + n + 2;
                if len < end {
                    return Ok(false);
                }
                if &self.buf[end - 2..end] != b"\r\n" {
                    return Err("Could not parse chunk: missing CRLF".into());
                }
                if n == 0 {
                    return Ok(true);
                }
                body.extend_from_slice(&self.buf[data_start..data_start + n]);
                *pos = end;
            },
        }
    }

    fn parse_head(head: &[u8], start: usize) -> Result<(i32, usize, Body)> {
        let head = std::str::from_utf8(head).map_err(|_| "Could not parse HTTP status line")?;
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
        if chunked {
            return Ok((status, start, Body::Chunked { pos: start, body: Vec::new() }));
        }
        let n = len.unwrap_or(0);
        if n > MAX_RESPONSE {
            return Err("KMS response exceeds the maximum supported length".into());
        }
        Ok((status, start, Body::Length(n)))
    }

    /// The complete response (Err: malformed, or the connection ended early)
    pub fn finish(self) -> Result<Response> {
        if let Some(e) = self.error {
            let status = self.head.as_ref().map_or(0, |h| h.0);
            return Err(format!("KMS response parser error with status {status}, error: '{e}'"));
        }
        match (self.done, self.head) {
            (true, Some((status, _, Body::Chunked { body, .. }))) => Ok(Response { status, body }),
            (true, Some((status, start, Body::Length(n)))) => Ok(Response { status, body: self.buf[start..start + n].to_vec() }),
            _ => Err("Failed to get response from parser: incomplete response".into()),
        }
    }
}

/// The whole of `buf` through a `ResponseParser`: Ok(None) when more bytes are needed
pub fn parse_response(buf: &[u8]) -> Result<Option<Response>> {
    let mut p = ResponseParser::new();
    if !p.feed(buf) {
        return Ok(None);
    }
    if let Some(e) = p.error {
        return Err(e);
    }
    p.finish().map(Some)
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
        // chunk sizes cannot overflow, chunks end with CRLF, chunk lines are bounded
        assert!(parse_response(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nffffffffffffffff\r\nabc").is_err());
        assert!(parse_response(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabcXY0\r\n\r\n").is_err());
        let mut long = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
        long.extend(std::iter::repeat_n(b'0', 2000));
        assert!(parse_response(&long).is_err());
        // byte-at-a-time feeding
        let whole = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n";
        let mut p = ResponseParser::new();
        let mut fed = 0;
        for b in whole.iter() {
            fed += 1;
            if p.feed(std::slice::from_ref(b)) {
                break;
            }
        }
        assert_eq!(fed, whole.len());
        assert_eq!(p.finish().unwrap().body, b"abcde");
        let mut p = ResponseParser::new();
        assert!(p.feed(b"HTTP/1.0 200 OK\r\n\r\n"));
        assert_eq!(p.finish().err().unwrap(), "KMS response parser error with status 0, error: 'Could not parse HTTP-version: HTTP/1.0 200 OK'");
    }
}
