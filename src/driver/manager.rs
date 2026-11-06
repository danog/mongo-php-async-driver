//! MongoDB\Driver\Manager (src/MongoDB/Manager.c, phongo_client.c, phongo_execute.c).

use mongo_core::log;
use super::command::Command;
use super::cursor::Cursor;
use super::readpref::ReadPreference;
use mongo_core::uri::ReadMode;
use super::ClientRef;
use crate::bson::{throw, E};
use crate::zend::call::{self, Ex};
use crate::zend::{hash, obj, sys, zstr, zval};
use mongo_core::client::{Client, Settings};
use mongo_core::error::{code, domain, Error};
use mongo_core::topology::OpType;
use mongo_core::uri::Uri;
use std::ptr::{addr_of_mut, null_mut};

const URI_DEFAULT: &str = "mongodb://127.0.0.1/";

/// MONGOC_LOG_DOMAIN of phongo
const PHONGO: &str = "PHONGO";

#[derive(Default)]
pub struct Manager {
    pub client: Option<ClientRef>,
    /// the URI string as given (mongoc_uri_get_string), with its secrets redacted
    pub uri_string: String,
    /// phongo's client hash (the persistent-client key; never shown)
    pub client_hash: Vec<u8>,
    /// Manager::addSubscriber registrations
    pub subscribers: super::apm::Subscribers,
}

unsafe extern "C" fn free(o: *mut sys::zend_object) {
    // freed by the GC as garbage: its subscribers may be garbage too, nothing is dispatched
    if obj::freed_by_gc(o) {
        super::apm::quiet(|| free_manager(o));
    } else {
        free_manager(o);
    }
}

/// The subscribers it holds, for the GC (a subscriber referencing the Manager, or an object
/// holding it, is a collectable cycle).
unsafe extern "C" fn get_gc(o: *mut sys::zend_object, table: *mut *mut sys::zval, n: *mut i32) -> *mut sys::HashTable {
    let m = obj::data::<Manager>(o);
    obj::get_gc_with(o, table, n, m.subscribers.iter_mut().map(|(_, z, _)| z as *mut sys::zval))
}

unsafe fn free_manager(o: *mut sys::zend_object) {
    let m = obj::data::<Manager>(o);
    // a client of this Manager only (disableClientPersistence) is destroyed with it
    if let Some(c) = m.client.as_ref() {
        // phongo_client_unregister
        if super::is_persistent(c) {
            log::debug(PHONGO, || "Not destroying persistent client for Manager".to_owned());
        } else if std::rc::Rc::strong_count(c) == 2 {
            log::debug(PHONGO, || "Destroying non-persistent client for Manager".to_owned());
        }
        if std::rc::Rc::strong_count(c) == 2 && !super::is_persistent(c) {
            if let Ok(mut cl) = c.try_borrow_mut() {
                cl.end_pooled_sessions();
                // mongoc_topology_destroy's ServerClosed/TopologyChanged/TopologyClosed
                cl.close();
                // request shutdown: no more events (phongo_pclient_destroy)
                if (*std::ptr::addr_of!(sys::executor_globals)).flags & EG_FLAGS_IN_SHUTDOWN != 0 {
                    cl.clear_events();
                }
            }
            super::apm::flush(c);
        }
    }
    super::apm::unregister_manager(o);
    // the subscriber references are released before the Manager's state is freed
    drop(std::mem::take(&mut obj::data::<Manager>(o).subscribers));
    obj::free::<Manager>(o);
}

/// The subscribers of the Manager object `o`.
pub unsafe fn subscribers_of<'a>(o: *mut sys::zend_object) -> &'a mut super::apm::Subscribers {
    &mut obj::data::<Manager>(o).subscribers
}

static mut H_MANAGER: sys::zend_object_handlers = unsafe { std::mem::zeroed() };
pub static mut CE: *mut sys::zend_class_entry = null_mut();

unsafe extern "C" fn create(c: *mut sys::zend_class_entry) -> *mut sys::zend_object {
    obj::alloc(c, Manager::default(), addr_of_mut!(H_MANAGER))
}

/// phongo_manager_get_debug_info: the URI (redacted), a Server for every known server, and the
/// crypt_shared version (none: client-side encryption is not supported yet).
unsafe extern "C" fn get_debug_info(o: *mut sys::zend_object, is_temp: *mut i32) -> *mut sys::HashTable {
    *is_temp = 1;
    let m = obj::data::<Manager>(o);
    let ht = hash::new(3);
    hash::str_update(ht, b"uri", zval::string(m.uri_string.as_bytes()));
    let ids: Vec<u32> = m.client.as_ref().and_then(|c| c.try_borrow().ok().map(|cl| cl.td.known_servers().map(|s| s.id).collect())).unwrap_or_default();
    let cluster = hash::new(ids.len() as u32);
    if let Some(c) = &m.client {
        for id in ids {
            hash::push(cluster, super::server::new_server(c.clone(), id, o));
        }
    }
    hash::str_update(ht, b"cluster", zval::arr(cluster));
    hash::str_update(ht, b"cryptSharedVersion", zval::null());
    ht
}

pub unsafe fn init_MongoDB_Driver_Manager(c: *mut sys::zend_class_entry) {
    CE = c;
    (*c).__bindgen_anon_2.create_object = Some(create);
    let h = obj::handlers::<Manager>(addr_of_mut!(H_MANAGER));
    h.free_obj = Some(free);
    h.get_debug_info = Some(get_debug_info);
    h.get_gc = Some(get_gc);
}

/// The process-wide handshake data (mongoc_handshake_data_append): libmongoc freezes it when
/// the first client is created, so the first Manager's "driver" option wins.
static HANDSHAKE: std::sync::OnceLock<(String, String, String)> = std::sync::OnceLock::new();

/// phongo_concat_handshake_data: "<default> / <custom> " (a trailing space, as phongo builds it).
fn concat_handshake(default: &str, custom: Option<&[u8]>) -> String {
    match custom {
        Some(c) => format!("{default} / {} ", String::from_utf8_lossy(c)),
        None => format!("{default} "),
    }
}

