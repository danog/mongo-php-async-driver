//! Automatic encryption without query analysis (`bypassQueryAnalysis`): libmongocrypt's auto
//! encrypt context (mongocrypt-ctx-encrypt.c) with `bypass_query_analysis`, the schema broker
//! (mc-schema-broker.c), EncryptedFieldConfig (mc-efc.c) and the collinfo cache. Commands keep
//! their explicit payloads; `encryptionInformation` (and compaction/cleanup tokens) are added for
//! Queryable Encryption collections. Messages are libmongocrypt's.

use mongo_core::bson::iter::{Iter, Value, T_ARRAY, T_DOCUMENT};
use mongo_core::bson::writer::Writer;

type Result<T> = std::result::Result<T, String>;

fn type_name(t: u8) -> &'static str {
    crate::range::type_name(t)
}

// supported_query_type_flags
pub const SUPPORTS_EQUALITY: u32 = 1 << 0;
pub const SUPPORTS_RANGE: u32 = 1 << 1;
pub const SUPPORTS_RANGE_PREVIEW: u32 = 1 << 2;
pub const SUPPORTS_SUBSTRING_PREVIEW: u32 = 1 << 3;
pub const SUPPORTS_SUFFIX_PREVIEW: u32 = 1 << 4;
pub const SUPPORTS_PREFIX_PREVIEW: u32 = 1 << 5;
pub const SUPPORTS_SUBSTRING: u32 = 1 << 6;
pub const SUPPORTS_SUFFIX: u32 = 1 << 7;
pub const SUPPORTS_PREFIX: u32 = 1 << 8;
const TEXT_QUERIES: u32 =
    SUPPORTS_SUBSTRING | SUPPORTS_SUFFIX | SUPPORTS_PREFIX | SUPPORTS_SUBSTRING_PREVIEW | SUPPORTS_SUFFIX_PREVIEW | SUPPORTS_PREFIX_PREVIEW;
/// fields whose compaction needs encryptionInformation and an anchor padding token
const RANGE_OR_TEXT: u32 = SUPPORTS_RANGE | TEXT_QUERIES;

/// LATEST_STR_ENCODE_VERSION / MIN_STR_ENCODE_VERSION
const LATEST_STR_ENCODE_VERSION: i32 = 1;
const MIN_STR_ENCODE_VERSION: i32 = 1;

/// `mc_EncryptedField_t`
#[derive(Clone, Debug)]
pub struct Field {
    pub key_id: Option<[u8; 16]>,
    pub key_alt_name: Option<Vec<u8>>,
    pub path: Vec<u8>,
    pub queries: u32,
}

/// `mc_EncryptedFieldConfig_t` (fields in libmongocrypt's order: the last parsed first)
#[derive(Clone, Debug, Default)]
pub struct Efc {
    pub fields: Vec<Field>,
    pub str_encode_version: u8,
}

fn query_type(s: &[u8]) -> Option<u32> {
    let s = String::from_utf8_lossy(s).to_ascii_lowercase();
    Some(match s.as_str() {
        "equality" => SUPPORTS_EQUALITY,
        "range" => SUPPORTS_RANGE,
        "rangepreview" => SUPPORTS_RANGE_PREVIEW,
        "substringpreview" => SUPPORTS_SUBSTRING_PREVIEW,
        "substring" => SUPPORTS_SUBSTRING,
        "suffix" => SUPPORTS_SUFFIX,
        "prefix" => SUPPORTS_PREFIX,
        "suffixpreview" => SUPPORTS_SUFFIX_PREVIEW,
        "prefixpreview" => SUPPORTS_PREFIX_PREVIEW,
        _ => return None,
    })
}

/// `_parse_supported_query_types`
fn supported_query(v: &Value) -> Result<u32> {
    const P: &str = "When parsing supported query types: ";
    let Value::Document(d) = v else { return Err(format!("{P}Expected type document, got: {}", v.type_code())) };
    let mut it = Iter::new(d).ok_or("invalid BSON")?;
    if !it.find(b"queryType") {
        return Err(format!("{P}Unable to find 'queryType' in query document"));
    }
    let Value::Utf8(q) = it.value() else { return Err(format!("{P}Expected 'queryType' to be type UTF-8, got: {}", it.value().type_code())) };
    query_type(q).ok_or_else(|| format!("{P}Did not recognize query type '{}'", String::from_utf8_lossy(q)))
}

fn get<'a>(doc: &'a [u8], key: &[u8]) -> Option<Value<'a>> {
    let mut it = Iter::new(doc)?;
    it.find(key).then(|| it.value())
}

