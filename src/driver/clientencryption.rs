//! MongoDB\Driver\ClientEncryption (phongo's src/MongoDB/ClientEncryption.c) over libmongoc's
//! mongoc_client_encryption_t (mongoc-client-side-encryption.c): the key vault operations run on
//! the keyVaultClient with readConcern and writeConcern "majority"; encryption, decryption and data
//! key creation are mongo-crypt's (libmongocrypt's).

use super::cursor::Cursor;
use super::ClientRef;
use crate::bson::{encode, throw, E};
use crate::zend::call::{self, Ex};
use crate::zend::{hash, obj, sys, zstr, zval};
use mongo_core::bson::iter::{Iter, Value, T_ARRAY, T_DOCUMENT};
use mongo_core::bson::writer::Writer;
use mongo_core::error::{code, domain, Error};
use mongo_core::topology::OpType;
use mongo_crypt::engine::{Crypt, ExplicitOpts, Io};
use std::ptr::{addr_of_mut, null_mut};
use std::rc::Rc;

/// The key vault collection (mongoc_client_encryption_t.keyvault_coll)
#[derive(Clone)]
pub struct KeyVault {
    pub client: ClientRef,
    pub db: String,
    pub coll: String,
}

pub struct ClientEncryption {
    pub crypt: Option<Rc<Crypt>>,
    pub kv: Option<KeyVault>,
    pub kv_namespace: Option<String>,
    /// the keyVaultClient Manager; released last (a non-persistent client must outlive the rest)
    pub kv_manager: sys::zval,
}

impl Default for ClientEncryption {
    fn default() -> Self {
        ClientEncryption { crypt: None, kv: None, kv_namespace: None, kv_manager: zval::undef() }
    }
}

impl Drop for ClientEncryption {
    fn drop(&mut self) {
        self.crypt = None;
        self.kv = None;
        self.kv_manager = zval::undef();
    }
}

static mut H_CLIENTENCRYPTION: sys::zend_object_handlers = unsafe { std::mem::zeroed() };
pub static mut CE: *mut sys::zend_class_entry = null_mut();

unsafe extern "C" fn create(c: *mut sys::zend_class_entry) -> *mut sys::zend_object {
    obj::alloc(c, ClientEncryption::default(), addr_of_mut!(H_CLIENTENCRYPTION))
}

/// phongo_clientencryption_get_debug_info: an empty array
unsafe extern "C" fn get_debug_info(_o: *mut sys::zend_object, is_temp: *mut i32) -> *mut sys::HashTable {
    *is_temp = 1;
    hash::new(0)
}

unsafe extern "C" fn get_gc(o: *mut sys::zend_object, table: *mut *mut sys::zval, n: *mut i32) -> *mut sys::HashTable {
    let c = obj::data::<ClientEncryption>(o);
    obj::get_gc_with(o, table, n, [&mut c.kv_manager as *mut sys::zval])
}

pub unsafe fn init_MongoDB_Driver_ClientEncryption(c: *mut sys::zend_class_entry) {
    CE = c;
    (*c).__bindgen_anon_2.create_object = Some(create);
    let h = obj::handlers::<ClientEncryption>(addr_of_mut!(H_CLIENTENCRYPTION));
    h.get_debug_info = Some(get_debug_info);
    h.get_gc = Some(get_gc);
}

type IoError = (Error, Option<Vec<u8>>);

fn majority_wc() -> mongo_core::concern::WriteConcern {
    let mut wc = mongo_core::concern::WriteConcern::default();
    wc.set_w(mongo_core::uri::W_MAJORITY);
    wc
}

fn invalid_arg(m: &str) -> Error {
    Error::new(domain::CLIENT, code::CLIENT_INVALID_ENCRYPTION_ARG, m)
}

impl KeyVault {
    /// mongoc_collection_find_with_opts(keyvault_coll, filter, NULL, NULL): a find cursor with
    /// readConcern majority and the client's read preference
    pub unsafe fn find_cursor(&self, filter: Vec<u8>) -> Result<Cursor, IoError> {
        let rp = super::manager::client_read_preference(&self.client.borrow().uri);
        let prefs = rp.prefs();
        let sel = self.client.borrow_mut().select_server_id(OpType::Read, Some(&prefs), &[]);
        super::apm::flush(&self.client);
        let sid = sel.map_err(|e| (e, None))?.0;
        let mut c = Cursor::for_find(self.client.clone(), sid, &self.db, &self.coll, filter, Writer::new().finish());
        c.read_prefs = Some(super::manager::rp_parts(&rp));
        c.retry_prefs = Some(prefs);
        c.read_concern = mongo_core::concern::ReadConcern { level: Some(b"majority".to_vec()) }.to_bson();
        Ok(c)
    }

    /// All documents matching `filter`
    pub unsafe fn find_all(&self, filter: Vec<u8>) -> Result<Vec<Vec<u8>>, IoError> {
        let mut c = self.find_cursor(filter)?;
        let mut out = Vec::new();
        while let Some(d) = c.next_raw()? {
            out.push(d);
        }
        Ok(out)
    }

    /// The first document matching `filter` (None: no match)
    pub unsafe fn find_one(&self, filter: Vec<u8>) -> Result<Option<Vec<u8>>, IoError> {
        let mut c = self.find_cursor(filter)?;
        c.next_raw()
    }

    /// A bulk write with writeConcern majority: libmongoc's default reply and success
    unsafe fn bulk(&self, f: impl FnOnce(&mut mongo_core::bulk::BulkOperation) -> Result<(), Error>) -> Result<Vec<u8>, IoError> {
        let mut b = mongo_core::bulk::BulkOperation::new(true);
        f(&mut b).map_err(|e| (e, None))?;
        let r = {
            let mut cl = self.client.borrow_mut();
            b.execute(&mut cl, &self.db, &self.coll, &majority_wc(), None)
        };
        super::apm::flush(&self.client);
        let (ok, reply, e) = r;
        if ok {
            Ok(reply)
        } else {
            Err((e, Some(reply)))
        }
    }

    /// mongoc_collection_insert_one
    pub unsafe fn insert_one(&self, doc: &[u8]) -> Result<(), IoError> {
        self.bulk(|b| b.insert(doc)).map(|_| ())
    }

    /// mongoc_collection_delete_one: {deletedCount: n} (+ the errors libmongoc appends)
    pub unsafe fn delete_one(&self, selector: &[u8]) -> Result<Vec<u8>, IoError> {
        let reply = self.bulk(|b| b.remove(selector, 1, None, None))?;
        Ok(counts_reply(&reply, &[("deletedCount", b"nRemoved")]))
    }

    /// mongoc_bulk_operation_execute of updateOne operations: the bulk write result
    pub unsafe fn update_many_ones(&self, ops: &[(Vec<u8>, Vec<u8>)]) -> Result<Vec<u8>, IoError> {
        self.bulk(|b| {
            for (sel, upd) in ops {
                b.update(sel, upd, &mongo_core::bulk::UpdateOpts::default(), false)?;
            }
            Ok(())
        })
    }

