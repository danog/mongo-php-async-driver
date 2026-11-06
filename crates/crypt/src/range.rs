//! Queryable Encryption range indexes (libmongocrypt's `mc-rangeopts.c`, `mc-range-encoding.c`,
//! `mc-range-edge-generation.c`): range options, the order-preserving encodings of int32, int64,
//! date and double values, and edge generation. Decimal128 is not supported yet (user decision).

use crate::fle2::{common, encrypted_tokens, token_key};
use mongo_core::bson::fmt::fmt_g;
use mongo_core::bson::iter::{Iter, Value, T_DATE_TIME, T_DECIMAL128, T_DOUBLE, T_INT32, T_INT64};
use mongo_core::bson::writer::Writer;

pub type Result<T> = std::result::Result<T, String>;

pub const SPARSITY_DEFAULT: i64 = 2;
const TRIM_FACTOR_DEFAULT: i32 = 6;

/// `mc_bson_type_to_string`
pub fn type_name(t: u8) -> &'static str {
    match t {
        0 => "EOD",
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

/// `%g`
fn g(v: f64) -> String {
    fmt_g(v, 6)
}

/// `mc_validate_sparsity`
pub fn validate_sparsity(s: i64) -> Result<()> {
    if s < 0 {
        return Err(format!("Error validating sparsity: sparsity must be non-negative, got: {s}"));
    }
    Ok(())
}

/// A BSON value kept as its `{"": value}` element: type and bytes
#[derive(Clone, Debug)]
pub struct Val {
    pub t: u8,
    pub elem: Vec<u8>,
}

impl Val {
    fn from_iter(it: &Iter) -> Val {
        let mut w = Writer::with_capacity(32);
        w.element_from(Some(b""), it);
        Val { t: it.value().type_code(), elem: w.finish() }
    }
    fn value(&self) -> Value<'_> {
        let mut it = Iter::new(&self.elem).expect("built by from_iter");
        it.next();
        it.value()
    }
    fn i32(&self) -> i32 {
        match self.value() {
            Value::Int32(v) => v,
            _ => 0,
        }
    }
    fn i64(&self) -> i64 {
        match self.value() {
            Value::Int64(v) | Value::DateTime(v) => v,
            _ => 0,
        }
    }
    fn f64(&self) -> f64 {
        match self.value() {
            Value::Double(v) => v,
            _ => 0.0,
        }
    }
    fn append(&self, w: &mut Writer, key: &[u8]) {
        let mut it = Iter::new(&self.elem).expect("built by from_iter");
        it.next();
        w.element_from(Some(key), &it);
    }
}

/// `mc_RangeOpts_t`
#[derive(Clone, Debug, Default)]
pub struct RangeOpts {
    pub min: Option<Val>,
    pub max: Option<Val>,
    pub sparsity: i64,
    pub precision: Option<i32>,
    pub trim_factor: Option<i32>,
}

