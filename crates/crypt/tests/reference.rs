//! Byte-for-byte parity with ext-mongodb 2.5.6 / libmongocrypt 1.21.1: tests/data/ref-fle1.json holds a
//! local data key and the reference's deterministic and random FLE1 ciphertexts of several values.
use mongo_core::bson::writer::Writer;
use mongo_crypt::engine::{Crypt, Fle1Algorithm, Io, KeySel};
use serde_json::Value;

fn hex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

struct Vault(Vec<u8>);
impl Io for Vault {
    type Error = ();
    fn kms(&mut self, _r: &mongo_crypt::kms::KmsRequest, _d: &mut dyn FnMut(&[u8]) -> bool) -> Result<(), mongo_crypt::engine::KmsFailure<()>> {
        unreachable!("local KMS only")
    }
    fn stream_error(&self, _m: String) {}
    fn find_keys(&mut self, _f: &[u8]) -> Result<Vec<Vec<u8>>, ()> {
        Ok(vec![self.0.clone()])
    }
}

#[test]
fn fle1_matches_reference() {
    let j: Value = serde_json::from_slice(&std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/ref-fle1.json")).unwrap()).unwrap();
    let key = hex(j["key"].as_str().unwrap());
    let id: [u8; 16] = key[14..30].try_into().unwrap();
    let mut l = Writer::new();
    l.binary(b"key", 0, &[7u8; 96]);
    let mut k = Writer::new();
    k.document(b"local", &l.finish());
    let crypt = Crypt::new(&k.finish(), None).unwrap();
    let mut vault = Vault(key);
    for c in j["cases"].as_array().unwrap() {
        let value = hex(c["value"].as_str().unwrap());
        let det = hex(c["deterministic"].as_str().unwrap());
        let ours = crypt.encrypt_fle1(&mut vault, &value, &KeySel::Id(id), Fle1Algorithm::Deterministic).unwrap();
        assert_eq!(ours, det, "deterministic ciphertext of {}", c["value"]);
        for ct in [det, hex(c["random"].as_str().unwrap())] {
            let mut m = Writer::new();
            m.binary(b"v", 6, &ct);
            assert_eq!(crypt.explicit_decrypt(&mut vault, &m.finish()).unwrap(), value);
        }
    }
}