/// `_parse_field`
fn parse_field(field: &[u8]) -> Result<Field> {
    let key_id = get(field, b"keyId");
    let alt = get(field, b"keyAltName");
    if key_id.is_none() && alt.is_none() {
        return Err("unable to find 'keyId' or 'keyAltName' in 'field' document".into());
    }
    if key_id.is_some() && alt.is_some() {
        return Err("only one of 'keyId' or 'keyAltName may be in 'field' document".into());
    }
    let key_id = match key_id {
        Some(Value::Binary(4, b)) if b.len() == 16 => Some(<[u8; 16]>::try_from(b).expect("16 bytes")),
        Some(Value::Binary(..)) => return Err("unable to parse uuid key from 'fields.keyId'".into()),
        Some(v) => return Err(format!("expected 'fields.keyId' to be type binary, got: {}", type_name(v.type_code()))),
        None => None,
    };
    let key_alt_name = match alt {
        Some(Value::Utf8(s)) => Some(s.to_vec()),
        Some(v) => return Err(format!("expected 'fields.keyAltName' to be type UTF-8, got: {}", v.type_code())),
        None => None,
    };
    let path = match get(field, b"path") {
        Some(Value::Utf8(s)) => s.to_vec(),
        Some(v) => return Err(format!("expected 'fields.path' to be type UTF-8, got: {}", v.type_code())),
        None => return Err("unable to find 'path' in 'field' document".into()),
    };
    let mut queries = 0;
    match get(field, b"queries") {
        Some(Value::Array(a)) => {
            let mut it = Iter::new(a).ok_or("Failed to parse 'queries' field")?;
            while it.next() {
                queries |= supported_query(&it.value())?;
            }
        }
        Some(v) => queries |= supported_query(&v)?,
        None => {}
    }
    let p = String::from_utf8_lossy(&path);
    if queries & SUPPORTS_RANGE_PREVIEW != 0 {
        return Err(format!(
            "Cannot use field '{p}' with 'rangePreview' queries. 'rangePreview' is unsupported. Use 'range' instead. 'range' is not compatible with 'rangePreview' and requires recreating the collection."
        ));
    }
    for (flag, removed, repl) in [
        (SUPPORTS_PREFIX_PREVIEW, "prefixPreview", "prefix"),
        (SUPPORTS_SUFFIX_PREVIEW, "suffixPreview", "suffix"),
        (SUPPORTS_SUBSTRING_PREVIEW, "substringPreview", "substring"),
    ] {
        if queries & flag != 0 {
            return Err(format!("Cannot use field '{p}' with '{removed}' queries. '{removed}' is unsupported. Use '{repl}' instead."));
        }
    }
    Ok(Field { key_id, key_alt_name, path, queries })
}

impl Efc {
    /// `mc_EncryptedFieldConfig_parse`
    pub fn parse(doc: &[u8]) -> Result<Efc> {
        let fields = match get(doc, b"fields") {
            Some(Value::Array(a)) => a,
            Some(v) => return Err(format!("expected 'fields' to be type array, got: {}", type_name(v.type_code()))),
            None => return Err("unable to find 'fields' in encrypted_field_config".into()),
        };
        let mut efc = Efc::default();
        let mut all = 0;
        let mut it = Iter::new(fields).ok_or("unable to recurse into encrypted_field_config 'fields'")?;
        while it.next() {
            let Value::Document(f) = it.value() else { return Err("expected document".into()) };
            let field = parse_field(f)?;
            all |= field.queries;
            // prepended, as libmongocrypt's linked list
            efc.fields.insert(0, field);
        }
        for (i, a) in efc.fields.iter().enumerate() {
            if let Some(n) = &a.key_alt_name {
                if efc.fields[i + 1..].iter().any(|b| b.key_alt_name.as_ref() == Some(n)) {
                    return Err(format!("duplicate keyAltName '{}' found in encrypted field config", String::from_utf8_lossy(n)));
                }
            }
        }
        efc.str_encode_version = match get(doc, b"strEncodeVersion") {
            None => {
                if all & TEXT_QUERIES != 0 {
                    LATEST_STR_ENCODE_VERSION as u8
                } else {
                    0
                }
            }
            Some(Value::Int32(v)) => {
                if !(MIN_STR_ENCODE_VERSION..=LATEST_STR_ENCODE_VERSION).contains(&v) {
                    return Err(format!("'strEncodeVersion' of {v} is not supported"));
                }
                v as u8
            }
            Some(v) => return Err(format!("expected 'strEncodeVersion' to be type int32, got: {}", type_name(v.type_code()))),
        };
        Ok(efc)
    }
}

// ---------------------------------------------------------------------------------------------
// Schema broker

#[derive(Clone, Debug, Default)]
struct Entry {
    coll: Vec<u8>,
    json_schema: Option<(Vec<u8>, bool)>,
    encrypted_fields: Option<(Vec<u8>, Efc)>,
    satisfied: bool,
}

/// `mc_schema_broker_t`
#[derive(Debug, Default)]
pub struct SchemaBroker {
    db: Option<String>,
    entries: Vec<Entry>,
}

fn ns(db: &str, coll: &[u8]) -> String {
    format!("{db}.{}", String::from_utf8_lossy(coll))
}

/// `_mongocrypt_check_no_embedded_nul`
fn no_nul(s: &[u8], what: &str) -> Result<()> {
    if s.contains(&0) {
        return Err(format!("{what} must not contain an embedded null byte"));
    }
    Ok(())
}

