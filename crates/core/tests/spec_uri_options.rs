//! The uri-options specification tests (tests/spec/source/uri-options) against the mongoc-uri.c
//! port, with the exceptions libmongoc's own runner (test-mongoc-connection-uri.c) applies.

use mongo_core::uri::{OptVal, Uri};
use serde_json::{Map, Value};

/// Skipped by libmongoc's runner too (features libmongoc does not have).
const SKIPS: &[&str] = &[
    "Valid connection pool options are parsed correctly",
    "Valid connection and timeout options are parsed correctly",
    "timeoutMS=0",
    "Non-numeric timeoutMS causes a warning",
    "Too low timeoutMS causes a warning",
    "proxyPort without proxyHost",
    "proxyUsername without proxyHost",
    "proxyPassword without proxyHost",
    "all other proxy options without proxyHost",
    "proxyUsername without proxyPassword",
    "proxyPassword without proxyUsername",
    "multiple proxyHost parameters",
    "multiple proxyPort parameters",
    "multiple proxyUsername parameters",
    "multiple proxyPassword parameters",
    "only host present",
    "host and default port present",
    "host and non-default port present",
    "replicaset, host and non-default port present",
    "all options present",
    "minPoolSize=0 does not error",
    "should throw an exception if neither environment nor callbacks specified (MONGODB-OIDC)",
];

/// libmongoc behaviour the spec does not expect; each checked against the reference build.
const DIVERGENCES: &[&str] = &[
    // libmongoc does not know srvAllowedHostsSuffix: an unknown option only warns
    "srv-options.json: Non-SRV URI with srvAllowedHostsSuffix",
];

fn opt_json(v: &OptVal) -> Value {
    match v {
        OptVal::Str(s) => Value::String(s.clone()),
        OptVal::Int32(i) => Value::from(*i),
        OptVal::Int64(i) => Value::from(*i),
        OptVal::Bool(b) => Value::Bool(*b),
        OptVal::Doc(d) => Value::Object(d.iter().map(|(k, v)| (k.clone(), Value::String(v.clone()))).collect()),
    }
}

fn find_case<'a>(m: &'a Map<String, Value>, k: &str) -> Option<&'a Value> {
    m.iter().find(|(x, _)| x.eq_ignore_ascii_case(k)).map(|(_, v)| v)
}

/// bson_contains_iter: every key of `want` (case-insensitive) is in `have` with an equal value.
fn contains(have: &Map<String, Value>, want: &Map<String, Value>, path: &str) -> Result<(), String> {
    for (k, w) in want {
        let Some(h) = find_case(have, k) else { return Err(format!("{path}{k} is not present")) };
        let ok = match w {
            Value::Object(wm) => match h {
                Value::Object(hm) => {
                    contains(hm, wm, &format!("{path}{k}."))?;
                    true
                }
                _ => false,
            },
            Value::Array(_) => return Err(format!("{path}{k}: array expectation")),
            Value::Bool(b) => match h {
                Value::Bool(x) => x == b,
                Value::Number(n) => (n.as_f64() != Some(0.0)) == *b,
                _ => false,
            },
            Value::String(s) => h.as_str() == Some(s),
            Value::Number(n) => h.as_f64() == n.as_f64(),
            Value::Null => false,
        };
        if !ok {
            return Err(format!("{path}{k}: {h} != {w}"));
        }
    }
    Ok(())
}

/// Rename mechanism -> authmechanism and mechanism_properties -> authmechanismproperties.
fn normalize_auth(src: &Map<String, Value>, exclude: &[&str]) -> Map<String, Value> {
    let mut out: Map<String, Value> = src.iter().filter(|(k, _)| !exclude.contains(&k.as_str())).map(|(k, v)| (k.clone(), v.clone())).collect();
    if let Some(v) = src.get("mechanism").or(src.get("authmechanism")).filter(|v| v.is_string()) {
        out.insert("authmechanism".into(), v.clone());
    }
    if let Some(v) = src.get("mechanism_properties").or(src.get("authmechanismproperties")).filter(|v| v.is_object()) {
        out.insert("authmechanismproperties".into(), v.clone());
    }
    out
}

const CRED_KEYS: &[&str] = &["authmechanism", "authmechanismproperties", "mechanism", "mechanism_properties"];

