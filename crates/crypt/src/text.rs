//! Queryable Encryption string search ("string" algorithm): libmongocrypt's mc-textopts.c,
//! the FLE2TextSearchInsertSpec parsers of mc-fle2-encryption-placeholder.c,
//! mc-text-search-str-encode.c and mc-str-encode-string-sets.c. Messages are libmongocrypt's.

use mongo_core::bson::iter::{Iter, Value, T_DOCUMENT};
use mongo_core::bson::writer::Writer;

type Result<T> = std::result::Result<T, String>;

/// `mc_TextOptsPerIndex_t`
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PerIndex {
    pub set: bool,
    pub str_max_length: Option<i32>,
    pub str_min_query_length: i32,
    pub str_max_query_length: i32,
}

/// `mc_TextOpts_t`
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TextOpts {
    pub case_sensitive: bool,
    pub diacritic_sensitive: bool,
    pub substring: PerIndex,
    pub prefix: PerIndex,
    pub suffix: PerIndex,
}

const OPTS: &str = "Error parsing TextOpts: ";

fn bson_type(t: u8) -> &'static str {
    crate::range::type_name(t)
}

/// `mc_TextOptsPerIndex_parse`
fn parse_per_index(doc: &[u8]) -> Result<PerIndex> {
    let mut out = PerIndex { set: true, ..Default::default() };
    let mut seen = [false; 3];
    let mut it = Iter::new(doc).ok_or_else(|| format!("{OPTS}Invalid BSON"))?;
    while it.next() {
        let key = String::from_utf8_lossy(it.key()).into_owned();
        let i = match key.as_str() {
            "strMaxLength" => 0,
            "strMinQueryLength" => 1,
            "strMaxQueryLength" => 2,
            _ => return Err(format!("{OPTS}Unrecognized field: '{key}'")),
        };
        if seen[i] {
            return Err(format!("{OPTS}Unexpected duplicate field '{key}'"));
        }
        seen[i] = true;
        let Value::Int32(v) = it.value() else { return Err(format!("{OPTS}'{key}' must be an int32")) };
        if v <= 0 {
            return Err(format!("{OPTS}'{key}' must be greater than zero"));
        }
        match i {
            0 => out.str_max_length = Some(v),
            1 => out.str_min_query_length = v,
            _ => out.str_max_query_length = v,
        }
    }
    Ok(out)
}

impl TextOpts {
    /// `mc_TextOpts_parse`
    pub fn parse(doc: &[u8]) -> Result<TextOpts> {
        let mut o = TextOpts::default();
        // caseSensitive, diacriticSensitive, substring, prefix, suffix
        let mut seen = [false; 5];
        let mut it = Iter::new(doc).ok_or_else(|| format!("{OPTS}Invalid BSON"))?;
        while it.next() {
            let key = String::from_utf8_lossy(it.key()).into_owned();
            let i = match key.as_str() {
                "caseSensitive" => 0,
                "diacriticSensitive" => 1,
                "substring" => 2,
                "prefix" => 3,
                "suffix" => 4,
                _ => return Err(format!("{OPTS}Unrecognized field: '{key}'")),
            };
            if seen[i] {
                return Err(format!("{OPTS}Unexpected duplicate field '{key}'"));
            }
            seen[i] = true;
            let v = it.value();
            match (i, v) {
                (0 | 1, Value::Bool(b)) => {
                    if i == 0 {
                        o.case_sensitive = b;
                    } else {
                        o.diacritic_sensitive = b;
                    }
                }
                (0 | 1, v) => return Err(format!("{OPTS}Expected bool for {key}, got: {}", bson_type(v.type_code()))),
                (_, Value::Document(d)) => {
                    let p = parse_per_index(d)?;
                    match i {
                        2 => {
                            if p.str_max_length.is_none() {
                                return Err(format!("{OPTS}'strMaxLength' must be set for substring"));
                            }
                            o.substring = p;
                        }
                        3 | 4 => {
                            if p.str_max_length.is_some() {
                                return Err(format!("{OPTS}'strMaxLength' is not allowed in '{key}'"));
                            }
                            if i == 3 {
                                o.prefix = p;
                            } else {
                                o.suffix = p;
                            }
                        }
                        _ => unreachable!(),
                    }
                }
                (_, v) => return Err(format!("{OPTS}Expected document for {key}, got: {}", bson_type(v.type_code()))),
            }
        }
        if !seen[0] {
            return Err(format!("{OPTS}'caseSensitive' is required"));
        }
        if !seen[1] {
            return Err(format!("{OPTS}'diacriticSensitive' is required"));
        }
        if seen[2] && (seen[3] || seen[4]) {
            return Err(format!("{OPTS}Cannot specify 'substring' with 'prefix' or 'suffix'"));
        }
        if !(seen[2] || seen[3] || seen[4]) {
            return Err(format!("{OPTS}One of 'prefix', 'suffix', or 'substring' is required"));
        }
        Ok(o)
    }