impl RangeOpts {
    /// `mc_RangeOpts_parse`
    pub fn parse(doc: &[u8]) -> Result<RangeOpts> {
        const P: &str = "Error parsing RangeOpts: ";
        let mut ro = RangeOpts { sparsity: SPARSITY_DEFAULT, ..Default::default() };
        let mut it = Iter::new(doc).ok_or(format!("{P}Invalid BSON"))?;
        let mut seen: Vec<&'static str> = Vec::new();
        let mut dup = |name: &'static str| -> Result<()> {
            if seen.contains(&name) {
                return Err(format!("{P}Unexpected duplicate field '{name}'"));
            }
            seen.push(name);
            Ok(())
        };
        while it.next() {
            match it.key() {
                b"min" => {
                    dup("min")?;
                    ro.min = Some(Val::from_iter(&it));
                }
                b"max" => {
                    dup("max")?;
                    ro.max = Some(Val::from_iter(&it));
                }
                b"sparsity" => {
                    dup("sparsity")?;
                    match it.value() {
                        Value::Int64(s) => {
                            validate_sparsity(s)?;
                            ro.sparsity = s;
                        }
                        v => return Err(format!("{P}Expected int64 for sparsity, got: {}", type_name(v.type_code()))),
                    }
                }
                b"precision" => {
                    dup("precision")?;
                    match it.value() {
                        Value::Int32(p) if p >= 0 => ro.precision = Some(p),
                        Value::Int32(_) => return Err(format!("{P}'precision' must be non-negative")),
                        _ => return Err(format!("{P}'precision' must be an int32")),
                    }
                }
                b"trimFactor" => {
                    dup("trimFactor")?;
                    match it.value() {
                        Value::Int32(t) if t >= 0 => ro.trim_factor = Some(t),
                        Value::Int32(_) => return Err(format!("{P}'trimFactor' must be non-negative")),
                        v => return Err(format!("{P}Expected int32 for trimFactor, got: {}", type_name(v.type_code()))),
                    }
                }
                k => return Err(format!("{P}Unrecognized field: '{}'", String::from_utf8_lossy(k))),
            }
        }
        let fp = |t: u8| t == T_DOUBLE || t == T_DECIMAL128;
        if ro.precision.is_some() {
            let min = ro.min.as_ref().ok_or(format!("{P}setting precision requires min"))?;
            if !fp(min.t) {
                return Err(format!("{P}expected 'precision' to be set with double or decimal128 index, but got: {} min", type_name(min.t)));
            }
            let max = ro.max.as_ref().ok_or(format!("{P}setting precision requires max"))?;
            if !fp(max.t) {
                return Err(format!("{P}expected 'precision' to be set with double or decimal128 index, but got: {} max", type_name(max.t)));
            }
        }
        if let (Some(a), Some(b)) = (&ro.min, &ro.max) {
            if a.t != b.t {
                return Err(format!("{P}expected 'min' and 'max' to be same type, but got: {} min and {} max", type_name(a.t), type_name(b.t)));
            }
        }
        for (v, name) in [(&ro.min, "min"), (&ro.max, "max")] {
            if let Some(v) = v {
                if fp(v.t) && ro.precision.is_none() {
                    return Err(format!("{P}expected 'precision' to be set with '{name}' for {}", type_name(v.t)));
                }
            }
        }
        Ok(ro)
    }

    /// `mc_RangeOpts_to_FLE2RangeInsertSpec` (+ appendMin/appendMax/appendTrimFactor)
    pub fn insert_spec(&self, v: &Val) -> Result<InsertSpec> {
        let bound = |o: &Option<Val>, which: &str, dflt: f64| -> Result<Val> {
            let p = format!("Error appending {which} to FLE2RangeInsertSpec: ");
            if let Some(b) = o {
                if b.t != v.t {
                    return Err(format!(
                        "{p}expected matching '{which}' and value type. Got range option '{which}' of type {} and value of type {}",
                        type_name(b.t),
                        type_name(v.t)
                    ));
                }
                return Ok(b.clone());
            }
            match v.t {
                T_INT32 | T_INT64 | T_DATE_TIME => Err(format!("{p}Range option '{which}' is required for type: {}", type_name(v.t))),
                T_DOUBLE => {
                    let mut w = Writer::with_capacity(16);
                    w.double(b"", dflt);
                    Ok(Val { t: T_DOUBLE, elem: w.finish() })
                }
                T_DECIMAL128 => Err(format!("{p}unsupported BSON type (Decimal128) for range: libmongocrypt was built without extended Decimal128 support")),
                t => Err(format!("{p}unsupported BSON type: {} for range", type_name(t))),
            }
        };
        let min = bound(&self.min, "min", -f64::MAX)?;
        let max = bound(&self.max, "max", f64::MAX)?;
        if let Some(tf) = self.trim_factor {
            let nbits = self.number_of_bits(v.t)?;
            let test = if nbits == 0 { 1 } else { nbits };
            if tf as i64 >= test as i64 {
                return Err(format!(
                    "Error appending trim factor to FLE2RangeInsertSpec: Trim factor ({tf}) must be less than the total number of bits ({nbits}) used to represent any element in the domain."
                ));
            }
        }
        Ok(InsertSpec { v: v.clone(), min, max, precision: self.precision, trim_factor: self.trim_factor })
    }

    /// `mc_getNumberOfBits`
    fn number_of_bits(&self, t: u8) -> Result<u32> {
        const P: &str = "Error in getNumberOfBits: ";
        let r = match t {
            T_INT32 => {
                let (v, b) = match (&self.min, &self.max) {
                    (Some(a), Some(b)) => (a.i32(), Some((a.i32(), b.i32()))),
                    _ => (0, None),
                };
                32 - type_info32(v, b)?.max.leading_zeros()
            }
            T_INT64 | T_DATE_TIME => {
                let (v, b) = match (&self.min, &self.max) {
                    (Some(a), Some(b)) => (a.i64(), Some((a.i64(), b.i64()))),
                    _ => (0, None),
                };
                64 - type_info64(v, b)?.max.leading_zeros()
            }
            T_DOUBLE => {
                let (v, b) = match (&self.min, &self.max) {
                    (Some(a), Some(b)) => (a.f64(), Some((a.f64(), b.f64()))),
                    _ => (0.0, None),
                };
                64 - type_info_double(v, b, self.precision)?.max.leading_zeros()
            }
            t => return Err(format!("{P}unsupported BSON type: {} for range", type_name(t))),
        };
        Ok(r)
    }
}