    /// mongoc_collection_find_and_modify_with_opts with `query` and `update`: the reply
    pub unsafe fn find_and_modify(&self, query: &[u8], update: &[u8], update_is_pipeline: bool) -> Result<Vec<u8>, IoError> {
        let mut w = Writer::with_capacity(query.len() + update.len() + 64);
        w.utf8(b"findAndModify", self.coll.as_bytes());
        w.document(b"query", query);
        if update_is_pipeline {
            w.array(b"update", update);
        } else {
            w.document(b"update", update);
        }
        let body = w.finish();
        let client = &self.client;
        let mut implicit = client.borrow_mut().start_session(true, false).ok();
        let result = {
            let mut cl = client.borrow_mut();
            match cl.select_for_session(OpType::Write, None, implicit.as_mut(), &[]) {
                Err(e) => Err(mongo_core::cmdparts::Failure::from(e)),
                Ok((sid, _)) => {
                    let mut parts = mongo_core::cmdparts::Parts::new(&self.db, &body);
                    parts.is_write = true;
                    parts.set_write_concern(&majority_wc());
                    parts.session = implicit.as_mut();
                    cl.operation_id += 1;
                    match cl.assemble(parts, sid, false) {
                        Err(e) => Err(mongo_core::cmdparts::Failure::from(e)),
                        Ok(mut c) => {
                            let r = cl.run_retryable_write(&mut c, &[], implicit.as_mut()).0;
                            cl.release_temp_session(&mut c);
                            r
                        }
                    }
                }
            }
        };
        if let Some(mut s) = implicit.take() {
            client.borrow_mut().end_session(&mut s);
        }
        super::apm::flush(client);
        let reply = result.map_err(|f| (f.error, (!f.reply.is_empty()).then_some(f.reply)))?;
        if let Some(Value::Document(wce)) = find(&reply, b"writeConcernError") {
            let code = find(wce, b"code").map_or(0, |v| mongo_core::sdam::as_i64(&v) as u32);
            let msg = match find(wce, b"errmsg") {
                Some(Value::Utf8(s)) => String::from_utf8_lossy(s).into_owned(),
                _ => String::new(),
            };
            return Err((Error::new(domain::WRITE_CONCERN, code, &msg), Some(reply)));
        }
        Ok(reply)
    }
}

fn find<'a>(d: &'a [u8], k: &[u8]) -> Option<Value<'a>> {
    let mut it = Iter::new(d)?;
    if it.find(k) {
        Some(it.value())
    } else {
        None
    }
}

/// _mongoc_write_result_complete with a field list: the counts asked for, then writeErrors (if
/// any), writeConcernErrors, errorReplies and errorLabels of the default reply
fn counts_reply(reply: &[u8], fields: &[(&str, &[u8])]) -> Vec<u8> {
    let mut w = Writer::with_capacity(64);
    for (name, from) in fields {
        let n = find(reply, from).map_or(0, |v| mongo_core::sdam::as_i64(&v) as i32);
        w.int32(name.as_bytes(), n);
    }
    if let Some(Value::Array(a)) = find(reply, b"writeErrors") {
        if a.len() > 5 {
            w.array(b"writeErrors", a);
        }
    }
    for k in [&b"writeConcernErrors"[..], b"errorReplies", b"errorLabels"] {
        if let Some(Value::Array(a)) = find(reply, k) {
            w.array(k, a);
        }
    }
    w.finish()
}

/// The state machine's key vault reads: errors get libmongoc's "key vault error: " prefix
struct KvIo<'a>(&'a KeyVault);

/// libmongoc's KMS round trip (`_state_need_kms`)
fn kms_exchange(req: &mongo_crypt::kms::KmsRequest, done: &mut dyn FnMut(&[u8]) -> bool) -> Result<Vec<u8>, mongo_crypt::engine::KmsFailure<Error>> {
    use mongo_core::kms_tls::Phase;
    use mongo_crypt::engine::KmsFailure;
    mongo_core::kms_tls::exchange(&req.endpoint, &req.tls, &req.message, done).map_err(|(p, e)| match p {
        Phase::Connect => KmsFailure::Connect(e),
        Phase::Write => KmsFailure::Write(e),
        Phase::Read => KmsFailure::Read(e),
    })
}

fn stream_error(msg: String) -> Error {
    let mut m = msg;
    mongo_core::bulk::truncate_message(&mut m);
    Error::new(domain::STREAM, code::STREAM_SOCKET, &m)
}

impl Io for KvIo<'_> {
    type Error = IoError;
    fn kms(&mut self, req: &mongo_crypt::kms::KmsRequest, done: &mut dyn FnMut(&[u8]) -> bool) -> Result<Vec<u8>, mongo_crypt::engine::KmsFailure<IoError>> {
        kms_exchange(req, done).map_err(|f| match f {
            mongo_crypt::engine::KmsFailure::Connect(e) => mongo_crypt::engine::KmsFailure::Connect((e, None)),
            mongo_crypt::engine::KmsFailure::Write(e) => mongo_crypt::engine::KmsFailure::Write((e, None)),
            mongo_crypt::engine::KmsFailure::Read(e) => mongo_crypt::engine::KmsFailure::Read((e, None)),
        })
    }
    fn stream_error(&self, msg: String) -> IoError {
        (stream_error(msg), None)
    }
    fn find_keys(&mut self, filter: &[u8]) -> Result<Vec<Vec<u8>>, IoError> {
        unsafe { self.0.find_all(filter.to_vec()) }.map_err(|(mut e, r)| {
            e.message = format!("key vault error: {}", e.message);
            mongo_core::bulk::truncate_message(&mut e.message);
            (e, r)
        })
    }
}

/// phongo_throw_exception_from_bson_error_t
unsafe fn throw_io((e, _reply): &IoError) {
    super::throw_error(e, None);
}

/// A libmongocrypt failure: MONGOC_ERROR_CLIENT_SIDE_ENCRYPTION, code 1 (MONGOCRYPT_GENERIC_ERROR_CODE)
unsafe fn throw_crypt(msg: &str) {
    let mut m = msg.to_string();
    mongo_core::bulk::truncate_message(&mut m);
    super::throw_error(&Error::new(domain::CLIENT_SIDE_ENCRYPTION, 1, &m), None);
}

unsafe fn throw_engine(e: &mongo_crypt::engine::Error<IoError>) {
    match e {
        mongo_crypt::engine::Error::Crypt(m) => throw_crypt(m),
        mongo_crypt::engine::Error::Io(e) => throw_io(e),
    }
}


/// `_mongoc_bson_type_to_str`
fn bson_type_str(t: u8) -> &'static str {
    match t {
        1 => "DOUBLE",
        2 => "UTF8",
        3 => "DOCUMENT",
        4 => "ARRAY",
        5 => "BINARY",
        6 => "UNDEFINED",
        7 => "OID",
        8 => "BOOL",
        9 => "DATE_TIME",
        10 => "NULL",
        11 => "REGEX",
        12 => "DBPOINTER",
        13 => "CODE",
        14 => "SYMBOL",
        15 => "CODEWSCOPE",
        16 => "INT32",
        17 => "TIMESTAMP",
        18 => "INT64",
        19 => "DECIMAL128",
        0x7f => "MAXKEY",
        0xff => "MINKEY",
        _ => "Unknown",
    }
}