    /// `mc_TextOpts_to_FLE2TextSearchInsertSpec` (`which`: None for all, else only that index):
    /// `{v: {v: <value>, casef, diacf, prefix?: {ub, lb}, suffix?: {ub, lb}, substr?: {mlen, ub, lb}}}`
    pub fn to_insert_spec(&self, input: &[u8], which: Option<Which>) -> Result<Vec<u8>> {
        let mut it = Iter::new(input).ok_or("Error making FLE2RangeInsertSpec: Unable to find 'v' in input")?;
        if !it.find(b"v") {
            return Err("Error making FLE2RangeInsertSpec: Unable to find 'v' in input".into());
        }
        let mut w = Writer::with_capacity(input.len() + 128);
        let p = w.begin(T_DOCUMENT, b"v");
        w.element_from(Some(b"v"), &it);
        w.bool(b"casef", !self.case_sensitive);
        w.bool(b"diacf", !self.diacritic_sensitive);
        let per = |w: &mut Writer, name: &[u8], o: &PerIndex| {
            let p = w.begin(T_DOCUMENT, name);
            if let Some(m) = o.str_max_length {
                w.int32(b"mlen", m);
            }
            w.int32(b"ub", o.str_max_query_length);
            w.int32(b"lb", o.str_min_query_length);
            w.end(p);
        };
        if self.prefix.set && which.is_none_or(|x| x == Which::Prefix) {
            per(&mut w, b"prefix", &self.prefix);
        }
        if self.suffix.set && which.is_none_or(|x| x == Which::Suffix) {
            per(&mut w, b"suffix", &self.suffix);
        }
        if self.substring.set && which.is_none_or(|x| x == Which::Substring) {
            per(&mut w, b"substr", &self.substring);
        }
        w.end(p);
        Ok(w.finish())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Which {
    Prefix,
    Suffix,
    Substring,
}

/// `mc_FLE2{Prefix,Suffix,Substring}InsertSpec_t` (mlen is 0 for prefix/suffix)
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AffixSpec {
    pub mlen: u32,
    pub ub: u32,
    pub lb: u32,
}

/// `mc_FLE2TextSearchInsertSpec_t`
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InsertSpec {
    pub v: Vec<u8>,
    pub casef: bool,
    pub diacf: bool,
    pub substr: Option<AffixSpec>,
    pub suffix: Option<AffixSpec>,
    pub prefix: Option<AffixSpec>,
}

/// `mc_FLE2SubstringInsertSpec_parse` / `_Suffix` / `_Prefix`
fn parse_affix(v: &Value, kind: &str, mlen: bool) -> Result<AffixSpec> {
    let pre = format!("Error parsing FLE2{kind}InsertSpec: ");
    let Value::Document(d) = v else { return Err(format!("{pre}must be an iterator to a document")) };
    let mut out = AffixSpec::default();
    let (mut has_mlen, mut has_ub, mut has_lb) = (false, false, false);
    let mut it = Iter::new(d).ok_or_else(|| format!("{pre}must be an iterator to a document"))?;
    while it.next() {
        let k = it.key();
        let (name, has, dest): (&str, &mut bool, &mut u32) = match k {
            b"mlen" if mlen => ("mlen", &mut has_mlen, &mut out.mlen),
            b"ub" => ("ub", &mut has_ub, &mut out.ub),
            b"lb" => ("lb", &mut has_lb, &mut out.lb),
            _ => continue,
        };
        if *has {
            return Err(format!("{pre}Unexpected duplicate field '{name}'"));
        }
        *has = true;
        let Value::Int32(x) = it.value() else { return Err(format!("{pre}'{name}' must be an int32")) };
        if x <= 0 {
            return Err(format!("{pre}'{name}' must be greater than zero"));
        }
        *dest = x as u32;
    }
    for (n, h) in [("mlen", has_mlen || !mlen), ("ub", has_ub), ("lb", has_lb)] {
        if !h {
            return Err(format!("{pre}Missing field '{n}' in placeholder"));
        }
    }
    if out.ub < out.lb {
        return Err(format!("{pre}upper bound cannot be less than the lower bound"));
    }
    if mlen && out.mlen < out.ub {
        return Err(format!("{pre}maximum indexed length cannot be less than the upper bound"));
    }
    Ok(out)
}

impl InsertSpec {
    /// `mc_FLE2TextSearchInsertSpec_parse` of the `v` document
    pub fn parse(v: &Value) -> Result<InsertSpec> {
        const PRE: &str = "Error parsing FLE2TextSearchInsertSpec: ";
        let Value::Document(d) = v else { return Err(format!("{PRE}must be an iterator to a document")) };
        let mut out = InsertSpec::default();
        let mut seen = [false; 6];
        let mut it = Iter::new(d).ok_or_else(|| format!("{PRE}must be an iterator to a document"))?;
        while it.next() {
            let key = String::from_utf8_lossy(it.key()).into_owned();
            let i = match key.as_str() {
                "v" => 0,
                "casef" => 1,
                "diacf" => 2,
                "substr" => 3,
                "suffix" => 4,
                "prefix" => 5,
                _ => continue,
            };
            if seen[i] {
                return Err(format!("{PRE}Unexpected duplicate field '{key}'"));
            }
            seen[i] = true;
            let val = it.value();
            match i {
                0 => match val {
                    Value::Utf8(s) => out.v = s.to_vec(),
                    v => return Err(format!("{PRE}unsupported BSON type: {} for text search", bson_type(v.type_code()))),
                },
                1 | 2 => {
                    let Value::Bool(b) = val else { return Err(format!("{PRE}'{key}' must be a bool")) };
                    if i == 1 {
                        out.casef = b;
                    } else {
                        out.diacf = b;
                    }
                }
                3 => out.substr = Some(parse_affix(&val, "Substring", true)?),
                4 => out.suffix = Some(parse_affix(&val, "Suffix", false)?),
                _ => out.prefix = Some(parse_affix(&val, "Prefix", false)?),
            }
        }
        for (i, n) in [(0, "v"), (1, "casef"), (2, "diacf")] {
            if !seen[i] {
                return Err(format!("{PRE}Missing field '{n}' in placeholder"));
            }
        }
        Ok(out)
    }

    fn fold_options(&self) -> u32 {
        (if self.casef { crate::unicode::TO_LOWER } else { 0 }) | (if self.diacf { crate::unicode::REMOVE_DIACRITICS } else { 0 })
    }
}

/// MAX_ENCODE_BYTE_LEN
const MAX_ENCODE_BYTE_LEN: usize = 16 * 1024 * 1024;
/// OVERHEAD_BYTES
const OVERHEAD_BYTES: u32 = 5;
/// The padding character appended to the base string
const BAD_CHAR: u8 = 0xFF;

/// `mc_utf8_string_with_bad_char_t`: the string plus 0xFF, and its codepoint offsets (the last
/// one is the 0xFF's)
pub struct BaseString {
    pub buf: Vec<u8>,
    offsets: Vec<u32>,
}

impl BaseString {
    fn new(s: &[u8]) -> BaseString {
        let mut buf = Vec::with_capacity(s.len() + 1);
        buf.extend_from_slice(s);
        buf.push(BAD_CHAR);
        let mut offsets: Vec<u32> = utf8_offsets(s).collect();
        offsets.push(s.len() as u32);
        BaseString { buf, offsets }
    }

    fn codepoint_len(&self) -> u32 {
        self.offsets.len() as u32
    }

    /// The bytes of codepoints [start, end) (end == codepoint_len: up to the end, 0xFF included)
    fn slice(&self, start: u32, end: u32) -> &[u8] {
        let s = self.offsets[start as usize] as usize;
        let e = if end == self.codepoint_len() { self.buf.len() } else { self.offsets[end as usize] as usize };
        &self.buf[s..e]
    }
}

/// The byte offset of each codepoint of valid UTF-8 (`bson_utf8_next_char`)
fn utf8_offsets(s: &[u8]) -> impl Iterator<Item = u32> + '_ {
    let mut i = 0usize;
    std::iter::from_fn(move || {
        if i >= s.len() {
            return None;
        }
        let at = i;
        let b = s[i];
        i += if b < 0x80 { 1 } else if b & 0xe0 == 0xc0 { 2 } else if b & 0xf0 == 0xe0 { 3 } else { 4 };
        Some(at as u32)
    })
}

fn codepoints(s: &[u8]) -> u32 {
    utf8_offsets(s).count() as u32
}

/// `mc_affix_set_t`: (start, end, count) in insertion order
pub struct AffixSet {
    entries: Vec<(u32, u32, u32)>,
}

/// `mc_substring_set_t`: unique substrings in FNV-1a buckets (iteration order: bucket by bucket,
/// each in insertion order), then the base string with the padding count
pub struct SubstringSet {
    buckets: Vec<Vec<(u32, u32)>>,
    base_count: u32,
}

/// HASHSET_SIZE
const HASHSET_SIZE: usize = 4096;

fn fnv1a(data: &[u8]) -> u32 {
    let mut h: u32 = 2166136261;
    for &b in data {
        h = (h ^ b as u32).wrapping_mul(16777619);
    }
    h
}

impl SubstringSet {
    fn insert(&mut self, base: &BaseString, start: u32, end: u32) -> bool {
        let s = base.offsets[start as usize];
        let bytes = base.slice(start, end);
        let len = bytes.len() as u32;
        let b = &mut self.buckets[fnv1a(bytes) as usize % HASHSET_SIZE];
        if b.iter().any(|&(o, l)| l == len && &base.buf[o as usize..(o + l) as usize] == bytes) {
            return false;
        }
        b.push((s, len));
        true
    }
}

/// `mc_str_encode_sets_t`
pub struct EncodeSets {
    pub base: BaseString,
    pub suffix: Option<AffixSet>,
    pub prefix: Option<AffixSet>,
    pub substring: Option<SubstringSet>,
    pub msize: u32,
}

impl EncodeSets {
    /// The exact string (the base string without the 0xFF)
    pub fn exact(&self) -> &[u8] {
        &self.base.buf[..self.base.buf.len() - 1]
    }