impl SchemaBroker {
    /// `mc_schema_broker_request`
    pub fn request(&mut self, db: &str, coll: &[u8]) -> Result<()> {
        if let Some(d) = &self.db {
            if d != db {
                return Err(format!("Cannot request schemas for different databases. Requested schemas for '{d}' and '{db}'."));
            }
        }
        if self.entries.iter().any(|e| e.coll == coll) {
            return Ok(());
        }
        if self.entries.is_empty() {
            self.db = Some(db.to_owned());
        }
        self.entries.push(Entry { coll: coll.to_vec(), ..Default::default() });
        Ok(())
    }

    fn db(&self) -> &str {
        self.db.as_deref().unwrap_or_default()
    }

    pub fn has_multiple_requests(&self) -> bool {
        self.entries.len() > 1
    }

    pub fn need_more(&self) -> bool {
        self.entries.iter().any(|e| !e.satisfied)
    }

    pub fn has_any_qe(&self) -> bool {
        self.entries.iter().any(|e| e.encrypted_fields.is_some())
    }

    /// `mc_schema_broker_append_listCollections_filter`
    pub fn list_collections_filter(&self) -> Result<Vec<u8>> {
        let un: Vec<&Entry> = self.entries.iter().filter(|e| !e.satisfied).collect();
        let mut w = Writer::with_capacity(64);
        match un.len() {
            0 => return Err("Unexpected: attempting to create listCollections filter but no schemas needed".into()),
            1 => w.utf8(b"name", &un[0].coll),
            _ => {
                let p = w.begin(T_DOCUMENT, b"name");
                let a = w.begin(T_ARRAY, b"$in");
                for (i, e) in un.iter().enumerate() {
                    w.utf8(i.to_string().as_bytes(), &e.coll);
                }
                w.end(a);
                w.end(p);
            }
        }
        Ok(w.finish())
    }

    /// `mc_schema_entry_satisfy_from_collinfo`
    fn satisfy_entry(e: &mut Entry, collinfo: &[u8], db: &str) -> Result<()> {
        let n = ns(db, &e.coll);
        if e.satisfied {
            return Err(format!("unexpected duplicate collinfo result for collection: {n}"));
        }
        if matches!(get(collinfo, b"type"), Some(Value::Utf8(b"view"))) {
            return Err(format!("cannot auto encrypt a view: {n}"));
        }
        let options = match get(collinfo, b"options") {
            Some(Value::Document(o)) => Some(o),
            _ => None,
        };
        if let Some(ef) = options.and_then(|o| get(o, b"encryptedFields")) {
            let Value::Document(d) = ef else {
                return Err(format!("expected document for `options.encryptedFields` but got {} for collection {n}", type_name(ef.type_code())));
            };
            e.encrypted_fields = Some((d.to_vec(), Efc::parse(d)?));
        }
        if let Some(Value::Document(v)) = options.and_then(|o| get(o, b"validator")) {
            let mut it = Iter::new(v).ok_or_else(|| format!("failed to iterate `options.validator` for collection: {n}"))?;
            let mut found = false;
            while it.next() {
                if it.key() == b"$jsonSchema" {
                    if found {
                        return Err(format!("duplicate `$jsonSchema` fields found for collection: {n}"));
                    }
                    let Value::Document(s) = it.value() else { return Err(format!("unable to copy `$jsonSchema` for collection: {n}")) };
                    e.json_schema = Some((s.to_vec(), true));
                    found = true;
                }
            }
        }
        e.satisfied = true;
        Ok(())
    }

    /// `mc_schema_broker_satisfy_from_collinfo`: also caches the collinfo
    pub fn satisfy_from_collinfo(&mut self, collinfo: &[u8], cache: &CollinfoCache) -> Result<()> {
        let db = self.db().to_owned();
        let coll = match get(collinfo, b"name") {
            Some(Value::Utf8(s)) => s.to_vec(),
            _ => return Err(format!("failed to find 'name' in collinfo in database: {db}")),
        };
        no_nul(&coll, "collection name in collinfo")?;
        cache.put(&ns(&db, &coll), collinfo);
        let e = self.entries.iter_mut().find(|e| e.coll == coll).ok_or_else(|| format!("got unexpected collinfo result for collection: {}", ns(&db, &coll)))?;
        Self::satisfy_entry(e, collinfo, &db)
    }

    /// `mc_schema_broker_satisfy_from_schemaMap` / `_from_encryptedFieldsMap`
    fn satisfy_from_map(&mut self, map: &[u8], ef: bool) -> Result<()> {
        let db = self.db().to_owned();
        for e in self.entries.iter_mut().filter(|e| !e.satisfied) {
            let n = ns(&db, &e.coll);
            let Some(v) = get(map, n.as_bytes()) else { continue };
            let Value::Document(d) = v else {
                return Err(if ef {
                    format!("failed to read encryptedFields from encryptedFields map for collection: {n}")
                } else {
                    format!("failed to read schema from schema map for collection: {n}")
                });
            };
            if ef {
                e.encrypted_fields = Some((d.to_vec(), Efc::parse(d)?));
            } else {
                e.json_schema = Some((d.to_vec(), false));
            }
            e.satisfied = true;
        }
        Ok(())
    }

