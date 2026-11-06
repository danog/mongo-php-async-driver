//! KMIP 1.0/1.2 over TTLV (kms-message's `kms_kmip_request.c`, `kms_kmip_response.c`,
//! `kms_kmip_reader_writer.c`, `kms_kmip_response_parser.c`): the requests libmongocrypt sends and
//! the response fields it reads. Every read is bounds-checked; messages are kms-message's.

pub type Result<T> = std::result::Result<T, String>;

const STRUCTURE: u8 = 0x01;
const INTEGER: u8 = 0x02;
const ENUMERATION: u8 = 0x05;
const BOOLEAN: u8 = 0x06;
const TEXT_STRING: u8 = 0x07;
const BYTE_STRING: u8 = 0x08;

// tags (0x42xxxx)
const REQUEST_MESSAGE: u32 = 0x420078;
const REQUEST_HEADER: u32 = 0x420077;
const PROTOCOL_VERSION: u32 = 0x420069;
const PROTOCOL_VERSION_MAJOR: u32 = 0x42006A;
const PROTOCOL_VERSION_MINOR: u32 = 0x42006B;
const BATCH_COUNT: u32 = 0x42000D;
const BATCH_ITEM: u32 = 0x42000F;
const OPERATION: u32 = 0x42005C;
const REQUEST_PAYLOAD: u32 = 0x420079;
const OBJECT_TYPE: u32 = 0x420057;
const TEMPLATE_ATTRIBUTE: u32 = 0x420091;
const ATTRIBUTE: u32 = 0x420008;
const ATTRIBUTE_NAME: u32 = 0x42000A;
const ATTRIBUTE_VALUE: u32 = 0x42000B;
const SECRET_DATA: u32 = 0x420085;
const SECRET_DATA_TYPE: u32 = 0x420086;
const KEY_BLOCK: u32 = 0x420040;
const KEY_FORMAT_TYPE: u32 = 0x420042;
const KEY_VALUE: u32 = 0x420045;
const KEY_MATERIAL: u32 = 0x420043;
const UNIQUE_IDENTIFIER: u32 = 0x420094;
const CRYPTOGRAPHIC_PARAMETERS: u32 = 0x42002B;
const BLOCK_CIPHER_MODE: u32 = 0x420011;
const PADDING_METHOD: u32 = 0x42005F;
const CRYPTOGRAPHIC_ALGORITHM: u32 = 0x420028;
const RANDOM_IV: u32 = 0x4200C5;
const DATA: u32 = 0x4200C2;
const IV_COUNTER_NONCE: u32 = 0x42003D;
const RESPONSE_MESSAGE: u32 = 0x42007B;
const RESPONSE_PAYLOAD: u32 = 0x42007C;
const RESULT_MESSAGE: u32 = 0x42007D;
const RESULT_REASON: u32 = 0x42007E;
const RESULT_STATUS: u32 = 0x42007F;

/// KMS_KMIP_REQUEST_SECRETDATA_LENGTH
pub const SECRETDATA_LENGTH: usize = 96;

fn tag_name(tag: u32) -> &'static str {
    match tag {
        RESPONSE_MESSAGE => "ResponseMessage",
        BATCH_ITEM => "BatchItem",
        RESPONSE_PAYLOAD => "ResponsePayload",
        RESULT_STATUS => "ResultStatus",
        UNIQUE_IDENTIFIER => "UniqueIdentifier",
        SECRET_DATA => "SecretData",
        KEY_BLOCK => "KeyBlock",
        KEY_VALUE => "KeyValue",
        KEY_MATERIAL => "KeyMaterial",
        DATA => "Data",
        IV_COUNTER_NONCE => "IVCounterNonce",
        _ => "Unknown KMIP tag",
    }
}

fn padded(len: usize) -> usize {
    len.div_ceil(8) * 8
}

/// `kmip_writer_t`
struct Writer {
    buf: Vec<u8>,
    open: Vec<usize>,
}