    /// (bytes, count) of an affix set
    pub fn affixes<'a>(&'a self, set: &'a AffixSet) -> impl Iterator<Item = (&'a [u8], u32)> + 'a {
        set.entries.iter().map(move |&(s, e, c)| (self.base.slice(s, e), c))
    }

    /// (bytes, count) of the substring set
    pub fn substrings<'a>(&'a self, set: &'a SubstringSet) -> impl Iterator<Item = (&'a [u8], u32)> + 'a {
        let base = &self.base.buf;
        set.buckets
            .iter()
            .flat_map(move |b| b.iter().map(move |&(o, l)| (&base[o as usize..(o + l) as usize], 1)))
            .chain((set.base_count > 0).then_some((&base[..], set.base_count)))
    }
}

fn padded_len(unfolded_byte_len: u32) -> u32 {
    let encrypted_len = 16 * ((unfolded_byte_len as u64 + OVERHEAD_BYTES as u64 + 16) / 16);
    (encrypted_len - OVERHEAD_BYTES as u64) as u32
}

/// `generate_prefix_or_suffix_tree`
fn affix_tree(base: &BaseString, unfolded_byte_len: u32, lb: u32, ub: u32, prefix: bool, msize_out: &mut u32) -> Option<AffixSet> {
    let padded = padded_len(unfolded_byte_len);
    if padded < lb {
        return None;
    }
    let msize = padded.min(ub) - lb + 1;
    let folded = base.codepoint_len() - 1;
    let real_max = folded.min(ub);
    let real = if real_max >= lb { real_max - lb + 1 } else { 0 };
    let mut entries = Vec::with_capacity((real + 1) as usize);
    for i in lb..real_max + 1 {
        entries.push(if prefix { (0, i, 1) } else { (folded - i, folded, 1) });
    }
    if msize != real {
        entries.push((0, base.codepoint_len(), msize - real));
    }
    *msize_out += msize;
    Some(AffixSet { entries })
}