    pub fn satisfy_from_schema_map(&mut self, map: &[u8]) -> Result<()> {
        self.satisfy_from_map(map, false)
    }

    pub fn satisfy_from_encrypted_fields_map(&mut self, map: &[u8]) -> Result<()> {
        self.satisfy_from_map(map, true)
    }

    /// `mc_schema_broker_satisfy_from_cache`
    pub fn satisfy_from_cache(&mut self, cache: &CollinfoCache) -> Result<()> {
        let db = self.db().to_owned();
        for e in self.entries.iter_mut().filter(|e| !e.satisfied) {
            if let Some(ci) = cache.get(&ns(&db, &e.coll)) {
                Self::satisfy_entry(e, &ci, &db)?;
            }
        }
        Ok(())
    }

    /// `mc_schema_broker_satisfy_from_create_or_collMod`
    pub fn satisfy_from_create_or_coll_mod(&mut self, cmd: &[u8]) -> Result<()> {
        let mut it = Iter::new(cmd).ok_or("Failed to get command name")?;
        if !it.next() {
            return Err("Failed to get command name".into());
        }
        let name = it.key().to_vec();
        if name != b"create" && name != b"collMod" {
            return Ok(());
        }
        let Value::Utf8(coll) = it.value() else { return Err("Failed to get collection name from command".into()) };
        no_nul(coll, "collection name")?;
        let Some(e) = self.entries.iter_mut().find(|e| e.coll == coll) else { return Ok(()) };
        if e.satisfied {
            return Ok(());
        }
        if let Some(Value::Document(v)) = get(cmd, b"validator") {
            if let Some(Value::Document(s)) = get(v, b"$jsonSchema") {
                e.json_schema = Some((s.to_vec(), true));
                e.satisfied = true;
                return Ok(());
            }
        }
        if let Some(v) = get(cmd, b"encryptedFields") {
            let Value::Document(d) = v else {
                return Err(format!("failed to read encryptedFields from command: {}", String::from_utf8_lossy(&name)));
            };
            e.encrypted_fields = Some((d.to_vec(), Efc::parse(d)?));
            e.satisfied = true;
        }
        Ok(())
    }

    /// `mc_schema_broker_satisfy_remaining_with_empty_schemas` (cached as empty collinfo)
    pub fn satisfy_remaining_with_empty(&mut self, cache: Option<&CollinfoCache>) {
        let db = self.db().to_owned();
        for e in self.entries.iter_mut().filter(|e| !e.satisfied) {
            if let Some(c) = cache {
                c.put(&ns(&db, &e.coll), &[5, 0, 0, 0, 0]);
            }
            e.satisfied = true;
        }
    }

    /// `mc_schema_broker_maybe_get_encryptedFields`
    pub fn maybe_encrypted_fields(&self, coll: &[u8]) -> Option<&Efc> {
        self.entries.iter().find(|e| e.coll == coll).and_then(|e| e.encrypted_fields.as_ref().map(|(_, c)| c))
    }

    /// `mc_schema_broker_get_encryptedFields`
    pub fn encrypted_fields(&self, coll: &[u8]) -> Result<&Efc> {
        let c = String::from_utf8_lossy(coll);
        let e = self.entries.iter().find(|e| e.coll == coll).ok_or_else(|| format!("Expected encryptedFields for '{c}', but did not find entry"))?;
        e.encrypted_fields.as_ref().map(|(_, c)| c).ok_or_else(|| format!("Expected encryptedFields for '{c}', but none set"))
    }

    /// `append_encryptionInformation`; `key_id_by_name` resolves keyAltName fields
    fn append_encryption_information(&self, w: &mut Writer, key_id_by_name: &dyn Fn(&[u8]) -> Result<[u8; 16]>) -> Result<()> {
        let ei = w.begin(T_DOCUMENT, b"encryptionInformation");
        w.int32(b"type", 1);
        let sc = w.begin(T_DOCUMENT, b"schema");
        for e in &self.entries {
            if e.encrypted_fields.is_none() && e.json_schema.is_some() {
                continue;
            }
            let n = ns(self.db(), &e.coll);
            let p = w.begin(T_DOCUMENT, n.as_bytes());
            let (doc, sev) = match &e.encrypted_fields {
                Some((d, c)) => (&d[..], c.str_encode_version),
                None => (&[5u8, 0, 0, 0, 0][..], 0),
            };
            append_encrypted_fields(w, doc, &e.coll, sev, key_id_by_name)?;
            w.end(p);
        }
        w.end(sc);
        w.end(ei);
        Ok(())
    }

