//! BSON corpus spec tests (tests/spec/source/bson-corpus/tests/decimal128-*.json) for the
//! Decimal128 string conversions: canonical BSON -> canonical string, string -> canonical BSON,
//! degenerate forms, and the parse errors.

use mongo_core::bson::decimal128::Decimal128;
use serde_json::Value;
use std::path::PathBuf;

fn corpus_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/spec/source/bson-corpus/tests")
}

fn hex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

/// The decimal of a one-element document {"d": decimal128}.
fn dec_of(bson: &[u8]) -> Decimal128 {
    assert_eq!(bson[4], 0x13, "not a decimal128 element");
    Decimal128::from_le_bytes(bson[7..23].try_into().unwrap())
}

fn string_of(extjson: &str) -> String {
    let v: Value = serde_json::from_str(extjson).unwrap();
    v["d"]["$numberDecimal"].as_str().unwrap().to_owned()
}

#[test]
fn decimal128_corpus() {
    let mut files: Vec<_> = std::fs::read_dir(corpus_dir())
        .expect("run `git submodule update --init` for tests/spec")
        .map(|e| e.unwrap().path())
        .filter(|p| p.file_name().unwrap().to_string_lossy().starts_with("decimal128-"))
        .collect();
    files.sort();
    let (mut valid, mut errors, mut failures) = (0, 0, vec![]);
    for f in &files {
        let j: Value = serde_json::from_str(&std::fs::read_to_string(f).unwrap()).unwrap();
        for case in j["valid"].as_array().into_iter().flatten() {
            let desc = case["description"].as_str().unwrap();
            let cb = hex(case["canonical_bson"].as_str().unwrap());
            let cs = string_of(case["canonical_extjson"].as_str().unwrap());
            let d = dec_of(&cb);
            if d.to_string() != cs {
                failures.push(format!("{desc}: to_string {} != {cs}", d.to_string()));
            }
            // lossy cases (non-canonical NaN payloads, invalid significands) do not round-trip
            let lossy = case["lossy"].as_bool().unwrap_or(false);
            match Decimal128::parse(cs.as_bytes()) {
                Some(p) if p == d || lossy => {}
                p => failures.push(format!("{desc}: parse({cs}) = {p:?} != {d:?}")),
            }
            if let Some(db) = case["degenerate_bson"].as_str() {
                let dd = dec_of(&hex(db));
                if dd.to_string() != cs {
                    failures.push(format!("{desc}: degenerate to_string {} != {cs}", dd.to_string()));
                }
            }
            if let Some(dj) = case["degenerate_extjson"].as_str() {
                let ds = string_of(dj);
                match Decimal128::parse(ds.as_bytes()) {
                    Some(p) if p == d || lossy => {}
                    p => failures.push(format!("{desc}: parse degenerate({ds}) = {p:?} != {d:?}")),
                }
            }
            valid += 1;
        }
        for case in j["parseErrors"].as_array().into_iter().flatten() {
            let s = case["string"].as_str().unwrap();
            if let Some(p) = Decimal128::parse(s.as_bytes()) {
                failures.push(format!("{}: parse({s}) should fail, got {p:?}", case["description"]));
            }
            errors += 1;
        }
    }
    assert!(valid > 300 && errors > 50, "too few cases: {valid} valid, {errors} parse errors");
    assert!(failures.is_empty(), "{} of {} cases failed:\n{}", failures.len(), valid + errors, failures.join("\n"));
}