/// `_parse_all_tls_opts` / `_parse_one_tls_opts` / `_mongoc_ssl_opts_from_bson`: the TLS options per
/// KMS provider id (Err: libmongoc's CSE INVALID_ENCRYPTION_ARG message)
fn kms_tls_options(doc: &[u8]) -> Result<Vec<(String, mongo_core::tls::TlsOptions)>, String> {
    let mut out: Vec<(String, mongo_core::tls::TlsOptions)> = Vec::new();
    let mut it = Iter::new(doc).ok_or("Error starting iteration of TLS options")?;
    while it.next() {
        let key = String::from_utf8_lossy(it.key()).into_owned();
        let known = matches!(key.as_str(), "aws" | "azure" | "gcp" | "kmip") || key.contains(':');
        if !known {
            return Err(format!("Cannot configure TLS options for KMS provider: {key}"));
        }
        if out.iter().any(|(k, _)| *k == key) {
            return Err(format!("Error parsing duplicate TLS options for {key}"));
        }
        let Value::Document(d) = it.value() else {
            return Err(format!("Expected TLS options for {key} to be a document, got: {}", bson_type_str(it.value().type_code())));
        };
        let mut o = mongo_core::tls::TlsOptions::default();
        let mut oi = Iter::new(d).ok_or_else(|| format!("Error iterating into TLS options document for {key}"))?;
        while oi.next() {
            let k = String::from_utf8_lossy(oi.key()).into_owned();
            let kl = k.to_ascii_lowercase();
            if !matches!(kl.as_str(), "tlscertificatekeyfilepassword" | "tlscertificatekeyfile" | "tlscafile" | "tlsdisableocspendpointcheck") {
                return Err(format!("Error setting TLS option {k} for {key}. Insecure TLS options prohibited."));
            }
            match (kl.as_str(), oi.value()) {
                ("tlscertificatekeyfile", Value::Utf8(v)) => o.pem_file = Some(String::from_utf8_lossy(v).into_owned()),
                ("tlscertificatekeyfilepassword", Value::Utf8(v)) => o.pem_pwd = Some(String::from_utf8_lossy(v).into_owned()),
                ("tlscafile", Value::Utf8(v)) => o.ca_file = Some(String::from_utf8_lossy(v).into_owned()),
                ("tlsdisableocspendpointcheck", Value::Bool(b)) => o.disable_ocsp_endpoint_check = b,
                (_, v) => return Err(format!("Error parsing TLS options for {key}: unexpected {} option: {k}", bson_type_str(v.type_code()))),
            }
        }
        out.push((key, o));
    }
    Ok(out)
}

fn throw_tls_arg(m: &str) {
    unsafe { super::throw_error(&Error::new(domain::CLIENT_SIDE_ENCRYPTION, code::CLIENT_INVALID_ENCRYPTION_ARG, m), None) };
}

/// php_array_fetchc_string: the option converted to a string
unsafe fn fetch_string(z: *mut sys::zval) -> Vec<u8> {
    let s = sys::zval_get_string_func(z);
    let r = zstr::bytes(s).to_vec();
    zstr::release(s);
    r
}

unsafe fn opt(options: *mut sys::zval, k: &[u8]) -> Option<*mut sys::zval> {
    if options.is_null() || !zval::is(options, sys::IS_ARRAY) {
        return None;
    }
    hash::sym_find((*options).value.arr, k).map(|z| zval::deref(z))
}

/// phongo_clientencryption_init (false: an exception was thrown)
pub unsafe fn init(ce: &mut ClientEncryption, options: *mut sys::zval, default_kv_manager: *mut sys::zend_object) -> bool {
    // phongo_clientencryption_opts_from_zval
    let mut kv_manager: *mut sys::zend_object = null_mut();
    let (mut kv_ns, mut kms, mut tls): (Option<(String, String)>, Option<Vec<u8>>, Option<Vec<u8>>) = (None, None, None);
    if !options.is_null() && zval::is(options, sys::IS_ARRAY) {
        if let Some(z) = opt(options, b"keyVaultClient") {
            if !zval::is(z, sys::IS_OBJECT) || !obj::instanceof((*(*z).value.obj).ce, super::manager::CE) {
                throw(E::InvalidArgument, format!("Expected \"keyVaultClient\" option to be MongoDB\\Driver\\Manager, {} given", zval::type_name(z)));
                return false;
            }
            kv_manager = (*z).value.obj;
        } else if !default_kv_manager.is_null() {
            kv_manager = default_kv_manager;
        } else {
            throw(E::InvalidArgument, "The \"keyVaultClient\" option is required when constructing a ClientEncryption object directly");
            return false;
        }
        if let Some(z) = opt(options, b"keyVaultNamespace") {
            let ns = fetch_string(z);
            if ns.contains(&0) {
                throw(E::InvalidArgument, "Invalid namespace provided: namespaces may not contain a null byte");
                return false;
            }
            let s = String::from_utf8_lossy(&ns).into_owned();
            match s.split_once('.') {
                Some((d, c)) => kv_ns = Some((d.to_string(), c.to_string())),
                None => {
                    throw(E::InvalidArgument, "Expected \"keyVaultNamespace\" option to contain a full collection namespace");
                    return false;
                }
            }
        }
        if let Some(z) = opt(options, b"kmsProviders") {
            if !zval::is(z, sys::IS_ARRAY) && !zval::is(z, sys::IS_OBJECT) {
                throw(E::InvalidArgument, format!("Expected \"kmsProviders\" option to be an array or object, {} given", zval::type_name(z)));
                return false;
            }
            let Some(b) = encode::to_bson(z, encode::NONE) else { return false };
            kms = Some(b);
        }
        if let Some(z) = opt(options, b"tlsOptions") {
            if !zval::is(z, sys::IS_ARRAY) && !zval::is(z, sys::IS_OBJECT) {
                throw(E::InvalidArgument, format!("Expected \"tlsOptions\" option to be an array or object, {} given", zval::type_name(z)));
                return false;
            }
            let Some(b) = encode::to_bson(z, encode::NONE) else { return false };
            tls = Some(b);
        }
    }
    // mongoc_client_encryption_new
    let client = if kv_manager.is_null() { None } else { obj::data::<super::manager::Manager>(kv_manager).client.clone() };
    let (Some(client), Some((db, coll))) = (client, kv_ns.clone()) else {
        super::throw_error(&invalid_arg("Key vault client and namespace option required"), None);
        return false;
    };
    // _mongoc_validate_db_name
    if db.is_empty() || db.bytes().any(|b| b"/\\. \"$".contains(&b)) || db.contains('\0') {
        let m = if db.is_empty() { "Database name cannot be empty".to_string() } else { format!("Invalid database name: '{db}'") };
        super::throw_error(&Error::new(domain::NAMESPACE, code::NAMESPACE_INVALID, &m), None);
        return false;
    }
    let Some(kms) = kms else {
        super::throw_error(&invalid_arg("KMS providers option required"), None);
        return false;
    };
    // _mongoc_crypt_new: mongocrypt_setopt_kms_providers + mongocrypt_init, then the TLS options
    let mut crypt = match Crypt::new(&kms, None) {
        Ok(c) => c,
        Err(m) => {
            throw_crypt(&m);
            return false;
        }
    };
    if let Some(t) = &tls {
        match kms_tls_options(t) {
            Ok(v) => crypt.tls = v,
            Err(m) => {
                throw_tls_arg(&m);
                return false;
            }
        }
    }
    ce.crypt = Some(Rc::new(crypt));
    ce.kv = Some(KeyVault { client, db, coll });
    ce.kv_manager = zval::copy(&*zval::borrowed_obj(kv_manager));
    ce.kv_namespace = kv_ns.map(|(d, c)| format!("{d}.{c}"));
    true
}

