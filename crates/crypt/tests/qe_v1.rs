//! Queryable Encryption v1 payloads (pre-7.0 servers) decrypt: libmongocrypt's
//! test/data/explicit-decrypt vectors with the plaintext key material of test/data/keys.

use mongo_crypt::fle2::{IevV1, IupV1, UevV1};

fn data(name: &str) -> Vec<u8> {
    let j: serde_json::Value = serde_json::from_slice(&std::fs::read(format!("{}/tests/data/explicit-decrypt/{name}.json", env!("CARGO_MANIFEST_DIR"))).unwrap()).unwrap();
    let b64 = j["v"]["$binary"]["base64"].as_str().unwrap();
    mongo_core::bson::fmt::base64_decode(b64.as_bytes()).unwrap()
}

fn key(id: &str) -> Vec<u8> {
    let hex = std::fs::read_to_string(format!("{}/tests/data/explicit-decrypt/{id}-key-material.txt", env!("CARGO_MANIFEST_DIR"))).unwrap();
    let hex = hex.trim();
    (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap()).collect()
}

/// The BSON value bytes of a string / int32
fn string(s: &str) -> Vec<u8> {
    let mut v = ((s.len() + 1) as i32).to_le_bytes().to_vec();
    v.extend_from_slice(s.as_bytes());
    v.push(0);
    v
}

#[test]
fn v1_payloads() {
    let (k123, kabc) = (key("12345678123498761234123456789012"), key("ABCDEFAB123498761234123456789012"));
    let b = data("FLE2InsertUpdatePayload");
    let u = IupV1::parse(&b).unwrap();
    assert_eq!((u.value_type, u.decrypt(&kabc).unwrap()), (2, string("value123")));
    let b = data("FLE2InsertUpdatePayload-RangeV1");
    let u = IupV1::parse(&b).unwrap();
    assert_eq!((u.value_type, u.decrypt(&kabc).unwrap()), (16, 123456i32.to_le_bytes().to_vec()));
    let b = data("FLE2UnindexedEncryptedValue");
    let u = UevV1::parse(&b).unwrap();
    assert_eq!((u.value_type, u.decrypt(&kabc).unwrap()), (2, string("value123")));
    for (name, want, t) in [("FLE2IndexedEqualityEncryptedValue", string("value123"), 2u8), ("FLE2IndexedRangeEncryptedValue", 123456i32.to_le_bytes().to_vec(), 16)] {
        let b = data(name);
        let iev = IevV1::parse(&b).unwrap();
        let inner = iev.add_s_key(&k123).unwrap();
        assert_eq!((iev.value_type, inner.add_k_key(&kabc).unwrap()), (t, want), "{name}");
    }
    // a short value is an error, not an abort
    assert!(IevV1::parse(&[7, 1, 2]).is_err());
}