impl Writer {
    fn new() -> Writer {
        Writer { buf: Vec::with_capacity(256), open: Vec::new() }
    }
    fn head(&mut self, tag: u32, t: u8, len: u32) {
        self.buf.extend_from_slice(&tag.to_be_bytes()[1..]);
        self.buf.push(t);
        self.buf.extend_from_slice(&len.to_be_bytes());
    }
    fn bytes(&mut self, tag: u32, t: u8, v: &[u8]) {
        self.head(tag, t, v.len() as u32);
        self.buf.extend_from_slice(v);
        self.buf.resize(self.buf.len() + padded(v.len()) - v.len(), 0);
    }
    fn int(&mut self, tag: u32, t: u8, v: u32) {
        self.head(tag, t, 4);
        self.buf.extend_from_slice(&v.to_be_bytes());
        self.buf.extend_from_slice(&[0; 4]);
    }
    fn integer(&mut self, tag: u32, v: u32) {
        self.int(tag, INTEGER, v)
    }
    fn enumeration(&mut self, tag: u32, v: u32) {
        self.int(tag, ENUMERATION, v)
    }
    fn boolean(&mut self, tag: u32, v: bool) {
        self.head(tag, BOOLEAN, 8);
        self.buf.extend_from_slice(&(v as u64).to_be_bytes());
    }
    fn begin(&mut self, tag: u32) {
        self.head(tag, STRUCTURE, 0);
        self.open.push(self.buf.len() - 4);
    }
    fn end(&mut self) {
        let p = self.open.pop().expect("balanced KMIP structures");
        let len = (self.buf.len() - p - 4) as u32;
        self.buf[p..p + 4].copy_from_slice(&len.to_be_bytes());
    }
    /// RequestMessage, RequestHeader (protocol 1.`minor`, one batch item), BatchItem with `op`,
    /// then RequestPayload left open
    fn request(minor: u32, op: u32) -> Writer {
        let mut w = Writer::new();
        w.begin(REQUEST_MESSAGE);
        w.begin(REQUEST_HEADER);
        w.begin(PROTOCOL_VERSION);
        w.integer(PROTOCOL_VERSION_MAJOR, 1);
        w.integer(PROTOCOL_VERSION_MINOR, minor);
        w.end();
        w.integer(BATCH_COUNT, 1);
        w.end();
        w.begin(BATCH_ITEM);
        w.enumeration(OPERATION, op);
        w.begin(REQUEST_PAYLOAD);
        w
    }
    fn finish(mut self) -> Vec<u8> {
        while !self.open.is_empty() {
            self.end();
        }
        self.buf
    }
    fn attribute(&mut self, name: &str, t: u8, v: u32) {
        self.begin(ATTRIBUTE);
        self.bytes(ATTRIBUTE_NAME, TEXT_STRING, name.as_bytes());
        self.int(ATTRIBUTE_VALUE, t, v);
        self.end();
    }
}

/// `kms_kmip_request_register_secretdata_new`
pub fn register_secretdata(data: &[u8]) -> Result<Vec<u8>> {
    if data.len() != SECRETDATA_LENGTH {
        return Err(format!("expected SecretData length of {SECRETDATA_LENGTH}, got {}", data.len()));
    }
    let mut w = Writer::request(0, 0x03);
    w.enumeration(OBJECT_TYPE, 0x07);
    w.begin(TEMPLATE_ATTRIBUTE);
    w.attribute("Cryptographic Usage Mask", INTEGER, 0);
    w.end();
    w.begin(SECRET_DATA);
    w.enumeration(SECRET_DATA_TYPE, 0x02);
    w.begin(KEY_BLOCK);
    w.enumeration(KEY_FORMAT_TYPE, 0x02);
    w.begin(KEY_VALUE);
    w.bytes(KEY_MATERIAL, BYTE_STRING, data);
    Ok(w.finish())
}

/// `kms_kmip_request_activate_new` / `kms_kmip_request_get_new`
fn with_uid(op: u32, uid: &str) -> Vec<u8> {
    let mut w = Writer::request(0, op);
    w.bytes(UNIQUE_IDENTIFIER, TEXT_STRING, uid.as_bytes());
    w.finish()
}

pub fn activate(uid: &str) -> Vec<u8> {
    with_uid(0x12, uid)
}

pub fn get(uid: &str) -> Vec<u8> {
    with_uid(0x0A, uid)
}

/// `kms_kmip_request_create_new`: an AES-256 key for encryption and decryption
pub fn create() -> Vec<u8> {
    let mut w = Writer::request(2, 0x01);
    w.enumeration(OBJECT_TYPE, 0x02);
    w.begin(TEMPLATE_ATTRIBUTE);
    w.attribute("Cryptographic Algorithm", ENUMERATION, 3);
    w.attribute("Cryptographic Length", INTEGER, 256);
    w.attribute("Cryptographic Usage Mask", INTEGER, 4 | 8);
    w.finish()
}

/// `kms_kmip_request_encrypt_new` / `_decrypt_new`: AES-CBC with PKCS#5 padding on the server
pub fn encrypt_decrypt(encrypt: bool, uid: &str, data: &[u8], iv: Option<&[u8]>) -> Vec<u8> {
    let mut w = Writer::request(2, if encrypt { 0x1F } else { 0x20 });
    w.bytes(UNIQUE_IDENTIFIER, TEXT_STRING, uid.as_bytes());
    w.begin(CRYPTOGRAPHIC_PARAMETERS);
    w.enumeration(BLOCK_CIPHER_MODE, 1);
    w.enumeration(PADDING_METHOD, 3);
    w.enumeration(CRYPTOGRAPHIC_ALGORITHM, 3);
    if encrypt {
        w.boolean(RANDOM_IV, true);
    }
    w.end();
    w.bytes(DATA, BYTE_STRING, data);
    if let Some(iv) = iv.filter(|_| !encrypt) {
        w.bytes(IV_COUNTER_NONCE, BYTE_STRING, iv);
    }
    w.finish()
}