    /// `mc_schema_broker_add_schemas_to_cmd` for the server (MC_CMD_SCHEMAS_FOR_SERVER)
    pub fn add_schemas_to_cmd(&self, cmd: &[u8], key_id_by_name: &dyn Fn(&[u8]) -> Result<[u8; 16]>) -> Result<Vec<u8>> {
        let name = cmd_name(cmd)?;
        let mut with_ef = None;
        let mut with_js = None;
        for e in &self.entries {
            if let Some((_, efc)) = &e.encrypted_fields {
                with_ef = Some(&e.coll);
                for f in &efc.fields {
                    if let (Some(a), None) = (&f.key_alt_name, f.key_id) {
                        key_id_by_name(a).map_err(|_| format!("Could not find key by keyAltName: {}", String::from_utf8_lossy(a)))?;
                    }
                }
            } else if e.json_schema.is_some() {
                with_js = Some(&e.coll);
            }
        }
        if let (Some(a), Some(b)) = (with_ef, with_js) {
            let (a, b) = (String::from_utf8_lossy(a), String::from_utf8_lossy(b));
            return Err(format!(
                "Collection '{a}' has an encryptedFields configured, but collection '{b}' has a JSON schema configured. This is not supported on mongocryptd/crypt_shared versions below 8.2. To ignore the JSON schema, add an empty entry for '{b}' to AutoEncryptionOpts.encryptedFieldsMap: \"{b}\": {{ \"fields\": [] }}"
            ));
        }
        // the server only takes encryptionInformation (CSFLE schemas are for query analysis)
        if with_ef.is_some() || (with_js.is_none() && name == b"bulkWrite") {
            return self.insert_encryption_information(&name, cmd, key_id_by_name);
        }
        Ok(cmd.to_vec())
    }

    /// `insert_encryptionInformation`
    fn insert_encryption_information(&self, name: &[u8], cmd: &[u8], key_id_by_name: &dyn Fn(&[u8]) -> Result<[u8; 16]>) -> Result<Vec<u8>> {
        let mut w = Writer::with_capacity(cmd.len() + 256);
        if name == b"bulkWrite" {
            let ns_info = bulk_write_ns_info(cmd)?;
            if get(ns_info, b"encryptionInformation").is_some() {
                return Err("unexpected `encryptionInformation` present in input `nsInfo`.".into());
            }
            copy_excluding(cmd, &[b"nsInfo"], &mut w);
            let a = w.begin(T_ARRAY, b"nsInfo");
            let d = w.begin(T_DOCUMENT, b"0");
            w.append_raw_elements(ns_info);
            self.append_encryption_information(&mut w, key_id_by_name)?;
            w.end(d);
            w.end(a);
            return Ok(w.finish());
        }
        if name == b"explain" {
            let Some(e) = get(cmd, b"explain") else { return Err("expected to find 'explain' field".into()) };
            let Value::Document(e) = e else { return Err("expected 'explain' to be document".into()) };
            let p = w.begin(T_DOCUMENT, b"explain");
            w.append_raw_elements(e);
            self.append_encryption_information(&mut w, key_id_by_name)?;
            w.end(p);
            copy_excluding(cmd, &[b"explain"], &mut w);
            return Ok(w.finish());
        }
        w.append_raw_elements(cmd);
        self.append_encryption_information(&mut w, key_id_by_name)?;
        Ok(w.finish())
    }
}

/// `append_encryptedFields`: the fields' keyAltName become keyId; default state collections
fn append_encrypted_fields(w: &mut Writer, ef: &[u8], coll: &[u8], default_sev: u8, key_id_by_name: &dyn Fn(&[u8]) -> Result<[u8; 16]>) -> Result<()> {
    let (mut esc, mut ecoc, mut fields, mut sev) = (false, false, false, false);
    let mut it = Iter::new(ef).ok_or("[append_encryptedFields] statement failed: BSON failure")?;
    while it.next() {
        let k = it.key();
        esc |= k == b"escCollection";
        ecoc |= k == b"ecocCollection";
        fields |= k == b"fields";
        sev |= k == b"strEncodeVersion";
        if let (b"fields", Value::Array(a)) = (k, it.value()) {
            let p = w.begin(T_ARRAY, b"fields");
            let mut ai = Iter::new(a).ok_or("failed to iterate 'fields' array")?;
            while ai.next() {
                match ai.value() {
                    Value::Document(d) => {
                        let fp = w.begin(T_DOCUMENT, ai.key());
                        copy_excluding(d, &[b"keyAltName"], w);
                        if let Some(Value::Utf8(name)) = get(d, b"keyAltName") {
                            let id = key_id_by_name(name)?;
                            w.binary(b"keyId", 4, &id);
                        }
                        w.end(fp);
                    }
                    _ => w.element_from(None, &ai),
                }
            }
            w.end(p);
        } else {
            w.element_from(None, &it);
        }
    }
    let c = String::from_utf8_lossy(coll);
    if !esc {
        w.utf8(b"escCollection", format!("enxcol_.{c}.esc").as_bytes());
    }
    if !ecoc {
        w.utf8(b"ecocCollection", format!("enxcol_.{c}.ecoc").as_bytes());
    }
    if !fields {
        w.array(b"fields", &[5, 0, 0, 0, 0]);
    }
    if !sev && default_sev != 0 {
        w.int32(b"strEncodeVersion", default_sev as i32);
    }
    Ok(())
}

/// `bson_copy_to_excluding_noinit`
fn copy_excluding(doc: &[u8], exclude: &[&[u8]], w: &mut Writer) {
    let Some(mut it) = Iter::new(doc) else { return };
    while it.next() {
        if !exclude.contains(&it.key()) {
            w.element_from(None, &it);
        }
    }
}

