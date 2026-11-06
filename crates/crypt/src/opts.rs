//! Option parsing of libmongocrypt (`mongocrypt-opts.c`, `mongocrypt-endpoint.c`, `mongocrypt-kek.c`):
//! endpoints, KMS provider ids (`<type>` or `<type>:<name>`), KMS provider credentials and key
//! encryption keys ("masterKey" documents). Messages are libmongocrypt's.

use mongo_core::bson::iter::{Iter, Value};
use mongo_core::bson::writer::Writer;

pub type Result<T> = std::result::Result<T, String>;

// ---------------------------------------------------------------------------------------------
// BSON helpers

/// `bson_iter_find_descendant`: a dotted path through nested documents (and arrays)
pub fn find<'a>(doc: &'a [u8], dotkey: &str) -> Option<Value<'a>> {
    let (head, rest) = match dotkey.split_once('.') {
        Some((h, r)) => (h, Some(r)),
        None => (dotkey, None),
    };
    let mut it = Iter::new(doc)?;
    if !it.find(head.as_bytes()) {
        return None;
    }
    let v = it.value();
    match rest {
        None => Some(v),
        Some(r) => match v {
            Value::Document(d) | Value::Array(d) => find(d, r),
            _ => None,
        },
    }
}

pub fn is_empty_doc(doc: &[u8]) -> bool {
    doc.len() <= 5
}

fn utf8(s: &[u8]) -> Result<String> {
    String::from_utf8(s.to_vec()).map_err(|_| "invalid BSON".to_string())
}

/// `_mongocrypt_parse_optional_utf8`
pub fn optional_utf8(doc: &[u8], dotkey: &str) -> Result<Option<String>> {
    match find(doc, dotkey) {
        None => Ok(None),
        Some(Value::Utf8(s)) => Ok(Some(utf8(s)?)),
        Some(_) => Err(format!("expected UTF-8 {dotkey}")),
    }
}

/// `_mongocrypt_parse_required_utf8`
pub fn required_utf8(doc: &[u8], dotkey: &str) -> Result<String> {
    optional_utf8(doc, dotkey)?.ok_or_else(|| format!("expected UTF-8 {dotkey}"))
}

/// `_mongocrypt_parse_optional_bool`
pub fn optional_bool(doc: &[u8], dotkey: &str) -> Result<bool> {
    match find(doc, dotkey) {
        None => Ok(false),
        Some(Value::Bool(b)) => Ok(b),
        Some(_) => Err(format!("expected bool {dotkey}")),
    }
}

/// `_mongocrypt_parse_optional_binary`: binary, or base64 in a string
pub fn optional_binary(doc: &[u8], dotkey: &str) -> Result<Option<Vec<u8>>> {
    match find(doc, dotkey) {
        None => Ok(None),
        Some(Value::Utf8(s)) => mongo_core::bson::fmt::base64_decode(s)
            .map(Some)
            .ok_or_else(|| format!("unable to parse base64 from UTF-8 field {dotkey}")),
        Some(Value::Binary(_, d)) => Ok(Some(d.to_vec())),
        Some(_) => Err(format!("expected UTF-8 or binary {dotkey}")),
    }
}

/// `_mongocrypt_parse_required_binary`
pub fn required_binary(doc: &[u8], dotkey: &str) -> Result<Vec<u8>> {
    match optional_binary(doc, dotkey)? {
        Some(b) if !b.is_empty() => Ok(b),
        _ => Err(format!("expected UTF-8 or binary {dotkey}")),
    }
}