/// `calc_number_of_substrings`
fn substring_count(len: u32, lb: u32, ub: u32) -> u32 {
    if lb > len {
        return 0;
    }
    let largest = len.min(ub);
    let largest_count = len - largest + 1;
    let smallest_count = len - lb + 1;
    ((largest_count as u64 + smallest_count as u64) * (smallest_count - largest_count + 1) as u64 / 2) as u32
}

/// `generate_substring_tree`
fn substring_tree(base: &BaseString, unfolded_byte_len: u32, spec: &AffixSpec, msize_out: &mut u32) -> Option<SubstringSet> {
    let padded = padded_len(unfolded_byte_len);
    if padded < spec.lb {
        return None;
    }
    let folded = base.codepoint_len() - 1;
    let msize = substring_count(spec.mlen.min(padded), spec.lb, spec.ub);
    let mut set = SubstringSet { buckets: vec![Vec::new(); HASHSET_SIZE], base_count: 0 };
    let mut real = 0u32;
    if folded >= spec.lb {
        for i in 0..folded - spec.lb + 1 {
            for j in i + spec.lb..folded.min(i + spec.ub) + 1 {
                if set.insert(base, i, j) {
                    real += 1;
                }
            }
        }
    }
    if msize != real {
        // msize > real (libmongocrypt asserts it)
        set.base_count += msize.saturating_sub(real);
    }
    *msize_out += msize;
    Some(set)
}

