//! Credentials from the environment and cloud metadata services: libmongoc's
//! `_mongoc_aws_credentials_obtain` (environment, AssumeRoleWithWebIdentity, ECS, EC2 IMDSv2,
//! with the process-wide cache), `mcd_azure_access_token_from_imds` and
//! `gcp_access_token_from_gcp_server`. Messages, timeouts and request shapes are libmongoc's.
//! The metadata services are reached over plain HTTP at their link-local addresses, as libmongoc.

use crate::bson::iter::{Iter, Value};
use crate::error::{code, domain, Error};
use crate::http::{self, Url};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// MONGOC_ERROR_KMS_SERVER_HTTP / _BAD_JSON
const KMS_SERVER_HTTP: u32 = 64;
const KMS_SERVER_BAD_JSON: u32 = 65;

/// The hosts that are asked, (host, port); the defaults are libmongoc's (tests use local ones)
pub struct Hosts {
    pub ecs: (String, u16),
    pub ec2: (String, u16),
    /// (host, port, TLS options) of STS
    pub sts: Option<(String, u16, crate::tls::TlsOptions)>,
    pub azure: (String, u16),
    pub gcp: (String, u16),
}

impl Default for Hosts {
    fn default() -> Hosts {
        Hosts {
            ecs: ("169.254.170.2".into(), 80),
            ec2: ("169.254.169.254".into(), 80),
            sts: None,
            azure: ("169.254.169.254".into(), 80),
            gcp: ("metadata.google.internal".into(), 80),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AwsCredentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
    /// when the credentials stop being used (their expiration less five minutes)
    pub expires: Option<Instant>,
}

fn auth_err(m: impl Into<String>) -> Error {
    Error::new(domain::CLIENT, code::CLIENT_AUTHENTICATE, m)
}

/// `_mongoc_getenv`
fn env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// `_validate_and_set_creds`: Ok(None) when nothing is set
fn validate(akid: Option<&str>, secret: Option<&str>, token: Option<&str>) -> Result<Option<AwsCredentials>, Error> {
    let has = |v: Option<&str>| v.is_some_and(|x| !x.is_empty());
    match (has(akid), has(secret)) {
        (true, false) => return Err(auth_err("ACCESS_KEY_ID is set, but SECRET_ACCESS_KEY is missing")),
        (false, true) => return Err(auth_err("SECRET_ACCESS_KEY is set, but ACCESS_KEY_ID is missing")),
        (false, false) if has(token) => {
            return Err(auth_err("AWS_SESSION_TOKEN is set, but ACCESS_KEY_ID and SECRET_ACCESS_KEY are missing"));
        }
        (false, false) => return Ok(None),
        (true, true) => {}
    }
    Ok(Some(AwsCredentials {
        access_key_id: akid.unwrap_or_default().to_owned(),
        secret_access_key: secret.unwrap_or_default().to_owned(),
        session_token: token.map(str::to_owned),
        expires: None,
    }))
}

/// MONGOC_AWS_CREDENTIALS_EXPIRATION_WINDOW
const EXPIRATION_WINDOW: Duration = Duration::from_secs(5 * 60);

/// `expiration_ms_to_timer`: milliseconds since the epoch to the instant the credentials expire
/// (less the window); already expired times give `now`
fn expires_at(expiration_ms: i64) -> Instant {
    let now_ms = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64);
    let left = Duration::from_millis(expiration_ms.saturating_sub(now_ms).max(0) as u64).saturating_sub(EXPIRATION_WINDOW);
    Instant::now() + left
}

/// `expiration_iso8601_to_timer`
fn expires_iso8601(s: &str) -> Result<Instant, Error> {
    let json = format!("{{\"Expiration\" : {{\"$date\" : \"{}\"}}}}", s.replace(['"', '\\'], ""));
    let doc = crate::bson::json_parse::from_json(json.as_bytes()).map_err(|e| auth_err(format!("failed to parse Expiration: {e}")))?;
    match Iter::new(&doc).and_then(|mut i| i.next().then(|| i.value())) {
        Some(Value::DateTime(ms)) => Ok(expires_at(ms)),
        _ => Err(auth_err("failed to parse Expiration")),
    }
}

/// A UTF-8 field of a JSON object, matched ASCII case-insensitively (`bson_iter_init_find_case`)
fn utf8_case(doc: &[u8], key: &str) -> Option<String> {
    let mut it = Iter::new(doc)?;
    if !it.find_case(key.as_bytes()) {
        return None;
    }
    match it.value() {
        Value::Utf8(v) => Some(String::from_utf8_lossy(v).into_owned()),
        _ => None,
    }
}

/// `_send_http_request` over plain HTTP: 10 s for the whole exchange
fn get(host: &(String, u16), method: &str, path: &str, headers: &str) -> Result<http::Response, String> {
    check_path(path)?;
    http::send(method, &Url { host: host.0.clone(), port: host.1, path: path.to_owned() }, headers, &[], Duration::from_secs(10))
}

/// Request targets are visible ASCII only, so no value (an environment variable, a token file, a
/// metadata reply) can add request lines
fn check_path(path: &str) -> Result<(), String> {
    if path.is_empty() || !path.bytes().all(|b| b.is_ascii_graphic()) {
        return Err("invalid characters in the HTTP request path".into());
    }
    Ok(())
}

/// ECS/EC2 credentials JSON (`_obtain_creds_from_ecs` / `_from_ec2`)
fn creds_from_json(r: &http::Response, what: &str) -> Result<Option<AwsCredentials>, Error> {
    let doc = crate::bson::json_parse::from_json(&r.body).map_err(|_| auth_err(format!("invalid JSON in {what} response. Response headers: {}", r.headers)))?;
    let expires = match utf8_case(&doc, "Expiration") {
        Some(e) => Some(expires_iso8601(&e)?),
        None => None,
    };
    let akid = utf8_case(&doc, "AccessKeyId");
    let secret = utf8_case(&doc, "SecretAccessKey");
    let token = utf8_case(&doc, "Token");
    Ok(validate(akid.as_deref(), secret.as_deref(), token.as_deref())?.map(|c| AwsCredentials { expires, ..c }))
}

fn from_ecs(hosts: &Hosts) -> Result<Option<AwsCredentials>, Error> {
    let Some(uri) = env("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI").filter(|u| !u.is_empty()) else { return Ok(None) };
    let r = get(&hosts.ecs, "GET", &uri, "").map_err(|e| auth_err(format!("failed to contact ECS link local server: {e}")))?;
    creds_from_json(&r, "ECS")
}

fn from_ec2(hosts: &Hosts) -> Result<Option<AwsCredentials>, Error> {
    let fail = |e: String| auth_err(format!("failed to contact EC2 link local server: {e}"));
    let r = get(&hosts.ec2, "PUT", "/latest/api/token", "X-aws-ec2-metadata-token-ttl-seconds: 30\r\n").map_err(fail)?;
    if r.body.is_empty() {
        return Err(auth_err(format!("unable to retrieve token from EC2 metadata. Headers: {}", r.headers)));
    }
    let token = String::from_utf8_lossy(&r.body).trim_end().to_owned();
    if !token.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(auth_err("unable to retrieve token from EC2 metadata: invalid characters"));
    }
    let header = format!("X-aws-ec2-metadata-token: {token}\r\n");
    let r = get(&hosts.ec2, "GET", "/latest/meta-data/iam/security-credentials/", &header).map_err(fail)?;
    if r.body.is_empty() {
        return Err(auth_err(format!("unable to retrieve role_name from EC2 metadata. Headers: {}", r.headers)));
    }
    let role = String::from_utf8_lossy(&r.body).trim_end().to_owned();
    let r = get(&hosts.ec2, "GET", &format!("/latest/meta-data/iam/security-credentials/{role}"), &header).map_err(fail)?;
    creds_from_json(&r, "EC2")
}