/// phongo_set_handshake_data: the "driver" driver option (name, version, platform strings).
unsafe fn handshake_data(driver_options: *mut sys::zval) -> Option<(String, String, String)> {
    let mut parts: [Option<Vec<u8>>; 3] = [None, None, None];
    if !driver_options.is_null() {
        if let Some(d) = hash::find((*driver_options).value.arr, b"driver") {
            let d = zval::deref(d);
            if !zval::is(d, sys::IS_ARRAY) {
                throw(E::InvalidArgument, format!("Expected \"driver\" driver option to be an array, {} given", zval::type_name(d)));
                return None;
            }
            for (i, k) in ["name", "version", "platform"].iter().enumerate() {
                if let Some(v) = hash::find((*d).value.arr, k.as_bytes()) {
                    let v = zval::deref(v);
                    if !zval::is(v, sys::IS_STRING) {
                        throw(E::InvalidArgument, format!("Expected \"{k}\" handshake option to be a string, {} given", zval::type_name(v)));
                        return None;
                    }
                    parts[i] = Some(zval::str_bytes(v).to_vec());
                }
            }
        }
    }
    let [n, v, p] = parts;
    Some((concat_handshake("ext-mongodb:PHP", n.as_deref()), concat_handshake(mongo_core::EXT_VERSION, v.as_deref()), concat_handshake(&php_platform(), p.as_deref())))
}

/// A shallow copy of `ht` (ZVAL_DUP of an array: the values are shared).
unsafe fn dup(ht: *mut sys::HashTable) -> *mut sys::HashTable {
    let out = hash::new(hash::count(ht));
    hash::each(ht, false, |k, v| {
        match k {
            hash::Key::Index(i) => hash::index_update(out, i, zval::copy(v)),
            k => {
                let mut buf = String::new();
                hash::str_update(out, hash::key_bytes(k, &mut buf), zval::copy(v));
            }
        }
        true
    });
    out
}

/// phongo_manager_prep_uri_options on a copy (the caller's array is not changed): the
/// elements of readPreferenceTags become objects (an empty array encodes as an empty
/// document), and a non-string true CANONICALIZE_HOST_NAME property becomes "true".
unsafe fn prep_uri_options(options: *mut sys::zval) -> sys::zval {
    let out = dup((*options).value.arr);
    let mut buf = String::new();
    let mut updates: Vec<(Vec<u8>, sys::zval)> = Vec::new();
    hash::each(out, true, |k, v| {
        let hash::Key::Str(_) = k else { return true };
        let key = hash::key_bytes(k, &mut buf).to_vec();
        let v = zval::deref(v);
        if key.eq_ignore_ascii_case(b"readpreferencetags") && zval::is(v, sys::IS_ARRAY) {
            let tags = hash::new(hash::count((*v).value.arr));
            hash::each((*v).value.arr, true, |tk, tv| {
                let mut c = zval::copy(zval::deref(tv));
                if zval::is(&c, sys::IS_ARRAY) {
                    sys::convert_to_object(&mut c);
                }
                match tk {
                    hash::Key::Index(i) => hash::index_update(tags, i, c),
                    tk => {
                        let mut b = String::new();
                        hash::str_update(tags, hash::key_bytes(tk, &mut b), c)
                    }
                }
                true
            });
            updates.push((key, zval::arr(tags)));
        } else if key.eq_ignore_ascii_case(b"authmechanismproperties") && zval::is(v, sys::IS_ARRAY) {
            let props = dup((*v).value.arr);
            let mut fix: Vec<Vec<u8>> = Vec::new();
            hash::each(props, true, |pk, pv| {
                if let hash::Key::Str(_) = pk {
                    let mut b = String::new();
                    let name = hash::key_bytes(pk, &mut b).to_vec();
                    let pv = zval::deref(pv);
                    if name.eq_ignore_ascii_case(b"CANONICALIZE_HOST_NAME") && !zval::is(pv, sys::IS_STRING) && sys::zend_is_true(pv) {
                        fix.push(name);
                    }
                }
                true
            });
            for name in fix {
                hash::str_update(props, &name, zval::string(b"true"));
            }
            updates.push((key, zval::arr(props)));
        }
        true
    });
    for (k, v) in updates {
        hash::str_update(out, &k, v);
    }
    zval::arr(out)
}

/// phongo_manager_make_client_hash: serialize(['pid' => getpid(), 'uri' => sha256(uri),
/// 'options' => options, 'driverOptions' => driverOptions]) with an autoEncryption
/// keyVaultClient Manager replaced by its own hash. None after an exception. The hash holds
/// the option values (credentials too): it is only ever a lookup key, never shown.
unsafe fn client_hash(uri_string: &str, options: *mut sys::zval, driver_options: *mut sys::zval) -> Option<Vec<u8>> {
    let args = hash::new(4);
    hash::str_update(args, b"pid", zval::long(i64::from(std::process::id())));
    hash::str_update(args, b"uri", zval::string(&mongo_core::auth::scram::sha256(uri_string.as_bytes())));
    hash::str_update(args, b"options", if options.is_null() { zval::null() } else { zval::copy(options) });
    let dopts = if driver_options.is_null() {
        zval::null()
    } else {
        let mut d = zval::copy(driver_options);
        let kvc = hash::find((*driver_options).value.arr, b"autoEncryption")
            .map(|a| zval::deref(a))
            .filter(|a| zval::is(*a, sys::IS_ARRAY))
            .and_then(|a| hash::find((*a).value.arr, b"keyVaultClient"))
            .map(|k| zval::deref(k))
            .filter(|k| zval::is(*k, sys::IS_OBJECT) && obj::instanceof((*(**k).value.obj).ce, CE));
        if let Some(k) = kvc {
            let h = obj::data::<Manager>((*k).value.obj).client_hash.clone();
            let src = hash::find((*driver_options).value.arr, b"autoEncryption").map(|a| zval::deref(a)).expect("checked above");
            let ae = dup((*src).value.arr);
            hash::str_update(ae, b"keyVaultClient", zval::string(&h));
            let dh = dup((*driver_options).value.arr);
            hash::str_update(dh, b"autoEncryption", zval::arr(ae));
            d = zval::arr(dh);
        }
        d
    };
    hash::str_update(args, b"driverOptions", dopts);
    let mut a = [zval::arr(args)];
    let r = call::method(null_mut(), c"serialize", &mut a);
    if call::has_exception() || !zval::is(&r, sys::IS_STRING) {
        return None;
    }
    Some(zval::str_bytes(&r).to_vec())
}

pub unsafe extern "C" fn zim_MongoDB_Driver_Manager___construct(ex: Ex, rv: *mut sys::zval) {
    construct(ex, rv);
    // the constructor's messages, before its exception (if any) propagates
    super::log::flush();
}

/// The URI warnings libmongoc logs while parsing or changing the URI (MONGOC_WARNING).
fn log_uri_warnings(uri: &mut Uri) {
    for w in std::mem::take(&mut uri.warnings) {
        log::warning(log::MONGOC, || w);
    }
}