fn fold_or_copy(spec: &InsertSpec) -> Result<Vec<u8>> {
    if spec.casef || spec.diacf {
        crate::unicode::fold(&spec.v, spec.fold_options())
    } else {
        Ok(spec.v.clone())
    }
}

/// `mc_text_search_str_encode`
pub fn str_encode(spec: &InsertSpec) -> Result<EncodeSets> {
    if spec.v.len() > MAX_ENCODE_BYTE_LEN {
        return Err(format!(
            "StrEncode: String passed in was too long: String was {} bytes, but max is {MAX_ENCODE_BYTE_LEN} bytes",
            spec.v.len()
        ));
    }
    if std::str::from_utf8(&spec.v).is_err() || spec.v.contains(&0) {
        return Err("StrEncode: String passed in was not valid UTF-8".into());
    }
    let base = BaseString::new(&fold_or_copy(spec)?);
    let len = spec.v.len() as u32;
    let mut sets = EncodeSets { base, suffix: None, prefix: None, substring: None, msize: 1 };
    if let Some(s) = &spec.suffix {
        sets.suffix = affix_tree(&sets.base, len, s.lb, s.ub, false, &mut sets.msize);
    }
    if let Some(p) = &spec.prefix {
        sets.prefix = affix_tree(&sets.base, len, p.lb, p.ub, true, &mut sets.msize);
    }
    if let Some(s) = &spec.substr {
        let unfolded = codepoints(&spec.v);
        if unfolded > s.mlen {
            return Err(format!(
                "StrEncode: String passed in was longer than the maximum length for substring indexing -- String len: {unfolded}, max len: {}",
                s.mlen
            ));
        }
        sets.substring = substring_tree(&sets.base, len, s, &mut sets.msize);
    }
    Ok(sets)
}

/// `mc_text_search_str_query`: the folded query string
pub fn str_query(spec: &InsertSpec) -> Result<Vec<u8>> {
    if spec.v.len() > MAX_ENCODE_BYTE_LEN {
        return Err(format!(
            "StrQuery: String passed in was too long: String was {} bytes, but max is {MAX_ENCODE_BYTE_LEN} bytes",
            spec.v.len()
        ));
    }
    if std::str::from_utf8(&spec.v).is_err() || spec.v.contains(&0) {
        return Err("StrQuery: String passed in was not valid UTF-8".into());
    }
    let folded = fold_or_copy(spec)?;
    let n = codepoints(&folded);
    let bounds = spec.substr.or(spec.suffix).or(spec.prefix);
    if let Some(b) = bounds {
        if n == 0 {
            return Err("StrQuery: string value cannot be empty for substring, suffix, or prefix queries".into());
        }
        if n > b.ub {
            return Err(format!(
                "StrQuery: string value was longer than the maximum query length for this field after folding -- folded codepoint len: {n}, max query len: {}",
                b.ub
            ));
        }
        if n < b.lb {
            return Err(format!(
                "StrQuery: string value was shorter than the minimum query length for this field after folding -- folded codepoint len: {n}, min query len: {}",
                b.lb
            ));
        }
    }
    Ok(folded)
}

// ---------------------------------------------------------------------------------------------
// Payloads (mongocrypt-marking.c, mc-fle2-insert-update-payload-v2.c, mc-fle2-find-text-payload.c)