/// `_mongoc_get_aws_sts_endpoint`
fn sts_endpoint() -> String {
    match (env("AWS_STS_REGIONAL_ENDPOINTS"), env("AWS_REGION")) {
        (Some(r), Some(region)) if r == "regional" && !region.is_empty() => format!("sts.{region}.amazonaws.com"),
        _ => "sts.amazonaws.com".into(),
    }
}

/// The largest web identity token file read
const MAX_TOKEN_FILE: u64 = 1024 * 1024;

fn from_web_identity(hosts: &Hosts) -> Result<Option<AwsCredentials>, Error> {
    let (Some(file), Some(arn)) = (env("AWS_WEB_IDENTITY_TOKEN_FILE"), env("AWS_ROLE_ARN")) else { return Ok(None) };
    if file.is_empty() || arn.is_empty() {
        return Ok(None);
    }
    let session = match env("AWS_ROLE_SESSION_NAME") {
        Some(s) => s,
        None => {
            let mut b = [0u8; 16];
            getrandom_fill(&mut b).map_err(|_| auth_err("unable to generate random bytes for AWS_ROLE_SESSION_NAME"))?;
            b.iter().map(|x| format!("{x:02x}")).collect()
        }
    };
    let jwt = {
        use std::io::Read;
        let f = std::fs::File::open(&file).map_err(|e| auth_err(format!("failed to open AWS_WEB_IDENTITY_TOKEN_FILE: {file}. Reason: {e}")))?;
        let mut s = Vec::new();
        f.take(MAX_TOKEN_FILE).read_to_end(&mut s).map_err(|e| auth_err(format!("failed to read AWS_WEB_IDENTITY_TOKEN_FILE: {file}. Reason: {e}")))?;
        String::from_utf8_lossy(&s).trim_end().to_owned()
    };
    let path = format!("/?Action=AssumeRoleWithWebIdentity&RoleSessionName={session}&RoleArn={arn}&WebIdentityToken={jwt}&Version=2011-06-15");
    let (host, port, tls) = match &hosts.sts {
        Some((h, p, t)) => (h.clone(), *p, t.clone()),
        None => (sts_endpoint(), 443, crate::tls::TlsOptions::default()),
    };
    let fail = |e: String| auth_err(format!("failed to contact {host}: {e}"));
    check_path(&path).map_err(fail)?;
    let url = Url { host: host.clone(), port, path };
    let msg = http::request_head("POST", &url, "Accept: application/json\r\n", 0);
    let mut buf = Vec::new();
    let mut too_big = false;
    let res = crate::kms_tls::exchange_timeout(&format!("{host}:{port}"), &tls, msg.as_bytes(), &mut |b: &[u8]| {
        if b.is_empty() {
            return true;
        }
        if buf.len() + b.len() > 8 * 1024 * 1024 {
            too_big = true;
            return true;
        }
        buf.extend_from_slice(b);
        false
    }, Duration::from_secs(10));
    res.map_err(|(_, e)| fail(e.message))?;
    if too_big {
        return Err(fail("HTTP response message is too large".into()));
    }
    let r = http::parse_response(&buf).map_err(fail)?;
    let doc = crate::bson::json_parse::from_json(&r.body)
        .map_err(|_| auth_err(format!("invalid JSON in response from {host}. Response headers: {}", r.headers)))?;
    if let Some(mut it) = Iter::new(&doc) {
        if it.find(b"Error") {
            let json = match it.value() {
                Value::Document(d) => crate::bson::json::to_json(d, crate::bson::json::Mode::Relaxed, false).map(|j| String::from_utf8_lossy(&j).into_owned()).unwrap_or_default(),
                _ => String::new(),
            };
            return Err(auth_err(format!("Response to AssumeRoleWithWebIdentity contains 'Error': {json}")));
        }
    }
    let path = "AssumeRoleWithWebIdentityResponse.AssumeRoleWithWebIdentityResult.Credentials";
    let mut creds: &[u8] = &doc;
    for k in path.split('.') {
        let mut it = Iter::new(creds).ok_or_else(|| auth_err(format!("did not find {path} in response from {host}.")))?;
        match it.find(k.as_bytes()).then(|| it.value()) {
            Some(Value::Document(d)) => creds = d,
            _ => return Err(auth_err(format!("did not find {path} in response from {host}."))),
        }
    }
    let field = |k: &str| -> Result<String, Error> {
        let mut it = Iter::new(creds).ok_or_else(|| auth_err(format!("did not find {k} in response from {host}")))?;
        match it.find(k.as_bytes()).then(|| it.value()) {
            Some(Value::Utf8(v)) => Ok(String::from_utf8_lossy(v).into_owned()),
            _ => Err(auth_err(format!("did not find {k} in response from {host}"))),
        }
    };
    let akid = field("AccessKeyId")?;
    let secret = field("SecretAccessKey")?;
    let token = field("SessionToken")?;
    let expires = {
        let mut it = Iter::new(creds).ok_or_else(|| auth_err(format!("did not find Expiration in response from {host}")))?;
        match it.find(b"Expiration").then(|| it.value()) {
            Some(Value::Double(d)) => expires_at((1000.0 * d) as i64),
            _ => return Err(auth_err(format!("did not find Expiration in response from {host}"))),
        }
    };
    Ok(validate(Some(&akid), Some(&secret), Some(&token))?.map(|c| AwsCredentials { expires: Some(expires), ..c }))
}