/// ClientEncryption::__construct(array $options)
pub unsafe extern "C" fn zim_MongoDB_Driver_ClientEncryption___construct(ex: Ex, _rv: *mut sys::zval) {
    let mut a: *mut sys::zval = null_mut();
    if sys::zend_parse_parameters(call::num_args(ex), c"a".as_ptr(), &mut a) != 0 {
        return;
    }
    init(obj::data::<ClientEncryption>(call::this(ex)), a, null_mut());
}

/// Manager::createClientEncryption(array $options)
pub unsafe extern "C" fn zim_MongoDB_Driver_Manager_createClientEncryption(ex: Ex, rv: *mut sys::zval) {
    let mut a: *mut sys::zval = null_mut();
    if sys::zend_parse_parameters(call::num_args(ex), c"a".as_ptr(), &mut a) != 0 {
        return;
    }
    let z = obj::new_zval(CE);
    if init(obj::data::<ClientEncryption>(z.value.obj), a, call::this(ex)) {
        zval::write(rv, z);
    }
}

unsafe fn me<'a>(ex: Ex) -> &'a mut ClientEncryption {
    obj::data::<ClientEncryption>(call::this(ex))
}

/// The key vault and the crypt handle (phongo dereferences a NULL client_encryption)
unsafe fn parts<'a>(c: &'a ClientEncryption) -> Option<(&'a KeyVault, &'a Crypt)> {
    match (&c.kv, &c.crypt) {
        (Some(k), Some(cr)) => Some((k, cr)),
        _ => {
            throw(E::Logic, "Failed to use an uninitialized ClientEncryption object");
            None
        }
    }
}

/// validate_keyid: a Binary of subtype 4 with 16 bytes; the UUID bytes
unsafe fn keyid_arg(z: *mut sys::zval) -> Option<Vec<u8>> {
    let b = obj::data::<crate::bson::types::Binary>((*z).value.obj);
    if b.ty != 4 {
        throw(E::InvalidArgument, format!("Expected keyid to have UUID Binary subtype (4), {} given", b.ty));
        return None;
    }
    if b.data.len() != 16 {
        throw(E::UnexpectedValue, format!("Expected keyid to have data length of 16 bytes, {} given", b.data.len()));
        return None;
    }
    Some(b.data.clone())
}

fn id_doc(id: &[u8]) -> Vec<u8> {
    let mut w = Writer::with_capacity(32);
    w.binary(b"_id", 4, id);
    w.finish()
}

/// RETVAL_OPTIONAL_BSON_T: null for an empty document, the document otherwise
unsafe fn ret_optional_doc(rv: *mut sys::zval, doc: Option<Vec<u8>>) {
    match doc {
        Some(d) if d.len() > 5 => {
            if let Some(z) = super::to_zval(&d, &Default::default()) {
                zval::write(rv, z);
            }
        }
        _ => zval::write(rv, zval::null()),
    }
}

/// findAndModify's "value": the document, or an empty one for null
fn fam_value(reply: &[u8]) -> Result<Option<Vec<u8>>, Error> {
    match find(reply, b"value") {
        Some(Value::Document(d)) => Ok(Some(d.to_vec())),
        Some(Value::Null) | None => Ok(None),
        Some(_) => Err(Error::new(domain::CLIENT, code::CLIENT_INVALID_ENCRYPTION_STATE, "expected field value to be a document or null")),
    }
}

unsafe fn parse_keyid_and_name(ex: Ex) -> Option<(Vec<u8>, Vec<u8>)> {
    let mut z: *mut sys::zval = null_mut();
    let (mut p, mut l): (*const u8, usize) = (null_mut(), 0);
    if sys::zend_parse_parameters(call::num_args(ex), c"Os".as_ptr(), &mut z, crate::bson::ce().binary, &mut p, &mut l) != 0 {
        return None;
    }
    let id = keyid_arg(z)?;
    Some((id, std::slice::from_raw_parts(p, l).to_vec()))
}

/// ClientEncryption::addKeyAltName(Binary $keyId, string $keyAltName): the previous key document or null
pub unsafe extern "C" fn zim_MongoDB_Driver_ClientEncryption_addKeyAltName(ex: Ex, rv: *mut sys::zval) {
    let Some((id, name)) = parse_keyid_and_name(ex) else { return };
    let Some((kv, _)) = parts(me(ex)) else { return };
    // BCON_UTF8(keyaltname): a C string
    let name = zstr::c_part(&name).to_vec();
    let mut u = Writer::with_capacity(64);
    let p = u.begin(T_DOCUMENT, b"$addToSet");
    u.utf8(b"keyAltNames", &name);
    u.end(p);
    match kv.find_and_modify(&id_doc(&id), &u.finish(), false) {
        Err(e) => throw_io(&e),
        Ok(reply) => match fam_value(&reply) {
            Err(e) => super::throw_error(&e, None),
            Ok(d) => ret_optional_doc(rv, d),
        },
    }
}

/// ClientEncryption::removeKeyAltName(Binary $keyId, string $keyAltName)
pub unsafe extern "C" fn zim_MongoDB_Driver_ClientEncryption_removeKeyAltName(ex: Ex, rv: *mut sys::zval) {
    let Some((id, name)) = parse_keyid_and_name(ex) else { return };
    let Some((kv, _)) = parts(me(ex)) else { return };
    let name = zstr::c_part(&name).to_vec();
    // [{$set: {keyAltNames: {$cond: [{$eq: ["$keyAltNames", [name]]}, "$$REMOVE",
    //   {$filter: {input: "$keyAltNames", cond: {$ne: ["$$this", name]}}}]}}}]
    let mut w = Writer::with_capacity(256);
    let s0 = w.begin(T_DOCUMENT, b"0");
    let set = w.begin(T_DOCUMENT, b"$set");
    let kan = w.begin(T_DOCUMENT, b"keyAltNames");
    let cond = w.begin(T_ARRAY, b"$cond");
    let c0 = w.begin(T_DOCUMENT, b"0");
    let eq = w.begin(T_ARRAY, b"$eq");
    w.utf8(b"0", b"$keyAltNames");
    let one = w.begin(T_ARRAY, b"1");
    w.utf8(b"0", &name);
    w.end(one);
    w.end(eq);
    w.end(c0);
    w.utf8(b"1", b"$$REMOVE");
    let c2 = w.begin(T_DOCUMENT, b"2");
    let filt = w.begin(T_DOCUMENT, b"$filter");
    w.utf8(b"input", b"$keyAltNames");
    let fc = w.begin(T_DOCUMENT, b"cond");
    let ne = w.begin(T_ARRAY, b"$ne");
    w.utf8(b"0", b"$$this");
    w.utf8(b"1", &name);
    w.end(ne);
    w.end(fc);
    w.end(filt);
    w.end(c2);
    w.end(cond);
    w.end(kan);
    w.end(set);
    w.end(s0);
    match kv.find_and_modify(&id_doc(&id), &w.finish(), true) {
        Err(e) => throw_io(&e),
        Ok(reply) => match fam_value(&reply) {
            Err(e) => super::throw_error(&e, None),
            Ok(d) => ret_optional_doc(rv, d),
        },
    }
}