/// `mc_FLE2RangeInsertSpec_t`
pub struct InsertSpec {
    pub v: Val,
    pub min: Val,
    pub max: Val,
    pub precision: Option<i32>,
    pub trim_factor: Option<i32>,
}

pub struct Ost<T> {
    pub value: T,
    pub max: T,
}

fn encode_i32(v: i32) -> u32 {
    (v as u32) ^ (1 << 31)
}

fn encode_i64(v: i64) -> u64 {
    (v as u64) ^ (1 << 63)
}

/// `mc_getTypeInfo32`
pub fn type_info32(value: i32, bounds: Option<(i32, i32)>) -> Result<Ost<u32>> {
    let Some((min, max)) = bounds else { return Ok(Ost { value: encode_i32(value), max: u32::MAX }) };
    if min >= max {
        return Err(format!("The minimum value must be less than the maximum value, got min: {min}, max: {max}"));
    }
    if value > max || value < min {
        return Err(format!(
            "Value must be greater than or equal to the minimum value and less than or equal to the maximum value, got min: {min}, max: {max}, value: {value}"
        ));
    }
    let m = encode_i32(min);
    Ok(Ost { value: encode_i32(value) - m, max: encode_i32(max) - m })
}

/// `mc_getTypeInfo64`
pub fn type_info64(value: i64, bounds: Option<(i64, i64)>) -> Result<Ost<u64>> {
    let Some((min, max)) = bounds else { return Ok(Ost { value: encode_i64(value), max: u64::MAX }) };
    if min >= max {
        return Err(format!("The minimum value must be less than the maximum value, got min: {min}, max: {max}"));
    }
    if value > max || value < min {
        return Err(format!(
            "Value must be greater than or equal to the minimum value and less than or equal to the maximum value, got min: {min}, max: {max}, value: {value}"
        ));
    }
    let m = encode_i64(min);
    Ok(Ost { value: encode_i64(value) - m, max: encode_i64(max) - m })
}

const SCALED_DOUBLE_BOUNDS: f64 = 9007199254740992.0;

/// `mc_canUsePrecisionModeDouble`: Ok(Some(bits)) when usable, Ok(None) when not (≥ 53 bits)
fn can_use_precision_mode(min: f64, max: f64, precision: i32) -> Result<Option<u32>> {
    if min >= max {
        return Err(format!("Invalid bounds for double range precision, min must be less than max. min: {}, max: {}", g(min), g(max)));
    }
    let scaled_prc = 10f64.powi(precision);
    let smax = max * scaled_prc;
    let smin = min * scaled_prc;
    if smax != smax.trunc() {
        return Err(format!("Invalid upper bound for double precision. Fractional digits must be less than the specified precision value. max: {}", g(max)));
    }
    if smin != smin.trunc() {
        return Err(format!("Invalid lower bound for double precision. Fractional digits must be less than the specified precision value. min: {}", g(min)));
    }
    if smax.abs() >= SCALED_DOUBLE_BOUNDS {
        return Err(format!(
            "Invalid upper bound for double precision. Absolute scaled value of max must be less than {}. max: {}",
            g(SCALED_DOUBLE_BOUNDS),
            g(max)
        ));
    }
    if smin.abs() >= SCALED_DOUBLE_BOUNDS {
        return Err(format!(
            "Invalid lower bound for double precision. Absolute scaled value of min must be less than {}. min: {}",
            g(SCALED_DOUBLE_BOUNDS),
            g(min)
        ));
    }
    let t1 = smax - smin;
    let t4 = u64::MAX as f64 - t1;
    let t5 = t4.log10().floor() - 1.0;
    if precision as f64 > t5 {
        return Err(format!("Invalid value for precision. precision: {precision}"));
    }
    let (i1, i2) = (smax as i64, smin as i64);
    let range = i1.wrapping_sub(i2) as u64;
    if (scaled_prc as u64) > u64::MAX - range {
        return Err(format!(
            "Invalid value for min, max, and precision. The calculated domain size is too large. min: {}, max: {}, precision: {precision}",
            g(min),
            g(max)
        ));
    }
    let i3 = range + scaled_prc as u64;
    let clz = i3.leading_zeros();
    let bits = if i3 & (i3 - 1) == 0 { 64 - clz - 1 } else { 64 - clz };
    Ok((bits < 53).then_some(bits))
}