fn getrandom_fill(b: &mut [u8]) -> Result<(), ()> {
    getrandom::fill(b).map_err(|_| ())
}

/// `mongoc_aws_credentials_cache`
static AWS_CACHE: Mutex<Option<AwsCredentials>> = Mutex::new(None);

fn cached() -> Option<AwsCredentials> {
    let c = AWS_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    c.as_ref().filter(|c| c.expires.is_some_and(|e| Instant::now() < e)).cloned()
}

/// `_mongoc_aws_credentials_obtain` without a URI: the cache, the environment, web identity,
/// ECS, then EC2 (credentials with an expiration are cached for the process)
pub fn aws_credentials(hosts: &Hosts) -> Result<AwsCredentials, Error> {
    if let Some(c) = cached() {
        return Ok(c);
    }
    let (akid, secret, token) = (env("AWS_ACCESS_KEY_ID"), env("AWS_SECRET_ACCESS_KEY"), env("AWS_SESSION_TOKEN"));
    if let Some(c) = validate(akid.as_deref(), secret.as_deref(), token.as_deref())? {
        return Ok(c);
    }
    // the remote sources run under the cache lock, as libmongoc's
    let mut cache = AWS_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(c) = cache.as_ref().filter(|c| c.expires.is_some_and(|e| Instant::now() < e)) {
        return Ok(c.clone());
    }
    for source in [from_web_identity, from_ecs, from_ec2] {
        if let Some(c) = source(hosts)? {
            if c.expires.is_some() {
                *cache = Some(c.clone());
            }
            return Ok(c);
        }
    }
    Err(auth_err("unable to get credentials\n"))
}

