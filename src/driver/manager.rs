//! MongoDB\Driver\Manager (src/MongoDB/Manager.c, phongo_client.c, phongo_execute.c).

use super::command::Command;
use super::cursor::Cursor;
use super::readpref::ReadPreference;
use mongo_core::uri::ReadMode;
use super::ClientRef;
use crate::bson::{throw, E};
use crate::zend::call::{self, Ex};
use crate::zend::{hash, obj, sys, zstr, zval};
use mongo_core::client::{Client, Settings};
use mongo_core::error::{domain, Error};
use mongo_core::topology::OpType;
use mongo_core::uri::Uri;
use std::ptr::{addr_of_mut, null_mut};

const URI_DEFAULT: &str = "mongodb://127.0.0.1/";

#[derive(Default)]
pub struct Manager {
    pub client: Option<ClientRef>,
}

static mut H_MANAGER: sys::zend_object_handlers = unsafe { std::mem::zeroed() };
pub static mut CE: *mut sys::zend_class_entry = null_mut();

unsafe extern "C" fn create(c: *mut sys::zend_class_entry) -> *mut sys::zend_object {
    obj::alloc(c, Manager::default(), addr_of_mut!(H_MANAGER))
}

pub unsafe fn init_MongoDB_Driver_Manager(c: *mut sys::zend_class_entry) {
    CE = c;
    (*c).__bindgen_anon_2.create_object = Some(create);
    obj::handlers::<Manager>(addr_of_mut!(H_MANAGER));
}

