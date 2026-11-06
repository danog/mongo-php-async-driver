//! Local-KMS data keys, FLE1 explicit encryption and decryption, against an in-memory key vault.
use mongo_core::bson::iter::{Iter, Value};
use mongo_core::bson::writer::Writer;
use mongo_crypt::engine::{Crypt, Error, Fle1Algorithm, Io, KeySel};
use mongo_crypt::opts::Kek;

struct Vault {
    keys: Vec<Vec<u8>>,
    finds: usize,
}

impl Io for Vault {
    type Error = ();
    fn kms(&mut self, _r: &mongo_crypt::kms::KmsRequest, _d: &mut dyn FnMut(&[u8]) -> bool) -> Result<(), mongo_crypt::engine::KmsFailure<()>> {
        unreachable!("local KMS only")
    }
    fn stream_error(&self, _m: String) {}
    fn find_keys(&mut self, _filter: &[u8]) -> Result<Vec<Vec<u8>>, ()> {
        self.finds += 1;
        Ok(self.keys.clone())
    }
}

fn kms() -> Vec<u8> {
    let mut l = Writer::new();
    l.binary(b"key", 0, &[7u8; 96]);
    let mut w = Writer::new();
    w.document(b"local", &l.finish());
    w.finish()
}

fn local_kek() -> Kek {
    let mut w = Writer::new();
    w.utf8(b"provider", b"local");
    Kek::parse(&w.finish()).unwrap()
}

fn v(f: impl FnOnce(&mut Writer)) -> Vec<u8> {
    let mut w = Writer::new();
    f(&mut w);
    w.finish()
}

fn key_id(doc: &[u8]) -> [u8; 16] {
    let mut it = Iter::new(doc).unwrap();
    assert!(it.find(b"_id"));
    match it.value() {
        Value::Binary(4, d) => d.try_into().unwrap(),
        _ => panic!(),
    }
}

#[test]
fn local_roundtrip() {
    let crypt = Crypt::new(&kms(), None).unwrap();
    let doc = crypt.data_key_doc(&mut Vault { keys: vec![], finds: 0 }, &local_kek(), None, &[b"name".to_vec()]).unwrap();
    let id = key_id(&doc);
    let mut vault = Vault { keys: vec![doc], finds: 0 };
    let value = v(|w| w.utf8(b"v", b"hello"));
    let a = crypt.encrypt_fle1(&mut vault, &value, &KeySel::Id(id), Fle1Algorithm::Deterministic).unwrap();
    let b = crypt.encrypt_fle1(&mut vault, &value, &KeySel::AltName(b"name".to_vec()), Fle1Algorithm::Deterministic).unwrap();
    assert_eq!(a, b, "deterministic");
    assert_eq!(a[0], 1);
    assert_eq!(&a[1..17], &id);
    assert_eq!(a[17], 2);
    let r = crypt.encrypt_fle1(&mut vault, &value, &KeySel::Id(id), Fle1Algorithm::Random).unwrap();
    assert_ne!(a, r);
    // the key cache: one key vault query for all of the above
    assert_eq!(vault.finds, 1);
    for ct in [&a, &r] {
        let msg = v(|w| w.binary(b"v", 6, ct));
        let out = crypt.explicit_decrypt(&mut vault, &msg).unwrap();
        assert_eq!(out, value);
    }
    // nested documents keep everything else
    let nested = v(|w| {
        w.int32(b"a", 1);
        let p = w.begin(3, b"b");
        w.binary(b"x", 6, &a);
        w.binary(b"y", 0, b"plain");
        w.end(p);
    });
    let out = crypt.decrypt_doc(&mut vault, &nested).unwrap();
    let expect = v(|w| {
        w.int32(b"a", 1);
        let p = w.begin(3, b"b");
        w.utf8(b"x", b"hello");
        w.binary(b"y", 0, b"plain");
        w.end(p);
    });
    assert_eq!(out, expect);
    // errors
    let bad = v(|w| w.double(b"v", 1.5));
    match crypt.encrypt_fle1(&mut vault, &bad, &KeySel::Id(id), Fle1Algorithm::Deterministic) {
        Err(Error::Crypt(m)) => assert_eq!(m, "BSON type invalid for deterministic encryption"),
        o => panic!("{o:?}"),
    }
    let mut tampered = a.clone();
    *tampered.last_mut().unwrap() ^= 1;
    match crypt.explicit_decrypt(&mut vault, &v(|w| w.binary(b"v", 6, &tampered))) {
        Err(Error::Crypt(m)) => assert_eq!(m, "HMAC validation failure"),
        o => panic!("{o:?}"),
    }
    let other = Crypt::new(&kms(), None).unwrap();
    let mut empty = Vault { keys: vec![], finds: 0 };
    match other.encrypt_fle1(&mut empty, &value, &KeySel::Id(id), Fle1Algorithm::Random) {
        Err(Error::Crypt(m)) => assert!(m.starts_with("not all keys requested were satisfied"), "{m}"),
        o => panic!("{o:?}"),
    }
}