/// `_mongoc_http_send` failures (MONGOC_ERROR_STREAM_SOCKET)
fn stream_socket(m: String) -> Error {
    Error::new(domain::STREAM, code::STREAM_SOCKET, m)
}

/// `mongoc_percent_encode`: everything but unreserved characters as %XX (upper-case hex)
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// A UTF-8 field of a JSON object (`bson_iter_init_find` + `bson_iter_utf8`)
fn utf8(doc: &[u8], key: &str) -> Option<String> {
    let mut it = Iter::new(doc)?;
    match it.find(key.as_bytes()).then(|| it.value()) {
        Some(Value::Utf8(v)) => Some(String::from_utf8_lossy(v).into_owned()),
        _ => None,
    }
}

/// `mcd_azure_access_token_from_imds` for `https://vault.azure.net`: the token and its lifetime
pub fn azure_imds_token(hosts: &Hosts) -> Result<(String, Duration), Error> {
    let e = |c: u32, m: String| Error::new(domain::AZURE, c, m);
    let path = format!("/metadata/identity/oauth2/token?api-version=2018-02-01&resource={}", percent_encode("https://vault.azure.net"));
    let url = Url { host: hosts.azure.0.clone(), port: hosts.azure.1, path };
    let r = http::send("GET", &url, "Metadata: true\r\nAccept: application/json\r\n", &[], Duration::from_secs(3)).map_err(stream_socket)?;
    let body = String::from_utf8_lossy(&r.body).into_owned();
    if r.status != 200 {
        return Err(e(KMS_SERVER_HTTP, format!("Error from Azure IMDS server while looking for Managed Identity access token: {body}")));
    }
    let doc = crate::bson::json_parse::from_json(&r.body).map_err(|m| Error::new(1, 1, m))?;
    let (Some(token), Some(_), Some(_), Some(expires)) = (utf8(&doc, "access_token"), utf8(&doc, "resource"), utf8(&doc, "token_type"), utf8(&doc, "expires_in")) else {
        return Err(e(KMS_SERVER_BAD_JSON, format!("One or more required JSON properties are missing/invalid: data: {body}")));
    };
    let secs = parse_strtoll(&expires).ok_or_else(|| e(KMS_SERVER_BAD_JSON, format!("Invalid 'expires_in' string \"{expires}\" from IMDS server")))?;
    Ok((token, Duration::from_secs(secs.max(0) as u64)))
}

/// `strtoll(s, &end, 0)` that must consume the whole string (decimal, 0x hex or 0 octal)
fn parse_strtoll(s: &str) -> Option<i64> {
    let t = s.trim_start();
    let (neg, t) = match t.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let v = if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        i64::from_str_radix(h, 16).ok()?
    } else if t.len() > 1 && t.starts_with('0') {
        i64::from_str_radix(&t[1..], 8).ok()?
    } else {
        t.parse::<i64>().ok()?
    };
    Some(if neg { -v } else { v })
}

