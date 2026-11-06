//! Data key documents (`mongocrypt-key.c`) and the FLE1 ciphertext blob (`mongocrypt-ciphertext.c`).

use crate::bson::{as_uuid, validate};
use crate::opts::Kek;
use mongo_core::bson::iter::{Iter, Value};

pub type Result<T> = std::result::Result<T, String>;

/// A keyAltName: libmongocrypt keeps the BSON value; only strings are accepted when parsing keys
pub type AltName = Vec<u8>;

#[derive(Clone, Debug)]
pub struct KeyDoc {
    pub id: [u8; 16],
    pub alt_names: Vec<AltName>,
    pub key_material: Vec<u8>,
    pub kek: Kek,
    pub creation_date: i64,
    pub update_date: i64,
    /// the whole document
    pub bson: Vec<u8>,
}

/// `_mongocrypt_key_alt_name_from_iter`
fn alt_names(v: &Value) -> Result<Vec<AltName>> {
    let Value::Array(a) = v else { return Err("malformed keyAltNames, expected array".into()) };
    let mut it = Iter::new(a).ok_or("malformed keyAltNames, could not recurse into array")?;
    let mut out: Vec<AltName> = Vec::new();
    while it.next() {
        match it.value() {
            Value::Utf8(s) => out.push(s.to_vec()),
            _ => return Err("unexpected non-UTF8 keyAltName".into()),
        }
    }
    for (i, a) in out.iter().enumerate() {
        if out[i + 1..].contains(a) {
            return Err("unexpected duplicate keyAltNames".into());
        }
    }
    Ok(out)
}

impl KeyDoc {
    /// `_mongocrypt_key_parse_owned`
    pub fn parse(doc: &[u8]) -> Result<KeyDoc> {
        if !validate(doc) {
            return Err("invalid BSON".into());
        }
        let mut it = Iter::new(doc).ok_or("invalid BSON")?;
        let (mut id, mut km, mut kek, mut status, mut cd, mut ud) = (None, None, None, false, None, None);
        let mut names = Vec::new();
        while it.next() {
            let v = it.value();
            match it.key() {
                b"_id" => id = Some(as_uuid(&v).ok_or("invalid key, '_id' is not a UUID")?),
                b"keyAltNames" => names = alt_names(&v)?,
                b"keyMaterial" => match v {
                    Value::Binary(0, d) => km = Some(d.to_vec()),
                    Value::Binary(..) => return Err("invalid 'keyMaterial', expected subtype 0".into()),
                    _ => return Err("invalid 'keyMaterial', expected binary".into()),
                },
                b"masterKey" => {
                    if kek.is_some() {
                        return Err("duplicate field 'masterKey' in key document".into());
                    }
                    let Value::Document(d) = v else { return Err("invalid 'masterKey', expected document".into()) };
                    kek = Some(Kek::parse(d)?);
                }
                b"version" => {
                    let n = match v {
                        Value::Int32(i) => i as i64,
                        Value::Int64(i) => i,
                        _ => return Err("invalid 'version', expect int".into()),
                    };
                    if n != 0 {
                        return Err("unsupported key document version, only supports version=0".into());
                    }
                }
                b"status" => status = true,
                b"creationDate" => match v {
                    Value::DateTime(d) => cd = Some(d),
                    _ => return Err("invalid 'creationDate', expect datetime".into()),
                },
                b"updateDate" => match v {
                    Value::DateTime(d) => ud = Some(d),
                    _ => return Err("invalid 'updateDate', expect datetime".into()),
                },
                k => return Err(format!("unrecognized field '{}'", String::from_utf8_lossy(k))),
            }
        }
        let id = id.ok_or("invalid key, no '_id'")?;
        let kek = kek.ok_or("invalid key, no 'masterKey'")?;
        let key_material = km.ok_or("invalid key, no 'keyMaterial'")?;
        if !status {
            return Err("invalid key, no 'status'".into());
        }
        let creation_date = cd.ok_or("invalid key, no 'creationDate'")?;
        let update_date = ud.ok_or("invalid key, no 'updateDate'")?;
        Ok(KeyDoc { id, alt_names: names, key_material, kek, creation_date, update_date, bson: doc.to_vec() })
    }
}

pub const FLE1_DETERMINISTIC: u8 = 1;
pub const FLE1_RANDOM: u8 = 2;

/// The FLE1 blob: subtype || key UUID || original BSON type || ciphertext
#[derive(Clone, Debug)]
pub struct Ciphertext<'a> {
    pub blob_subtype: u8,
    pub key_id: [u8; 16],
    pub original_bson_type: u8,
    pub data: &'a [u8],
}

impl<'a> Ciphertext<'a> {
    /// `_mongocrypt_ciphertext_parse_unowned`
    pub fn parse(b: &'a [u8]) -> Result<Ciphertext<'a>> {
        if b.len() < 19 {
            return Err("malformed ciphertext, too small".into());
        }
        if b[0] != 1 && b[0] != 2 {
            return Err("malformed ciphertext, expected blob subtype of 1 or 2".into());
        }
        Ok(Ciphertext {
            blob_subtype: b[0],
            key_id: b[1..17].try_into().expect("16 bytes"),
            original_bson_type: b[17],
            data: &b[18..],
        })
    }

    /// `_mongocrypt_ciphertext_serialize_associated_data`
    pub fn associated_data(&self) -> Option<[u8; 18]> {
        if self.original_bson_type == 0 || (self.blob_subtype != FLE1_DETERMINISTIC && self.blob_subtype != FLE1_RANDOM) {
            return None;
        }
        let mut a = [0u8; 18];
        a[0] = self.blob_subtype;
        a[1..17].copy_from_slice(&self.key_id);
        a[17] = self.original_bson_type;
        Some(a)
    }

    /// `_mongocrypt_serialize_ciphertext`
    pub fn serialize(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(18 + self.data.len());
        v.push(self.blob_subtype);
        v.extend_from_slice(&self.key_id);
        v.push(self.original_bson_type);
        v.extend_from_slice(self.data);
        v
    }
}
