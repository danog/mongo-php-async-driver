//! The Manager's `uriOptions` array (phongo_client.c: phongo_apply_options_to_uri and the read
//! concern, read preference and write concern passes), applied to the parsed URI with
//! phongo's type checks and messages. The options arrive as the BSON document phongo builds
//! from the array (phongo_zval_to_bson).

use mongo_core::bson::iter::{Iter, Value, *};
use mongo_core::uri::{self, ReadMode, Uri, NO_MAX_STALENESS, W_MAJORITY, W_TAG, W_UNACKNOWLEDGED};

/// phongo_bson_type_to_string
pub(crate) fn type_name(t: u8) -> &'static str {
    match t {
        0x00 => "EOD",
        T_DOUBLE => "double",
        T_UTF8 => "string",
        T_DOCUMENT => "document",
        T_ARRAY => "array",
        T_BINARY => "Binary",
        T_UNDEFINED => "undefined",
        T_OID => "ObjectId",
        T_BOOL => "boolean",
        T_DATE_TIME => "UTCDateTime",
        T_NULL => "null",
        T_REGEX => "Regex",
        T_DBPOINTER => "DBPointer",
        T_CODE => "Javascript",
        T_SYMBOL => "symbol",
        T_CODEWSCOPE => "Javascript with scope",
        T_INT32 => "32-bit integer",
        T_TIMESTAMP => "Timestamp",
        T_INT64 => "64-bit integer",
        T_DECIMAL128 => "Decimal128",
        T_MAXKEY => "MaxKey",
        T_MINKEY => "MinKey",
        _ => "unknown",
    }
}

type R = Result<(), String>;

fn invalid(key: &str, v: &Value, expected: &str) -> String {
    format!("Expected {expected} for \"{key}\" URI option, {} given", type_name(v.type_code()))
}

fn failed(key: &str) -> String {
    format!("Failed to parse \"{key}\" URI option")
}

fn utf8<'a>(key: &str, v: &Value<'a>) -> Result<&'a str, String> {
    match v {
        Value::Utf8(s) => std::str::from_utf8(s).map_err(|_| failed(key)),
        v => Err(invalid(key, v, "string")),
    }
}

/// A document's (key, value) pairs as strings (libmongoc keeps the BSON values; true and
/// false become "true" and "false", as libmongoc reads CANONICALIZE_HOST_NAME).
fn string_pairs(d: &[u8]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let Some(mut it) = Iter::new(d) else { return out };
    while it.next() {
        let k = String::from_utf8_lossy(it.key()).into_owned();
        let v = match it.value() {
            Value::Utf8(s) => String::from_utf8_lossy(s).into_owned(),
            Value::Bool(b) => b.to_string(),
            Value::Int32(i) => i.to_string(),
            Value::Int64(i) => i.to_string(),
            Value::Double(f) => f.to_string(),
            _ => String::new(),
        };
        out.push((k, v));
    }
    out
}

fn is(key: &str, name: &str) -> bool {
    key.eq_ignore_ascii_case(name)
}

/// phongo_apply_options_to_uri
fn apply_generic(u: &mut Uri, doc: &[u8]) -> R {
    let Some(mut it) = Iter::new(doc) else { return Ok(()) };
    while it.next() {
        let key = String::from_utf8_lossy(it.key()).into_owned();
        let key = key.as_str();
        let v = it.value();
        // read preference, read concern and write concern options have their own passes
        const SKIP: &[&str] = &["journal", "maxstalenessseconds", "readconcernlevel", "readpreference", "readpreferencetags", "safe", "w", "wtimeoutms"];
        if SKIP.iter().any(|s| is(key, s)) {
            continue;
        }
        if uri::option_is_bool(key) {
            let Value::Bool(b) = v else { return Err(invalid(key, &v, "boolean")) };
            if !u.set_option_as_bool(key, b) {
                return Err(failed(key));
            }
        } else if uri::option_is_int32(key) {
            let Value::Int32(i) = v else { return Err(invalid(key, &v, "32-bit integer")) };
            if !u.set_option_as_int32(key, i) {
                return Err(failed(key));
            }
        } else if uri::option_is_utf8(key) {
            let s = utf8(key, &v)?;
            if is(key, "replicaset") && s.is_empty() {
                return Err(format!("Value for URI option \"{key}\" cannot be empty string."));
            }
            if !u.set_option_as_utf8(key, s) {
                return Err(if is(key, "appname") { format!("Invalid appname value: '{s}'") } else { failed(key) });
            }
        } else if is(key, "username") {
            u.set_username(utf8(key, &v)?);
        } else if is(key, "password") {
            u.set_password(utf8(key, &v)?);
        } else if is(key, "authmechanism") {
            u.set_auth_mechanism(utf8(key, &v)?);
        } else if is(key, "authsource") {
            u.set_auth_source(utf8(key, &v)?);
        } else if is(key, "authmechanismproperties") {
            let Value::Document(d) = v else { return Err(invalid(key, &v, "array or object")) };
            u.set_mechanism_properties_pub(string_pairs(d));
        } else if is(key, "gssapiservicename") {
            if u.has_mechanism_properties() {
                return Err(format!("authMechanismProperties SERVICE_NAME already set, ignoring \"{key}\""));
            }
            let s = utf8(key, &v)?;
            u.set_mechanism_properties_pub(vec![("SERVICE_NAME".to_owned(), s.to_owned())]);
        } else if is(key, "compressors") {
            u.set_compressors(utf8(key, &v)?);
        }
    }
    Ok(())
}

