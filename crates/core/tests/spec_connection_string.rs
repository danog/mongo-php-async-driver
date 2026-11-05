//! The connection-string specification tests (tests/spec/source/connection-string) against the
//! mongoc-uri.c port: validity, hosts and auth. Known libmongoc divergences are listed.

use mongo_core::uri::{Family, Uri};
use serde_json::Value;

/// libmongoc behaviour that the spec tests do not expect (ext-mongodb parity wins).
/// Each one checked against the reference ext-mongodb 2.5.6 build (same error message).
const DIVERGENCES: &[&str] = &[
    // libmongoc splits authMechanismProperties at every comma
    "valid-warnings.json: Comma in a key value pair causes a warning",
    // libmongoc accepts only "true"/"false" for CANONICALIZE_HOST_NAME
    "valid-auth.json: Escaped username (GSSAPI)",
    // libmongoc rejects "." in the database path
    "valid-db-with-dotted-name.json: Multiple Unix domain sockets",
];

#[test]
fn connection_string() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/spec/source/connection-string/tests");
    let mut n = 0;
    let mut failures = Vec::new();
    for f in std::fs::read_dir(dir).unwrap() {
        let p = f.unwrap().path();
        if p.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let j: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        for t in j["tests"].as_array().unwrap() {
            n += 1;
            let desc = format!("{}: {}", p.file_name().unwrap().to_string_lossy(), t["description"].as_str().unwrap());
            let r = Uri::parse(t["uri"].as_str().unwrap());
            let valid = t["valid"].as_bool().unwrap();
            match (valid, &r) {
                (true, Err(e)) => failures.push(format!("{desc}: rejected: {e}")),
                (false, Ok(_)) => failures.push(format!("{desc}: accepted")),
                (true, Ok(u)) => {
                    if t["warning"].as_bool() == Some(true) && u.warnings.is_empty() {
                        failures.push(format!("{desc}: no warning"));
                    }
                    if let Some(hosts) = t["hosts"].as_array() {
                        let got: Vec<(String, Option<u64>)> = u.hosts.iter().map(|h| (h.host.clone(), if h.family == Family::Unix { None } else { Some(h.port as u64) })).collect();
                        let want: Vec<(String, Option<u64>)> = hosts.iter().map(|h| ({ let x = h["host"].as_str().unwrap(); if h["type"] == "unix" { x.to_owned() } else { x.to_ascii_lowercase() } }, Some(h["port"].as_u64().unwrap_or(27017)).filter(|_| h["type"] != "unix"))).collect();
                        if got != want {
                            failures.push(format!("{desc}: hosts {got:?} != {want:?}"));
                        }
                    }
                    if let Some(a) = t["auth"].as_object() {
                        let s = |v: &Value| v.as_str().map(str::to_owned);
                        if u.username != s(&a["username"]) || u.password != s(&a["password"]) || u.database != s(&a["db"]) {
                            failures.push(format!("{desc}: auth {:?}/{:?}/{:?}", u.username, u.password, u.database));
                        }
                    }
                }
                _ => {}
            }
        }
    }
    failures.retain(|f| !DIVERGENCES.iter().any(|d| f.starts_with(d)));
    eprintln!("connection-string: {n} cases, {} failures", failures.len());
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
