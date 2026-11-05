//! The whole BSON corpus (tests/spec/source/bson-corpus/tests/*.json) against the libbson port:
//! valid cases (canonical/relaxed Extended JSON output, JSON input, degenerate forms),
//! decodeErrors (corrupt BSON must be rejected) and parseErrors (bad Extended JSON must be
//! rejected). Decimal128 string parse errors are covered by spec_bson_corpus_decimal128.rs.

use mongo_core::bson::iter::{self, Iter};
use mongo_core::bson::json::{to_json, Mode};
use mongo_core::bson::json_parse::from_json;
use serde_json::Value;
use std::path::PathBuf;

fn corpus_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/spec/source/bson-corpus/tests")
}

fn hex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

fn tohex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{:02X}", x)).collect()
}

fn same_json(a: &[u8], b: &str) -> bool {
    let a: Value = match serde_json::from_slice(a) {
        Ok(v) => v,
        Err(_) => return false,
    };
    let b: Value = serde_json::from_str(b).unwrap();
    a == b
}

/// Full decode as phongo does it: read one document, exhaust the buffer, visit everything.
fn decodes(b: &[u8]) -> bool {
    match iter::read_first(b) {
        Some((d, true)) => Iter::new(d).is_some() && to_json(d, Mode::Canonical, false).is_some(),
        _ => false,
    }
}

/// Cases where libbson 2.5.8 (and therefore ext-mongodb) knowingly differs from the corpus; the
/// port keeps libbson's behaviour.
const LIBBSON_DIVERGENCES: &[(&str, &str)] = &[
    // "%.20g" prints 1234567892123200000.0 (same double; the PHPT compares after json_decode)
    ("double.json: 1.2345678921232E+18: cB->cEJ", "libbson formats doubles with %.20g"),
    ("double.json: -1.2345678921232E+18: cB->cEJ", "libbson formats doubles with %.20g"),
    // CDRIVER-2223: legacy {"$date": <number>} is accepted (XFAIL in the PHPT suite too)
    ("top.json: Bad $date (number, not string or hash): bad JSON was accepted", "CDRIVER-2223"),
];

#[test]
fn bson_corpus() {
    let mut files: Vec<_> = std::fs::read_dir(corpus_dir())
        .expect("run `git submodule update --init` for tests/spec")
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    files.sort();
    let (mut n, mut failures) = (0, Vec::<String>::new());
    for f in &files {
        let fname = f.file_name().unwrap().to_string_lossy().to_string();
        let j: Value = serde_json::from_str(&std::fs::read_to_string(f).unwrap()).unwrap();
        let is_dec = fname.starts_with("decimal128-");
        for case in j["valid"].as_array().into_iter().flatten() {
            let desc = format!("{fname}: {}", case["description"].as_str().unwrap());
            let cb = hex(case["canonical_bson"].as_str().unwrap());
            let cej = case["canonical_extjson"].as_str().unwrap();
            let lossy = case["lossy"].as_bool().unwrap_or(false);
            n += 1;
            if !decodes(&cb) {
                failures.push(format!("{desc}: canonical BSON does not decode"));
                continue;
            }
            match to_json(&cb, Mode::Canonical, false) {
                Some(out) if same_json(&out, cej) => {}
                out => failures.push(format!("{desc}: cB->cEJ {:?} != {cej}", out.map(|o| String::from_utf8_lossy(&o).into_owned()))),
            }
            if let Some(rej) = case["relaxed_extjson"].as_str() {
                match to_json(&cb, Mode::Relaxed, false) {
                    Some(out) if same_json(&out, rej) => {}
                    out => failures.push(format!("{desc}: cB->rEJ {:?} != {rej}", out.map(|o| String::from_utf8_lossy(&o).into_owned()))),
                }
                match from_json(rej.as_bytes()) {
                    Ok(b) => match to_json(&b, Mode::Relaxed, false) {
                        Some(out) if same_json(&out, rej) => {}
                        out => failures.push(format!("{desc}: rEJ->BSON->rEJ {:?}", out.map(|o| String::from_utf8_lossy(&o).into_owned()))),
                    },
                    Err(e) => failures.push(format!("{desc}: rEJ parse failed: {e}")),
                }
            }
            match from_json(cej.as_bytes()) {
                Ok(b) if lossy || b == cb => {}
                Ok(b) => failures.push(format!("{desc}: cEJ->BSON {} != {}", tohex(&b), tohex(&cb))),
                Err(e) => failures.push(format!("{desc}: cEJ parse failed: {e}")),
            }
            if let Some(db) = case["degenerate_bson"].as_str() {
                let db = hex(db);
                match to_json(&db, Mode::Canonical, false) {
                    Some(out) if same_json(&out, cej) || is_dec => {}
                    out => failures.push(format!("{desc}: dB->cEJ {:?}", out.map(|o| String::from_utf8_lossy(&o).into_owned()))),
                }
            }
            if let Some(dej) = case["degenerate_extjson"].as_str() {
                match from_json(dej.as_bytes()) {
                    Ok(b) if lossy || b == cb => {}
                    Ok(b) => failures.push(format!("{desc}: dEJ->BSON {} != {}", tohex(&b), tohex(&cb))),
                    Err(e) => failures.push(format!("{desc}: dEJ parse failed: {e}")),
                }
            }
        }
        for case in j["decodeErrors"].as_array().into_iter().flatten() {
            n += 1;
            let desc = format!("{fname}: {}", case["description"].as_str().unwrap());
            if decodes(&hex(case["bson"].as_str().unwrap())) {
                failures.push(format!("{desc}: corrupt BSON was accepted"));
            }
        }
        if !is_dec {
            for case in j["parseErrors"].as_array().into_iter().flatten() {
                n += 1;
                let desc = format!("{fname}: {}", case["description"].as_str().unwrap());
                let s = case["string"].as_str().unwrap();
                if let Ok(b) = from_json(s.as_bytes()) {
                    failures.push(format!("{desc}: bad JSON was accepted: {}", tohex(&b)));
                }
            }
        }
    }
    let before = failures.len();
    failures.retain(|f| !LIBBSON_DIVERGENCES.iter().any(|(p, _)| f.starts_with(p)));
    eprintln!("bson corpus: {} cases, {} failures, {} known libbson divergences", n, failures.len(), before - failures.len());
    assert!(failures.is_empty(), "{} failures:\n{}", failures.len(), failures.join("\n"));
}