fn cmd_name(cmd: &[u8]) -> Result<Vec<u8>> {
    let mut it = Iter::new(cmd).ok_or("unable to iterate over command BSON")?;
    if !it.next() {
        return Err("unexpected empty BSON for command".into());
    }
    Ok(it.key().to_vec())
}

/// The single `nsInfo.0` document of a bulkWrite
fn bulk_write_ns_info(cmd: &[u8]) -> Result<&[u8]> {
    let Some(Value::Array(a)) = get(cmd, b"nsInfo") else { return Err("expected one namespace in `bulkWrite`, but found zero.".into()) };
    let Some(Value::Document(d)) = get(a, b"0") else { return Err("expected one namespace in `bulkWrite`, but found zero.".into()) };
    if get(a, b"1").is_some() {
        return Err("expected one namespace in `bulkWrite`, but found more than one. Only one namespace is supported.".into());
    }
    Ok(d)
}

// ---------------------------------------------------------------------------------------------
// Commands

/// `_check_cmd_for_auto_encrypt`: (bypass, target collection)
pub fn check_cmd(cmd: &[u8]) -> Result<(bool, Option<Vec<u8>>)> {
    let mut it = Iter::new(cmd).ok_or("invalid BSON")?;
    if !it.next() {
        return Err("invalid empty BSON".into());
    }
    let name = String::from_utf8_lossy(it.key()).into_owned();
    let target = if name == "explain" {
        let Value::Document(e) = it.value() else { return Err("explain value is not a document".into()) };
        let mut ei = Iter::new(e).ok_or("malformed BSON for encrypt command")?;
        if !ei.next() {
            return Err("invalid empty BSON".into());
        }
        ei.value()
    } else {
        it.value()
    };
    let coll = match target {
        Value::Utf8(s) => {
            no_nul(s, "collection name")?;
            Some(s.to_vec())
        }
        _ => None,
    };
    const ELIGIBLE: &[&str] = &[
        "aggregate", "count", "distinct", "delete", "find", "findAndModify", "insert", "update", "create", "createIndexes", "explain",
        "compactStructuredEncryptionData", "cleanupStructuredEncryptionData", "collMod",
    ];
    const BYPASS: &[&str] = &[
        "getMore", "authenticate", "getnonce", "logout", "abortTransaction", "commitTransaction", "endSessions", "startSession", "drop",
        "dropDatabase", "dropIndexes", "killCursors", "listCollections", "listDatabases", "listIndexes", "renameCollection", "ping",
        "saslStart", "saslContinue", "killAllSessions", "killSessions", "killAllSessionsByPattern", "refreshSessions", "hello", "buildInfo",
        "buildinfo", "getCmdLineOpts", "getLog", "createSearchIndexes", "listSearchIndexes", "dropSearchIndex", "updateSearchIndex",
        "serverStatus",
    ];
    let eligible = ELIGIBLE.contains(&name.as_str());
    let bypass = BYPASS.contains(&name.as_str()) || name.eq_ignore_ascii_case("ismaster");
    if eligible {
        match &coll {
            None => return Err(format!("non-collection command not supported for auto encryption: {name}")),
            Some(c) if c.is_empty() => return Err(format!("empty collection name on command: {name}")),
            _ => {}
        }
    }
    if eligible || bypass {
        return Ok((bypass, coll));
    }
    Err(format!("command not supported for auto encryption: {name}"))
}

/// `_check_cmd_for_auto_encrypt_bulkWrite`: (target db, target collection)
pub fn check_bulk_write(cmd: &[u8]) -> Result<(String, Vec<u8>)> {
    let ns = match get(cmd, b"nsInfo") {
        Some(Value::Array(a)) => match get(a, b"0") {
            Some(Value::Document(d)) => get(d, b"ns"),
            _ => None,
        },
        _ => None,
    };
    let ns = match ns {
        Some(Value::Utf8(s)) => s,
        Some(v) => return Err(format!("expected namespace to be UTF8, got: {}", type_name(v.type_code()))),
        None => return Err("failed to find namespace in `bulkWrite` command".into()),
    };
    no_nul(ns, "namespace in `bulkWrite` command")?;
    let dot = ns.iter().position(|&c| c == b'.').ok_or_else(|| format!("expected namespace to contain dot, got: {}", String::from_utf8_lossy(ns)))?;
    if let Some(Value::Array(a)) = get(cmd, b"nsInfo") {
        if get(a, b"1").is_some() {
            return Err("expected one namespace in `bulkWrite`, but found more than one. Only one namespace is supported.".into());
        }
    }
    Ok((String::from_utf8_lossy(&ns[..dot]).into_owned(), ns[dot + 1..].to_vec()))
}