/// `kmip_reader_find`: the value of the first item of `tag` and type `t` in `items` (a
/// structure's contents), or None (also when the items are malformed or out of bounds)
fn find(items: &[u8], tag: u32, t: u8) -> Option<&[u8]> {
    let mut p = 0usize;
    while p < items.len() {
        let h = items.get(p..p.checked_add(8)?)?;
        if h[0] != 0x42 {
            return None;
        }
        let read_tag = u32::from_be_bytes([0, h[0], h[1], h[2]]);
        let len = u32::from_be_bytes([h[4], h[5], h[6], h[7]]) as usize;
        let start = p + 8;
        let end = start.checked_add(padded(len))?;
        if end > items.len() {
            return None;
        }
        if read_tag == tag && h[3] == t {
            return Some(&items[start..start + len]);
        }
        p = end;
    }
    None
}

fn find_struct<'a>(items: &'a [u8], tag: u32) -> Result<&'a [u8]> {
    find(items, tag, STRUCTURE).ok_or_else(|| format!("unable to find tag: {}", tag_name(tag)))
}

fn enum_value(v: &[u8]) -> Option<u32> {
    v.get(..4).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

fn result_status_name(s: u32) -> &'static str {
    match s {
        0 => "Success",
        1 => "Operation Failed",
        2 => "Operation Pending",
        3 => "Operation Undone",
        _ => "Unknown KMIP result status",
    }
}

fn result_reason_name(r: u32) -> &'static str {
    match r {
        0x00000001 => "Item Not Found",
        0x00000002 => "Response Too Large",
        0x00000003 => "Authentication Not Successful",
        0x00000004 => "Invalid Message",
        0x00000005 => "Operation Not Supported",
        0x00000006 => "Missing Data",
        0x00000007 => "Invalid Field",
        0x00000008 => "Feature Not Supported",
        0x00000009 => "Operation Canceled By Requester",
        0x0000000A => "Cryptographic Failure",
        0x0000000B => "Illegal Operation",
        0x0000000C => "Permission Denied",
        0x0000000D => "Object archived",
        0x0000000E => "Index Out of Bounds",
        0x0000000F => "Application Namespace Not Supported",
        0x00000010 => "Key Format Type Not Supported",
        0x00000011 => "Key Compression Type Not Supported",
        0x00000012 => "Encoding Option Error",
        0x00000013 => "Key Value Not Present",
        0x00000014 => "Attestation Required",
        0x00000015 => "Attestation Failed",
        0x00000016 => "Sensitive",
        0x00000017 => "Not Extractable",
        0x00000018 => "Object Already Exists",
        0x00000100 => "General Failure",
        _ => "Unknown KMIP result reason",
    }
}

/// `kms_kmip_response_ok` then the ResponsePayload of the (only) batch item
fn payload(msg: &[u8]) -> Result<&[u8]> {
    let batch = find_struct(find_struct(msg, RESPONSE_MESSAGE)?, BATCH_ITEM)?;
    let reason = match find(batch, RESULT_REASON, ENUMERATION) {
        Some(v) => enum_value(v).ok_or("unable to read result reason value")?,
        None => 0,
    };
    let message = find(batch, RESULT_MESSAGE, TEXT_STRING).unwrap_or_default();
    let status = find(batch, RESULT_STATUS, ENUMERATION).ok_or_else(|| format!("unable to find tag: {}", tag_name(RESULT_STATUS)))?;
    let status = enum_value(status).ok_or("unable to read result status value")?;
    if status != 0 {
        return Err(format!(
            "KMIP response error. Result Status ({status}): {}. Result Reason ({reason}): {}. Result Message: {}",
            result_status_name(status),
            result_reason_name(reason),
            String::from_utf8_lossy(message)
        ));
    }
    find_struct(batch, RESPONSE_PAYLOAD)
}

fn field(p: &[u8], tag: u32, t: u8) -> Result<Vec<u8>> {
    find(p, tag, t).map(<[u8]>::to_vec).ok_or_else(|| format!("unable to find tag: {}", tag_name(tag)))
}

/// `kms_kmip_response_get_unique_identifier`
pub fn unique_identifier(msg: &[u8]) -> Result<String> {
    let v = field(payload(msg)?, UNIQUE_IDENTIFIER, TEXT_STRING)?;
    String::from_utf8(v).map_err(|_| "unable to read unique identifier".into())
}