use crate::fle2::{encrypted_tokens, hmac, hmac_u64, token_key, Token};

/// MC_SUBTYPE_FLE2FindTextPayload
pub const SUBTYPE_FIND_TEXT: u8 = 18;

/// TEXT_{EXACT,SUBSTRING,SUFFIX,PREFIX}_ID
#[derive(Clone, Copy)]
enum Kind {
    Exact = 1,
    Substring = 2,
    Suffix = 3,
    Prefix = 4,
}

/// A string as a BSON value (`_mongocrypt_buffer_copy_from_string_as_bson_value`)
fn bson_string(s: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(s.len() + 5);
    v.extend_from_slice(&((s.len() + 1) as i32).to_le_bytes());
    v.extend_from_slice(s);
    v.push(0);
    v
}

/// The level-1 tokens of a token key
struct Level1 {
    collections: Token,
    server_derivation: Token,
}

fn level1(index_key: &[u8]) -> Result<Level1> {
    let tk = token_key(index_key)?;
    Ok(Level1 { collections: hmac_u64(tk, 1), server_derivation: hmac_u64(tk, 2) })
}

/// (EDC derived, ESC derived, server derived) of `value` for `kind`, with the contention factor
/// (inserts) or without (finds)
fn text_tokens(l1: &Level1, kind: Kind, value: &[u8], contention: Option<u64>) -> (Token, Token, Token) {
    let t = kind as u64;
    let edc = hmac_u64(&hmac_u64(&l1.collections, 1), t);
    let esc = hmac_u64(&hmac_u64(&l1.collections, 2), t);
    let mut d = hmac(&edc, value);
    let mut s = hmac(&esc, value);
    if let Some(cf) = contention {
        d = hmac_u64(&d, cf);
        s = hmac_u64(&s, cf);
    }
    let l = hmac(&hmac_u64(&l1.server_derivation, t), value);
    (d, s, l)
}

/// One insert token set `{d, s, l, p}`; p = EncryptCTR(ECOC, s || msize as 3 bytes LE)
fn token_set(l1: &Level1, kind: Kind, value: &[u8], cf: u64, msize: u32) -> Result<Vec<u8>> {
    let (d, s, l) = text_tokens(l1, kind, value, Some(cf));
    let p = encrypted_tokens(&l1.collections, &s, &msize.to_le_bytes()[..3])?;
    let mut t = Writer::with_capacity(160);
    t.binary(b"d", 0, &d);
    t.binary(b"s", 0, &s);
    t.binary(b"l", 0, &l);
    t.binary(b"p", 0, &p);
    Ok(t.finish())
}