fn pct_encode(s: &[u8], out: &mut String) {
    for &b in s {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
}

/// The value of a uriOptions entry as connection-string text (None: unsupported type).
unsafe fn option_text(key: &str, z: *mut sys::zval) -> Option<Vec<String>> {
    let z = zval::deref(z);
    let mut s = String::new();
    match zval::type_of(z) as u32 {
        sys::IS_TRUE => s.push_str("true"),
        sys::IS_FALSE => s.push_str("false"),
        sys::IS_LONG => s.push_str(&(*z).value.lval.to_string()),
        sys::IS_STRING => pct_encode(zval::str_bytes(z), &mut s),
        sys::IS_ARRAY | sys::IS_OBJECT => {
            let ht = if zval::is(z, sys::IS_ARRAY) { (*z).value.arr } else { sys::zend_std_get_properties((*z).value.obj) };
            let pairs = |ht: *mut sys::HashTable, out: &mut String| {
                let mut first = true;
                let mut buf = String::new();
                hash::each(ht, true, |k, v| {
                    if !first {
                        out.push(',');
                    }
                    first = false;
                    pct_encode(hash::key_bytes(k, &mut buf), out);
                    out.push(':');
                    let v = zval::deref(v);
                    if zval::is(v, sys::IS_STRING) {
                        pct_encode(zval::str_bytes(v), out);
                    } else if zval::type_of(v) as u32 == sys::IS_TRUE {
                        out.push_str("true");
                    } else if zval::type_of(v) as u32 == sys::IS_FALSE {
                        out.push_str("false");
                    } else if zval::is(v, sys::IS_LONG) {
                        out.push_str(&(*v).value.lval.to_string());
                    }
                    true
                });
            };
            if key.eq_ignore_ascii_case("readpreferencetags") {
                let mut sets = Vec::new();
                hash::each(ht, true, |_, set| {
                    let set = zval::deref(set);
                    let mut t = String::new();
                    if zval::is(set, sys::IS_ARRAY) {
                        pairs((*set).value.arr, &mut t);
                    } else if zval::is(set, sys::IS_OBJECT) {
                        pairs(sys::zend_std_get_properties((*set).value.obj), &mut t);
                    }
                    sets.push(t);
                    true
                });
                return Some(sets);
            }
            if key.eq_ignore_ascii_case("authmechanismproperties") {
                pairs(ht, &mut s);
            } else {
                return None;
            }
        }
        _ => return None,
    }
    Some(vec![s])
}

/// The URI string with `options` appended as query parameters (they override the URI's).
unsafe fn uri_with_options(uri: &str, options: *mut sys::HashTable) -> Option<String> {
    let mut q = String::new();
    let mut ok = true;
    let mut buf = String::new();
    hash::each(options, true, |k, v| {
        let key = match k {
            hash::Key::Str(_) => String::from_utf8_lossy(hash::key_bytes(k, &mut buf)).into_owned(),
            _ => return true,
        };
        match option_text(&key, v) {
            Some(vals) => {
                for val in vals {
                    if !q.is_empty() {
                        q.push('&');
                    }
                    pct_encode(key.as_bytes(), &mut q);
                    q.push('=');
                    q.push_str(&val);
                }
                true
            }
            None => {
                throw(E::InvalidArgument, format!("Failed to parse \"{key}\" URI option"));
                ok = false;
                false
            }
        }
    });
    if !ok {
        return None;
    }
    if q.is_empty() {
        return Some(uri.to_owned());
    }
    let after = uri.find("://").map_or(0, |p| p + 3);
    Some(if uri.contains('?') {
        format!("{uri}&{q}")
    } else if uri[after..].contains('/') {
        format!("{uri}?{q}")
    } else {
        format!("{uri}/?{q}")
    })
}

pub unsafe extern "C" fn zim_MongoDB_Driver_Manager___construct(ex: Ex, _rv: *mut sys::zval) {
    let (mut p, mut l): (*const u8, usize) = (null_mut(), 0);
    let mut options: *mut sys::zval = null_mut();
    let mut driver_options: *mut sys::zval = null_mut();
    if sys::zend_parse_parameters(call::num_args(ex), c"|s!a!a!".as_ptr(), &mut p, &mut l, &mut options, &mut driver_options) != 0 {
        return;
    }
    let uri_string = if p.is_null() { URI_DEFAULT.to_owned() } else { String::from_utf8_lossy(std::slice::from_raw_parts(p, l)).into_owned() };
    let full = if options.is_null() { Some(uri_string.clone()) } else { uri_with_options(&uri_string, (*options).value.arr) };
    let Some(full) = full else { return };
    let uri = match Uri::parse(&full) {
        Ok(u) => u,
        Err(e) => {
            throw(E::InvalidArgument, format!("Failed to parse MongoDB URI: '{uri_string}'. {e}."));
            return;
        }
    };
    // TLS settings among the driver options request TLS too: no cleartext fallback
    if !driver_options.is_null() && has_tls_driver_option((*driver_options).value.arr) {
        throw(E::InvalidArgument, format!("Failed to parse URI options: {}", mongo_core::client::tls_unavailable().message));
        return;
    }
    let me = obj::data::<Manager>(call::this(ex));
    let r = super::persistent_client(&full, || {
        let mut s = Settings::from_uri(&uri);
        s.metadata.driver_name = "ext-mongodb:PHP".into();
        s.metadata.driver_version = mongo_core::EXT_VERSION.into();
        s.metadata.platform = php_platform();
        Client::new(uri, s)
    });
    match r {
        Ok(c) => me.client = Some(c),
        // phongo_client: mongoc_client_new_from_uri_with_error failed
        Err(e) => throw(E::InvalidArgument, format!("Failed to parse URI options: {}", e.message)),
    }
}

/// Driver options that configure TLS (phongo's ca_dir/crl_file, the 1.x TLS keys and any
/// tls*/ssl* key).
unsafe fn has_tls_driver_option(ht: *mut sys::HashTable) -> bool {
    const KEYS: &[&str] = &["ca_dir", "crl_file", "ca_file", "pem_file", "pem_pwd", "context", "allow_invalid_hostname", "weak_cert_validation", "allow_self_signed"];
    let mut found = false;
    let mut buf = String::new();
    hash::each(ht, true, |k, _| {
        if let hash::Key::Str(_) = k {
            let key = String::from_utf8_lossy(hash::key_bytes(k, &mut buf)).to_ascii_lowercase();
            if KEYS.contains(&key.as_str()) || key.starts_with("tls") || key.starts_with("ssl") {
                found = true;
                return false;
            }
        }
        true
    });
    found
}

/// phongo's handshake platform string ("PHP 8.4.13 ...")
fn php_platform() -> String {
    format!("PHP {}", std::ffi::CStr::from_bytes_with_nul(sys::PHP_VERSION).map(|c| c.to_string_lossy().into_owned()).unwrap_or_default())
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CmdType {
    Raw,
    Read,
    Write,
    ReadWrite,
}

/// phongo_execute_command for Manager::execute*Command and Server::execute*Command.
pub unsafe fn execute_command_args(ex: Ex, rv: *mut sys::zval, client: ClientRef, ty: CmdType, server_id: Option<u32>) {
    let (mut p, mut l): (*const u8, usize) = (null_mut(), 0);
    let mut command: *mut sys::zval = null_mut();
    let mut options: *mut sys::zval = null_mut();
    let spec = if ty == CmdType::Raw { c"sO|z!" } else { c"sO|a!" };
    if sys::zend_parse_parameters(call::num_args(ex), spec.as_ptr(), &mut p, &mut l, &mut command, super::command::CE, &mut options) != 0 {
        return;
    }
    if !options.is_null() && !zval::is(options, sys::IS_ARRAY) {
        throw(E::InvalidArgument, format!("Expected options to be array, {} given", zval::type_name(options)));
        return;
    }
    // phongo_parse_read_preference (raw and read commands)
    let mut rp: Option<ReadPreference> = None;
    if !options.is_null() && matches!(ty, CmdType::Raw | CmdType::Read) {
        match parse_read_preference((*options).value.arr) {
            Ok(r) => rp = r,
            Err(()) => return,
        }
    }
    if rp.is_none() && ty == CmdType::Read {
        rp = Some(client_read_preference(&client.borrow().uri));
    }
    let db = String::from_utf8_lossy(std::slice::from_raw_parts(p, l)).into_owned();
    let cmd = obj::data::<Command>((*command).value.obj);
    let Some(bson) = cmd.bson.clone() else { return };
    let (batch_size, max_await) = (cmd.batch_size, cmd.max_await_time_ms);
    let optype = if matches!(ty, CmdType::Write | CmdType::ReadWrite) { OpType::Write } else { OpType::Read };
    let prefs = rp.as_ref().map(ReadPreference::prefs);
    let (sid, must_use_primary) = match server_id {
        Some(s) => (s, false),
        None => match client.borrow_mut().select_server_id(optype, if optype == OpType::Read { prefs.as_ref() } else { None }, &[]) {
            Ok(s) => s,
            Err(e) => {
                super::throw_error(&e, None);
                return;
            }
        },
    };
    // the command with $db
    let mut w = mongo_core::bson::writer::Writer::with_capacity(bson.len() + 32);
    if let Some(mut it) = mongo_core::bson::iter::Iter::new(&bson) {
        while it.next() {
            w.element_from(None, &it);
        }
    }
    if !has_key(&bson, b"$db") {
        w.utf8(b"$db", db.as_bytes());
    }
    // mongoc_cmd_parts_assemble: $readPreference for reads on non-standalone servers
    if optype == OpType::Read {
        let cl = client.borrow();
        let sty = cl.td.server(sid).map(|s| s.ty);
        let single_direct = cl.td.ty == mongo_core::topology::TopologyType::Single && sty != Some(mongo_core::sdam::ServerType::Mongos);
        let mut eff = rp.clone().unwrap_or_default();
        if eff.mode == ReadMode::Primary && single_direct {
            eff = ReadPreference { mode: ReadMode::PrimaryPreferred, ..Default::default() };
        }
        let mode = if must_use_primary { ReadMode::Primary } else { eff.mode };
        if mode != ReadMode::Primary && sty != Some(mongo_core::sdam::ServerType::Standalone) {
            let d = w.begin(3, b"$readPreference");
            eff.append_contents(&mut w, mode);
            w.end(d);
        }
    }
    let body = w.finish();
    let name = super::command::name(&bson);
    let r = client.borrow_mut().run_command(sid, &name, &db, &body);
    let reply = match r {
        Ok(r) => r,
        Err(e) => {
            super::throw_error(&e, None);
            return;
        }
    };
    if let Err(e) = mongo_core::cmd::check_ok(&reply) {
        client.borrow_mut().handle_command_error(sid, &reply);
        super::throw_error(&e.into_error(), Some(&reply));
        return;
    }
    if matches!(ty, CmdType::Write | CmdType::ReadWrite) {
        if let Some(e) = write_concern_error(&reply) {
            super::throw_error(&e, Some(&reply));
            return;
        }
    }
    let mut c = Cursor::from_reply(reply, client, sid, &db);
    c.batch_size = batch_size;
    c.max_await_time_ms = max_await;
    *rv = super::cursor::new_cursor(c);
}

fn has_key(doc: &[u8], k: &[u8]) -> bool {
    mongo_core::bson::iter::Iter::new(doc).is_some_and(|mut it| it.find(k))
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
    use mongo_core::bson::iter::{Iter, Value};
    let mut it = Iter::new(reply)?;
    if !it.find(b"writeConcernError") {
        return None;
    }
    let Value::Document(w) = it.value() else { return None };
    let (mut code, mut msg) = (0u32, String::new());
    let mut i = Iter::new(w)?;
    while i.next() {
        match i.key() {
            b"code" => code = mongo_core::sdam::as_i64(&i.value()) as u32,
            b"errmsg" => {
                if let Value::Utf8(s) = i.value() {
                    msg = String::from_utf8_lossy(s).into_owned();
                }
            }
            _ => {}
        }
    }
    Some(Error::new(domain::WRITE_CONCERN, code, format!("Write Concern error: {msg}")))
}

#[inline]
unsafe fn client_of(ex: Ex) -> Option<ClientRef> {
    obj::data::<Manager>(call::this(ex)).client.clone()
}

macro_rules! exec {
    ($f:ident, $ty:expr) => {
        pub unsafe extern "C" fn $f(ex: Ex, rv: *mut sys::zval) {
            let Some(c) = client_of(ex) else { return };
            execute_command_args(ex, rv, c, $ty, None);
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
    match r {
        Ok((id, _)) => *rv = super::server::new_server(c, id),
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
            hash::push(ht, super::server::new_server(c.clone(), id));
        }
    }
    *rv = zval::arr(ht);
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
        *rv = super::readpref::new_readpreference(rp);
    }
}