unsafe fn construct(ex: Ex, _rv: *mut sys::zval) {
    let (mut p, mut l): (*const u8, usize) = (null_mut(), 0);
    let mut options: *mut sys::zval = null_mut();
    let mut driver_options: *mut sys::zval = null_mut();
    if sys::zend_parse_parameters(call::num_args(ex), c"|s!a!a!".as_ptr(), &mut p, &mut l, &mut options, &mut driver_options) != 0 {
        return;
    }
    let uri_string = if p.is_null() { URI_DEFAULT.to_owned() } else { String::from_utf8_lossy(std::slice::from_raw_parts(p, l)).into_owned() };
    // the prepared copy of the options is what phongo hashes and encodes
    let mut prepared = if options.is_null() { None } else { Some(prep_uri_options(options)) };
    let options: *mut sys::zval = prepared.as_mut().map_or(null_mut(), |z| z as *mut sys::zval);
    // phongo_manager_init: the hash first; a persistent client found for it is used as is
    let Some(key) = client_hash(&uri_string, options, driver_options) else { return };
    let me = obj::data::<Manager>(call::this(ex));
    me.client_hash = key.clone();
    // safety: the reference shows the URI as given, password included
    me.uri_string = mongo_core::uri::redact(&uri_string);
    let disable_persistence = !driver_options.is_null() && hash::find((*driver_options).value.arr, b"disableClientPersistence").is_some_and(|z| sys::zend_is_true(zval::deref(z)));
    if !disable_persistence {
        if let Some(c) = super::find_persistent_client(&key) {
            // safety: the reference prints the hash itself (the URI's SHA-256 and the option values)
            log::debug(PHONGO, || format!("Found client for hash: {}", log::fingerprint(&key)));
            super::apm::register_manager(call::this(ex), c.clone());
            me.client = Some(c);
            return;
        }
    }
    let doc = if options.is_null() {
        None
    } else {
        match crate::bson::encode::to_bson(options, 0) {
            Some(d) => Some(d),
            None => return,
        }
    };
    let mut parsed = Uri::parse(&uri_string);
    // libmongoc logs while parsing, phongo after
    if let Ok(u) = &mut parsed {
        log_uri_warnings(u);
    }
    // safety: the reference logs the URI as given, password included
    log::debug(PHONGO, || format!("Connection string: '{}'", mongo_core::uri::redact(&uri_string)));
    let mut uri = match parsed {
        Ok(u) => u,
        Err(e) => {
            // safety: the reference quotes the URI as given, password included
            throw(E::InvalidArgument, format!("Failed to parse MongoDB URI: '{}'. {e}.", mongo_core::uri::redact(&uri_string)));
            return;
        }
    };
    if let Some(d) = &doc {
        let r = super::uri_options::apply(&mut uri, d);
        log_uri_warnings(&mut uri);
        if let Err(m) = r {
            throw(E::InvalidArgument, m);
            return;
        }
    }
    // phongo_make_ssl_opt: ca_dir and crl_file (they turn TLS on, as mongoc_client_set_ssl_opts
    // does); the TLS keys of other drivers or of ext-mongodb 1.x are refused rather than ignored
    let (mut ca_dir, mut crl_file) = (None, None);
    if !driver_options.is_null() {
        if let Some(k) = legacy_tls_driver_option((*driver_options).value.arr) {
            throw(E::InvalidArgument, format!("Unsupported driver option \"{k}\": TLS is configured with the URI options (tls, tlsCAFile, tlsCertificateKeyFile, ...) and the \"ca_dir\" and \"crl_file\" driver options"));
            return;
        }
        let ht = (*driver_options).value.arr;
        ca_dir = hash::find(ht, b"ca_dir").map(|z| fetch_string(zval::deref(z)));
        crl_file = hash::find(ht, b"crl_file").map(|z| fetch_string(zval::deref(z)));
    }
    log::debug(PHONGO, || {
        format!(
            "Creating Manager, phongo-{}[{}] - mongoc-{}(bundled), libbson-{}(bundled), php-{}",
            mongo_core::EXT_VERSION,
            mongo_core::EXT_STABILITY,
            mongo_core::LIBMONGOC_VERSION,
            mongo_core::LIBMONGOC_VERSION,
            php_version()
        )
    });
    let Some(hs) = handshake_data(driver_options) else { return };
    log::debug(PHONGO, || format!("Setting driver handshake data: {{ name: '{}', version: '{}', platform: '{}' }}", hs.0, hs.1, hs.2));
    let hs = HANDSHAKE.get_or_init(|| hs).clone();
    // phongo checks the option after creating the client: a client error comes first
    let api = super::serverapi::from_driver_options(driver_options);
    let make = || {
        // mongoc_topology_new finalizes the URI again (the options may have changed it)
        let r = uri.finalize();
        log_uri_warnings(&mut uri);
        r.map_err(|m| Error::new(domain::COMMAND, code::COMMAND_INVALID_ARG, m))?;
        let mut s = Settings::from_uri(&uri);
        if ca_dir.is_some() || crl_file.is_some() {
            let t = s.tls.get_or_insert_with(|| mongo_core::tls::TlsOptions::from_uri(&uri));
            t.ca_dir = ca_dir.clone();
            t.crl_file = crl_file.clone();
        }
        (s.metadata.driver_name, s.metadata.driver_version, s.metadata.platform) = hs;
        if let Ok(Some(a)) = &api {
            s.hello.api_version = Some(a.version.clone());
            s.hello.api_strict = a.strict;
            s.hello.api_deprecation_errors = a.deprecation_errors;
        }
        Client::new(uri, s)
    };
    // phongo_manager_set_auto_encryption_opts of a build without libmongocrypt: refused, so that
    // nothing is ever written unencrypted when automatic encryption was asked for
    let auto_encryption = !driver_options.is_null() && hash::find((*driver_options).value.arr, b"autoEncryption").is_some();
    let r = match &api {
        Err(msg) => {
            if let Err(e) = make() {
                throw(E::InvalidArgument, format!("Failed to parse URI options: {}", e.message));
            } else if auto_encryption {
                throw(E::InvalidArgument, no_cse("Cannot enable automatic field-level encryption."));
            } else {
                throw(E::InvalidArgument, msg.clone());
            }
            return;
        }
        Ok(_) if auto_encryption => {
            if let Err(e) = make() {
                throw(E::InvalidArgument, format!("Failed to parse URI options: {}", e.message));
            } else {
                throw(E::InvalidArgument, no_cse("Cannot enable automatic field-level encryption."));
            }
            return;
        }
        Ok(_) => make().map(|c| std::rc::Rc::new(std::cell::RefCell::new(c))),
    };
    match r {
        Ok(c) => {
            log::debug(PHONGO, || format!("Created client with hash: {}", log::fingerprint(&key)));
            if !disable_persistence {
                log::debug(PHONGO, || format!("Stored persistent client with hash: {}", log::fingerprint(&key)));
                super::store_persistent_client(key, c.clone());
            } else {
                log::debug(PHONGO, || "Stored non-persistent client".to_owned());
            }
            super::apm::register_manager(call::this(ex), c.clone());
            me.client = Some(c);
        }
        // phongo_client: mongoc_client_new_from_uri_with_error failed
        Err(e) => throw(E::InvalidArgument, format!("Failed to parse URI options: {}", e.message)),
    }
}