/// `kms_kmip_response_get_secretdata`
pub fn secretdata(msg: &[u8]) -> Result<Vec<u8>> {
    let p = payload(msg)?;
    let kv = find_struct(find_struct(find_struct(p, SECRET_DATA)?, KEY_BLOCK)?, KEY_VALUE)?;
    field(kv, KEY_MATERIAL, BYTE_STRING)
}

/// `kms_kmip_response_get_data` / `_get_iv`
pub fn data(msg: &[u8]) -> Result<Vec<u8>> {
    field(payload(msg)?, DATA, BYTE_STRING)
}

pub fn iv(msg: &[u8]) -> Result<Vec<u8>> {
    field(payload(msg)?, IV_COUNTER_NONCE, BYTE_STRING)
}

/// `kms_kmip_response_parser_t`: one TTLV message (8-byte header, then its length)
#[derive(Default)]
pub struct Parser {
    buf: Vec<u8>,
    error: Option<String>,
}

impl Parser {
    fn total(&self) -> Option<usize> {
        self.buf.get(4..8).map(|b| 8 + u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize)
    }

    /// Adds bytes read; true once the message is complete or malformed
    pub fn feed(&mut self, data: &[u8]) -> bool {
        if self.error.is_some() {
            return true;
        }
        let max = crate::kms::MAX_RESPONSE;
        if data.len() > max || self.buf.len() > max - data.len() {
            self.error = Some("KMS response too large".into());
            return true;
        }
        self.buf.extend_from_slice(data);
        match self.total() {
            Some(t) if t - 8 > max => {
                self.error = Some("KMS KMIP response too large".into());
                true
            }
            Some(t) => self.buf.len() >= t,
            None => false,
        }
    }

    /// The message (extra bytes, which libmongoc never reads, are dropped)
    pub fn finish(mut self) -> Result<Vec<u8>> {
        if let Some(e) = self.error {
            return Err(format!("KMS response parser error with error: '{e}'"));
        }
        match self.total() {
            Some(t) if self.buf.len() >= t => {
                self.buf.truncate(t);
                Ok(self.buf)
            }
            _ => Err("Error getting KMIP response: KMIP parser does not have a complete message".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(op: u32, status: u32, payload: impl FnOnce(&mut Writer)) -> Vec<u8> {
        let mut w = Writer::new();
        w.begin(RESPONSE_MESSAGE);
        w.begin(BATCH_ITEM);
        w.enumeration(OPERATION, op);
        w.enumeration(RESULT_STATUS, status);
        if status != 0 {
            w.enumeration(RESULT_REASON, 0x0C);
            w.bytes(RESULT_MESSAGE, TEXT_STRING, b"no");
        }
        w.begin(RESPONSE_PAYLOAD);
        payload(&mut w);
        w.finish()
    }

    #[test]
    fn requests_and_responses() {
        // kms-message's test vector for Activate("39")
        let a = activate("39");
        assert_eq!(a.len() % 8, 0);
        assert_eq!(&a[..8], &[0x42, 0x00, 0x78, 0x01, 0, 0, 0, (a.len() - 8) as u8]);
        let r = response(3, 0, |w| w.bytes(UNIQUE_IDENTIFIER, TEXT_STRING, b"39"));
        assert_eq!(unique_identifier(&r).unwrap(), "39");
        let r = response(3, 1, |_| {});
        assert_eq!(
            unique_identifier(&r).unwrap_err(),
            "KMIP response error. Result Status (1): Operation Failed. Result Reason (12): Permission Denied. Result Message: no"
        );
        // truncated or lying lengths never read out of bounds
        let mut bad = response(3, 0, |w| w.bytes(UNIQUE_IDENTIFIER, TEXT_STRING, b"39"));
        let n = bad.len();
        bad[n - 13] = 0xff;
        assert!(unique_identifier(&bad).is_err());
        assert!(unique_identifier(&bad[..20]).is_err());
        let mut p = Parser::default();
        assert!(!p.feed(&r[..5]));
        assert!(p.feed(&r[5..]));
        assert_eq!(p.finish().unwrap(), r);
        assert!(register_secretdata(&[0; 95]).is_err());
    }

    /// kms-message's test_kms_kmip_request.c vectors
    #[test]
    fn kms_message_vectors() {
        let v = |n: &str| std::fs::read(format!("{}/tests/data/kmip/{n}.bin", env!("CARGO_MANIFEST_DIR"))).unwrap();
        let reg = v("register_secretdata_request");
        // the SecretData is the request's last 96 bytes (no padding)
        let secret = &reg[reg.len() - 96..];
        assert_eq!(register_secretdata(secret).unwrap(), reg);
        assert_eq!(get("7FJYvnV6XkaUCWuY96bCSc6AuhvkPpqI"), v("get_request"));
        assert_eq!(activate("7FJYvnV6XkaUCWuY96bCSc6AuhvkPpqI"), v("activate_request"));
    }
}