/// phongo_apply_rc_options_to_uri
fn apply_rc(u: &mut Uri, doc: &[u8]) -> R {
    let Some(mut it) = Iter::new(doc) else { return Ok(()) };
    while it.next() {
        let key = String::from_utf8_lossy(it.key()).into_owned();
        if is(&key, "readconcernlevel") {
            let s = utf8(&key, &it.value())?;
            u.read_concern_level = Some(s.to_owned());
        }
    }
    Ok(())
}

/// MONGOC_SMALLEST_MAX_STALENESS_SECONDS
const SMALLEST_MAX_STALENESS_SECONDS: i64 = 90;

/// phongo_apply_rp_options_to_uri
fn apply_rp(u: &mut Uri, doc: &[u8]) -> R {
    let Some(mut it) = Iter::new(doc) else { return Ok(()) };
    let (mut mode, mut tags, mut staleness) = (u.read_mode, u.read_tags.clone(), u.max_staleness_seconds);
    while it.next() {
        let key = String::from_utf8_lossy(it.key()).into_owned();
        let key = key.as_str();
        let v = it.value();
        if is(key, "readpreference") {
            let s = utf8(key, &v)?;
            mode = match s.to_ascii_lowercase().as_str() {
                "primary" => ReadMode::Primary,
                "primarypreferred" => ReadMode::PrimaryPreferred,
                "secondary" => ReadMode::Secondary,
                "secondarypreferred" => ReadMode::SecondaryPreferred,
                "nearest" => ReadMode::Nearest,
                _ => return Err(format!("Unsupported {key} value: '{s}'")),
            };
        }
        if is(key, "readpreferencetags") {
            let Value::Array(a) = v else { return Err(invalid(key, &v, "array")) };
            let mut sets = Vec::new();
            if let Some(mut t) = Iter::new(a) {
                while t.next() {
                    match t.value() {
                        Value::Document(d) => sets.push(string_pairs(d)),
                        _ => return Err("Read preference tags must be an array of zero or more documents".into()),
                    }
                }
            }
            tags = sets;
        }
        if is(key, "maxstalenessseconds") {
            let s = match v {
                Value::Int32(i) => i64::from(i),
                Value::Int64(i) => i,
                _ => return Err(invalid(key, &v, "integer")),
            };
            if s != i64::from(NO_MAX_STALENESS) {
                if s < SMALLEST_MAX_STALENESS_SECONDS {
                    return Err(format!("Expected maxStalenessSeconds to be >= {SMALLEST_MAX_STALENESS_SECONDS}, {s} given"));
                }
                if s > i64::from(i32::MAX) {
                    return Err(format!("Expected maxStalenessSeconds to be <= {}, {s} given", i32::MAX));
                }
                if mode == ReadMode::Primary {
                    return Err("Primary read preference mode conflicts with maxStalenessSeconds".into());
                }
            }
            staleness = s as i32;
        }
    }
    if mode == ReadMode::Primary && !tags.is_empty() {
        return Err("Primary read preference mode conflicts with tags".into());
    }
    if mode == ReadMode::Primary && staleness != NO_MAX_STALENESS {
        return Err("Primary read preference mode conflicts with maxStalenessSeconds".into());
    }
    if staleness != NO_MAX_STALENESS && staleness <= 0 {
        return Err("Read preference is not valid".into());
    }
    u.read_mode = mode;
    u.read_tags = tags;
    u.max_staleness_seconds = staleness;
    Ok(())
}

/// phongo_apply_wc_options_to_uri
fn apply_wc(u: &mut Uri, doc: &[u8]) -> R {
    let Some(mut it) = Iter::new(doc) else { return Ok(()) };
    let mut wc = u.write_concern.clone();
    let mut ignore_safe = false;
    while it.next() {
        let key = String::from_utf8_lossy(it.key()).into_owned();
        let key = key.as_str();
        let v = it.value();
        if !ignore_safe && is(key, "safe") {
            let Value::Bool(b) = v else { return Err(invalid(key, &v, "boolean")) };
            wc.w = if b { 1 } else { W_UNACKNOWLEDGED };
            wc.wtag = None;
        }
        if is(key, "wtimeoutms") {
            let t = match v {
                Value::Int32(i) => i64::from(i),
                Value::Int64(i) => i,
                _ => return Err(invalid(key, &v, "integer")),
            };
            if t < 0 {
                return Err(format!("Expected wtimeoutMS to be >= 0, {t} given"));
            }
            wc.wtimeout = t;
        }
        if is(key, "journal") {
            let Value::Bool(b) = v else { return Err(invalid(key, &v, "boolean")) };
            wc.journal = Some(b);
        }
        if is(key, "w") {
            match v {
                Value::Int32(i) if i == W_UNACKNOWLEDGED || i > 0 => {
                    wc.w = i;
                    wc.wtag = None;
                }
                Value::Int32(i) => return Err(format!("Unsupported w value: {i}")),
                Value::Utf8(s) => {
                    let s = String::from_utf8_lossy(s).into_owned();
                    if s.eq_ignore_ascii_case("majority") {
                        wc.w = W_MAJORITY;
                        wc.wtag = None;
                    } else {
                        wc.w = W_TAG;
                        wc.wtag = Some(s);
                    }
                }
                _ => return Err(invalid(key, &v, "32-bit integer or string")),
            }
            ignore_safe = true;
        }
    }
    if wc.journal == Some(true) && wc.w == W_UNACKNOWLEDGED {
        return Err(format!("Journal conflicts with w value: {}", wc.w));
    }
    u.write_concern = wc;
    Ok(())
}

/// The four passes, in phongo's order; Err = the InvalidArgumentException message.
pub fn apply(u: &mut Uri, doc: &[u8]) -> R {
    apply_generic(u, doc)?;
    apply_rc(u, doc)?;
    apply_rp(u, doc)?;
    apply_wc(u, doc)
}