/// A TLS key of ext-mongodb 1.x's driver options (or any other tls*/ssl* key): ext-mongodb 2.x
/// ignores them, which would leave a connection the application meant to configure unverified
/// or in cleartext.
unsafe fn legacy_tls_driver_option(ht: *mut sys::HashTable) -> Option<String> {
    const KEYS: &[&str] = &["ca_file", "pem_file", "pem_pwd", "context", "allow_invalid_hostname", "weak_cert_validation", "allow_self_signed"];
    let mut found = None;
    let mut buf = String::new();
    hash::each(ht, true, |k, _| {
        if let hash::Key::Str(_) = k {
            let raw = String::from_utf8_lossy(hash::key_bytes(k, &mut buf)).into_owned();
            let key = raw.to_ascii_lowercase();
            if KEYS.contains(&key.as_str()) || key.starts_with("tls") || key.starts_with("ssl") {
                found = Some(raw);
                return false;
            }
        }
        true
    });
    found
}

/// phongo_fetch_string (php_array_fetch_string): the value converted to a string.
unsafe fn fetch_string(z: *mut sys::zval) -> String {
    let s = sys::zval_get_string_func(z);
    let r = String::from_utf8_lossy(zstr::bytes(s)).into_owned();
    zstr::release(s);
    r
}

/// phongo's handshake platform string ("PHP 8.4.13 ...")
fn php_platform() -> String {
    format!("PHP {}", php_version())
}

/// phongo_throw_exception_no_cse
pub fn no_cse(message: &str) -> String {
    format!("{message} Please recompile with support for libmongocrypt using the with-mongodb-client-side-encryption configure switch.")
}

/// Manager::getEncryptedFieldsMap(): null (automatic encryption cannot be enabled)
pub unsafe extern "C" fn zim_MongoDB_Driver_Manager_getEncryptedFieldsMap(ex: Ex, _rv: *mut sys::zval) {
    let _ = call::none(ex);
}

/// Manager::createClientEncryption(array $options): phongo_clientencryption_init of a build
/// without libmongocrypt
pub unsafe extern "C" fn zim_MongoDB_Driver_Manager_createClientEncryption(ex: Ex, _rv: *mut sys::zval) {
    let mut a: *mut sys::zval = null_mut();
    if sys::zend_parse_parameters(call::num_args(ex), c"a".as_ptr(), &mut a) != 0 {
        return;
    }
    throw(E::Runtime, no_cse("Cannot configure clientEncryption object."));
}

/// ClientEncryption::__construct(array $options): as createClientEncryption
pub unsafe extern "C" fn zim_MongoDB_Driver_ClientEncryption___construct(ex: Ex, rv: *mut sys::zval) {
    zim_MongoDB_Driver_Manager_createClientEncryption(ex, rv);
}