/// `find_collections_in_pipeline` (`$lookup.from`, `$unionWith.coll`, nested pipelines and `$facet`)
fn collections_in_pipeline(sb: &mut SchemaBroker, pipeline: &Value, db: &str, path: &str, depth: u32) -> Result<()> {
    let Value::Array(a) = pipeline else { return Err(format!("failed to recurse pipeline at path: {path}")) };
    if depth > 100 {
        return Err(format!("failed to recurse pipeline at path: {path}"));
    }
    let mut it = Iter::new(a).ok_or_else(|| format!("failed to recurse pipeline at path: {path}"))?;
    while it.next() {
        let key = String::from_utf8_lossy(it.key()).into_owned();
        let Value::Document(stage) = it.value() else { return Err(format!("failed to recurse stage at path: {path}.{key}")) };
        let mut si = Iter::new(stage).ok_or_else(|| format!("failed to recurse stage at path: {path}.{key}"))?;
        if !si.next() {
            return Err(format!("failed to recurse stage at path: {path}.{key}"));
        }
        let name = si.key().to_vec();
        let (field, sub) = match &name[..] {
            b"$lookup" => (&b"from"[..], "$lookup"),
            b"$unionWith" => (&b"coll"[..], "$unionWith"),
            b"$facet" => {
                let Value::Document(f) = si.value() else { return Err(format!("failed to recurse $facet at path: {path}.{key}")) };
                let mut fi = Iter::new(f).ok_or_else(|| format!("failed to recurse $facet at path: {path}.{key}"))?;
                while fi.next() {
                    let sub = format!("{path}.{key}.$facet.{}", String::from_utf8_lossy(fi.key()));
                    collections_in_pipeline(sb, &fi.value(), db, &sub, depth + 1)?;
                }
                continue;
            }
            _ => continue,
        };
        let Value::Document(d) = si.value() else { return Err(format!("failed to recurse {sub} at path: {path}.{key}")) };
        let mut di = Iter::new(d).ok_or_else(|| format!("failed to recurse {sub} at path: {path}.{key}"))?;
        while di.next() {
            if di.key() == field {
                let Value::Utf8(c) = di.value() else {
                    let fld = String::from_utf8_lossy(field);
                    return Err(if sub == "$lookup" {
                        format!("expected string, but '{}' for '{fld}' field at path: {path}.{key}", type_name(di.value().type_code()))
                    } else {
                        format!("expected string, but got '{}' for '{fld}' field at path: {path}.{key}", type_name(di.value().type_code()))
                    });
                };
                no_nul(c, &format!("'{}' collection name", String::from_utf8_lossy(field)))?;
                sb.request(db, c)?;
            }
            if di.key() == b"pipeline" {
                collections_in_pipeline(sb, &di.value(), db, &format!("{path}.{key}.{sub}.pipeline"), depth + 1)?;
            }
        }
    }
    Ok(())
}

/// `find_collections_in_agg`
pub fn collections_in_agg(sb: &mut SchemaBroker, cmd: &[u8], db: &str) -> Result<()> {
    match get(cmd, b"pipeline") {
        Some(p) => collections_in_pipeline(sb, &p, db, "aggregate.pipeline", 0),
        None => Ok(()),
    }
}

/// `must_omit_encryptionInformation`
pub fn must_omit_encryption_information(name: &[u8], cmd: &[u8], efc: Option<&Efc>) -> Result<bool> {
    if name == b"compactStructuredEncryptionData" {
        let efc = efc.ok_or("expected to have encryptedFields for compactStructuredEncryptionData command but have none")?;
        return Ok(!efc.fields.iter().any(|f| f.queries & RANGE_OR_TEXT != 0));
    }
    if [&b"cleanupStructuredEncryptionData"[..], b"create", b"collMod", b"createIndexes"].contains(&name) {
        return Ok(true);
    }
    if ![&b"find"[..], b"aggregate", b"distinct", b"count", b"insert"].contains(&name) {
        return Ok(false);
    }
    // only commands with payloads requiring encryptionInformation keep it
    let mut found = false;
    crate::engine::traverse_subtype6(cmd, &mut |b: &[u8]| {
        if b.is_empty() {
            return Err("unexpected empty FLE payload".into());
        }
        if [crate::fle2::SUBTYPE_IUP_V2, crate::fle2::SUBTYPE_FIND_EQUALITY_V2, crate::fle2::SUBTYPE_FIND_RANGE_V2, crate::text::SUBTYPE_FIND_TEXT].contains(&b[0]) {
            found = true;
        }
        Ok(())
    })?;
    Ok(!found)
}

/// `_fle2_strip_encryptionInformation`
pub fn strip_encryption_information(name: &[u8], cmd: &[u8]) -> Result<Vec<u8>> {
    let mut w = Writer::with_capacity(cmd.len());
    if name == b"bulkWrite" {
        let ns_info = bulk_write_ns_info(cmd)?;
        copy_excluding(cmd, &[b"nsInfo"], &mut w);
        let a = w.begin(T_ARRAY, b"nsInfo");
        let d = w.begin(T_DOCUMENT, b"0");
        copy_excluding(ns_info, &[b"encryptionInformation"], &mut w);
        w.end(d);
        w.end(a);
        return Ok(w.finish());
    }
    if name == b"explain" {
        let Some(Value::Document(e)) = get(cmd, b"explain") else { return Err("expected 'explain' to be document".into()) };
        let p = w.begin(T_DOCUMENT, b"explain");
        copy_excluding(e, &[b"encryptionInformation"], &mut w);
        w.end(p);
        copy_excluding(cmd, &[b"explain"], &mut w);
        return Ok(w.finish());
    }
    copy_excluding(cmd, &[b"encryptionInformation"], &mut w);
    Ok(w.finish())
}