/// `_mongocrypt_fle2_placeholder_to_insert_update_ciphertextForTextSearch`: the
/// FLE2InsertUpdatePayloadV2 (without the subtype byte). `value` is the string as a BSON value.
pub fn insert_payload(
    spec: &InsertSpec,
    index_key_id: &[u8; 16],
    index_key: &[u8],
    user_key_id: &[u8; 16],
    user_key: &[u8],
    value: &[u8],
    max_contention: i64,
) -> Result<Vec<u8>> {
    if spec.substr.is_none() && spec.suffix.is_none() && spec.prefix.is_none() {
        return Err("FLE2TextSearchInsertSpec is missing a substring, suffix, or prefix index specification".into());
    }
    // _fle2_choose_contention_factor: uniform in [0, maxContentionFactor]
    let k = if max_contention > 0 { crate::crypto::random_int64(max_contention + 1)? } else { 0 };
    let c = crate::fle2::common(token_key(index_key)?, value, Some(k as u64));
    // d, s and l are unused for text search: zeros; p is a ciphertext of zeros || 0x000000
    let bogus = [0u8; 32];
    let p = encrypted_tokens(&c.collections_level1, &bogus, &[0, 0, 0])?;
    let ct = {
        let iv = crate::crypto::random(crate::crypto::IV_LEN)?;
        crate::crypto::Algorithm::Fle2v2Aead.encrypt(&iv, Some(user_key_id), user_key, value)?
    };
    let mut v = user_key_id.to_vec();
    v.extend_from_slice(&ct);
    // b: the token sets
    let sets = str_encode(spec)?;
    let l1 = level1(index_key)?;
    let exact = token_set(&l1, Kind::Exact, &bson_string(sets.exact()), k as u64, sets.msize)?;
    let mut arrays: [Vec<Vec<u8>>; 3] = Default::default();
    if let Some(set) = &sets.substring {
        for (s, n) in sets.substrings(set) {
            let ts = token_set(&l1, Kind::Substring, &bson_string(s), k as u64, 0)?;
            arrays[0].extend(std::iter::repeat_n(ts, n as usize));
        }
    }
    if let Some(set) = &sets.suffix {
        for (s, n) in sets.affixes(set) {
            let ts = token_set(&l1, Kind::Suffix, &bson_string(s), k as u64, 0)?;
            arrays[1].extend(std::iter::repeat_n(ts, n as usize));
        }
    }
    if let Some(set) = &sets.prefix {
        for (s, n) in sets.affixes(set) {
            let ts = token_set(&l1, Kind::Prefix, &bson_string(s), k as u64, 0)?;
            arrays[2].extend(std::iter::repeat_n(ts, n as usize));
        }
    }
    let mut w = Writer::with_capacity(1024);
    w.binary(b"d", 0, &bogus);
    w.binary(b"s", 0, &bogus);
    w.binary(b"p", 0, &p);
    w.binary(b"u", 4, index_key_id);
    w.int32(b"t", 2);
    w.binary(b"v", 0, &v);
    w.binary(b"e", 0, &c.server_data_encryption_level1);
    w.binary(b"l", 0, &bogus);
    w.int64(b"k", k);
    let b = w.begin(T_DOCUMENT, b"b");
    w.document(b"e", &exact);
    let mut idx = mongo_core::bson::writer::itoa_buf::Buf::default();
    for (name, arr) in [(&b"s"[..], &arrays[0]), (b"u", &arrays[1]), (b"p", &arrays[2])] {
        let a = w.begin(mongo_core::bson::iter::T_ARRAY, name);
        for (i, ts) in arr.iter().enumerate() {
            let key = mongo_core::bson::writer::index_key(i, &mut idx).to_vec();
            w.document(&key, ts);
        }
        w.end(a);
    }
    w.end(b);
    Ok(w.finish())
}

/// `_mongocrypt_fle2_placeholder_to_find_ciphertextForTextSearch`: the FLE2FindTextPayload
/// (without the subtype byte)
pub fn find_payload(spec: &InsertSpec, index_key: &[u8], max_contention: i64) -> Result<Vec<u8>> {
    let count = spec.substr.is_some() as u8 + spec.suffix.is_some() as u8 + spec.prefix.is_some() as u8;
    if count > 1 {
        return Err("Text search query specification cannot contain multiple query type specifications".into());
    }
    let folded = bson_string(&str_query(spec)?);
    let l1 = level1(index_key)?;
    let (kind, name) = if spec.substr.is_some() {
        (Kind::Substring, &b"s"[..])
    } else if spec.suffix.is_some() {
        (Kind::Suffix, &b"u"[..])
    } else if spec.prefix.is_some() {
        (Kind::Prefix, &b"p"[..])
    } else {
        (Kind::Exact, &b"e"[..])
    };
    let (d, s, l) = text_tokens(&l1, kind, &folded, None);
    let mut w = Writer::with_capacity(256);
    let ts = w.begin(T_DOCUMENT, b"ts");
    let t = w.begin(T_DOCUMENT, name);
    w.binary(b"d", 0, &d);
    w.binary(b"s", 0, &s);
    w.binary(b"l", 0, &l);
    w.end(t);
    w.end(ts);
    w.int64(b"cm", max_contention);
    w.bool(b"cf", spec.casef);
    w.bool(b"df", spec.diacf);
    let spec_doc = |w: &mut Writer, name: &[u8], a: &AffixSpec, mlen: bool| {
        let p = w.begin(T_DOCUMENT, name);
        if mlen {
            w.int32(b"mlen", a.mlen as i32);
        }
        w.int32(b"ub", a.ub as i32);
        w.int32(b"lb", a.lb as i32);
        w.end(p);
    };
    if let Some(a) = &spec.substr {
        spec_doc(&mut w, b"ss", a, true);
    } else if let Some(a) = &spec.suffix {
        spec_doc(&mut w, b"fs", a, false);
    } else if let Some(a) = &spec.prefix {
        spec_doc(&mut w, b"ps", a, false);
    }
    Ok(w.finish())
}