/// `mc_getTypeInfoDouble`
pub fn type_info_double(mut value: f64, bounds: Option<(f64, f64)>, precision: Option<i32>) -> Result<Ost<u64>> {
    if bounds.is_some() != precision.is_some() {
        return Err("min, max, and precision must all be set or must all be unset".into());
    }
    if value.is_infinite() || value.is_nan() {
        return Err("Infinity and NaN double values are not supported.".into());
    }
    if let Some((min, max)) = bounds {
        if min >= max {
            return Err(format!("The minimum value must be less than the maximum value, got min: {}, max: {}", g(min), g(max)));
        }
        if value > max || value < min {
            return Err(format!(
                "Value must be greater than or equal to the minimum value and less than or equal to the maximum value, got min: {}, max: {}, value: {}",
                g(min),
                g(max),
                g(value)
            ));
        }
    }
    let is_neg = value < 0.0;
    if value == 0.0 {
        value = 0.0;
    }
    if let (Some((min, max)), Some(p)) = (bounds, precision) {
        match can_use_precision_mode(min, max, p)? {
            None => {
                return Err(format!(
                    "The domain of double values specified by the min, max, and precision cannot be represented in fewer than 53 bits. min: {}, max: {}, precision: {p}",
                    g(min),
                    g(max)
                ))
            }
            Some(bits) => {
                let v_prime = (value * 10f64.powi(p)).trunc() as i64;
                let scaled_min = (min * 10f64.powi(p)) as i64;
                return Ok(Ost { value: v_prime.wrapping_sub(scaled_min) as u64, max: (1u64 << bits) - 1 });
            }
        }
    }
    value *= -1.0;
    let mut uv = value.to_bits();
    if is_neg {
        uv = (1u64 << 63).wrapping_sub(uv);
    }
    Ok(Ost { value: uv, max: u64::MAX })
}

/// `mc_edges_t`: the edge strings and the trim factor used
pub struct Edges {
    pub edges: Vec<String>,
    pub leaf: String,
    pub used_trim_factor: i32,
}

/// `trimFactorDefault`
fn trim_factor_default(maxlen: usize, tf: Option<i32>) -> i32 {
    if let Some(t) = tf {
        return t;
    }
    if TRIM_FACTOR_DEFAULT as i128 > maxlen as i128 - 1 && maxlen > 0 {
        (maxlen - 1) as i32
    } else {
        TRIM_FACTOR_DEFAULT
    }
}

/// `mc_edges_new`
fn edges_new(leaf: &str, sparsity: usize, tf: Option<i32>) -> Result<Edges> {
    if sparsity < 1 {
        return Err("sparsity must be 1 or larger".into());
    }
    let leaf_len = leaf.len();
    let trim = trim_factor_default(leaf_len, tf);
    if trim != 0 && trim as i64 >= leaf_len as i64 {
        return Err(format!(
            "trimFactor must be less than the number of bits ({leaf_len}) used to represent an element of the domain, but got {trim}"
        ));
    }
    if trim < 0 {
        return Err(format!("trimFactor must be >= 0, but got {trim}"));
    }
    let mut edges = Vec::new();
    if trim == 0 {
        edges.push("root".to_string());
    }
    edges.push(leaf.to_string());
    let start = if trim > 0 { trim as usize } else { 1 };
    for i in start..leaf_len {
        if i % sparsity == 0 {
            edges.push(leaf[..i].to_string());
        }
    }
    Ok(Edges { edges, leaf: leaf.to_string(), used_trim_factor: trim })
}

fn bits(v: u64, width: u32, max_leading_zeros: u32) -> String {
    let s = format!("{v:0w$b}", w = width as usize);
    s[max_leading_zeros as usize..].to_string()
}

