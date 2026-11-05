//! Connection strings: a port of libmongoc's mongoc-uri.c (and the host parsing of
//! mongoc-host-list.c), with its error messages. Option keys are stored lowercased, as libmongoc
//! stores them; `warnings` collects what libmongoc logs with MONGOC_WARNING.

/// MONGOC_DEFAULT_PORT
pub const DEFAULT_PORT: u16 = 27017;
/// BSON_HOST_NAME_MAX
const HOST_NAME_MAX: usize = 255;
/// MONGOC_TOPOLOGY_MIN_HEARTBEAT_FREQUENCY_MS
const MIN_HEARTBEAT_FREQUENCY_MS: i32 = 500;
/// MONGOC_HANDSHAKE_APPNAME_MAX
const APPNAME_MAX: usize = 128;
/// MONGOC_NO_MAX_STALENESS
pub const NO_MAX_STALENESS: i32 = -1;

/// mongoc_write_concern_t w sentinels
pub const W_UNACKNOWLEDGED: i32 = 0;
pub const W_DEFAULT: i32 = -2;
pub const W_MAJORITY: i32 = -3;
pub const W_TAG: i32 = -4;

const ESCAPE_INSTRUCTIONS: &str = "Percent-encode username and password according to RFC 3986";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Family {
    Unspec,
    Inet6,
    Unix,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Host {
    pub host: String,
    pub port: u16,
    pub host_and_port: String,
    pub family: Family,
}

#[derive(Clone, Debug, PartialEq)]
pub enum OptVal {
    Str(String),
    Int32(i32),
    Int64(i64),
    Bool(bool),
    /// authMechanismProperties: (key, value) pairs
    Doc(Vec<(String, String)>),
}

/// An ordered option map with case-insensitive lookups (bson documents in libmongoc).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Opts(pub Vec<(String, OptVal)>);

impl Opts {
    pub fn get(&self, key: &str) -> Option<&OptVal> {
        self.0.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v)
    }
    pub fn has(&self, key: &str) -> bool {
        self.get(key).is_some()
    }
    fn get_mut(&mut self, key: &str) -> Option<&mut OptVal> {
        self.0.iter_mut().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v)
    }
    /// _bson_upsert_utf8_icase: erase every case variant, append under the lowercased key.
    fn upsert(&mut self, key: &str, v: OptVal) {
        self.0.retain(|(k, _)| !k.eq_ignore_ascii_case(key));
        self.0.push((key.to_ascii_lowercase(), v));
    }
    pub fn str(&self, key: &str) -> Option<&str> {
        match self.get(key) {
            Some(OptVal::Str(s)) => Some(s),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadMode {
    Primary = 1,
    Secondary = 2,
    PrimaryPreferred = 5,
    SecondaryPreferred = 6,
    Nearest = 10,
}

#[derive(Clone, Debug, PartialEq)]
pub struct WriteConcern {
    pub w: i32,
    pub wtag: Option<String>,
    pub wtimeout: i64,
    pub journal: Option<bool>,
}

impl Default for WriteConcern {
    fn default() -> Self {
        WriteConcern { w: W_DEFAULT, wtag: None, wtimeout: 0, journal: None }
    }
}

#[derive(Clone, Debug)]
pub struct Uri {
    pub string: String,
    pub is_srv: bool,
    pub srv: Option<String>,
    pub hosts: Vec<Host>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub database: Option<String>,
    /// options as presented (lowercased keys, string values)
    pub raw: Opts,
    pub options: Opts,
    /// authmechanism, authsource, authmechanismproperties
    pub credentials: Opts,
    pub compressors: Vec<String>,
    pub read_mode: ReadMode,
    pub read_tags: Vec<Vec<(String, String)>>,
    pub max_staleness_seconds: i32,
    pub read_concern_level: Option<String>,
    pub write_concern: WriteConcern,
    pub warnings: Vec<String>,
}

type R<T> = Result<T, String>;

const INT64_OPTS: &[&str] = &["wtimeoutms"];
const INT32_OPTS: &[&str] = &[
    "connecttimeoutms", "heartbeatfrequencyms", "serverselectiontimeoutms", "socketcheckintervalms", "sockettimeoutms",
    "localthresholdms", "maxpoolsize", "maxstalenessseconds", "waitqueuetimeoutms", "zlibcompressionlevel", "srvmaxhosts",
    "maxadaptiveretries",
];
const BOOL_OPTS: &[&str] = &[
    "canonicalizehostname", "directconnection", "journal", "retryreads", "retrywrites", "enableoverloadretargeting", "safe",
    "serverselectiontryonce", "tls", "tlsinsecure", "tlsallowinvalidcertificates", "tlsallowinvalidhostnames",
    "tlsdisablecertificaterevocationcheck", "tlsdisableocspendpointcheck", "loadbalanced", "ssl",
    "sslallowinvalidcertificates", "sslallowinvalidhostnames",
];
const UTF8_OPTS: &[&str] = &[
    "appname", "replicaset", "readpreference", "servermonitoringmode", "srvservicename", "tlscertificatekeyfile",
    "tlscertificatekeyfilepassword", "tlscafile", "sslclientcertificatekeyfile", "sslclientcertificatekeypassword",
    "sslcertificateauthorityfile",
];

fn is_in(list: &[&str], key: &str) -> bool {
    list.iter().any(|k| k.eq_ignore_ascii_case(key))
}
pub fn option_is_int64(key: &str) -> bool {
    is_in(INT64_OPTS, key)
}
pub fn option_is_int32(key: &str) -> bool {
    option_is_int64(key) || is_in(INT32_OPTS, key)
}
pub fn option_is_bool(key: &str) -> bool {
    is_in(BOOL_OPTS, key)
}
pub fn option_is_utf8(key: &str) -> bool {
    is_in(UTF8_OPTS, key)
}

/// mongoc_uri_canonicalize_option
pub fn canonicalize(key: &str) -> &str {
    const MAP: &[(&str, &str)] = &[
        ("ssl", "tls"),
        ("sslclientcertificatekeyfile", "tlscertificatekeyfile"),
        ("sslclientcertificatekeypassword", "tlscertificatekeyfilepassword"),
        ("sslcertificateauthorityfile", "tlscafile"),
        ("sslallowinvalidcertificates", "tlsallowinvalidcertificates"),
        ("sslallowinvalidhostnames", "tlsallowinvalidhostnames"),
    ];
    MAP.iter().find(|(a, _)| a.eq_ignore_ascii_case(key)).map(|(_, b)| *b).unwrap_or(key)
}

/// mlib_nat64_parse: Err(true) = EINVAL, Err(false) = ERANGE.
fn nat64_parse(s: &str, base: u32) -> Result<u64, bool> {
    if s.is_empty() {
        return Err(true);
    }
    let mut v: u64 = 0;
    let mut overflow = false;
    for c in s.bytes() {
        let (m, o1) = v.overflowing_mul(base as u64);
        overflow |= o1;
        let c = c.to_ascii_lowercase();
        let d = match c {
            b'0'..=b'9' => (c - b'0') as u32,
            b'a'..=b'z' => (c - b'a') as u32 + 10,
            _ => return Err(true),
        };
        if d >= base {
            return Err(true);
        }
        let (a, o2) = m.overflowing_add(d as u64);
        overflow |= o2;
        v = a;
    }
    if overflow {
        Err(false)
    } else {
        Ok(v)
    }
}

/// mlib_i64_parse (base 0: auto-detect 0x/0o/0b and a leading 0 as octal).
fn i64_parse(s: &str, base: u32) -> Result<i64, bool> {
    if s.is_empty() {
        return Err(true);
    }
    let mut s = s;
    let mut neg = false;
    if let Some(r) = s.strip_prefix('+') {
        s = r;
    } else if let Some(r) = s.strip_prefix('-') {
        s = r;
        neg = true;
    }
    let mut base = base;
    if base == 0 {
        let b = s.as_bytes();
        if !b.is_empty() && b[0] == b'0' {
            if b.len() > 1 {
                match b[1].to_ascii_lowercase() {
                    b'x' => {
                        base = 16;
                        s = &s[2..];
                    }
                    b'o' => {
                        base = 8;
                        s = &s[2..];
                    }
                    b'b' => {
                        base = 2;
                        s = &s[2..];
                    }
                    _ => {}
                }
            }
            if base == 0 {
                base = 8;
            }
        } else {
            base = 10;
        }
    }
    let n = nat64_parse(s, base)?;
    if neg {
        if n > i64::MAX as u64 + 1 {
            return Err(false);
        }
        Ok((n as i64).wrapping_neg())
    } else {
        i64::try_from(n).map_err(|_| false)
    }
}

fn i32_parse(s: &str, base: u32) -> Result<i32, bool> {
    i32::try_from(i64_parse(s, base)?).map_err(|_| false)
}

/// _strdup_pct_decode
fn pct_decode(sv: &str) -> R<String> {
    let b = sv.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'%' {
            out.push(b[i]);
            i += 1;
            continue;
        }
        if b.len() - i < 3 {
            return Err(format!("At offset {}: Truncated %-sequence \"{}\"", i, String::from_utf8_lossy(&b[i..])));
        }
        let pair = std::str::from_utf8(&b[i + 1..i + 3]).map_err(|_| ())
            .and_then(|p| nat64_parse(p, 16).map_err(|_| ()));
        match pair {
            Ok(v) => out.push(v as u8),
            Err(()) => return Err(format!("At offset {}: Invalid %-sequence \"{}\"", i, String::from_utf8_lossy(&b[i..i + 3]))),
        }
        i += 3;
    }
    if out.contains(&0) || !crate::bson::utf8::validate(&out, false) {
        return Err("Invalid %-encoded string: The decoded result is not valid UTF-8 or contains null characters".into());
    }
    Ok(String::from_utf8(out).unwrap())
}

/// _mongoc_parse_port
fn parse_port(s: &str) -> R<u16> {
    match nat64_parse(s, 10) {
        Ok(0) => Err("Port number cannot be zero".into()),
        Err(true) => Err("Port string is not a valid integer".into()),
        Ok(v) if v <= u16::MAX as u64 => Ok(v as u16),
        _ => Err("Port number is out-of-range for a 16-bit integer".into()),
    }
}

/// _mongoc_host_list_from_hostport_with_err
pub fn host_from_hostport(host: &str, port: u16) -> R<Host> {
    if host.is_empty() {
        return Err("Empty hostname in URI".into());
    }
    if host.len() > HOST_NAME_MAX {
        return Err(format!("Hostname provided in URI is too long, max is {} chars", HOST_NAME_MAX));
    }
    if host.contains(':') {
        if host.len() > HOST_NAME_MAX - 2 {
            return Err(format!("IPv6 literal provided in URI is too long, max is {} chars", HOST_NAME_MAX - 2));
        }
        let h = host.to_ascii_lowercase();
        Ok(Host { host_and_port: format!("[{h}]:{port}"), host: h, port, family: Family::Inet6 })
    } else if host.contains('/') && host.contains(".sock") {
        Ok(Host { host: host.to_owned(), port, host_and_port: host.to_owned(), family: Family::Unix })
    } else {
        let h = host.to_ascii_lowercase();
        Ok(Host { host_and_port: format!("{h}:{port}"), host: h, port, family: Family::Unspec })
    }
}

/// _mongoc_host_list_from_string_with_err (_parse_host / _parse_host_ipv6)
pub fn host_from_string(spec: &str) -> R<Host> {
    if spec.contains(']') {
        let base = format!("Invalid IPv6 literal address '{spec}'");
        if !spec.starts_with('[') {
            return Err(format!("{base}: Must start with a bracket '['"));
        }
        let Some(close) = spec.find(']') else { return Err(format!("{base}: Mising closing bracket ']'")) };
        let has_port = spec.as_bytes().get(close + 1) == Some(&b':');
        if !has_port && close != spec.len() - 1 {
            return Err(format!("{base}: Invalid trailing content following closing bracket ']'"));
        }
        let mut port = DEFAULT_PORT;
        if has_port {
            let ps = &spec[close + 2..];
            port = parse_port(ps).map_err(|e| format!("{base}: Invalid port '{ps}': {e}"))?;
        }
        return host_from_hostport(&spec[1..close], port);
    }
    let mut port = DEFAULT_PORT;
    let mut hostname = spec;
    if let Some((h, ps)) = spec.split_once(':') {
        port = parse_port(ps).map_err(|e| format!("Invalid host specifier '{spec}': Invalid port string '{ps}': {e}"))?;
        hostname = h;
    }
    host_from_hostport(hostname, port)
}

fn valid_hostname(s: &str) -> bool {
    s.len() > 1 && !s.starts_with('.')
}

/// mongoc_uri_check_srv_service_name
fn check_srv_service_name(s: &str) -> bool {
    let b = s.as_bytes();
    if b.is_empty() || b.len() > 62 {
        return false;
    }
    let mut prev = 0u8;
    let mut alpha = 0;
    for &c in b {
        if !c.is_ascii_alphanumeric() && c != b'-' {
            return false;
        }
        if c == b'-' && prev == b'-' {
            return false;
        }
        if c.is_ascii_alphabetic() {
            alpha += 1;
        }
        prev = c;
    }
    alpha > 0 && b[0] != b'-' && b[b.len() - 1] != b'-'
}

/// (environment, supports a username, requires TOKEN_RESOURCE): mongoc-oidc-env.c
const OIDC_ENVS: &[(&str, bool, bool)] = &[("test", false, false), ("azure", true, true), ("gcp", false, true), ("k8s", false, false)];

impl Uri {
    /// mongoc_uri_new_with_error
    pub fn parse(s: &str) -> R<Uri> {
        let mut u = Uri {
            string: s.to_owned(),
            is_srv: false,
            srv: None,
            hosts: Vec::new(),
            username: None,
            password: None,
            database: None,
            raw: Opts::default(),
            options: Opts::default(),
            credentials: Opts::default(),
            compressors: Vec::new(),
            read_mode: ReadMode::Primary,
            read_tags: Vec::new(),
            max_staleness_seconds: NO_MAX_STALENESS,
            read_concern_level: None,
            write_concern: WriteConcern::default(),
            warnings: Vec::new(),
        };
        u.parse_str(s)?;
        u.assign_read_prefs_mode()?;
        u.max_staleness_seconds = u.max_staleness_option();
        if !u.read_prefs_valid() {
            return Err("Invalid readPreferences".into());
        }
        u.build_write_concern()?;
        let wc = &u.write_concern;
        if (wc.journal == Some(true) && wc.w == W_UNACKNOWLEDGED) || wc.wtimeout < 0 {
            return Err("Invalid writeConcern".into());
        }
        Ok(u)
    }

    fn warn(&mut self, w: String) {
        self.warnings.push(w);
    }

    /// mongoc_uri_parse (+ _decompose_uri_string)
    fn parse_str(&mut self, s: &str) -> R<()> {
        let Some((scheme, mut remain)) = s.split_once("://") else { return Err("Invalid URI, no scheme part specified".into()) };
        let userinfo_end = remain.find('@').unwrap_or(0);
        let term = remain[userinfo_end..].find(['/', '?']).map(|p| p + userinfo_end).unwrap_or(remain.len());
        let authority = &remain[..term];
        remain = &remain[term..];
        let path_end = remain.find('?').unwrap_or(remain.len());
        let (path, query) = remain.split_at(path_end);
        match scheme {
            "mongodb" => self.is_srv = false,
            "mongodb+srv" => self.is_srv = true,
            _ => return Err(format!("Invalid URI scheme \"{scheme}://\". Expected one of \"mongodb://\" or \"mongodb+srv://\"")),
        }
        self.parse_authority(authority)?;
        self.parse_path(path)?;
        if !query.is_empty() {
            self.apply_query_string(&query[1..], false)?;
        }
        self.finalize()
    }

    fn parse_authority(&mut self, authority: &str) -> R<()> {
        let hostinfo = match authority.split_once('@') {
            Some((userinfo, hostinfo)) => {
                self.parse_userinfo(userinfo)?;
                hostinfo
            }
            None => authority,
        };
        if hostinfo.starts_with('@') {
            return Err(format!("Invalid username or password. {ESCAPE_INSTRUCTIONS}"));
        }
        if self.is_srv {
            self.parse_srv_hostname(hostinfo)
        } else {
            self.parse_hosts_csv(hostinfo)
        }
    }

    fn parse_userinfo(&mut self, userpass: &str) -> R<()> {
        let (user, pass) = match userpass.split_once(':') {
            Some((u, p)) => (u, Some(p)),
            None => (userpass, None),
        };
        const PROHIBITED: &[char] = &['@', ':', '/'];
        if user.contains(PROHIBITED) {
            return Err(format!("Username must not have unescaped chars. {ESCAPE_INSTRUCTIONS}"));
        }
        if pass.is_some_and(|p| p.contains(PROHIBITED)) {
            return Err(format!("Password must not have unescaped chars. {ESCAPE_INSTRUCTIONS}"));
        }
        self.username = Some(pct_decode(user).map_err(|_| "Invalid %-encoding in username in URI string".to_owned())?);
        if let Some(p) = pass {
            self.password = Some(pct_decode(p).map_err(|_| "Invalid %-encoding in password in URI string".to_owned())?);
        }
        Ok(())
    }

    fn parse_srv_hostname(&mut self, s: &str) -> R<()> {
        if s.is_empty() {
            return Err("Missing service name in SRV URI".into());
        }
        let service = match pct_decode(s) {
            Ok(v) if valid_hostname(&v) && v.matches('.').count() - usize::from(v.starts_with('.')) >= 2 => v,
            Ok(_) => return Err(format!("Invalid SRV service name \"{s}\" in URI: ")),
            Err(e) => return Err(format!("Invalid SRV service name \"{s}\" in URI: {e}")),
        };
        if service.contains(',') {
            return Err("Multiple service names are prohibited in an SRV URI".into());
        }
        if service.contains(':') {
            return Err("Port numbers are prohibited in an SRV URI".into());
        }
        self.srv = Some(service);
        Ok(())
    }

    fn parse_hosts_csv(&mut self, hosts: &str) -> R<()> {
        if hosts.contains('?') {
            return Err("A '/' is required between the host list and any options.".into());
        }
        if hosts.is_empty() {
            return Err("Host list of URI string cannot be empty".into());
        }
        for h in hosts.split(',') {
            // an empty entry after a trailing comma ends the loop in libmongoc
            if h.is_empty() && hosts.ends_with(',') {
                continue;
            }
            self.parse_one_host(h)?;
        }
        Ok(())
    }

    fn parse_one_host(&mut self, hostport: &str) -> R<()> {
        if hostport.contains('/') {
            self.warn("Unix Domain Sockets must be escaped (e.g. / = %2F)".into());
            return Err(String::new());
        }
        let hp = pct_decode(hostport).map_err(|e| format!("Invalid host specifier \"{hostport}\": {e}"))?;
        let h = host_from_string(&hp).map_err(|e| format!("Invalid host specifier \"{hp}\": {e}"))?;
        // _mongoc_host_list_upsert: replace a case-insensitively equal host_and_port in place
        match self.hosts.iter_mut().find(|x| x.host_and_port.eq_ignore_ascii_case(&h.host_and_port)) {
            Some(x) => *x = h,
            None => self.hosts.push(h),
        }
        Ok(())
    }

    fn parse_path(&mut self, path: &str) -> R<()> {
        let rel = path.strip_prefix('/').unwrap_or(path);
        if rel.is_empty() {
            self.database = None;
            return Ok(());
        }
        let db = pct_decode(rel).map_err(|e| format!("Invalid database specifier \"{rel}\": {e}"))?;
        if db.contains(['/', '\\', '.', ' ', '"', '$']) {
            return Err(format!("Invalid database specifier \"{db}\": Contains disallowed characters"));
        }
        self.database = Some(db);
        Ok(())
    }

    /// _mongoc_uri_apply_query_string
    pub fn apply_query_string(&mut self, q: &str, from_dns: bool) -> R<()> {
        let mut options = Opts::default();
        let mut remain = q;
        while !remain.is_empty() {
            let (entry, rest) = remain.split_once('&').unwrap_or((remain, ""));
            remain = rest;
            self.handle_query_param(&mut options, entry, from_dns)?;
        }
        validate_names(&self.options, &options)?;
        validate_names(&options, &self.options)?;
        self.apply_options(&options, from_dns)
    }

    fn handle_query_param(&mut self, options: &mut Opts, s: &str, from_dns: bool) -> R<()> {
        let Some((key, val)) = s.split_once('=') else { return Err(format!("URI option \"{s}\" contains no \"=\" sign")) };
        if from_dns && !["authsource", "loadbalanced", "replicaset"].iter().any(|k| k.eq_ignore_ascii_case(key)) {
            return Err(format!("URI option \"{key}\" prohibited in TXT records"));
        }
        let value = pct_decode(val).map_err(|e| format!("Value for URI option \"{key}\" contains is invalid: {e}"))?;
        if key.eq_ignore_ascii_case("readpreferencetags") {
            let mut tags = Vec::new();
            let mut r = value.as_str();
            while !r.is_empty() {
                let (entry, rest) = r.split_once(',').unwrap_or((r, ""));
                r = rest;
                match entry.split_once(':') {
                    Some((k, v)) => tags.push((k.to_owned(), v.to_owned())),
                    None => {
                        self.warn(format!("Unsupported value for \"readpreferencetags\": \"{value}\""));
                        return Err(format!("Unsupported value for \"{key}\": \"{value}\""));
                    }
                }
            }
            self.read_tags.push(tags);
            return Ok(());
        }
        let existing = self.raw.get(key).or_else(|| options.get(key)).cloned();
        if let Some(OptVal::Str(prev)) = existing {
            if key.eq_ignore_ascii_case("w") && i64_parse(&prev, 0).is_err() {
                return Ok(());
            }
            if from_dns {
                if !key.eq_ignore_ascii_case("authsource") {
                    self.warn(format!("Ignoring URI option \"{key}\" from TXT record \"{s}\". Option is already present in URI"));
                }
                return Ok(());
            }
            self.warn(format!("Overwriting previously provided value for '{key}'"));
        }
        if key.eq_ignore_ascii_case("replicaset") && value.is_empty() {
            return Err(format!("Value for URI option \"{key}\" cannot be empty string"));
        }
        options.upsert(key, OptVal::Str(value));
        Ok(())
    }

    fn set_int32(&mut self, option_orig: &str, value: i32) -> R<()> {
        let option = canonicalize(option_orig);
        if option.eq_ignore_ascii_case("heartbeatfrequencyms") && value < MIN_HEARTBEAT_FREQUENCY_MS {
            return Err(format!("Invalid \"{option_orig}\" of {value}: must be at least {MIN_HEARTBEAT_FREQUENCY_MS}"));
        }
        if option.eq_ignore_ascii_case("zlibcompressionlevel") && !(-1..=9).contains(&value) {
            return Err(format!("Invalid \"{option_orig}\" of {value}: must be between -1 and 9"));
        }
        match self.options.get_mut(option) {
            Some(OptVal::Int32(v)) => *v = value,
            Some(_) => return Err(format!("Cannot set URI option \"{option}\" to {value}, it already has a non-32-bit integer value")),
            None => self.options.0.push((option.to_ascii_lowercase(), OptVal::Int32(value))),
        }
        Ok(())
    }

    fn set_int64(&mut self, option_orig: &str, value: i64) -> R<()> {
        let option = canonicalize(option_orig);
        match self.options.get_mut(option) {
            Some(OptVal::Int64(v)) => *v = value,
            Some(_) => return Err(format!("Cannot set URI option \"{option}\" to {value}, it already has a non-64-bit integer value")),
            None => self.options.0.push((option.to_ascii_lowercase(), OptVal::Int64(value))),
        }
        Ok(())
    }

    fn set_bool(&mut self, option: &str, value: bool) {
        let option = canonicalize(option);
        match self.options.get_mut(option) {
            Some(v) => *v = OptVal::Bool(value),
            None => self.options.0.push((option.to_ascii_lowercase(), OptVal::Bool(value))),
        }
    }

    /// mongoc_uri_get_option_as_bool
    pub fn get_bool(&self, option: &str, fallback: bool) -> bool {
        match self.options.get(canonicalize(option)) {
            Some(OptVal::Bool(b)) => *b,
            _ => fallback,
        }
    }

    /// mongoc_uri_get_option_as_int32 (0 means "unset": the fallback)
    pub fn get_int32(&self, option: &str, fallback: i32) -> i32 {
        let v: i64 = if option_is_int64(option) {
            let v = self.get_int64(option, 0);
            if v > i32::MAX as i64 || v < i32::MIN as i64 {
                0
            } else {
                v
            }
        } else {
            match self.options.get(canonicalize(option)) {
                Some(OptVal::Int32(i)) => *i as i64,
                _ => 0,
            }
        };
        if v == 0 {
            fallback
        } else {
            v as i32
        }
    }

    /// mongoc_uri_get_option_as_int64
    pub fn get_int64(&self, option: &str, fallback: i64) -> i64 {
        match self.options.get(canonicalize(option)) {
            Some(OptVal::Int32(i)) if *i != 0 => *i as i64,
            Some(OptVal::Int64(i)) if *i != 0 => *i,
            _ => fallback,
        }
    }

    fn set_mechanism_properties(&mut self, props: Vec<(String, String)>) {
        self.credentials.0.retain(|(k, _)| k != "authmechanismproperties");
        self.credentials.0.push(("authmechanismproperties".into(), OptVal::Doc(props)));
    }

    fn parse_mechanism_properties(&mut self, s: &str) {
        let mut props = Vec::new();
        let mut r = s;
        while !r.is_empty() {
            let (entry, rest) = r.split_once(',').unwrap_or((r, ""));
            r = rest;
            let (k, v) = entry.split_once(':').unwrap_or((entry, ""));
            props.push((k.to_owned(), v.to_owned()));
        }
        self.set_mechanism_properties(props);
    }

    /// mongoc_uri_apply_options
    fn apply_options(&mut self, options: &Opts, from_dns: bool) -> R<()> {
        for (key, v) in &options.0 {
            let OptVal::Str(value) = v else { continue };
            let key = key.as_str();
            let canon = canonicalize(key).to_owned();
            self.raw.upsert(key, OptVal::Str(value.clone()));
            let unsupported = || Err(format!("Unsupported value for \"{key}\": \"{value}\""));
            macro_rules! dupe {
                () => {
                    if from_dns {
                        if key != "authsource" {
                            self.warn(format!("Cannot override URI option \"{key}\" from TXT record"));
                        }
                        continue;
                    }
                    self.warn(format!("Overwriting previously provided value for '{key}'"));
                };
            }
            if option_is_int64(key) {
                if value.is_empty() {
                    self.warn(format!("Empty value provided for \"{key}\""));
                    continue;
                }
                let Ok(i) = i64_parse(value, 0) else { return unsupported() };
                self.set_int64(&canon, i)?;
            } else if option_is_int32(key) {
                if value.is_empty() {
                    self.warn(format!("Empty value provided for \"{key}\""));
                    continue;
                }
                let retry = canon.eq_ignore_ascii_case("maxadaptiveretries");
                let i = match i32_parse(value, 0) {
                    Ok(i) => i,
                    Err(_) if retry => {
                        self.warn(format!("Unsupported value for \"{key}\": \"{value}\""));
                        continue;
                    }
                    Err(_) => return unsupported(),
                };
                if retry && i < 0 {
                    self.warn(format!("Invalid \"{key}\" of {i}: must be a non-negative integer"));
                    continue;
                }
                self.set_int32(&canon, i)?;
            } else if key == "w" {
                if let Ok(i) = i32_parse(value, 10) {
                    match self.options.get_mut("w") {
                        Some(OptVal::Int32(v)) => *v = i,
                        Some(_) => {}
                        None => self.options.0.push(("w".into(), OptVal::Int32(i))),
                    }
                } else if value.eq_ignore_ascii_case("majority") {
                    self.options.upsert("w", OptVal::Str("majority".into()));
                } else if !value.is_empty() {
                    self.options.upsert("w", OptVal::Str(value.clone()));
                }
            } else if option_is_bool(key) {
                if key.eq_ignore_ascii_case("canonicalizehostname") {
                    self.warn("canonicalizehostname is deprecated, use authmechanismproperties with CANONICALIZE_HOST_NAME instead".into());
                }
                if value.is_empty() {
                    self.warn(format!("Empty value provided for \"{key}\""));
                    continue;
                }
                let lv = value.to_ascii_lowercase();
                let b = if lv == "true" {
                    true
                } else if lv == "false" {
                    false
                } else if value == "1" || lv == "yes" || lv == "y" || lv == "t" {
                    self.warn(format!("Deprecated boolean value for \"{key}\": \"{value}\", please update to \"{key}=true\""));
                    true
                } else if lv == "0" || lv == "-1" || value == "no" || value == "n" || value == "f" {
                    self.warn(format!("Deprecated boolean value for \"{key}\": \"{value}\", please update to \"{key}=false\""));
                    false
                } else {
                    if canon.eq_ignore_ascii_case("enableoverloadretargeting") {
                        self.warn(format!("Unsupported value for \"{key}\": \"{value}\""));
                        continue;
                    }
                    return unsupported();
                };
                self.set_bool(&canon, b);
            } else if key == "readpreferencetags" {
            } else if key == "authmechanism" || key == "authsource" {
                if self.credentials.0.iter().any(|(k, _)| k == key) {
                    dupe!();
                }
                self.credentials.upsert(&canon, OptVal::Str(value.clone()));
            } else if key == "readconcernlevel" {
                if self.read_concern_level.is_some() {
                    dupe!();
                }
                self.read_concern_level = Some(value.clone());
            } else if key == "gssapiservicename" {
                if self.credentials.0.iter().any(|(k, _)| k == "authmechanismproperties") {
                    self.warn(format!("authMechanismProperties SERVICE_NAME already set, ignoring '{key}'"));
                } else {
                    self.warn("gssapiservicename is deprecated, use authmechanismproperties with SERVICE_NAME instead".into());
                    self.parse_mechanism_properties(&format!("SERVICE_NAME:{value}"));
                }
            } else if key == "srvservicename" {
                if !check_srv_service_name(value) {
                    return unsupported();
                }
                self.options.upsert(&canon, OptVal::Str(value.clone()));
            } else if key == "authmechanismproperties" {
                if self.credentials.0.iter().any(|(k, _)| k == key) {
                    dupe!();
                }
                self.parse_mechanism_properties(value);
            } else if key == "appname" {
                if value.len() > APPNAME_MAX {
                    return unsupported();
                }
                self.options.upsert("appname", OptVal::Str(value.clone()));
            } else if key == "compressors" {
                if !self.compressors.is_empty() {
                    dupe!();
                }
                self.compressors.clear();
                let mut r = value.as_str();
                while !r.is_empty() {
                    let (c, rest) = r.split_once(',').unwrap_or((r, ""));
                    r = rest;
                    if ["snappy", "zlib", "zstd"].iter().any(|s| s.eq_ignore_ascii_case(c)) {
                        let lc = c.to_ascii_lowercase();
                        self.compressors.retain(|x| *x != lc);
                        self.compressors.push(lc);
                    } else {
                        self.warn(format!("Unsupported compressor: '{c}'"));
                    }
                }
            } else if key == "servermonitoringmode" {
                if !["stream", "poll", "auto"].contains(&value.as_str()) {
                    return unsupported();
                }
                self.options.upsert(key, OptVal::Str(value.clone()));
            } else if option_is_utf8(key) {
                self.options.upsert(&canon, OptVal::Str(value.clone()));
            } else {
                self.warn(format!("Unsupported URI option \"{key}\""));
            }
        }
        Ok(())
    }

    fn finalize(&mut self) -> R<()> {
        self.finalize_tls()?;
        self.finalize_auth()?;
        if self.get_bool("directconnection", false) {
            if self.is_srv {
                return Err("SRV URI not allowed with directConnection option".into());
            }
            if self.hosts.len() > 1 {
                return Err("Multiple seeds not allowed with directConnection option".into());
            }
        }
        if self.get_bool("loadbalanced", false) {
            if self.hosts.len() > 1 {
                return Err("URI with \"loadbalanced\" enabled must not contain more than one host".into());
            }
            if self.options.has("replicaset") {
                return Err("URI with \"loadbalanced\" enabled must not contain option \"replicaset\"".into());
            }
            if self.get_bool("directconnection", false) {
                return Err("URI with \"loadbalanced\" enabled must not contain option \"directconnection\" enabled".into());
            }
        }
        if !self.is_srv {
            let opt = if self.options.has("srvservicename") {
                Some("srvservicename")
            } else if self.options.has("srvmaxhosts") {
                Some("srvmaxhosts")
            } else {
                None
            };
            if let Some(o) = opt {
                return Err(format!("{o} must not be specified with a non-SRV URI"));
            }
        } else {
            let max = self.get_int32("srvmaxhosts", 0);
            if max < 0 {
                return Err(format!("srvmaxhosts is required to be a non-negative integer, but has value {max}"));
            }
            if max > 0 {
                if self.options.has("replicaset") {
                    return Err("srvmaxhosts must not be specified with replicaset".into());
                }
                if self.get_bool("loadbalanced", false) {
                    return Err("srvmaxhosts must not be specified with loadbalanced=true".into());
                }
            }
        }
        Ok(())
    }

    fn finalize_tls(&mut self) -> R<()> {
        if self.is_srv && !self.options.has("tls") {
            self.set_bool("tls", true);
        }
        let h = |k: &str| self.options.has(k);
        if h("tlsinsecure")
            && (h("tlsallowinvalidcertificates") || h("tlsallowinvalidhostnames") || h("tlsdisableocspendpointcheck") || h("tlsdisablecertificaterevocationcheck"))
        {
            return Err("tlsinsecure may not be specified with tlsallowinvalidcertificates, tlsallowinvalidhostnames, tlsdisableocspendpointcheck, or tlsdisablecertificaterevocationcheck".into());
        }
        if h("tlsallowinvalidcertificates") && (h("tlsdisablecertificaterevocationcheck") || h("tlsdisableocspendpointcheck")) {
            return Err("tlsallowinvalidcertificates may not be specified with tlsdisableocspendpointcheck or tlsdisablecertificaterevocationcheck".into());
        }
        if h("tlsdisablecertificaterevocationcheck") && h("tlsdisableocspendpointcheck") {
            return Err("tlsdisablecertificaterevocationcheck may not be specified with tlsdisableocspendpointcheck".into());
        }
        Ok(())
    }

    pub fn auth_mechanism(&self) -> Option<&str> {
        self.credentials.str("authmechanism")
    }
    pub fn auth_source_option(&self) -> Option<&str> {
        self.credentials.str("authsource")
    }
    /// mongoc_uri_get_auth_source
    pub fn auth_source(&self) -> &str {
        if let Some(s) = self.auth_source_option() {
            return s;
        }
        let db = self.database.as_deref();
        match self.auth_mechanism() {
            None => db.unwrap_or("admin"),
            Some(m) if m.eq_ignore_ascii_case("SCRAM-SHA-1") || m.eq_ignore_ascii_case("SCRAM-SHA-256") => db.unwrap_or("admin"),
            Some(m) if m.eq_ignore_ascii_case("PLAIN") => db.unwrap_or("$external"),
            Some(_) => "$external",
        }
    }
    pub fn mechanism_properties(&self) -> Option<&Vec<(String, String)>> {
        match self.credentials.get("authmechanismproperties") {
            Some(OptVal::Doc(d)) => Some(d),
            _ => None,
        }
    }

    fn finalize_auth(&mut self) -> R<()> {
        let source = self.auth_source_option().map(str::to_owned);
        if source.as_deref() == Some("") {
            return Err("authSource may not be specified as an empty string".into());
        }
        if let Some(s) = &source {
            if s.contains('.') {
                return Err(format!("database name \"{s}\" invalid: contains \".\""));
            }
        }
        let mech = self.auth_mechanism().map(str::to_owned);
        let (user, pass) = (self.username.clone(), self.password.clone());
        if mech.is_none() && user.is_none() {
            return Ok(());
        }
        let props = self.mechanism_properties().cloned();
        let Some(m) = mech else {
            return req_user(user.as_deref(), "default");
        };
        let mu = m.to_ascii_uppercase();
        match mu.as_str() {
            "SCRAM-SHA-1" | "SCRAM-SHA-256" | "PLAIN" => {
                req_user(user.as_deref(), &m)?;
                if pass.is_none() {
                    return Err(format!("'{m}' authentication mechanism requires a password"));
                }
            }
            "MONGODB-X509" => {
                allowed_user(user.as_deref(), &m)?;
                prohibit_pass(pass.as_deref(), &m)?;
                self.default_external(source.as_deref(), &m)?;
            }
            "GSSAPI" => {
                req_user(user.as_deref(), &m)?;
                self.default_external(source.as_deref(), &m)?;
                check_props(props.as_deref(), &["SERVICE_NAME", "CANONICALIZE_HOST_NAME", "SERVICE_REALM", "SERVICE_HOST"], "GSSAPI")?;
                if let Some(p) = &props {
                    if let Some((_, v)) = p.iter().find(|(k, _)| k.eq_ignore_ascii_case("CANONICALIZE_HOST_NAME")) {
                        if !v.eq_ignore_ascii_case("true") && !v.eq_ignore_ascii_case("false") {
                            return Err("'GSSAPI' authentication mechanism requires CANONICALIZE_HOST_NAME is either \"true\" or \"false\"".into());
                        }
                    }
                }
                if !props.as_ref().is_some_and(|p| p.iter().any(|(k, _)| k.eq_ignore_ascii_case("SERVICE_NAME"))) {
                    let mut p = props.clone().unwrap_or_default();
                    p.push(("SERVICE_NAME".into(), "mongodb".into()));
                    self.set_mechanism_properties(p);
                }
            }
            "MONGODB-AWS" => {
                allowed_user(user.as_deref(), &m)?;
                self.default_external(source.as_deref(), &m)?;
                check_props(props.as_deref(), &["AWS_SESSION_TOKEN"], "MONGODB-AWS")?;
                if user.is_none() != pass.is_none() {
                    return Err(format!("'{m}' authentication mechanism does not accept a username or a password without the other"));
                }
            }
            "MONGODB-OIDC" => {
                allowed_user(user.as_deref(), &m)?;
                self.default_external(source.as_deref(), &m)?;
                prohibit_pass(pass.as_deref(), &m)?;
                check_props(props.as_deref(), &["ENVIRONMENT", "TOKEN_RESOURCE"], "MONGODB-OIDC")?;
                if let Some(p) = &props {
                    if let Some((_, env)) = p.iter().find(|(k, _)| k.eq_ignore_ascii_case("ENVIRONMENT")) {
                        let Some(&(name, user_ok, token_req)) = OIDC_ENVS.iter().find(|(n, _, _)| n == env) else {
                            return Err(format!("'{m}' authentication has unrecognized ENVIRONMENT property '{env}'"));
                        };
                        if user.is_some() && !user_ok {
                            return Err(format!("'{m}' authentication with {name} environment does not accept a username"));
                        }
                        let has_tr = p.iter().any(|(k, _)| k.eq_ignore_ascii_case("TOKEN_RESOURCE"));
                        if has_tr && !token_req {
                            return Err(format!("'{m}' authentication with {name} environment does not accept a TOKEN_RESOURCE"));
                        }
                        if !has_tr && token_req {
                            return Err(format!("'{m}' authentication with {name} environment requires a TOKEN_RESOURCE"));
                        }
                    }
                }
            }
            _ => {
                return Err(format!(
                    "Unsupported value for authMechanism '{m}': must be one of ['MONGODB-OIDC', 'SCRAM-SHA-1', 'SCRAM-SHA-256', 'PLAIN', 'MONGODB-X509', 'GSSAPI', 'MONGODB-AWS']"
                ))
            }
        }
        Ok(())
    }

    fn default_external(&mut self, source: Option<&str>, m: &str) -> R<()> {
        match source {
            None => {
                self.credentials.0.push(("authsource".into(), OptVal::Str("$external".into())));
                Ok(())
            }
            Some(s) if !s.eq_ignore_ascii_case("$external") => {
                Err(format!("'{m}' authentication mechanism requires \"$external\" authSource, but \"{s}\" was specified"))
            }
            Some(_) => Ok(()),
        }
    }

    fn assign_read_prefs_mode(&mut self) -> R<()> {
        if let Some(p) = self.options.str("readpreference") {
            self.read_mode = match p.to_ascii_lowercase().as_str() {
                "primary" => ReadMode::Primary,
                "primarypreferred" => ReadMode::PrimaryPreferred,
                "secondary" => ReadMode::Secondary,
                "secondarypreferred" => ReadMode::SecondaryPreferred,
                "nearest" => ReadMode::Nearest,
                _ => return Err(format!("Error while assigning URI read preference: Unsupported readPreference value [readPreference={p}]")),
            };
        }
        Ok(())
    }

    fn max_staleness_option(&mut self) -> i32 {
        let Some(OptVal::Int32(v)) = self.options.get("maxstalenessseconds").cloned() else { return NO_MAX_STALENESS };
        if v == 0 || (v < 0 && v != -1) {
            self.warn(format!("Unsupported value for \"maxstalenessseconds\": \"{v}\""));
            return NO_MAX_STALENESS;
        }
        v
    }

    fn read_prefs_valid(&self) -> bool {
        if self.read_mode == ReadMode::Primary && (!self.read_tags.is_empty() || self.max_staleness_seconds != NO_MAX_STALENESS) {
            return false;
        }
        !(self.max_staleness_seconds != NO_MAX_STALENESS && self.max_staleness_seconds <= 0)
    }

    fn build_write_concern(&mut self) -> R<()> {
        let mut wc = WriteConcern::default();
        if let Some(OptVal::Bool(b)) = self.options.get("safe") {
            wc.w = if *b { 1 } else { W_UNACKNOWLEDGED };
        }
        let wt = self.get_int64("wtimeoutms", 0);
        if wt < 0 {
            return Err(format!("Unsupported wtimeoutMS value [w={wt}]"));
        }
        wc.wtimeout = wt;
        if let Some(OptVal::Bool(b)) = self.options.get("journal") {
            wc.journal = Some(*b);
        }
        let prefix = "Error while parsing the 'w' URI option";
        match self.options.get("w") {
            Some(OptVal::Int32(0)) => {
                if wc.journal == Some(true) {
                    return Err(format!("{prefix}: Journal conflicts with w value [w=0]"));
                }
                wc.w = 0;
            }
            Some(OptVal::Int32(i)) if *i > 0 => wc.w = *i,
            Some(OptVal::Int32(i)) => return Err(format!("{prefix}: Unsupported w value [w={i}]")),
            Some(OptVal::Str(s)) if s.eq_ignore_ascii_case("majority") => wc.w = W_MAJORITY,
            Some(OptVal::Str(s)) => {
                wc.w = W_TAG;
                wc.wtag = Some(s.clone());
            }
            _ => {}
        }
        self.write_concern = wc;
        Ok(())
    }
}

fn req_user(u: Option<&str>, m: &str) -> R<()> {
    if u.is_none_or(str::is_empty) {
        return Err(format!("'{m}' authentication mechanism requires a username"));
    }
    Ok(())
}
fn allowed_user(u: Option<&str>, m: &str) -> R<()> {
    if u == Some("") {
        return Err(format!("'{m}' authentication mechanism requires a non-empty username"));
    }
    Ok(())
}
fn prohibit_pass(p: Option<&str>, m: &str) -> R<()> {
    if p.is_some() {
        return Err(format!("'{m}' authentication mechanism does not accept a password"));
    }
    Ok(())
}
/// _supported_mechanism_properties_check (URI properties are always strings)
fn check_props(props: Option<&[(String, String)]>, supported: &[&str], m: &str) -> R<()> {
    for (k, _) in props.unwrap_or(&[]) {
        if !supported.iter().any(|s| s.eq_ignore_ascii_case(k)) {
            return Err(format!("Unsupported '{m}' authentication mechanism property: '{k}'"));
        }
    }
    Ok(())
}

/// mongoc_uri_options_validate_names
fn validate_names(a: &Opts, b: &Opts) -> R<()> {
    for (key, v) in &a.0 {
        let OptVal::Str(value) = v else { continue };
        let canon = canonicalize(key);
        if canon.eq_ignore_ascii_case(key) {
            continue;
        }
        for other in [a, b] {
            if let Some((_, OptVal::Str(cval))) = other.0.iter().find(|(k, _)| k == canon) {
                if cval != value {
                    return Err(format!("Deprecated option '{key}={value}' conflicts with canonical name '{canon}={cval}'"));
                }
            }
        }
    }
    Ok(())
}