/// `_fle2_fixup_encryptedFields_strEncodeVersion` for `create`
pub fn fixup_str_encode_version(name: &[u8], cmd: &[u8], efc: Option<&Efc>) -> Result<Vec<u8>> {
    if name != b"create" {
        return Ok(cmd.to_vec());
    }
    let efc = efc.ok_or("expected to have encryptedFields for create command but have none")?;
    let Some(ef) = get(cmd, b"encryptedFields") else { return Ok(cmd.to_vec()) };
    let Value::Document(ef) = ef else {
        return Err(format!("_fle2_fixup_encryptedFields_strEncodeVersion: Expected encryptedFields to be type obj, got: {}", type_name(ef.type_code())));
    };
    match get(ef, b"strEncodeVersion") {
        None => {
            if efc.str_encode_version == 0 {
                return Ok(cmd.to_vec());
            }
            let mut w = Writer::with_capacity(cmd.len() + 32);
            copy_excluding(cmd, &[b"encryptedFields"], &mut w);
            let p = w.begin(T_DOCUMENT, b"encryptedFields");
            w.append_raw_elements(ef);
            w.int32(b"strEncodeVersion", efc.str_encode_version as i32);
            w.end(p);
            Ok(w.finish())
        }
        Some(Value::Int32(v)) => {
            if v != efc.str_encode_version as i32 {
                return Err(format!(
                    "encryptedFields.strEncodeVersion must match EncryptedFieldConfig.strEncodeVersion, but got {v} vs {}",
                    efc.str_encode_version
                ));
            }
            Ok(cmd.to_vec())
        }
        Some(v) => Err(format!("expected 'strEncodeVersion' to be type int32, got: {}", type_name(v.type_code()))),
    }
}

/// `_fle2_append_compactionTokens`: compactionTokens / cleanupTokens per field (`key_by_id` gives
/// the decrypted key)
pub fn append_compaction_tokens(name: &[u8], efc: Option<&Efc>, cmd: &[u8], key_by_id: &dyn Fn(&[u8; 16]) -> Result<Vec<u8>>) -> Result<Vec<u8>> {
    let cleanup = name == b"cleanupStructuredEncryptionData";
    if name != b"compactStructuredEncryptionData" && !cleanup {
        return Ok(cmd.to_vec());
    }
    let efc = efc.ok_or_else(|| format!("expected to have encryptedFields for {} command but have none", String::from_utf8_lossy(name)))?;
    let mut w = Writer::with_capacity(cmd.len() + 128 * efc.fields.len());
    w.append_raw_elements(cmd);
    let p = w.begin(T_DOCUMENT, if cleanup { b"cleanupTokens" } else { b"compactionTokens" });
    for f in &efc.fields {
        let id = f.key_id.ok_or("could not find key")?;
        let key = key_by_id(&id)?;
        if key.len() < crate::crypto::TOKEN_KEY_LEN {
            return Err("key too short".into());
        }
        let tk = &key[key.len() - crate::crypto::TOKEN_KEY_LEN..];
        let cl1 = crate::fle2::hmac_u64(tk, 1);
        let ecoc = crate::fle2::hmac_u64(&cl1, 4);
        if f.queries & RANGE_OR_TEXT != 0 {
            let esc = crate::fle2::hmac_u64(&cl1, 2);
            let pad = crate::fle2::hmac(&esc, &[0u8; 17]);
            let t = w.begin(T_DOCUMENT, &f.path);
            w.binary(b"ecoc", 0, &ecoc);
            w.binary(b"anchorPaddingToken", 0, &pad);
            w.end(t);
        } else {
            w.binary(&f.path, 0, &ecoc);
        }
    }
    w.end(p);
    Ok(w.finish())
}

// ---------------------------------------------------------------------------------------------
// The collinfo cache

/// `cache_collinfo`: listCollections results per namespace (CACHE_EXPIRATION_MS_DEFAULT)
#[derive(Default)]
pub struct CollinfoCache {
    entries: std::sync::Mutex<Vec<(String, Vec<u8>, std::time::Instant)>>,
}

const COLLINFO_EXPIRATION: std::time::Duration = std::time::Duration::from_millis(60_000);

impl CollinfoCache {
    pub fn get(&self, ns: &str) -> Option<Vec<u8>> {
        let mut g = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        g.retain(|(_, _, t)| t.elapsed() < COLLINFO_EXPIRATION);
        g.iter().find(|(n, _, _)| n == ns).map(|(_, d, _)| d.clone())
    }

    pub fn put(&self, ns: &str, doc: &[u8]) {
        let mut g = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        g.retain(|(n, _, _)| n != ns);
        g.push((ns.to_owned(), doc.to_vec(), std::time::Instant::now()));
    }
}