/// `get_edges` (marking): the edges of an insert spec
pub fn get_edges(spec: &InsertSpec, sparsity: usize) -> Result<Edges> {
    match spec.v.t {
        T_INT32 => {
            let o = type_info32(spec.v.i32(), Some((spec.min.i32(), spec.max.i32())))?;
            edges_new(&bits(o.value as u64, 32, o.max.leading_zeros()), sparsity, spec.trim_factor)
        }
        T_INT64 | T_DATE_TIME => {
            let o = type_info64(spec.v.i64(), Some((spec.min.i64(), spec.max.i64())))?;
            edges_new(&bits(o.value, 64, o.max.leading_zeros()), sparsity, spec.trim_factor)
        }
        T_DOUBLE => {
            let b = spec.precision.map(|_| (spec.min.f64(), spec.max.f64()));
            let o = type_info_double(spec.v.f64(), b, spec.precision)?;
            edges_new(&bits(o.value, 64, o.max.leading_zeros()), sparsity, spec.trim_factor)
        }
        T_DECIMAL128 => Err("unsupported BSON type (Decimal128) for range: libmongocrypt was built without extended Decimal128 support".into()),
        t => Err(format!("unsupported BSON type: {} for range", type_name(t))),
    }
}

/// FLE2InsertUpdatePayloadV2 for a range insert (`..._to_insert_update_ciphertextForRange`)
#[allow(clippy::too_many_arguments)]
pub fn insert_payload(
    key_id: &[u8; 16],
    key: &[u8],
    spec: &InsertSpec,
    value_bytes: &[u8],
    sparsity: i64,
    contention: i64,
) -> Result<Vec<u8>> {
    let tk = token_key(key)?;
    let c = common(tk, value_bytes, Some(contention as u64));
    let p = encrypted_tokens(&c.collections_level1, &c.esc_derived, &[0])?;
    let iv = crate::crypto::random(crate::crypto::IV_LEN)?;
    let ct = crate::crypto::Algorithm::Fle2v2Aead.encrypt(&iv, Some(key_id), key, value_bytes)?;
    let mut v = key_id.to_vec();
    v.extend_from_slice(&ct);
    let edges = get_edges(spec, sparsity as usize)?;
    let mut w = Writer::with_capacity(4096);
    w.binary(b"d", 0, &c.edc_derived);
    w.binary(b"s", 0, &c.esc_derived);
    w.binary(b"p", 0, &p);
    w.binary(b"u", 4, key_id);
    w.int32(b"t", spec.v.t as i32);
    w.binary(b"v", 0, &v);
    w.binary(b"e", 0, &c.server_data_encryption_level1);
    w.binary(b"l", 0, &c.server_derived_from_data);
    w.int64(b"k", contention);
    let ga = w.begin(mongo_core::bson::iter::T_ARRAY, b"g");
    for (i, e) in edges.edges.iter().enumerate() {
        let ec = common(tk, e.as_bytes(), Some(contention as u64));
        let leaf = [u8::from(*e == edges.leaf)];
        let ep = encrypted_tokens(&ec.collections_level1, &ec.esc_derived, &leaf)?;
        let d = w.begin(mongo_core::bson::iter::T_DOCUMENT, i.to_string().as_bytes());
        w.binary(b"d", 0, &ec.edc_derived);
        w.binary(b"s", 0, &ec.esc_derived);
        w.binary(b"l", 0, &ec.server_derived_from_data);
        w.binary(b"p", 0, &ep);
        w.end(d);
    }
    w.end(ga);
    w.int64(b"sp", sparsity);
    if let Some(p) = spec.precision {
        w.int32(b"pn", p);
    }
    w.int32(b"tf", edges.used_trim_factor);
    spec.min.append(&mut w, b"mn");
    spec.max.append(&mut w, b"mx");
    Ok(w.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edges_like_libmongocrypt() {
        // test-mc-range-edge-generation.c: value 2, min 0, max 7, sparsity 1, trimFactor 0
        let o = type_info32(2, Some((0, 7))).unwrap();
        let e = edges_new(&bits(o.value as u64, 32, o.max.leading_zeros()), 1, Some(0)).unwrap();
        assert_eq!(e.edges, ["root", "010", "0", "01"]);
        // default trim factor clamps to maxlen - 1
        let e = edges_new("010", 1, None).unwrap();
        assert_eq!(e.used_trim_factor, 2);
        assert_eq!(e.edges, ["010", "01"]);
    }

    #[test]
    fn encodings() {
        assert_eq!(type_info32(i32::MIN, None).unwrap().value, 0);
        assert_eq!(type_info32(0, None).unwrap().value, 1 << 31);
        assert_eq!(type_info64(-1, None).unwrap().value, (1u64 << 63) - 1);
        assert!(type_info_double(f64::NAN, None, None).is_err());
        let a = type_info_double(-1.5, None, None).unwrap().value;
        let b = type_info_double(-0.5, None, None).unwrap().value;
        let c = type_info_double(0.0, None, None).unwrap().value;
        let d = type_info_double(2.0, None, None).unwrap().value;
        assert!(a < b && b < c && c < d);
    }
}