/// ClientEncryption::createDataKey(string $kmsProvider, ?array $options = null): the new key's id
pub unsafe extern "C" fn zim_MongoDB_Driver_ClientEncryption_createDataKey(ex: Ex, rv: *mut sys::zval) {
    let (mut p, mut l): (*const u8, usize) = (null_mut(), 0);
    let mut options: *mut sys::zval = null_mut();
    if sys::zend_parse_parameters(call::num_args(ex), c"s|a!".as_ptr(), &mut p, &mut l, &mut options) != 0 {
        return;
    }
    let provider = zstr::c_part(std::slice::from_raw_parts(p, l)).to_vec();
    // phongo_clientencryption_datakey_opts_from_zval
    let mut alt_names: Vec<Vec<u8>> = Vec::new();
    let mut key_material: Option<Vec<u8>> = None;
    let mut master_key: Option<Vec<u8>> = None;
    if let Some(z) = opt(options, b"keyAltNames") {
        if !zval::is(z, sys::IS_ARRAY) {
            throw(E::InvalidArgument, format!("Expected keyAltNames to be array, {} given", zval::type_name(z)));
            return;
        }
        let mut failed = false;
        hash::each((*z).value.arr, true, |k, v| {
            if !zval::is(v, sys::IS_STRING) {
                let mut buf = String::new();
                let key = String::from_utf8_lossy(hash::key_bytes(k, &mut buf)).into_owned();
                throw(E::InvalidArgument, format!("Expected keyAltName with index \"{key}\" to be string, {} given", zval::type_name(v)));
                failed = true;
                return false;
            }
            alt_names.push(zstr::c_part(zval::str_bytes(v)).to_vec());
            true
        });
        if failed {
            return;
        }
    }
    if let Some(z) = opt(options, b"keyMaterial") {
        if !zval::is(z, sys::IS_OBJECT) || !obj::instanceof((*(*z).value.obj).ce, crate::bson::ce().binary) {
            throw(E::InvalidArgument, format!("Expected \"keyMaterial\" option to be MongoDB\\BSON\\Binary, {} given", zval::type_name(z)));
            return;
        }
        key_material = Some(obj::data::<crate::bson::types::Binary>((*z).value.obj).data.clone());
    }
    if let Some(z) = opt(options, b"masterKey") {
        if !zval::is(z, sys::IS_OBJECT) && !zval::is(z, sys::IS_ARRAY) {
            throw(E::InvalidArgument, format!("Expected \"masterKey\" option to be array or object, {} given", zval::type_name(z)));
            return;
        }
        let Some(b) = encode::to_bson(z, encode::NONE) else { return };
        master_key = Some(b);
    }
    let Some((kv, crypt)) = parts(me(ex)) else { return };
    // _mongoc_crypt_create_datakey
    let doc = match create_datakey(crypt, kv, &provider, master_key.as_deref(), &alt_names, key_material.as_deref()) {
        Ok(d) => d,
        Err(e) => {
            throw_engine(&e);
            return;
        }
    };
    if let Err(e) = kv.insert_one(&doc) {
        throw_io(&e);
        return;
    }
    let mut it = Iter::new(&doc).expect("built above");
    it.find(b"_id");
    let mut w = Writer::with_capacity(48);
    w.element_from(Some(b"v"), &it);
    if let Some(z) = super::legacy_value(&w.finish()) {
        zval::write(rv, z);
    }
}

/// _mongoc_crypt_create_datakey: mongocrypt_ctx_setopt_key_encryption_key / key_alt_name /
/// key_material, mongocrypt_ctx_datakey_init and the state machine
fn create_datakey(crypt: &Crypt, kv: &KeyVault, provider: &[u8], master_key: Option<&[u8]>, alt_names: &[Vec<u8>], key_material: Option<&[u8]>) -> Result<Vec<u8>, mongo_crypt::engine::Error<IoError>> {
    // {provider: <kms_provider>} + the masterKey fields
    let mut w = Writer::with_capacity(128);
    w.utf8(b"provider", provider);
    if let Some(m) = master_key {
        w.concat(m);
    }
    let kek = mongo_crypt::opts::Kek::parse(&w.finish())?;
    for (i, a) in alt_names.iter().enumerate() {
        if alt_names[..i].contains(a) {
            return Err("duplicate keyAltNames found".into());
        }
    }
    crypt.data_key_doc(&mut KvIo(kv), &kek, key_material, alt_names)
}

/// ClientEncryption::deleteKey(Binary $keyId): the deleteOne result
pub unsafe extern "C" fn zim_MongoDB_Driver_ClientEncryption_deleteKey(ex: Ex, rv: *mut sys::zval) {
    let mut z: *mut sys::zval = null_mut();
    if sys::zend_parse_parameters(call::num_args(ex), c"O".as_ptr(), &mut z, crate::bson::ce().binary) != 0 {
        return;
    }
    let Some(id) = keyid_arg(z) else { return };
    let Some((kv, _)) = parts(me(ex)) else { return };
    match kv.delete_one(&id_doc(&id)) {
        Err(e) => throw_io(&e),
        Ok(r) if r.len() <= 5 => throw(E::UnexpectedValue, "mongoc_client_encryption_delete_key returned an empty document"),
        Ok(r) => {
            if let Some(z) = super::to_zval(&r, &Default::default()) {
                zval::write(rv, z);
            }
        }
    }
}

/// ClientEncryption::getKey(Binary $keyId)
pub unsafe extern "C" fn zim_MongoDB_Driver_ClientEncryption_getKey(ex: Ex, rv: *mut sys::zval) {
    let mut z: *mut sys::zval = null_mut();
    if sys::zend_parse_parameters(call::num_args(ex), c"O".as_ptr(), &mut z, crate::bson::ce().binary) != 0 {
        return;
    }
    let Some(id) = keyid_arg(z) else { return };
    let Some((kv, _)) = parts(me(ex)) else { return };
    match kv.find_one(id_doc(&id)) {
        Err(e) => throw_io(&e),
        Ok(d) => ret_optional_doc(rv, d),
    }
}