fn run(t: &Value) -> Result<(), String> {
    let uri_s = t["uri"].as_str().unwrap();
    let valid = t["valid"].as_bool().unwrap_or(true);
    let r = Uri::parse(uri_s);
    let obj = |k: &str| t.get(k).and_then(Value::as_object);
    let (auth, mut options, credential) = (obj("auth"), obj("options").cloned(), obj("credential"));
    // BEGIN exceptions (as libmongoc's runner)
    if let Some(a) = auth {
        if let Some(db) = a.get("db").or(a.get("source")).and_then(Value::as_str) {
            if db.contains('.') {
                return if r.is_err() { Ok(()) } else { Err("accepted a dotted auth db".into()) };
            }
        }
    }
    let mut warned = false;
    if valid {
        if let Err(e) = &r {
            let eager = ["=invalid", "heartbeatFrequencyMS=-2", "w=-2", "wTimeoutMS=-2", "zlibCompressionLevel=-2", "zlibCompressionLevel=10", "srvMaxHosts=-1", "srvMaxHosts=foo"];
            if eager.iter().any(|x| uri_s.contains(x)) || (!uri_s.contains("mongodb+srv") && uri_s.contains("srvServiceName=customname")) || uri_s.contains("TOKEN_RESOURCE:mongodb://host1%2Chost2") {
                let _ = e;
                return Ok(()); // libmongoc fails eagerly where the spec expects a warning (CDRIVER-3167/5580)
            }
            if uri_s.contains("CANONICALIZE_HOST_NAME:none") || uri_s.contains("CANONICALIZE_HOST_NAME:forward") {
                return Ok(()); // CDRIVER-4128
            }
        }
    }
    if let Ok(u) = &r {
        let neg = |k: &str| u.get_int32(k, 0) < 0;
        if neg("connecttimeoutms") || neg("localthresholdms") || neg("serverselectiontimeoutms") || neg("sockettimeoutms") {
            warned = true; // the runner logs a warning itself
        }
        if uri_s.contains("compressors=") || uri_s.contains("readPreferenceTags=") {
            options = None;
        }
    }
    // END exceptions
    let u = match (valid, r) {
        (true, Err(e)) => return Err(format!("rejected: {e}")),
        (false, Ok(_)) => return Err("accepted".into()),
        (false, Err(_)) => return Ok(()),
        (true, Ok(u)) => u,
    };
    warned |= !u.warnings.is_empty();
    if let Some(hosts) = t.get("hosts").and_then(Value::as_array) {
        for h in hosts {
            let host = h["host"].as_str().unwrap_or("localhost");
            let port = h["port"].as_u64().unwrap_or(27017);
            if !u.hosts.iter().any(|x| x.host == host && x.port as u64 == port) {
                return Err(format!("host {host}:{port} not found"));
            }
        }
    }
    if let Some(a) = auth {
        if let Some(x) = a.get("username").and_then(Value::as_str) {
            if u.username.as_deref() != Some(x) {
                return Err(format!("username {:?}", u.username));
            }
        }
        if let Some(x) = a.get("password").and_then(Value::as_str) {
            if u.password.as_deref() != Some(x) {
                return Err(format!("password {:?}", u.password));
            }
        }
        if let Some(x) = a.get("db").or(a.get("source")).and_then(Value::as_str) {
            if u.auth_source() != x {
                return Err(format!("auth source {:?}", u.auth_source()));
            }
        }
    }
    let mut actual = Map::new();
    for (k, v) in u.options.0.iter().chain(u.credentials.0.iter()) {
        actual.insert(k.clone(), opt_json(v));
    }
    if let Some(l) = &u.read_concern_level {
        actual.insert("readconcernlevel".into(), Value::String(l.clone()));
    }
    if let Some(o) = &options {
        let mut excl = vec!["username", "password", "source"];
        excl.extend_from_slice(CRED_KEYS);
        contains(&actual, &normalize_auth(o, &excl), "")?;
    }
    if let Some(c) = credential {
        for (k, v) in normalize_auth(c, CRED_KEYS) {
            let ok = match (k.as_str(), v) {
                (_, Value::Null) => true,
                ("username", Value::String(s)) => u.username.as_deref() == Some(&s),
                ("password", Value::String(s)) => u.password.as_deref() == Some(&s),
                ("source", Value::String(s)) => u.auth_source() == s,
                ("authmechanism", Value::String(s)) => u.auth_mechanism() == Some(&s),
                ("authmechanismproperties", Value::Object(mut p)) => {
                    if let Some(Value::Bool(b)) = p.get("CANONICALIZE_HOST_NAME") {
                        let s = if *b { "true" } else { "false" };
                        p.insert("CANONICALIZE_HOST_NAME".into(), Value::String(s.into()));
                    }
                    let Some(have) = u.mechanism_properties() else { return Err("no authmechanismproperties".into()) };
                    let have: Map<String, Value> = have.iter().map(|(k, v)| (k.clone(), Value::String(v.clone()))).collect();
                    contains(&have, &p, "authmechanismproperties.")?;
                    true
                }
                (k, v) => return Err(format!("unexpected credential field {k}: {v}")),
            };
            if !ok {
                return Err(format!("credential {k} mismatch"));
            }
        }
    }
    match t.get("warning").and_then(Value::as_bool) {
        Some(true) if !warned => Err("no warning".into()),
        Some(false) if warned => Err(format!("unexpected warning: {:?}", u.warnings)),
        _ => Ok(()),
    }
}

#[test]
fn uri_options() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/spec/source/uri-options/tests");
    let (mut n, mut skipped) = (0, 0);
    let mut failures = Vec::new();
    let mut files: Vec<_> = std::fs::read_dir(dir).unwrap().map(|f| f.unwrap().path()).filter(|p| p.extension().is_some_and(|e| e == "json")).collect();
    files.sort();
    for p in files {
        let j: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        for t in j["tests"].as_array().unwrap() {
            let d = t["description"].as_str().unwrap();
            if SKIPS.contains(&d) {
                skipped += 1;
                continue;
            }
            n += 1;
            if let Err(e) = run(t) {
                failures.push(format!("{}: {d}: {e} [{}]", p.file_name().unwrap().to_string_lossy(), t["uri"].as_str().unwrap()));
            }
        }
    }
    failures.retain(|f| !DIVERGENCES.iter().any(|d| f.starts_with(d)));
    eprintln!("uri-options: {n} cases run, {skipped} skipped, {} failures", failures.len());
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