/// `_mongocrypt_check_allowed_fields` (root only)
pub fn check_allowed_fields(doc: &[u8], allowed: &[&str]) -> Result<()> {
    let mut it = Iter::new(doc).ok_or("invalid BSON")?;
    while it.next() {
        if !allowed.iter().any(|a| a.as_bytes() == it.key()) {
            return Err(format!("Unexpected field: '{}'", String::from_utf8_lossy(it.key())));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Endpoints

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Endpoint {
    pub original: String,
    pub protocol: Option<String>,
    pub host: String,
    pub port: Option<String>,
    pub domain: String,
    pub subdomain: Option<String>,
    pub path: Option<String>,
    pub query: Option<String>,
    pub host_and_port: String,
}

fn is_unreserved(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'-' | b'.' | b'_' | b'~')
}
fn is_sub_delim(c: u8) -> bool {
    b"!$&'()*+,;=".contains(&c)
}
fn is_reserved(c: u8) -> bool {
    b":/?#[]@".contains(&c) || is_sub_delim(c)
}

impl Endpoint {
    /// `_mongocrypt_endpoint_new`
    pub fn parse(raw: &str, allow_empty_subdomain: bool) -> Result<Endpoint> {
        let b = raw.as_bytes();
        if b.contains(&0) {
            return Err("Invalid endpoint".into());
        }
        let mut i = 0;
        while i < b.len() {
            let c = b[i];
            if is_unreserved(c) || is_reserved(c) {
                i += 1;
                continue;
            }
            if c == b'%' {
                for k in 1..=2 {
                    let d = b.get(i + k).copied().unwrap_or(0);
                    if !d.is_ascii_hexdigit() {
                        return Err(format!("Invalid percent-encoding in endpoint at offset {}: 0x{:02x}", i + k, d));
                    }
                }
                i += 3;
                continue;
            }
            return Err(format!("Invalid character in endpoint at offset {i}: 0x{c:02x}"));
        }
        let mut e = Endpoint { original: raw.to_string(), ..Default::default() };
        let mut pos = 0;
        if let Some(p) = raw.find("://") {
            e.protocol = Some(raw[..p].to_string());
            pos = p + 3;
        }
        let host_start = pos;
        match raw[pos..].find('.') {
            Some(d) => {
                e.subdomain = Some(raw[pos..pos + d].to_string());
                pos += d + 1;
            }
            None if allow_empty_subdomain => {}
            None => return Err(format!("Invalid endpoint, expected dot separator in host, but got: {raw}")),
        }
        let rest = &raw[pos..];
        let colon = rest.find(':').map(|x| x + pos);
        let qmark = rest.find('?').map(|x| x + pos);
        let slash = rest.find('/').map(|x| x + pos);
        let mut host_end = colon;
        if let Some(s) = slash {
            if host_end.is_none_or(|h| s < h) {
                host_end = Some(s);
            }
        }
        if let Some(q) = qmark {
            if host_end.is_none_or(|h| q < h) {
                host_end = Some(q);
            }
        }
        match host_end {
            Some(h) => {
                e.domain = raw[pos..h].to_string();
                e.host = raw[host_start..h].to_string();
            }
            None => {
                e.domain = raw[pos..].to_string();
                e.host = raw[host_start..].to_string();
            }
        }
        let hb = e.host.as_bytes();
        let mut k = 0;
        while k < hb.len() {
            let c = hb[k];
            if is_unreserved(c) || is_sub_delim(c) {
                k += 1;
            } else if c == b'%' {
                k += 3;
            } else {
                return Err(format!("Invalid character in endpoint host: 0x{c:02x}"));
            }
        }
        if let (Some(c), Some(h)) = (colon, host_end) {
            if c == h {
                let after = &raw[c + 1..];
                let q = after.find('?');
                let s = after.find('/');
                let port = match (s, q) {
                    (Some(s), q) if q.is_none_or(|q| s < q) => &after[..s],
                    (_, Some(q)) => &after[..q],
                    _ => after,
                };
                if port.is_empty() {
                    return Err("Invalid endpoint, expected a port after the colon".into());
                }
                if let Some(bad) = port.bytes().find(|c| !c.is_ascii_digit()) {
                    return Err(format!("Invalid character in endpoint port: 0x{bad:02x}"));
                }
                e.port = Some(port.to_string());
            }
        }
        if let Some(s) = slash {
            let after = &raw[s + 1..];
            let mut path = match after.find('?') {
                Some(q) => after[..q].to_string(),
                None => after.to_string(),
            };
            if path.ends_with('/') {
                path.pop();
            }
            e.path = Some(path);
        }
        if let Some(q) = qmark {
            e.query = Some(raw[q + 1..].to_string());
        }
        e.host_and_port = match &e.port {
            Some(p) => format!("{}:{p}", e.host),
            None => e.host.clone(),
        };
        Ok(e)
    }
}

fn optional_endpoint(doc: &[u8], dotkey: &str, allow_empty_subdomain: bool) -> Result<Option<Endpoint>> {
    match optional_utf8(doc, dotkey)? {
        None => Ok(None),
        Some(s) => Endpoint::parse(&s, allow_empty_subdomain).map(Some),
    }
}

fn required_endpoint(doc: &[u8], dotkey: &str, allow_empty_subdomain: bool) -> Result<Endpoint> {
    optional_endpoint(doc, dotkey, allow_empty_subdomain)?.ok_or_else(|| format!("expected endpoint {dotkey}"))
}

// ---------------------------------------------------------------------------------------------
// KMS provider ids

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum KmsType {
    Aws,
    Azure,
    Gcp,
    Kmip,
    Local,
}

impl KmsType {
    pub fn name(self) -> &'static str {
        match self {
            KmsType::Aws => "aws",
            KmsType::Azure => "azure",
            KmsType::Gcp => "gcp",
            KmsType::Kmip => "kmip",
            KmsType::Local => "local",
        }
    }
}

const KEY_HELP: &str = "Expected `<type>` or `<type>:<name>`. Example: `local` or `local:name`.";

/// `mc_kmsid_parse`. The type is matched with `strncmp(type, kmsid, nchars)` as libmongocrypt
/// does, so a prefix of a type name (also the empty one) selects that type.
pub fn parse_kmsid(kmsid: &str) -> Result<(KmsType, Option<&str>)> {
    let (ty, name) = match kmsid.find(':') {
        Some(p) => (&kmsid[..p], Some(&kmsid[p + 1..])),
        None => (kmsid, None),
    };
    let prefix_of = |full: &str| full.as_bytes().starts_with(ty.as_bytes()) || ty.is_empty();
    let t = if prefix_of("aws") {
        KmsType::Aws
    } else if prefix_of("azure") {
        KmsType::Azure
    } else if prefix_of("gcp") {
        KmsType::Gcp
    } else if prefix_of("kmip") {
        KmsType::Kmip
    } else if prefix_of("local") {
        KmsType::Local
    } else {
        return Err(format!("unrecognized KMS provider `{kmsid}`: unrecognized type. {KEY_HELP}"));
    };
    if let Some(n) = name {
        if n.is_empty() {
            return Err(format!("unrecognized KMS provider `{kmsid}`: empty name. {KEY_HELP}"));
        }
        if let Some(c) = n.chars().find(|c| !(c.is_ascii_alphanumeric() || *c == '_')) {
            return Err(format!(
                "unrecognized KMS provider `{kmsid}`: unsupported character `{c}`. Must be of the form `<provider type>:<name>` where `<name>` only contain characters [a-zA-Z0-9_]"
            ));
        }
    }
    Ok((t, name))
}

// ---------------------------------------------------------------------------------------------
// KMS provider credentials

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Creds {
    Aws { access_key_id: String, secret_access_key: String, session_token: Option<String> },
    Azure(AzureCreds),
    Gcp(GcpCreds),
    Kmip { endpoint: Endpoint },
    Local { key: Vec<u8> },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AzureCreds {
    AccessToken(String),
    Client { tenant_id: String, client_id: String, client_secret: String, identity_platform_endpoint: Option<Endpoint> },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GcpCreds {
    AccessToken(String),
    ServiceAccount { email: String, private_key: Vec<u8>, endpoint: Option<Endpoint> },
}

fn parse_creds(t: KmsType, kmsid: &str, def: &[u8]) -> Result<Creds> {
    let r = (|| -> Result<Creds> {
        match t {
            KmsType::Local => {
                let key = required_binary(def, "key")?;
                if key.len() != crate::crypto::KEY_LEN {
                    return Err(format!("local key must be {} bytes", crate::crypto::KEY_LEN));
                }
                check_allowed_fields(def, &["key"])?;
                Ok(Creds::Local { key })
            }
            KmsType::Azure => {
                if let Some(tok) = optional_utf8(def, "accessToken")? {
                    check_allowed_fields(def, &["accessToken"])?;
                    return Ok(Creds::Azure(AzureCreds::AccessToken(tok)));
                }
                let tenant_id = required_utf8(def, "tenantId")?;
                let client_id = required_utf8(def, "clientId")?;
                let client_secret = required_utf8(def, "clientSecret")?;
                let identity_platform_endpoint = optional_endpoint(def, "identityPlatformEndpoint", false)?;
                check_allowed_fields(def, &["tenantId", "clientId", "clientSecret", "identityPlatformEndpoint"])?;
                Ok(Creds::Azure(AzureCreds::Client { tenant_id, client_id, client_secret, identity_platform_endpoint }))
            }
            KmsType::Gcp => {
                if let Some(tok) = optional_utf8(def, "accessToken")? {
                    check_allowed_fields(def, &["accessToken"])?;
                    return Ok(Creds::Gcp(GcpCreds::AccessToken(tok)));
                }
                let email = required_utf8(def, "email")?;
                let private_key = required_binary(def, "privateKey")?;
                let endpoint = optional_endpoint(def, "endpoint", false)?;
                check_allowed_fields(def, &["email", "privateKey", "endpoint"])?;
                Ok(Creds::Gcp(GcpCreds::ServiceAccount { email, private_key, endpoint }))
            }
            KmsType::Aws => {
                let access_key_id = required_utf8(def, "accessKeyId")?;
                let secret_access_key = required_utf8(def, "secretAccessKey")?;
                let session_token = optional_utf8(def, "sessionToken")?;
                check_allowed_fields(def, &["accessKeyId", "secretAccessKey", "sessionToken"])?;
                Ok(Creds::Aws { access_key_id, secret_access_key, session_token })
            }
            KmsType::Kmip => {
                let endpoint = required_endpoint(def, "endpoint", true)?;
                check_allowed_fields(def, &["endpoint"])?;
                Ok(Creds::Kmip { endpoint })
            }
        }
    })();
    r.map_err(|e| format!("Failed to parse KMS provider `{kmsid}`: {e}"))
}

/// The configured KMS providers (`_mongocrypt_opts_kms_providers_t`)
#[derive(Clone, Debug, Default)]
pub struct KmsProviders {
    /// unnamed providers by type
    pub configured: Vec<(KmsType, Creds)>,
    /// `<type>:<name>` providers
    pub named: Vec<(String, Creds)>,
    /// unnamed providers given as `{}`: credentials on demand
    pub need_credentials: Vec<KmsType>,
}

impl KmsProviders {
    /// `_mongocrypt_parse_kms_providers`
    pub fn parse(doc: &[u8]) -> Result<KmsProviders> {
        let mut out = KmsProviders::default();
        let mut it = Iter::new(doc).ok_or("invalid BSON")?;
        while it.next() {
            let field = String::from_utf8_lossy(it.key()).into_owned();
            let def = match it.value() {
                Value::Document(d) => d,
                _ => return Err(format!("expected BSON document for field: {field}")),
            };
            let (t, name) = parse_kmsid(&field)?;
            if name.is_some() {
                if out.named.iter().any(|(k, _)| *k == field) {
                    return Err(format!("Got unexpected duplicate entry for KMS provider: `{field}`"));
                }
                if is_empty_doc(def) {
                    return Err(format!(
                        "Unexpected empty document for named KMS provider: '{field}'. On-demand credentials are not supported for named KMS providers."
                    ));
                }
                let c = parse_creds(t, &field, def)?;
                out.named.push((field, c));
                continue;
            }
            // unnamed: the field must be the exact type name (strcmp)
            let exact = match field.as_str() {
                "azure" => KmsType::Azure,
                "gcp" => KmsType::Gcp,
                "local" => KmsType::Local,
                "aws" => KmsType::Aws,
                "kmip" => KmsType::Kmip,
                _ => return Err(format!("unsupported KMS provider: {field}")),
            };
            if is_empty_doc(def) {
                if !out.need_credentials.contains(&exact) {
                    out.need_credentials.push(exact);
                }
                continue;
            }
            if out.configured.iter().any(|(k, _)| *k == exact) {
                return Err(format!("{} KMS provider already set", exact.name()));
            }
            let c = parse_creds(exact, &field, def)?;
            out.configured.push((exact, c));
        }
        Ok(out)
    }

    /// The credentials of a kmsid (`aws` or `aws:name`)
    pub fn get(&self, kmsid: &str) -> Option<&Creds> {
        if kmsid.contains(':') {
            self.named.iter().find(|(k, _)| k == kmsid).map(|(_, c)| c)
        } else {
            let (t, _) = parse_kmsid(kmsid).ok()?;
            self.configured.iter().find(|(k, _)| *k == t).map(|(_, c)| c)
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Key encryption keys ("masterKey")

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KekProvider {
    Aws { cmk: String, region: String, endpoint: Option<Endpoint> },
    Azure { key_vault_endpoint: Endpoint, key_name: String, key_version: Option<String> },
    Gcp {
        endpoint: Option<Endpoint>,
        project_id: String,
        location: String,
        key_ring: String,
        key_name: String,
        key_version: Option<String>,
    },
    Kmip { endpoint: Option<Endpoint>, key_id: Option<String>, delegated: bool },
    Local,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Kek {
    pub kmsid: String,
    pub provider: KekProvider,
}

impl Kek {
    pub fn kms_type(&self) -> KmsType {
        match self.provider {
            KekProvider::Aws { .. } => KmsType::Aws,
            KekProvider::Azure { .. } => KmsType::Azure,
            KekProvider::Gcp { .. } => KmsType::Gcp,
            KekProvider::Kmip { .. } => KmsType::Kmip,
            KekProvider::Local => KmsType::Local,
        }
    }

    /// `_mongocrypt_kek_parse_owned`
    pub fn parse(doc: &[u8]) -> Result<Kek> {
        let kmsid = required_utf8(doc, "provider")?;
        let (t, _) = parse_kmsid(&kmsid)?;
        let provider = match t {
            KmsType::Aws => {
                let cmk = required_utf8(doc, "key")?;
                let region = required_utf8(doc, "region")?;
                let endpoint = optional_endpoint(doc, "endpoint", false)?;
                check_allowed_fields(doc, &["provider", "key", "region", "endpoint"])?;
                KekProvider::Aws { cmk, region, endpoint }
            }
            KmsType::Local => {
                check_allowed_fields(doc, &["provider"])?;
                KekProvider::Local
            }
            KmsType::Azure => {
                let key_vault_endpoint = required_endpoint(doc, "keyVaultEndpoint", false)?;
                let key_name = required_utf8(doc, "keyName")?;
                let key_version = optional_utf8(doc, "keyVersion")?;
                check_allowed_fields(doc, &["provider", "keyVaultEndpoint", "keyName", "keyVersion"])?;
                KekProvider::Azure { key_vault_endpoint, key_name, key_version }
            }
            KmsType::Gcp => {
                let endpoint = optional_endpoint(doc, "endpoint", false)?;
                let project_id = required_utf8(doc, "projectId")?;
                let location = required_utf8(doc, "location")?;
                let key_ring = required_utf8(doc, "keyRing")?;
                let key_name = required_utf8(doc, "keyName")?;
                let key_version = optional_utf8(doc, "keyVersion")?;
                check_allowed_fields(
                    doc,
                    &["provider", "endpoint", "projectId", "location", "keyRing", "keyName", "keyVersion"],
                )?;
                KekProvider::Gcp { endpoint, project_id, location, key_ring, key_name, key_version }
            }
            KmsType::Kmip => {
                let endpoint = optional_endpoint(doc, "endpoint", true)?;
                let key_id = optional_utf8(doc, "keyId")?;
                let delegated = optional_bool(doc, "delegated")?;
                check_allowed_fields(doc, &["provider", "endpoint", "keyId", "delegated"])?;
                KekProvider::Kmip { endpoint, key_id, delegated }
            }
        };
        Ok(Kek { kmsid, provider })
    }

    /// `_mongocrypt_kek_append`
    pub fn append(&self, w: &mut Writer) -> Result<()> {
        w.utf8(b"provider", self.kmsid.as_bytes());
        match &self.provider {
            KekProvider::Aws { cmk, region, endpoint } => {
                w.utf8(b"region", region.as_bytes());
                w.utf8(b"key", cmk.as_bytes());
                if let Some(e) = endpoint {
                    w.utf8(b"endpoint", e.host_and_port.as_bytes());
                }
            }
            KekProvider::Local => {}
            KekProvider::Azure { key_vault_endpoint, key_name, key_version } => {
                w.utf8(b"keyVaultEndpoint", key_vault_endpoint.host_and_port.as_bytes());
                w.utf8(b"keyName", key_name.as_bytes());
                if let Some(v) = key_version {
                    w.utf8(b"keyVersion", v.as_bytes());
                }
            }
            KekProvider::Gcp { endpoint, project_id, location, key_ring, key_name, key_version } => {
                w.utf8(b"projectId", project_id.as_bytes());
                w.utf8(b"location", location.as_bytes());
                w.utf8(b"keyRing", key_ring.as_bytes());
                w.utf8(b"keyName", key_name.as_bytes());
                if let Some(v) = key_version {
                    w.utf8(b"keyVersion", v.as_bytes());
                }
                if let Some(e) = endpoint {
                    w.utf8(b"endpoint", e.host_and_port.as_bytes());
                }
            }
            KekProvider::Kmip { endpoint, key_id, delegated } => {
                if let Some(e) = endpoint {
                    w.utf8(b"endpoint", e.host_and_port.as_bytes());
                }
                if *delegated {
                    w.bool(b"delegated", true);
                }
                match key_id {
                    Some(k) => w.utf8(b"keyId", k.as_bytes()),
                    None => return Err("keyId required for KMIP".into()),
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints() {
        let e = Endpoint::parse("https://kms.us-east-1.amazonaws.com:443/path/?q=1", false).unwrap();
        assert_eq!(e.protocol.as_deref(), Some("https"));
        assert_eq!(e.host, "kms.us-east-1.amazonaws.com");
        assert_eq!(e.subdomain.as_deref(), Some("kms"));
        assert_eq!(e.domain, "us-east-1.amazonaws.com");
        assert_eq!(e.port.as_deref(), Some("443"));
        assert_eq!(e.path.as_deref(), Some("path"));
        assert_eq!(e.query.as_deref(), Some("q=1"));
        assert_eq!(e.host_and_port, "kms.us-east-1.amazonaws.com:443");
        assert!(Endpoint::parse("localhost", false).unwrap_err().contains("expected dot separator"));
        assert_eq!(Endpoint::parse("localhost:5698", true).unwrap().host_and_port, "localhost:5698");
        assert_eq!(Endpoint::parse("a.b:", false).unwrap_err(), "Invalid endpoint, expected a port after the colon");
        assert_eq!(Endpoint::parse("a.b c", false).unwrap_err(), "Invalid character in endpoint at offset 3: 0x20");
        assert_eq!(Endpoint::parse("a.b/c:8080", false).unwrap().port, None);
    }

    #[test]
    fn kmsids() {
        assert_eq!(parse_kmsid("local").unwrap(), (KmsType::Local, None));
        assert_eq!(parse_kmsid("local:a_1").unwrap(), (KmsType::Local, Some("a_1")));
        assert!(parse_kmsid("local:").unwrap_err().contains("empty name"));
        assert!(parse_kmsid("local:a-b").unwrap_err().contains("unsupported character `-`"));
        assert!(parse_kmsid("foo").unwrap_err().contains("unrecognized type"));
        // strncmp quirk kept: a prefix selects the type
        assert_eq!(parse_kmsid("a").unwrap().0, KmsType::Aws);
    }
}