/// ClientEncryption::getKeyByAltName(string $keyAltName)
pub unsafe extern "C" fn zim_MongoDB_Driver_ClientEncryption_getKeyByAltName(ex: Ex, rv: *mut sys::zval) {
    let (mut p, mut l): (*const u8, usize) = (null_mut(), 0);
    if sys::zend_parse_parameters(call::num_args(ex), c"s".as_ptr(), &mut p, &mut l) != 0 {
        return;
    }
    let name = zstr::c_part(std::slice::from_raw_parts(p, l)).to_vec();
    let Some((kv, _)) = parts(me(ex)) else { return };
    let mut w = Writer::with_capacity(32 + name.len());
    w.utf8(b"keyAltNames", &name);
    match kv.find_one(w.finish()) {
        Err(e) => throw_io(&e),
        Ok(d) => ret_optional_doc(rv, d),
    }
}

/// ClientEncryption::getKeys(): a Cursor over the key vault
pub unsafe extern "C" fn zim_MongoDB_Driver_ClientEncryption_getKeys(ex: Ex, rv: *mut sys::zval) {
    if !call::none(ex) {
        return;
    }
    let c = me(ex);
    let Some((kv, _)) = parts(c) else { return };
    let query = super::query::new_empty();
    let mut cur = match kv.find_cursor(Writer::new().finish()) {
        Ok(cur) => cur,
        Err(e) => {
            throw_io(&e);
            return;
        }
    };
    if !cur.advance_first() {
        return;
    }
    cur.database = Some(kv.db.clone());
    cur.collection = Some(kv.coll.clone());
    cur.zquery = query;
    let mgr = if zval::is(&c.kv_manager, sys::IS_OBJECT) { c.kv_manager.value.obj } else { null_mut() };
    zval::write(rv, super::cursor::new_cursor(cur, mgr));
}

/// ClientEncryption::rewrapManyDataKey(array|object $filter, ?array $options = null)
pub unsafe extern "C" fn zim_MongoDB_Driver_ClientEncryption_rewrapManyDataKey(ex: Ex, rv: *mut sys::zval) {
    let mut zfilter: *mut sys::zval = null_mut();
    let mut options: *mut sys::zval = null_mut();
    if sys::zend_parse_parameters(call::num_args(ex), c"A|a!".as_ptr(), &mut zfilter, &mut options) != 0 {
        return;
    }
    let Some(filter) = encode::to_bson(zfilter, encode::NONE) else { return };
    let provider = opt(options, b"provider").map(|z| fetch_string(z));
    let mut master_key = None;
    if let Some(z) = opt(options, b"masterKey") {
        if !zval::is(z, sys::IS_OBJECT) && !zval::is(z, sys::IS_ARRAY) {
            throw(E::InvalidArgument, format!("Expected \"masterKey\" option to be array or object, {} given", zval::type_name(z)));
            return;
        }
        if provider.is_none() {
            throw(E::InvalidArgument, "The \"masterKey\" option should not be specified without \"provider\"");
            return;
        }
        let Some(b) = encode::to_bson(z, encode::NONE) else { return };
        master_key = Some(b);
    }
    let Some((kv, crypt)) = parts(me(ex)) else { return };
    // _mongoc_crypt_rewrap_many_datakey: the key documents matching `filter`, re-encrypted
    let docs = match kv.find_all(filter) {
        Ok(d) => d,
        Err((mut e, r)) => {
            e.message = format!("key vault error: {}", e.message);
            throw_io(&(e, r));
            return;
        }
    };
    let new_kek = match &provider {
        None => None,
        Some(p) => {
            let mut w = Writer::with_capacity(128);
            w.utf8(b"provider", zstr::c_part(p));
            if let Some(m) = &master_key {
                w.concat(m);
            }
            match mongo_crypt::opts::Kek::parse(&w.finish()) {
                Ok(k) => Some(k),
                Err(m) => {
                    throw_crypt(&m);
                    return;
                }
            }
        }
    };
    let mut ops = Vec::new();
    for d in &docs {
        match crypt.rewrap(&mut KvIo(kv), d, new_kek.as_ref()) {
            Ok((id, set)) => {
                let mut u = Writer::with_capacity(set.len() + 64);
                u.document(b"$set", &set);
                let p = u.begin(T_DOCUMENT, b"$currentDate");
                u.bool(b"updateDate", true);
                u.end(p);
                ops.push((id_doc(&id), u.finish()));
            }
            Err(e) => {
                throw_engine(&e);
                return;
            }
        }
    }
    let mut w = Writer::with_capacity(256);
    if ops.is_empty() {
        w.null(b"bulkWriteResult");
    } else {
        match kv.update_many_ones(&ops) {
            Ok(r) => w.document(b"bulkWriteResult", &r),
            Err(e) => {
                throw_io(&e);
                return;
            }
        }
    }
    if let Some(z) = super::to_zval(&w.finish(), &Default::default()) {
        zval::write(rv, z);
    }
}

/// phongo_clientencryption_encrypt_opts_from_zval (the FLE1 and Queryable Encryption options)
unsafe fn encrypt_opts(options: *mut sys::zval) -> Option<ExplicitOpts> {
    let mut o = ExplicitOpts::default();
    if let Some(z) = opt(options, b"contentionFactor") {
        o.contention_factor = Some(sys::zval_get_long_func(z, false));
    }
    if let Some(z) = opt(options, b"keyId") {
        let d = encode::to_bson_value_doc(z, b"v")?;
        // _create_explicit_state_machine: only a binary key id is used
        if let Some(Value::Binary(st, b)) = find(&d, b"v") {
            if st != 4 {
                super::throw_error(&invalid_arg("keyid must be a UUID"), None);
                return None;
            }
            o.key_id = Some(b.to_vec());
        }
    }
    if let Some(z) = opt(options, b"keyAltName") {
        o.key_alt_name = Some(zstr::c_part(&fetch_string(z)).to_vec());
    }
    if let Some(z) = opt(options, b"algorithm") {
        o.algorithm = Some(zstr::c_part(&fetch_string(z)).to_vec());
    }
    if let Some(z) = opt(options, b"queryType") {
        o.query_type = Some(zstr::c_part(&fetch_string(z)).to_vec());
    }
    if let Some(r) = opt(options, b"rangeOpts") {
        // phongo_clientencryption_encrypt_range_opts_from_zval + append_bson_range_opts
        let (mut min, mut max, mut precision, mut sparsity, mut trim) = (None, None, None, None, None);
        if zval::is(r, sys::IS_ARRAY) {
            if let Some(z) = opt(r, b"trimFactor") {
                let v = sys::zval_get_long_func(z, false);
                if !(0..=i32::MAX as i64).contains(&v) {
                    throw(E::InvalidArgument, format!("Expected \"trimFactor\" range option to be a positive 32-bit integer, {v} given"));
                    return None;
                }
                trim = Some(v as i32);
            }
            if let Some(z) = opt(r, b"sparsity") {
                let v = sys::zval_get_long_func(z, false);
                if v < 0 {
                    throw(E::InvalidArgument, format!("Expected \"sparsity\" range option to be a positive 64-bit integer, {v} given"));
                    return None;
                }
                sparsity = Some(v);
            }
            if let Some(z) = opt(r, b"precision") {
                let v = sys::zval_get_long_func(z, false);
                if !(0..=i32::MAX as i64).contains(&v) {
                    throw(E::InvalidArgument, format!("Expected \"precision\" range option to be a positive 32-bit integer, {v} given"));
                    return None;
                }
                precision = Some(v as i32);
            }
            if let Some(z) = opt(r, b"min") {
                min = Some(encode::to_bson_value_doc(z, b"min")?);
            }
            if let Some(z) = opt(r, b"max") {
                max = Some(encode::to_bson_value_doc(z, b"max")?);
            }
        }
        let mut w = Writer::with_capacity(96);
        for d in [&min, &max].into_iter().flatten() {
            w.concat(d);
        }
        if let Some(p) = precision {
            w.int32(b"precision", p);
        }
        if let Some(sp) = sparsity {
            w.int64(b"sparsity", sp);
        }
        if let Some(t) = trim {
            w.int32(b"trimFactor", t);
        }
        o.range_opts = Some(w.finish());
    }
    if opt(options, b"stringOpts").is_some() {
        throw_crypt("Queryable Encryption's string algorithm is not supported by this build yet");
        return None;
    }
    Some(o)
}

