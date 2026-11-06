//! libmongocrypt's test/data/roundtrip vectors (test-mongocrypt-crypto.c `_test_roundtrip_single`).
use mongo_crypt::crypto::{Algorithm, IV_LEN};
use serde_json::Value;

fn hex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

fn algo(s: &str) -> Algorithm {
    match s {
        "AES-256-CBC/SHA-512-256" => Algorithm::Fle1,
        "AES-256-CTR/SHA-256" => Algorithm::Fle2Aead,
        "AES-256-CTR/NONE" => Algorithm::Fle2,
        "AES-256-CBC/SHA-256" => Algorithm::Fle2v2Aead,
        o => panic!("unknown algo {o}"),
    }
}

fn has_hmac(a: Algorithm) -> bool {
    a != Algorithm::Fle2
}

#[test]
fn roundtrip_vectors() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/roundtrip");
    let mut n = 0;
    for e in std::fs::read_dir(dir).unwrap() {
        let path = e.unwrap().path();
        let tests: Vec<Value> = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        for t in tests {
            let name = t["name"].as_str().unwrap();
            let a = algo(t["algo"].as_str().unwrap());
            let iv = hex(t["iv"].as_str().unwrap());
            let aad = hex(t.get("aad").and_then(Value::as_str).unwrap_or(""));
            let key = hex(t["key"].as_str().unwrap());
            let plaintext = hex(t.get("plaintext").and_then(Value::as_str).unwrap_or(""));
            let ciphertext = hex(t.get("ciphertext").and_then(Value::as_str).unwrap_or(""));
            let enc = a.encrypt(&iv, Some(&aad), &key, &plaintext);
            if let Some(err) = t.get("encrypt_error").and_then(Value::as_str) {
                let got = enc.expect_err(name);
                assert!(got.contains(err), "{name}: encrypt error {got:?} lacks {err:?}");
                n += 1;
                continue;
            }
            if t.get("ignore_ciphertext_mismatch_on_encrypt").and_then(Value::as_bool) != Some(true) {
                assert_eq!(enc.expect(name), ciphertext, "{name}: ciphertext");
            }
            let dec = a.decrypt(Some(&aad), &key, &ciphertext);
            if let Some(err) = t.get("decrypt_error").and_then(Value::as_str) {
                let got = dec.expect_err(name);
                assert!(got.contains(err), "{name}: decrypt error {got:?} lacks {err:?}");
                n += 1;
                continue;
            }
            assert_eq!(dec.expect(name), plaintext, "{name}: plaintext");
            // mutations: with a MAC every one must fail; without, the output must differ
            for pos in [0, IV_LEN, ciphertext.len() - 1] {
                let mut m = ciphertext.clone();
                m[pos] ^= 1;
                match a.decrypt(Some(&aad), &key, &m) {
                    Err(e) => assert!(has_hmac(a), "{name}@{pos}: unexpected error {e}"),
                    Ok(p) => {
                        assert!(!has_hmac(a), "{name}@{pos}: mutation accepted");
                        assert_ne!(p, plaintext, "{name}@{pos}: same plaintext");
                    }
                }
            }
            n += 1;
        }
    }
    assert!(n > 40, "only {n} vectors");
}