/// PHP_VERSION
fn php_version() -> String {
    std::ffi::CStr::from_bytes_with_nul(sys::PHP_VERSION).map(|c| c.to_string_lossy().into_owned()).unwrap_or_default()
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CmdType {
    Raw,
    Read,
    Write,
    ReadWrite,
}

/// The $readPreference contents for `rp` (mode, document).
pub fn rp_parts(rp: &ReadPreference) -> (ReadMode, Vec<u8>) {
    let mut w = mongo_core::bson::writer::Writer::with_capacity(64);
    rp.append_contents(&mut w, rp.mode);
    (rp.mode, w.finish())
}

/// phongo_parse_read_concern: the "readConcern" option.
unsafe fn parse_read_concern(options: *mut sys::zval) -> Result<Option<super::readconcern::ReadConcern>, ()> {
    if options.is_null() {
        return Ok(None);
    }
    if !zval::is(options, sys::IS_ARRAY) {
        throw(E::InvalidArgument, format!("Expected options to be array, {} given", zval::type_name(options)));
        return Err(());
    }
    let Some(z) = hash::sym_find((*options).value.arr, b"readConcern") else { return Ok(None) };
    let z = zval::deref(z);
    if !zval::is(z, sys::IS_OBJECT) || !obj::instanceof((*(*z).value.obj).ce, super::readconcern::CE) {
        throw(E::InvalidArgument, format!("Expected \"readConcern\" option to be MongoDB\\Driver\\ReadConcern, {} given", zval::type_name(z)));
        return Err(());
    }
    Ok(Some(obj::data::<super::readconcern::ReadConcern>((*z).value.obj).clone()))
}

/// phongo_validate_dbname
unsafe fn validate_dbname(db: &[u8]) -> bool {
    if db.contains(&0) {
        throw(E::InvalidArgument, "Invalid database name provided: database names may not contain a null byte");
        return false;
    }
    if db.contains(&b'.') {
        throw(E::InvalidArgument, format!("Invalid database name provided: database names may not contain a '.' character: {}", String::from_utf8_lossy(db)));
        return false;
    }
    true
}

/// phongo_execute_command for Manager::execute*Command and Server::execute*Command.
pub unsafe fn execute_command_args(ex: Ex, rv: *mut sys::zval, client: ClientRef, ty: CmdType, server_id: Option<u32>, mgr: *mut sys::zend_object) {
    let (mut p, mut l): (*const u8, usize) = (null_mut(), 0);
    let mut command: *mut sys::zval = null_mut();
    let mut options: *mut sys::zval = null_mut();
    let spec = if ty == CmdType::Raw { c"sO|z!" } else { c"sO|a!" };
    if sys::zend_parse_parameters(call::num_args(ex), spec.as_ptr(), &mut p, &mut l, &mut command, super::command::CE, &mut options) != 0 {
        return;
    }
    let dbb = std::slice::from_raw_parts(p, l);
    // phongo_parse_read_preference / phongo_parse_session (Manager: before server selection)
    let mut rp: Option<ReadPreference> = None;
    if matches!(ty, CmdType::Raw | CmdType::Read) {
        if !options.is_null() && !zval::is(options, sys::IS_ARRAY) {
            throw(E::InvalidArgument, format!("Expected options to be array, {} given", zval::type_name(options)));
            return;
        }
        if !options.is_null() {
            match parse_read_preference((*options).value.arr) {
                Ok(r) => rp = r,
                Err(()) => return,
            }
        }
    }
    let Ok(zsession) = super::parse_session(options, &client) else { return };
    let sid = match server_id {
        Some(s) => s,
        None => {
            let pinned = zsession.map_or(0, |z| super::session_server_id(z));
            if pinned > 0 {
                pinned
            } else {
                // phongo_manager_select_server: reads with the given (or, for read commands, the
                // client's) read preference; write commands select a primary
                let for_writes = matches!(ty, CmdType::Write | CmdType::ReadWrite);
                let eff = match (&rp, ty) {
                    (Some(r), _) => Some(r.clone()),
                    (None, CmdType::Read) => Some(client_read_preference(&client.borrow().uri)),
                    _ => None,
                };
                let prefs = eff.as_ref().map(ReadPreference::prefs);
                let op = if for_writes { OpType::Write } else { OpType::Read };
                let selected = client.borrow_mut().select_server_id(op, prefs.as_ref(), &[]);
                super::apm::flush(&client);
                match selected {
                    Ok((s, _)) => s,
                    Err(e) => {
                        super::throw_error(&e, None);
                        return;
                    }
                }
            }
        }
    };
    if !validate_dbname(dbb) {
        return;
    }
    let db = String::from_utf8_lossy(dbb).into_owned();
    let has_rc = matches!(ty, CmdType::Raw | CmdType::Read | CmdType::ReadWrite);
    let has_wc = matches!(ty, CmdType::Raw | CmdType::Write | CmdType::ReadWrite);
    let rc = if has_rc {
        match parse_read_concern(options) {
            Ok(r) => r,
            Err(()) => return,
        }
    } else {
        None
    };
    let mut wc: Option<super::writeconcern::WriteConcern> = None;
    let mut unack = false;
    if has_wc {
        match super::parse_write_concern(options) {
            Ok(w) => wc = w,
            Err(()) => return,
        }
        unack = match &wc {
            Some(w) => !w.is_acknowledged(),
            None if ty != CmdType::Raw => !super::writeconcern::WriteConcern::from_uri(&client.borrow().uri).is_acknowledged(),
            None => false,
        };
    }
    if zsession.is_some() && unack {
        throw(E::InvalidArgument, "Cannot combine \"session\" option with an unacknowledged write concern");
        return;
    }
    let cmd = obj::data::<Command>((*command).value.obj);
    let Some(bson) = cmd.bson.clone() else { return };
    let (batch_size, max_await) = (cmd.batch_size, cmd.max_await_time_ms);
    // phongo_create_implicit_session (ignoring errors)
    let mut implicit = if zsession.is_none() && !unack { client.borrow_mut().start_session(true, false).ok() } else { None };
    // _mongoc_client_command_with_opts
    let mode_read = matches!(ty, CmdType::Read | CmdType::ReadWrite);
    let mode_write = matches!(ty, CmdType::Write | CmdType::ReadWrite);
    let zs_rc = zsession.map(|z| super::session_rc(z));
    let result = {
        let mut cl = client.borrow_mut();
        let mut guard = zs_rc.as_ref().map(|r| r.borrow_mut());
        let mut sess: Option<&mut mongo_core::session::ClientSession> = match guard.as_deref_mut() {
            Some(g) => Some(g),
            None => implicit.as_mut(),
        };
        let in_txn = sess.as_ref().is_some_and(|s| s.in_txn());
        let cmd_name = mongo_core::cmdparts::command_name(&bson).unwrap_or_default();
        let invalid = |m: &str| Err(mongo_core::cmdparts::Failure::from(Error::new(domain::COMMAND, code::COMMAND_INVALID_ARG, m)));
        if in_txn && matches!(ty, CmdType::Read | CmdType::Raw) && rp.as_ref().is_some_and(|r| r.mode != ReadMode::Primary) {
            invalid("Read preference in a transaction must be primary")
        } else if in_txn && rc.is_some() {
            invalid("Cannot set read concern after starting transaction")
        } else if in_txn && wc.is_some() && cmd_name != "commitTransaction" && cmd_name != "abortTransaction" {
            invalid("Cannot set write concern after starting transaction")
        } else if let Err(e) = cl.stream_for_server(sid, true).map(|_| ()).and_then(|_| cl.check_server_for_session(sid, sess.as_deref_mut())) {
            Err(mongo_core::cmdparts::Failure::from(e))
        } else {
            let mut parts = mongo_core::cmdparts::Parts::new(&db, &bson);
            parts.is_read = mode_read;
            parts.is_write = mode_write;
            parts.is_raw = ty == CmdType::Raw;
            let mut retry_prefs = None;
            if matches!(ty, CmdType::Raw | CmdType::Read) {
                let eff = match (&rp, ty) {
                    (Some(r), _) => Some(r.clone()),
                    (None, CmdType::Read) => Some(client_read_preference(&cl.uri)),
                    _ => None,
                };
                parts.read_prefs = eff.as_ref().map(rp_parts);
                retry_prefs = eff.as_ref().map(ReadPreference::prefs);
            }
            if let Some(r) = &rc {
                parts.read_concern = r.to_bson();
            }
            if let Some(w) = &wc {
                parts.set_write_concern(w);
            } else if mode_write {
                let cw = super::writeconcern::WriteConcern::from_uri(&cl.uri);
                if !cw.is_default {
                    parts.set_write_concern(&cw);
                }
            }
            if mode_read && rc.is_none() && !in_txn {
                let crc = mongo_core::concern::ReadConcern::from_uri(&cl.uri);
                if !crc.is_default() {
                    parts.read_concern = crc.to_bson();
                }
            }
            parts.session = sess.as_deref_mut();
            cl.operation_id += 1;
            match cl.assemble(parts, sid, false) {
                Err(e) => Err(mongo_core::cmdparts::Failure::from(e)),
                Ok(mut c) => {
                    c.read_prefs = retry_prefs;
                    let r = if mode_write {
                        cl.run_retryable_write(&mut c, &[], sess.as_deref_mut()).0
                    } else {
                        let el = cl.read_eligibility(&c, ty == CmdType::Raw, mode_read);
                        cl.run_retryable_read(&mut c, sess.as_deref_mut(), el)
                    };
                    cl.release_temp_session(&mut c);
                    r
                }
            }
        }
    };
    super::apm::flush(&client);
    let reply = match result {
        Ok(r) => r,
        Err(f) => {
            if let Some(mut s) = implicit.take() {
                client.borrow_mut().end_session(&mut s);
            }
            super::throw_error(&f.error, (!f.reply.is_empty()).then_some(&f.reply[..]));
            return;
        }
    };
    if mode_write {
        if let Some(e) = write_concern_error(&reply) {
            if let Some(mut s) = implicit.take() {
                client.borrow_mut().end_session(&mut s);
            }
            super::throw_error(&e, Some(&reply));
            return;
        }
    }
    let has_cursor = matches!(mongo_core::cmdparts::find(&reply, b"cursor"), Some(mongo_core::bson::iter::Value::Document(_)));
    let mut c = if has_cursor {
        Cursor::from_reply(reply, client.clone(), sid, &db)
    } else {
        // create_wrapped_command_envelope
        let mut w = mongo_core::bson::writer::Writer::with_capacity(reply.len() + 64);
        let cd = w.begin(3, b"cursor");
        w.int64(b"id", 0);
        w.utf8(b"ns", format!("{db}.$cmd").as_bytes());
        let fb = w.begin(4, b"firstBatch");
        w.document(b"0", &reply);
        w.end(fb);
        w.end(cd);
        Cursor::from_reply(w.finish(), client.clone(), sid, &db)
    };
    if has_cursor {
        if max_await > 0 {
            c.opts.await_data = true;
            c.opts.max_await_time_ms = max_await as i64;
            c.opts.tailable = true;
        }
        if batch_size > 0 {
            c.opts.batch_size = batch_size as i64;
        }
        if let Some(mut it) = mongo_core::bson::iter::Iter::new(&bson) {
            if it.find(b"comment") {
                let mut w = mongo_core::bson::writer::Writer::with_capacity(64);
                w.element_from(Some(b"comment"), &it);
                c.opts.comment = Some(w.finish());
            }
        }
    }
    if let Some(z) = zsession {
        c.session = zval::copy(z);
    } else if let Some(s) = implicit {
        // phongo_create_implicit_session: a Session object the cursor holds until it is
        // exhausted (phongo_cursor_free_session_if_exhausted)
        let drp = client_read_preference(&client.borrow().uri);
        c.session = super::session::new_session(client.clone(), s, drp, mgr);
    }
    c.database = Some(db.clone());
    c.zcommand = zval::copy(command);
    if let Some(r) = &rp {
        c.zread_preference = super::readpref::new_readpreference(r.clone());
    }
    zval::write(rv, super::cursor::new_cursor(c, mgr));
}


/// phongo_parse_read_preference
unsafe fn parse_read_preference(options: *mut sys::HashTable) -> Result<Option<ReadPreference>, ()> {
    let Some(z) = hash::find(options, b"readPreference") else { return Ok(None) };
    let z = zval::deref(z);
    if !zval::is(z, sys::IS_OBJECT) || !obj::instanceof((*(*z).value.obj).ce, super::readpref::CE) {
        throw(E::InvalidArgument, format!("Expected \"readPreference\" option to be MongoDB\\Driver\\ReadPreference, {} given", zval::type_name(z)));
        return Err(());
    }
    Ok(Some(obj::data::<ReadPreference>((*z).value.obj).clone()))
}

/// The client's read preference (from the URI).
pub fn client_read_preference(uri: &Uri) -> ReadPreference {
    let mut w = mongo_core::bson::writer::Writer::with_capacity(64);
    for (i, set) in uri.read_tags.iter().enumerate() {
        let d = w.begin(3, i.to_string().as_bytes());
        for (k, v) in set {
            w.utf8(k.as_bytes(), v.as_bytes());
        }
        w.end(d);
    }
    ReadPreference { mode: uri.read_mode, tags: w.finish(), max_staleness_seconds: uri.max_staleness_seconds as i64, hedge: None }
}

/// _mongoc_parse_wc_err
fn write_concern_error(reply: &[u8]) -> Option<Error> {
    mongo_core::cmd::parse_wc_error(reply)
}

#[inline]
pub(crate) unsafe fn client_of(ex: Ex) -> Option<ClientRef> {
    obj::data::<Manager>(call::this(ex)).client.clone()
}

macro_rules! exec {
    ($f:ident, $ty:expr) => {
        pub unsafe extern "C" fn $f(ex: Ex, rv: *mut sys::zval) {
            let Some(c) = client_of(ex) else { return };
            execute_command_args(ex, rv, c, $ty, None, call::this(ex));
        }
    };
}
exec!(zim_MongoDB_Driver_Manager_executeCommand, CmdType::Raw);
exec!(zim_MongoDB_Driver_Manager_executeReadCommand, CmdType::Read);
exec!(zim_MongoDB_Driver_Manager_executeWriteCommand, CmdType::Write);
exec!(zim_MongoDB_Driver_Manager_executeReadWriteCommand, CmdType::ReadWrite);

pub unsafe extern "C" fn zim_MongoDB_Driver_Manager_selectServer(ex: Ex, rv: *mut sys::zval) {
    let mut rp: *mut sys::zval = null_mut();
    if sys::zend_parse_parameters(call::num_args(ex), c"|z!".as_ptr(), &mut rp) != 0 {
        return;
    }
    let Some(c) = client_of(ex) else { return };
    let rp = if rp.is_null() {
        client_read_preference(&c.borrow().uri)
    } else if zval::is(rp, sys::IS_OBJECT) && obj::instanceof((*(*rp).value.obj).ce, super::readpref::CE) {
        obj::data::<ReadPreference>((*rp).value.obj).clone()
    } else {
        sys::zend_argument_type_error(1, c"must be of type ?MongoDB\\Driver\\ReadPreference, %s given".as_ptr(), sys::zend_zval_value_name(rp));
        return;
    };
    let prefs = rp.prefs();
    let r = c.borrow_mut().select_server_id(OpType::Read, Some(&prefs), &[]);
    super::apm::flush(&c);
    match r {
        Ok((id, _)) => zval::write(rv, super::server::new_server(c, id, call::this(ex))),
        Err(e) => super::throw_error(&e, None),
    }
}

pub unsafe extern "C" fn zim_MongoDB_Driver_Manager_getServers(ex: Ex, rv: *mut sys::zval) {
    if !call::none(ex) {
        return;
    }
    let ht = hash::new(0);
    if let Some(c) = client_of(ex) {
        let ids: Vec<u32> = c.borrow().td.known_servers().map(|s| s.id).collect();
        for id in ids {
            hash::push(ht, super::server::new_server(c.clone(), id, call::this(ex)));
        }
    }
    zval::write(rv, zval::arr(ht));
}

#[allow(dead_code)]
fn _unused() -> *mut sys::zend_string {
    unsafe { zstr::new(b"") }
}

pub unsafe extern "C" fn zim_MongoDB_Driver_Manager_getReadPreference(ex: Ex, rv: *mut sys::zval) {
    if !call::none(ex) {
        return;
    }
    if let Some(c) = client_of(ex) {
        let rp = client_read_preference(&c.borrow().uri);
        zval::write(rv, super::readpref::new_readpreference(rp));
    }
}

/// Manager::startSession
pub unsafe extern "C" fn zim_MongoDB_Driver_Manager_startSession(ex: Ex, rv: *mut sys::zval) {
    let mut options: *mut sys::zval = null_mut();
    if sys::zend_parse_parameters(call::num_args(ex), c"|a!".as_ptr(), &mut options) != 0 {
        return;
    }
    let Some(c) = client_of(ex) else { return };
    let mut causal: Option<bool> = None;
    let mut snapshot: Option<bool> = None;
    let mut txn: Option<mongo_core::session::TxnOpts> = None;
    let mut rp = None;
    if !options.is_null() {
        let ht = (*options).value.arr;
        if let Some(z) = hash::find(ht, b"causalConsistency") {
            causal = Some(sys::zend_is_true(zval::deref(z)));
        }
        if let Some(z) = hash::find(ht, b"defaultTransactionOptions") {
            let z = zval::deref(z);
            if !zval::is(z, sys::IS_ARRAY) {
                throw(E::InvalidArgument, format!("Expected \"defaultTransactionOptions\" option to be an array, {} given", zval::type_name(z)));
                return;
            }
            match super::session::parse_txn_options(z) {
                Ok((t, r)) => {
                    txn = t;
                    rp = r;
                }
                Err(()) => return,
            }
        }
        if let Some(z) = hash::find(ht, b"snapshot") {
            snapshot = Some(sys::zend_is_true(zval::deref(z)));
        }
    }
    let snap = snapshot.unwrap_or(false);
    let cc = causal.unwrap_or(!snap);
    if cc && snap {
        throw(E::InvalidArgument, "Only one of \"causalConsistency\" and \"snapshot\" can be enabled");
        return;
    }
    let r = c.borrow_mut().start_session_with(cc, snap, txn.as_ref());
    super::apm::flush(&c);
    match r {
        Ok(cs) => {
            let drp = rp.unwrap_or_else(|| client_read_preference(&c.borrow().uri));
            zval::write(rv, super::session::new_session(c.clone(), cs, drp, call::this(ex)));
        }
        Err(e) => super::throw_error(&e, None),
    }
}

pub unsafe extern "C" fn zim_MongoDB_Driver_Manager_getWriteConcern(ex: Ex, rv: *mut sys::zval) {
    if !call::none(ex) {
        return;
    }
    if let Some(c) = client_of(ex) {
        let wc = super::writeconcern::WriteConcern::from_uri(&c.borrow().uri);
        zval::write(rv, super::writeconcern::new_writeconcern(wc));
    }
}

/// phongo_execute_bulk_write (Manager::executeBulkWrite selects a primary first; a Server
/// passes its id).
pub unsafe fn execute_bulk_write(ex: Ex, rv: *mut sys::zval, client: ClientRef, server_id: Option<u32>, mgr: *mut sys::zend_object) {
    let (mut p, mut l): (*const u8, usize) = (null_mut(), 0);
    let mut zbulk: *mut sys::zval = null_mut();
    let mut options: *mut sys::zval = null_mut();
    if sys::zend_parse_parameters(call::num_args(ex), c"sO|z!".as_ptr(), &mut p, &mut l, &mut zbulk, super::bulkwrite::CE, &mut options) != 0 {
        return;
    }
    let ns = std::slice::from_raw_parts(p, l).to_vec();
    let Ok(session) = super::parse_session(options, &client) else { return };
    let server_id = match server_id {
        Some(s) => s,
        None => {
            // phongo_manager_select_server(for_writes)
            let pinned = session.map_or(0, |z| super::session_server_id(z));
            if pinned > 0 {
                pinned
            } else {
                let selected = client.borrow_mut().select_server_id(OpType::Write, None, &[]);
                super::apm::flush(&client);
                match selected {
                    Ok((s, _)) => s,
                    Err(e) => {
                        super::throw_error(&e, None);
                        return;
                    }
                }
            }
        }
    };
    let bw = obj::data::<super::bulkwrite::BulkWrite>((*zbulk).value.obj);
    if bw.executed {
        throw(E::InvalidArgument, "BulkWrite objects may only be executed once and this instance has already been executed");
        return;
    }
    let Some((db, coll)) = super::split_namespace(&ns) else { return };
    let Ok(session) = super::parse_session(options, &client) else { return };
    let Ok(wc_opt) = super::parse_write_concern(options) else { return };
    let wc = wc_opt.clone().unwrap_or_else(|| super::writeconcern::WriteConcern::from_uri(&client.borrow().uri));
    if session.is_some() && !wc.is_acknowledged() {
        throw(E::InvalidArgument, "Cannot combine \"session\" option with an unacknowledged write concern");
        return;
    }
    bw.database = Some(db.clone());
    bw.collection = Some(coll.clone());
    bw.bulk.server_id = server_id;
    if let Some(z) = session {
        bw.session = zval::copy(z);
    }
    if let Some(w) = &wc_opt {
        bw.write_concern = Some(w.clone());
    }
    let (ok, reply, error) = {
        let mut cl = client.borrow_mut();
        match session {
            Some(z) => super::with_session(z, |s| bw.bulk.execute(&mut cl, &db, &coll, &wc, Some(s))),
            None => bw.bulk.execute(&mut cl, &db, &coll, &wc, None),
        }
    };
    bw.executed = true;
    super::apm::flush(&client);
    let result = super::writeresult::new_writeresult(reply.clone(), client.clone(), bw.bulk.server_id, wc, mgr);
    zval::write(rv, result);
    if ok {
        return;
    }
    use mongo_core::error::{code, domain};
    if error.domain != domain::SERVER && error.domain != domain::WRITE_CONCERN {
        super::throw_error(&error, Some(&reply));
    }
    if error.domain == domain::COMMAND && error.code == code::COMMAND_INVALID_ARG {
        return;
    }
    let bwe = super::lookup(c"mongodb\\driver\\exception\\bulkwriteexception");
    let ex_obj = if call::has_exception() {
        let prev = (*std::ptr::addr_of_mut!(sys::executor_globals)).exception;
        let name = crate::zend::class::name((*prev).ce);
        let msg = format!("Bulk write failed due to previous {name}: {}", error.message);
        super::throw_code(bwe, &msg, 0)
    } else {
        super::throw_code(bwe, &error.message, error.code as i64)
    };
    super::add_error_labels(ex_obj, &reply);
    // phongo_add_exception_prop("writeResult")
    super::update_prop(ex_obj, b"writeResult", rv);
}

pub unsafe extern "C" fn zim_MongoDB_Driver_Manager_executeBulkWrite(ex: Ex, rv: *mut sys::zval) {
    let Some(c) = client_of(ex) else { return };
    execute_bulk_write(ex, rv, c, None, call::this(ex));
}

/// phongo_execute_query (Manager selects a server with the read preference first).
pub unsafe fn execute_query(ex: Ex, rv: *mut sys::zval, client: ClientRef, server_id: Option<u32>, mgr: *mut sys::zend_object) {
    let (mut p, mut l): (*const u8, usize) = (null_mut(), 0);
    let mut zquery: *mut sys::zval = null_mut();
    let mut options: *mut sys::zval = null_mut();
    if sys::zend_parse_parameters(call::num_args(ex), c"sO|z!".as_ptr(), &mut p, &mut l, &mut zquery, super::query::CE, &mut options) != 0 {
        return;
    }
    let ns = std::slice::from_raw_parts(p, l).to_vec();
    let mut rp: Option<ReadPreference> = None;
    if !options.is_null() {
        if !zval::is(options, sys::IS_ARRAY) {
            throw(E::InvalidArgument, format!("Expected options to be array, {} given", zval::type_name(options)));
            return;
        }
        match parse_read_preference((*options).value.arr) {
            Ok(r) => rp = r,
            Err(()) => return,
        }
    }
    let Ok(zsession) = super::parse_session(options, &client) else { return };
    let sid = match server_id {
        Some(s) => s,
        None => {
            let pinned = zsession.map_or(0, |z| super::session_server_id(z));
            if pinned > 0 {
                pinned
            } else {
                // inherit the client's read preference
                let eff = rp.clone().unwrap_or_else(|| client_read_preference(&client.borrow().uri));
                let prefs = eff.prefs();
                let selected = client.borrow_mut().select_server_id(OpType::Read, Some(&prefs), &[]);
                super::apm::flush(&client);
                match selected {
                    Ok((s, _)) => s,
                    Err(e) => {
                        super::throw_error(&e, None);
                        return;
                    }
                }
            }
        }
    };
    let Some((db, coll)) = super::split_namespace(&ns) else { return };
    let q = obj::data::<super::query::Query>((*zquery).value.obj);
    let filter = q.filter.clone().unwrap_or_else(|| mongo_core::bson::writer::Writer::new().finish());
    let qopts = q.opts.clone().unwrap_or_else(|| mongo_core::bson::writer::Writer::new().finish());
    // the cursor options (_mongoc_cursor_new_with_opts) and the find command's options
    let mut co = super::cursor::CursorOpts::default();
    let mut exhaust = false;
    let mut fw = mongo_core::bson::writer::Writer::with_capacity(qopts.len());
    if let Some(mut it) = mongo_core::bson::iter::Iter::new(&qopts) {
        while it.next() {
            let v = it.value();
            let as_i64 = || match v {
                mongo_core::bson::iter::Value::Int32(_) | mongo_core::bson::iter::Value::Int64(_) | mongo_core::bson::iter::Value::Double(_) | mongo_core::bson::iter::Value::Bool(_) => mongo_core::sdam::as_i64(&v),
                _ => 0,
            };
            let as_bool = || matches!(v, mongo_core::bson::iter::Value::Bool(true));
            match it.key() {
                b"batchSize" => co.batch_size = as_i64(),
                b"limit" => co.limit = as_i64(),
                b"singleBatch" => co.single_batch = as_bool(),
                b"tailable" => co.tailable = as_bool(),
                b"awaitData" => co.await_data = as_bool(),
                b"comment" => {
                    let mut w = mongo_core::bson::writer::Writer::with_capacity(64);
                    w.element_from(Some(b"comment"), &it);
                    co.comment = Some(w.finish());
                }
                b"exhaust" => {
                    exhaust = as_bool();
                    continue;
                }
                _ => {}
            }
            fw.element_from(None, &it);
        }
    }
    co.max_await_time_ms = q.max_await_time_ms as i64;
    let mut c = super::cursor::Cursor::for_find(client.clone(), sid, &db, &coll, filter, fw.finish());
    c.opts = co;
    let read_prefs = rp.clone().unwrap_or_else(|| client_read_preference(&client.borrow().uri));
    c.read_prefs = Some(rp_parts(&read_prefs));
    c.retry_prefs = Some(read_prefs.prefs());
    c.read_concern = match &q.read_concern {
        Some(r) if !r.is_default() => r.to_bson(),
        Some(_) => Vec::new(),
        None => {
            let crc = mongo_core::concern::ReadConcern::from_uri(&client.borrow().uri);
            if crc.is_default() { Vec::new() } else { crc.to_bson() }
        }
    };
    if exhaust && c.opts.limit != 0 {
        c.set_error(mongo_core::error::Error::new(mongo_core::error::domain::CURSOR, mongo_core::error::code::CURSOR_INVALID_CURSOR, "Cannot specify both 'exhaust' and 'limit'."));
    }
    if let Some(z) = zsession {
        c.session = zval::copy(z);
    }
    if !c.advance_first() {
        return;
    }
    c.database = Some(db);
    c.collection = Some(coll);
    c.zquery = zval::copy(zquery);
    if let Some(r) = &rp {
        c.zread_preference = super::readpref::new_readpreference(r.clone());
    }
    zval::write(rv, super::cursor::new_cursor(c, mgr));
}

pub unsafe extern "C" fn zim_MongoDB_Driver_Manager_executeQuery(ex: Ex, rv: *mut sys::zval) {
    let Some(c) = client_of(ex) else { return };
    execute_query(ex, rv, c, None, call::this(ex));
}

pub unsafe extern "C" fn zim_MongoDB_Driver_Manager_getReadConcern(ex: Ex, rv: *mut sys::zval) {
    if !call::none(ex) {
        return;
    }
    if let Some(c) = client_of(ex) {
        let rc = mongo_core::concern::ReadConcern::from_uri(&c.borrow().uri);
        zval::write(rv, super::readconcern::new_readconcern(rc));
    }
}

/// EG_FLAGS_IN_SHUTDOWN (zend_globals.h)
const EG_FLAGS_IN_SHUTDOWN: u8 = 1 << 0;