/// ClientEncryption::encrypt(mixed $value, ?array $options = null): the Binary ciphertext
pub unsafe extern "C" fn zim_MongoDB_Driver_ClientEncryption_encrypt(ex: Ex, rv: *mut sys::zval) {
    let mut value: *mut sys::zval = null_mut();
    let mut options: *mut sys::zval = null_mut();
    if sys::zend_parse_parameters(call::num_args(ex), c"z|a!".as_ptr(), &mut value, &mut options) != 0 {
        return;
    }
    let Some(v) = encode::to_bson_value_doc(value, b"v") else { return };
    let Some(o) = encrypt_opts(options) else { return };
    let Some((kv, crypt)) = parts(me(ex)) else { return };
    match crypt.explicit_encrypt(&mut KvIo(kv), &v, &o) {
        Err(e) => throw_engine(&e),
        Ok(ct) => {
            let mut w = Writer::with_capacity(ct.len() + 16);
            w.binary(b"v", 6, &ct);
            if let Some(z) = super::legacy_value(&w.finish()) {
                zval::write(rv, z);
            }
        }
    }
}

/// ClientEncryption::encryptExpression(array|object $expr, ?array $options = null)
pub unsafe extern "C" fn zim_MongoDB_Driver_ClientEncryption_encryptExpression(ex: Ex, rv: *mut sys::zval) {
    let mut expr: *mut sys::zval = null_mut();
    let mut options: *mut sys::zval = null_mut();
    if sys::zend_parse_parameters(call::num_args(ex), c"z|a!".as_ptr(), &mut expr, &mut options) != 0 {
        return;
    }
    let Some(e) = encode::to_bson(expr, encode::NONE) else { return };
    let Some(o) = encrypt_opts(options) else { return };
    let Some((kv, crypt)) = parts(me(ex)) else { return };
    match crypt.explicit_encrypt_expression(&mut KvIo(kv), &e, &o) {
        Err(e) => throw_engine(&e),
        Ok(d) => {
            if let Some(z) = super::to_zval(&d, &Default::default()) {
                zval::write(rv, z);
            }
        }
    }
}

/// ClientEncryption::decrypt(BinaryInterface $value): the original value (legacy decoding)
pub unsafe extern "C" fn zim_MongoDB_Driver_ClientEncryption_decrypt(ex: Ex, rv: *mut sys::zval) {
    let mut z: *mut sys::zval = null_mut();
    let iface = super::lookup(c"mongodb\\bson\\binaryinterface");
    if sys::zend_parse_parameters(call::num_args(ex), c"O".as_ptr(), &mut z, iface) != 0 {
        return;
    }
    let Some(v) = encode::to_bson_value_doc(z, b"v") else { return };
    let Some((kv, crypt)) = parts(me(ex)) else { return };
    // mongoc_client_encryption_decrypt
    if !matches!(find(&v, b"v"), Some(Value::Binary(6, _))) {
        super::throw_error(&invalid_arg("ciphertext must be BSON binary subtype 6"), None);
        return;
    }
    match crypt.explicit_decrypt(&mut KvIo(kv), &v) {
        Err(e) => throw_engine(&e),
        Ok(d) => {
            if let Some(z) = super::legacy_value(&d) {
                zval::write(rv, z);
            }
        }
    }
}

/// Automatic decryption (mongoc_client_enable_auto_encryption with bypassAutoEncryption): every
/// successful reply of the client has its ciphertexts decrypted
struct AutoDecrypt {
    crypt: Crypt,
    /// the keyVaultClient's client (None: the encrypted client itself)
    kv: Option<ClientRef>,
    db: String,
    coll: String,
}

/// The key vault reads of automatic decryption, on the client given (no Cursor: the encrypted
/// client may be the one running the command)
struct CoreIo<'a> {
    cl: &'a mut mongo_core::client::Client,
    kv: Option<&'a ClientRef>,
    db: &'a str,
    coll: &'a str,
}

impl Io for CoreIo<'_> {
    type Error = Error;
    fn kms(&mut self, req: &mongo_crypt::kms::KmsRequest, done: &mut dyn FnMut(&[u8]) -> bool) -> Result<Vec<u8>, mongo_crypt::engine::KmsFailure<Error>> {
        kms_exchange(req, done)
    }
    fn stream_error(&self, msg: String) -> Error {
        stream_error(msg)
    }
    fn find_keys(&mut self, filter: &[u8]) -> Result<Vec<Vec<u8>>, Error> {
        let (db, coll) = (self.db, self.coll);
        let run = |cl: &mut mongo_core::client::Client| {
            let rp = super::manager::client_read_preference(&cl.uri);
            let rc = mongo_core::concern::ReadConcern { level: Some(b"majority".to_vec()) }.to_bson();
            cl.find_all(db, coll, filter, Some((&rp.prefs(), super::manager::rp_parts(&rp))), &rc)
        };
        let r = match self.kv {
            Some(kv) if kv.as_ptr() != (self.cl as *mut mongo_core::client::Client) => match kv.try_borrow_mut() {
                Ok(mut k) => run(&mut k),
                Err(_) => Err(Error::new(domain::CLIENT, code::CLIENT_INVALID_ENCRYPTION_STATE, "key vault client is busy")),
            },
            _ => run(self.cl),
        };
        r.map_err(|mut e| {
            e.message = format!("key vault error: {}", e.message);
            mongo_core::bulk::truncate_message(&mut e.message);
            e
        })
    }
}

