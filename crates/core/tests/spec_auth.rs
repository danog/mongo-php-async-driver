//! The auth specification's connection-string tests (tests/spec/source/auth/tests/legacy) against
//! the mongoc-uri.c port, checked as libmongoc's runner does (test-mongoc-connection-uri.c):
//! validity, then username, password, source, mechanism and the listed mechanism properties
//! (CANONICALIZE_HOST_NAME is a string in libmongoc, CDRIVER-4128).

use mongo_core::uri::Uri;
use serde_json::Value;

/// libmongoc behaviour that the spec tests do not expect (ext-mongodb parity wins). Each one
/// checked against the reference ext-mongodb 2.5.6 build (same result, same message).
const DIVERGENCES: &[&str] = &[
    // libmongoc accepts only "true"/"false" for CANONICALIZE_HOST_NAME
    "should accept generic mechanism property (GSSAPI)",
    "should accept forwardAndReverse hostname canonicalization (GSSAPI)",
    "should accept no hostname canonicalization (GSSAPI)",
    // libmongoc does not reject these MONGODB-AWS / MONGODB-OIDC combinations when parsing
    // (both mechanisms fail closed here when a command is run)
    "should throw an exception if username and password provided (MONGODB-AWS)",
    "should throw an exception if AWS_SESSION_TOKEN provided (MONGODB-AWS)",
    "should throw an exception if neither environment nor callbacks specified (MONGODB-OIDC)",
];

#[test]
fn auth_connection_string() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/spec/source/auth/tests/legacy/connection-string.json");
    let j: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let mut failures = Vec::new();
    let tests = j["tests"].as_array().unwrap();
    for t in tests {
        let desc = t["description"].as_str().unwrap();
        let r = Uri::parse(t["uri"].as_str().unwrap());
        let valid = t["valid"].as_bool().unwrap();
        let u = match (valid, r) {
            (true, Err(e)) => {
                failures.push(format!("{desc}: rejected: {e}"));
                continue;
            }
            (false, Ok(_)) => {
                failures.push(format!("{desc}: accepted"));
                continue;
            }
            (false, Err(_)) => continue,
            (true, Ok(u)) => u,
        };
        let c = &t["credential"];
        if c.is_null() {
            if u.username.is_some() || u.auth_mechanism().is_some() {
                failures.push(format!("{desc}: credential {:?}/{:?}", u.username, u.auth_mechanism()));
            }
            continue;
        }
        let s = |v: &Value| v.as_str().map(str::to_owned);
        if let Some(x) = c["username"].as_str() {
            if u.username.as_deref() != Some(x) {
                failures.push(format!("{desc}: username {:?}", u.username));
            }
        }
        if let Some(x) = c["password"].as_str() {
            if u.password.as_deref() != Some(x) {
                failures.push(format!("{desc}: password differs"));
            }
        }
        if let Some(x) = c["source"].as_str() {
            if u.auth_source() != x {
                failures.push(format!("{desc}: source {:?}", u.auth_source()));
            }
        }
        if s(&c["mechanism"]).as_deref() != u.auth_mechanism() {
            failures.push(format!("{desc}: mechanism {:?}", u.auth_mechanism()));
        }
        if let Some(props) = c["mechanism_properties"].as_object() {
            let got = u.mechanism_properties().cloned().unwrap_or_default();
            for (k, v) in props {
                let want = match v {
                    Value::Bool(b) => b.to_string(),
                    Value::String(x) => x.clone(),
                    v => v.to_string(),
                };
                if !got.iter().any(|(gk, gv)| gk.eq_ignore_ascii_case(k) && *gv == want) {
                    failures.push(format!("{desc}: property {k}={want} not in {got:?}"));
                }
            }
        }
    }
    failures.retain(|f| !DIVERGENCES.iter().any(|d| f.starts_with(d)));
    eprintln!("auth connection-string: {} cases, {} failures", tests.len(), failures.len());
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