/// `gcp_access_token_from_gcp_server`: the default service account's token
pub fn gcp_metadata_token(hosts: &Hosts) -> Result<String, Error> {
    let e = |c: u32, m: String| Error::new(domain::GCP, c, m);
    let url = Url { host: hosts.gcp.0.clone(), port: hosts.gcp.1, path: "/computeMetadata/v1/instance/service-accounts/default/token".into() };
    let r = http::send("GET", &url, "Metadata-Flavor: Google\r\n", &[], Duration::from_secs(3)).map_err(stream_socket)?;
    let body = String::from_utf8_lossy(&r.body).into_owned();
    if r.status != 200 {
        return Err(e(KMS_SERVER_HTTP, format!("Error from the GCP metadata server while looking for access token: {body}")));
    }
    let doc = crate::bson::json_parse::from_json(&r.body).map_err(|m| Error::new(1, 1, m))?;
    match (utf8(&doc, "access_token"), utf8(&doc, "token_type")) {
        (Some(t), Some(_)) => Ok(t),
        _ => Err(e(KMS_SERVER_BAD_JSON, format!("One or more required JSON properties are missing/invalid: data: {body}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation_and_parsing() {
        assert_eq!(validate(None, None, None).unwrap(), None);
        assert_eq!(validate(Some("a"), None, None).unwrap_err().message, "ACCESS_KEY_ID is set, but SECRET_ACCESS_KEY is missing");
        assert_eq!(validate(Some(""), Some(""), Some("t")).unwrap_err().message, "AWS_SESSION_TOKEN is set, but ACCESS_KEY_ID and SECRET_ACCESS_KEY are missing");
        assert_eq!(percent_encode("https://vault.azure.net"), "https%3A%2F%2Fvault.azure.net");
        assert_eq!(parse_strtoll("3599"), Some(3599));
        assert_eq!(parse_strtoll("0x10"), Some(16));
        assert_eq!(parse_strtoll("12a"), None);
        assert!(check_path("/a\r\nX: y").is_err());
        assert!(expires_iso8601("2000-01-01T00:00:00Z").unwrap() <= Instant::now());
    }

    /// A one-connection-per-request HTTP server answering by request line
    fn serve(answers: Vec<(&'static str, &'static str)>) -> u16 {
        use std::io::{Read, Write};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for c in l.incoming() {
                let mut c = c.unwrap();
                let mut buf = [0u8; 4096];
                let n = c.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).into_owned();
                let line = req.lines().next().unwrap_or_default().to_owned();
                let body = answers.iter().find(|(p, _)| line.contains(p)).map_or("", |(_, b)| b);
                let _ = write!(c, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}", body.len());
            }
        });
        port
    }

    #[test]
    fn metadata_services() {
        let exp = "2100-01-01T00:00:00Z";
        let ec2 = serve(vec![
            ("PUT /latest/api/token", "tok\n"),
            ("GET /latest/meta-data/iam/security-credentials/ ", "role\n"),
            ("GET /latest/meta-data/iam/security-credentials/role", Box::leak(format!("{{\"AccessKeyId\":\"AK\",\"SecretAccessKey\":\"SK\",\"Token\":\"T\",\"Expiration\":\"{exp}\"}}").into_boxed_str())),
            ("/metadata/identity/oauth2/token?api-version=2018-02-01&resource=https%3A%2F%2Fvault.azure.net", "{\"access_token\":\"az\",\"resource\":\"r\",\"token_type\":\"Bearer\",\"expires_in\":\"3599\"}"),
            ("/computeMetadata/v1/instance/service-accounts/default/token", "{\"access_token\":\"g\",\"token_type\":\"Bearer\"}"),
        ]);
        let local = ("127.0.0.1".to_string(), ec2);
        let hosts = Hosts { ecs: local.clone(), ec2: local.clone(), sts: None, azure: local.clone(), gcp: local };
        let c = from_ec2(&hosts).unwrap().unwrap();
        assert_eq!((c.access_key_id.as_str(), c.secret_access_key.as_str(), c.session_token.as_deref()), ("AK", "SK", Some("T")));
        assert!(c.expires.is_some_and(|e| e > Instant::now()));
        assert_eq!(azure_imds_token(&hosts).unwrap(), ("az".to_string(), Duration::from_secs(3599)));
        assert_eq!(gcp_metadata_token(&hosts).unwrap(), "g");
    }
}