impl mongo_core::autocrypt::AutoCrypt for AutoDecrypt {
    fn decrypt(&mut self, client: &mut mongo_core::client::Client, _db: &str, reply: &[u8]) -> Result<Vec<u8>, Error> {
        let mut io = CoreIo { cl: client, kv: self.kv.as_ref(), db: &self.db, coll: &self.coll };
        self.crypt.decrypt_doc(&mut io, reply).map_err(|e| match e {
            mongo_crypt::engine::Error::Crypt(m) => {
                let mut m = m;
                mongo_core::bulk::truncate_message(&mut m);
                Error::new(domain::CLIENT_SIDE_ENCRYPTION, 1, &m)
            }
            mongo_crypt::engine::Error::Io(e) => e,
        })
    }
}

/// phongo_manager_set_auto_encryption_opts + mongoc_client_enable_auto_encryption: the hook to
/// install (None: an exception was thrown)
pub unsafe fn auto_encryption(m: &mut super::manager::Manager, driver_options: *mut sys::zval, use_persistent: bool) -> Option<Box<dyn mongo_core::autocrypt::AutoCrypt>> {
    let Some(z) = opt(driver_options, b"autoEncryption") else { return None };
    if !zval::is(z, sys::IS_ARRAY) {
        throw(E::InvalidArgument, format!("Expected \"autoEncryption\" driver option to be array, {} given", zval::type_name(z)));
        return None;
    }
    let is_true = |k: &[u8]| opt(z, k).is_some_and(|v| sys::zend_is_true(v));
    let bypass_auto = is_true(b"bypassAutoEncryption");
    let bypass_qa = is_true(b"bypassQueryAnalysis");
    let doc_opt = |k: &str| -> Result<Option<Vec<u8>>, ()> {
        let Some(v) = opt(z, k.as_bytes()) else { return Ok(None) };
        if !zval::is(v, sys::IS_OBJECT) && !zval::is(v, sys::IS_ARRAY) {
            throw(E::InvalidArgument, format!("Expected \"{k}\" autoEncryption option to be an array or object, {} given", zval::type_name(v)));
            return Err(());
        }
        encode::to_bson(v, encode::NONE).map(Some).ok_or(())
    };
    let efc = doc_opt("encryptedFieldsMap").ok()?;
    if efc.is_some() {
        m.enc_fields_map = zval::copy(opt(z, b"encryptedFieldsMap").expect("found above"));
    }
    let mut kv: Option<ClientRef> = None;
    if let Some(v) = opt(z, b"keyVaultClient") {
        if !zval::is(v, sys::IS_OBJECT) || !obj::instanceof((*(*v).value.obj).ce, super::manager::CE) {
            throw(E::InvalidArgument, format!("Expected \"keyVaultClient\" autoEncryption option to be MongoDB\\Driver\\Manager, {} given", zval::type_name(v)));
            return None;
        }
        let km = obj::data::<super::manager::Manager>((*v).value.obj);
        if km.client.as_ref().is_some_and(super::is_persistent) != use_persistent {
            throw(E::InvalidArgument, "The \"disableClientPersistence\" option for a Manager and its \"keyVaultClient\" must be the same");
            return None;
        }
        kv = km.client.clone();
        m.kv_manager = zval::copy(v);
    }
    let mut ns = None;
    if let Some(v) = opt(z, b"keyVaultNamespace") {
        let s = fetch_string(v);
        if s.contains(&0) {
            throw(E::InvalidArgument, "Invalid namespace provided: namespaces may not contain a null byte");
            return None;
        }
        match String::from_utf8_lossy(&s).split_once('.') {
            Some((d, c)) => ns = Some((d.to_string(), c.to_string())),
            None => {
                throw(E::InvalidArgument, "Expected \"keyVaultNamespace\" autoEncryption option to contain a full collection namespace");
                return None;
            }
        }
    }
    let kms = doc_opt("kmsProviders").ok()?;
    let schema_map = doc_opt("schemaMap").ok()?;
    let _tls = doc_opt("tlsOptions").ok()?;
    let extra = doc_opt("extraOptions").ok()?;
    // mongoc_client_enable_auto_encryption
    let Some((db, coll)) = ns else {
        super::throw_error(&invalid_arg("Key vault namespace option required"), None);
        return None;
    };
    let Some(kms) = kms else {
        super::throw_error(&invalid_arg("KMS providers option required"), None);
        return None;
    };
    // _parse_extra
    if let Some(x) = &extra {
        let checks: [(&[u8], fn(&Value) -> bool, &str); 6] = [
            (b"mongocryptdBypassSpawn", |v| matches!(v, Value::Bool(_)), "Expected bool for option 'mongocryptdBypassSpawn'"),
            (b"mongocryptdSpawnPath", |v| matches!(v, Value::Utf8(_)), "Expected string for option 'mongocryptdSpawnPath'"),
            (b"mongocryptdSpawnArgs", |v| matches!(v, Value::Array(_)), "Expected array for option 'mongocryptdSpawnArgs'"),
            (b"mongocryptdURI", |v| matches!(v, Value::Utf8(_)), "Expected string for option 'mongocryptdURI'"),
            (b"cryptSharedLibPath", |v| matches!(v, Value::Utf8(_)), "Expected a string for 'cryptSharedLibPath'"),
            (b"cryptSharedLibRequired", |v| matches!(v, Value::Bool(_)), "Expected a bool for 'cryptSharedLibRequired'"),
        ];
        for (k, ok, msg) in checks {
            if let Some(v) = find(x, k) {
                if !ok(&v) {
                    super::throw_error(&invalid_arg(msg), None);
                    return None;
                }
            }
        }
    }
    // _mongoc_crypt_new: the KMS providers, then mongocrypt_init's map checks
    let mut crypt = match Crypt::new(&kms, None) {
        Ok(c) => c,
        Err(msg) => {
            throw_crypt(&msg);
            return None;
        }
    };
    if let Some(t) = &_tls {
        match kms_tls_options(t) {
            Ok(v) => crypt.tls = v,
            Err(m) => {
                throw_tls_arg(&m);
                return None;
            }
        }
    }
    if let (Some(sm), Some(ef)) = (&schema_map, &efc) {
        if let Some(mut it) = Iter::new(sm) {
            while it.next() {
                if find(ef, it.key()).is_some() {
                    throw_crypt(&format!("{} is present in both schema_map and encrypted_field_config_map", String::from_utf8_lossy(it.key())));
                    return None;
                }
            }
        }
    }
    if !bypass_auto {
        // query analysis (mongocryptd or crypt_shared, MongoDB Enterprise) and Queryable
        // Encryption's automatic encryption are not ported yet: refuse rather than send in clear
        let what = if bypass_qa { "Queryable Encryption's automatic encryption (\"bypassQueryAnalysis\")" } else { "automatic encryption (query analysis through mongocryptd or crypt_shared)" };
        throw(E::InvalidArgument, format!("Cannot enable automatic field-level encryption: {what} is not supported by this build yet. Set \"bypassAutoEncryption\" for automatic decryption with explicit encryption."));
        return None;
    }
    Some(Box::new(AutoDecrypt { crypt, kv, db, coll }))
}
