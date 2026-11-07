//! The CBOR subset of the wire (RFC 8949): unsigned and negative integers,
//! byte and text strings, arrays, maps, `false` / `true` / `null` and
//! floats. Definite lengths only; no tags. Encoding is deterministic:
//! shortest integer heads, every float as a 64-bit float, map entries in
//! the order given.
//!
//! Decoding never panics: a frame from a plugin is untrusted (a truncated
//! item, a length past the end, nesting deeper than [`MAX_DEPTH`], a tag,
//! a string that is not UTF-8 are errors).

use std::fmt;

/// How deep arrays and maps may nest (a WIT value of tmi:format nests far
/// less; a deeper item is refused rather than risking the stack).
pub const MAX_DEPTH: usize = 128;

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Uint(u64),
    /// `-1 - n`.
    Nint(u64),
    Bytes(Vec<u8>),
    Text(String),
    Array(Vec<Value>),
    Map(Vec<(Value, Value)>),
    Bool(bool),
    Null,
    Float(f64),
}

/// A CBOR item that does not decode, or a value that is not what the WIT
/// says it should be.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValueError(pub String);

impl fmt::Display for ValueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ValueError {}

fn err<T>(m: impl Into<String>) -> Result<T, ValueError> {
    Err(ValueError(m.into()))
}

// -- building ------------------------------------------------------------------

impl Value {
    pub fn int(i: i64) -> Value {
        if i >= 0 {
            Value::Uint(i as u64)
        } else {
            Value::Nint((-1 - i) as u64)
        }
    }

    pub fn uint(u: impl Into<u64>) -> Value {
        Value::Uint(u.into())
    }

    pub fn text(s: impl Into<String>) -> Value {
        Value::Text(s.into())
    }

    pub fn bytes(b: impl Into<Vec<u8>>) -> Value {
        Value::Bytes(b.into())
    }

    /// A record: text keys in the order given.
    pub fn record<const N: usize>(fields: [(&str, Value); N]) -> Value {
        Value::Map(fields.into_iter().map(|(k, v)| (Value::text(k), v)).collect())
    }

    /// A variant case with a payload: `{tag: payload}`.
    pub fn case(tag: &str, payload: Value) -> Value {
        Value::Map(vec![(Value::text(tag), payload)])
    }

    /// A variant case without a payload: `{tag: null}`.
    pub fn unit(tag: &str) -> Value {
        Value::case(tag, Value::Null)
    }

    pub fn ok(v: Value) -> Value {
        Value::case("ok", v)
    }

    pub fn err(v: Value) -> Value {
        Value::case("err", v)
    }

    pub fn option(v: Option<Value>) -> Value {
        v.unwrap_or(Value::Null)
    }

    pub fn list<T>(items: impl IntoIterator<Item = T>, f: impl FnMut(T) -> Value) -> Value {
        Value::Array(items.into_iter().map(f).collect())
    }
}

// -- reading (with the name of what is read, for the error) --------------------

impl Value {
    pub fn kind(&self) -> &'static str {
        match self {
            Value::Uint(_) | Value::Nint(_) => "an integer",
            Value::Bytes(_) => "a byte string",
            Value::Text(_) => "a text string",
            Value::Array(_) => "an array",
            Value::Map(_) => "a map",
            Value::Bool(_) => "a boolean",
            Value::Null => "null",
            Value::Float(_) => "a float",
        }
    }

    fn expected<T>(&self, what: &str, want: &str) -> Result<T, ValueError> {
        err(format!("{what}: expected {want}, got {}", self.kind()))
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// None for `null` (an `option` that is none).
    pub fn opt(&self) -> Option<&Value> {
        if self.is_null() {
            None
        } else {
            Some(self)
        }
    }

    pub fn u64(&self, what: &str) -> Result<u64, ValueError> {
        match self {
            Value::Uint(u) => Ok(*u),
            _ => self.expected(what, "an unsigned integer"),
        }
    }

    pub fn i64(&self, what: &str) -> Result<i64, ValueError> {
        match self {
            Value::Uint(u) if *u <= i64::MAX as u64 => Ok(*u as i64),
            Value::Nint(n) if *n <= i64::MAX as u64 => Ok(-1 - *n as i64),
            Value::Uint(_) | Value::Nint(_) => err(format!("{what}: {} is out of the range of s64", self.kind())),
            _ => self.expected(what, "an integer"),
        }
    }

    fn ranged<T: TryFrom<i64>>(&self, what: &str, ty: &str) -> Result<T, ValueError> {
        let i = self.i64(what)?;
        T::try_from(i).map_err(|_| ValueError(format!("{what}: {i} is out of the range of {ty}")))
    }

    pub fn u32(&self, what: &str) -> Result<u32, ValueError> {
        self.ranged(what, "u32")
    }

    pub fn u8(&self, what: &str) -> Result<u8, ValueError> {
        self.ranged(what, "u8")
    }

    pub fn i32(&self, what: &str) -> Result<i32, ValueError> {
        self.ranged(what, "s32")
    }

    pub fn i8(&self, what: &str) -> Result<i8, ValueError> {
        self.ranged(what, "s8")
    }

    /// A float, or an integer (Python writes `1` for `1.0`).
    pub fn f64(&self, what: &str) -> Result<f64, ValueError> {
        match self {
            Value::Float(f) => Ok(*f),
            Value::Uint(u) => Ok(*u as f64),
            Value::Nint(n) => Ok(-1.0 - *n as f64),
            _ => self.expected(what, "a float"),
        }
    }

    pub fn bool(&self, what: &str) -> Result<bool, ValueError> {
        match self {
            Value::Bool(b) => Ok(*b),
            _ => self.expected(what, "a boolean"),
        }
    }

    pub fn str(&self, what: &str) -> Result<&str, ValueError> {
        match self {
            Value::Text(s) => Ok(s),
            _ => self.expected(what, "a text string"),
        }
    }

    pub fn string(&self, what: &str) -> Result<String, ValueError> {
        self.str(what).map(str::to_string)
    }

    pub fn byte_slice(&self, what: &str) -> Result<&[u8], ValueError> {
        match self {
            Value::Bytes(b) => Ok(b),
            _ => self.expected(what, "a byte string"),
        }
    }

    pub fn byte_vec(&self, what: &str) -> Result<Vec<u8>, ValueError> {
        self.byte_slice(what).map(<[u8]>::to_vec)
    }

    pub fn array(&self, what: &str) -> Result<&[Value], ValueError> {
        match self {
            Value::Array(a) => Ok(a),
            _ => self.expected(what, "an array"),
        }
    }

    /// An array of exactly `n` items (a tuple).
    pub fn tuple(&self, what: &str, n: usize) -> Result<&[Value], ValueError> {
        let a = self.array(what)?;
        if a.len() != n {
            return err(format!("{what}: expected {n} items, got {}", a.len()));
        }
        Ok(a)
    }

    pub fn map(&self, what: &str) -> Result<&[(Value, Value)], ValueError> {
        match self {
            Value::Map(m) => Ok(m),
            _ => self.expected(what, "a map"),
        }
    }

    /// The value under text key `key` of a map, if any.
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Map(m) => m.iter().find(|(k, _)| matches!(k, Value::Text(t) if t == key)).map(|(_, v)| v),
            _ => None,
        }
    }

    /// Record field `key` (it must be there).
    pub fn field(&self, what: &str, key: &str) -> Result<&Value, ValueError> {
        self.map(what)?;
        self.get(key).ok_or_else(|| ValueError(format!("{what}: no field {key:?}")))
    }

    /// A variant: its case name and payload (`{tag: payload}`).
    pub fn case_of(&self, what: &str) -> Result<(&str, &Value), ValueError> {
        match self {
            Value::Map(m) if m.len() == 1 => match &m[0] {
                (Value::Text(t), v) => Ok((t, v)),
                _ => err(format!("{what}: a variant's case name is a text string")),
            },
            Value::Map(m) => err(format!("{what}: a variant is a map of one entry, not {}", m.len())),
            _ => self.expected(what, "a variant (a map of one entry)"),
        }
    }

    /// An enum: its case name.
    pub fn enum_name(&self, what: &str) -> Result<&str, ValueError> {
        self.str(what)
    }

    /// `result<T, E>`: `Ok(payload)` or `Err(payload)`.
    pub fn result(&self, what: &str) -> Result<Result<&Value, &Value>, ValueError> {
        match self.case_of(what)? {
            ("ok", v) => Ok(Ok(v)),
            ("err", v) => Ok(Err(v)),
            (t, _) => err(format!("{what}: a result is ok or err, not {t:?}")),
        }
    }
}

// -- encoding --------------------------------------------------------------------

fn head(out: &mut Vec<u8>, major: u8, n: u64) {
    let m = major << 5;
    if n < 24 {
        out.push(m | n as u8);
    } else if n <= u8::MAX as u64 {
        out.push(m | 24);
        out.push(n as u8);
    } else if n <= u16::MAX as u64 {
        out.push(m | 25);
        out.extend_from_slice(&(n as u16).to_be_bytes());
    } else if n <= u32::MAX as u64 {
        out.push(m | 26);
        out.extend_from_slice(&(n as u32).to_be_bytes());
    } else {
        out.push(m | 27);
        out.extend_from_slice(&n.to_be_bytes());
    }
}

impl Value {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode_into(&mut out);
        out
    }

    pub fn encode_into(&self, out: &mut Vec<u8>) {
        match self {
            Value::Uint(u) => head(out, 0, *u),
            Value::Nint(n) => head(out, 1, *n),
            Value::Bytes(b) => {
                head(out, 2, b.len() as u64);
                out.extend_from_slice(b);
            }
            Value::Text(s) => {
                head(out, 3, s.len() as u64);
                out.extend_from_slice(s.as_bytes());
            }
            Value::Array(a) => {
                head(out, 4, a.len() as u64);
                for v in a {
                    v.encode_into(out);
                }
            }
            Value::Map(m) => {
                head(out, 5, m.len() as u64);
                for (k, v) in m {
                    k.encode_into(out);
                    v.encode_into(out);
                }
            }
            Value::Bool(false) => out.push(0xf4),
            Value::Bool(true) => out.push(0xf5),
            Value::Null => out.push(0xf6),
            Value::Float(f) => {
                out.push(0xfb);
                out.extend_from_slice(&f.to_bits().to_be_bytes());
            }
        }
    }
}

// -- the canonical form ----------------------------------------------------------

/// `f` as an IEEE half, if that holds it exactly.
fn to_half(f: f64) -> Option<u16> {
    let f32v = f as f32;
    if f64::from(f32v) != f {
        return None;
    }
    let bits = f32v.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exp = ((bits >> 23) & 0xff) as i32;
    let mant = bits & 0x7f_ffff;
    if exp == 0xff {
        return if mant == 0 { Some(sign | 0x7c00) } else { None };
    }
    if exp == 0 && mant == 0 {
        return Some(sign);
    }
    let e = exp - 127;
    if (-14..=15).contains(&e) {
        // a normal half: 10 mantissa bits
        if mant & 0x1fff != 0 {
            return None;
        }
        return Some(sign | (((e + 15) as u16) << 10) | (mant >> 13) as u16);
    }
    if (-24..-14).contains(&e) {
        // a subnormal half
        let full = mant | 0x80_0000;
        let shift = (-14 - e) + 13;
        if full & ((1 << shift) - 1) != 0 {
            return None;
        }
        return Some(sign | (full >> shift) as u16);
    }
    None
}

impl Value {
    /// The canonical encoding (RFC 8949 4.2.1, "core deterministic
    /// encoding"): shortest integer heads, each float in the shortest of
    /// half, single and double precision that holds it exactly (NaN is the
    /// half 0x7e00), map entries sorted by their keys' encodings. Two values
    /// are the same WIT value iff their canonical encodings are equal: `tmi
    /// plugin test` compares in it (PLUGINS.md 7.1). The wire itself
    /// ([`Value::encode`]) keeps the order given and 64-bit floats.
    pub fn canonical(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.canonical_into(&mut out);
        out
    }

    fn canonical_into(&self, out: &mut Vec<u8>) {
        match self {
            Value::Float(f) => {
                if f.is_nan() {
                    out.extend_from_slice(&[0xf9, 0x7e, 0x00]);
                } else if let Some(h) = to_half(*f) {
                    out.push(0xf9);
                    out.extend_from_slice(&h.to_be_bytes());
                } else if f64::from(*f as f32) == *f {
                    out.push(0xfa);
                    out.extend_from_slice(&(*f as f32).to_be_bytes());
                } else {
                    out.push(0xfb);
                    out.extend_from_slice(&f.to_be_bytes());
                }
            }
            Value::Array(a) => {
                head(out, 4, a.len() as u64);
                for v in a {
                    v.canonical_into(out);
                }
            }
            Value::Map(m) => {
                let mut entries: Vec<(Vec<u8>, Vec<u8>)> = m.iter().map(|(k, v)| (k.canonical(), v.canonical())).collect();
                entries.sort_by(|a, b| a.0.cmp(&b.0));
                head(out, 5, entries.len() as u64);
                for (k, v) in entries {
                    out.extend_from_slice(&k);
                    out.extend_from_slice(&v);
                }
            }
            other => other.encode_into(out),
        }
    }
}

// -- decoding --------------------------------------------------------------------

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn byte(&mut self) -> Result<u8, ValueError> {
        let b = *self.data.get(self.pos).ok_or_else(|| ValueError("the CBOR item ends early".into()))?;
        self.pos += 1;
        Ok(b)
    }

    fn take(&mut self, n: u64) -> Result<&[u8], ValueError> {
        let left = (self.data.len() - self.pos) as u64;
        if n > left {
            return err(format!("the CBOR item says {n} bytes follow, but {left} do"));
        }
        let s = &self.data[self.pos..self.pos + n as usize];
        self.pos += n as usize;
        Ok(s)
    }

    fn arg(&mut self, info: u8) -> Result<u64, ValueError> {
        Ok(match info {
            0..=23 => info as u64,
            24 => self.byte()? as u64,
            25 => u16::from_be_bytes(self.take(2)?.try_into().unwrap()) as u64,
            26 => u32::from_be_bytes(self.take(4)?.try_into().unwrap()) as u64,
            27 => u64::from_be_bytes(self.take(8)?.try_into().unwrap()),
            31 => return err("indefinite-length CBOR items are not used"),
            _ => return err(format!("CBOR additional information {info} is reserved")),
        })
    }

    /// A count of items, each taking at least one byte.
    fn count(&mut self, info: u8) -> Result<usize, ValueError> {
        let n = self.arg(info)?;
        let left = (self.data.len() - self.pos) as u64;
        if n > left {
            return err(format!("the CBOR item says {n} items follow, but only {left} bytes do"));
        }
        Ok(n as usize)
    }

    fn value(&mut self, depth: usize) -> Result<Value, ValueError> {
        if depth > MAX_DEPTH {
            return err(format!("the CBOR item nests deeper than {MAX_DEPTH}"));
        }
        let b = self.byte()?;
        let (major, info) = (b >> 5, b & 0x1f);
        Ok(match major {
            0 => Value::Uint(self.arg(info)?),
            1 => Value::Nint(self.arg(info)?),
            2 => {
                let n = self.arg(info)?;
                Value::Bytes(self.take(n)?.to_vec())
            }
            3 => {
                let n = self.arg(info)?;
                let s = self.take(n)?;
                Value::Text(String::from_utf8(s.to_vec()).map_err(|_| ValueError("a CBOR text string is not UTF-8".into()))?)
            }
            4 => {
                let n = self.count(info)?;
                let mut a = Vec::with_capacity(n);
                for _ in 0..n {
                    a.push(self.value(depth + 1)?);
                }
                Value::Array(a)
            }
            5 => {
                let n = self.count(info)?;
                let mut m = Vec::with_capacity(n);
                for _ in 0..n {
                    let k = self.value(depth + 1)?;
                    let v = self.value(depth + 1)?;
                    m.push((k, v));
                }
                Value::Map(m)
            }
            6 => return err("CBOR tags are not used"),
            _ => match info {
                20 => Value::Bool(false),
                21 => Value::Bool(true),
                22 => Value::Null,
                25 => Value::Float(half(u16::from_be_bytes(self.take(2)?.try_into().unwrap()))),
                26 => Value::Float(f32::from_bits(u32::from_be_bytes(self.take(4)?.try_into().unwrap())) as f64),
                27 => Value::Float(f64::from_bits(u64::from_be_bytes(self.take(8)?.try_into().unwrap()))),
                _ => return err(format!("CBOR simple value {info} is not used")),
            },
        })
    }
}

/// An IEEE 754 half-precision float.
fn half(h: u16) -> f64 {
    let sign = if h & 0x8000 != 0 { -1.0 } else { 1.0 };
    let exp = ((h >> 10) & 0x1f) as i32;
    let frac = (h & 0x3ff) as f64;
    sign * match exp {
        0 => frac * 2f64.powi(-24),
        31 if frac == 0.0 => f64::INFINITY,
        31 => f64::NAN,
        e => (1.0 + frac / 1024.0) * 2f64.powi(e - 15),
    }
}

impl Value {
    /// Exactly one item, nothing after it.
    pub fn decode(data: &[u8]) -> Result<Value, ValueError> {
        let mut r = Reader { data, pos: 0 };
        let v = r.value(0)?;
        if r.pos != data.len() {
            return err(format!("{} bytes after the CBOR item", data.len() - r.pos));
        }
        Ok(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    #[test]
    fn encodes_as_rfc_8949_says() {
        // RFC 8949 appendix A
        assert_eq!(hex(&Value::Uint(0).encode()), "00");
        assert_eq!(hex(&Value::Uint(23).encode()), "17");
        assert_eq!(hex(&Value::Uint(24).encode()), "1818");
        assert_eq!(hex(&Value::Uint(1000).encode()), "1903e8");
        assert_eq!(hex(&Value::Uint(1000000).encode()), "1a000f4240");
        assert_eq!(hex(&Value::Uint(u64::MAX).encode()), "1bffffffffffffffff");
        assert_eq!(hex(&Value::int(-1).encode()), "20");
        assert_eq!(hex(&Value::int(-1000).encode()), "3903e7");
        assert_eq!(hex(&Value::Float(1.1).encode()), "fb3ff199999999999a");
        assert_eq!(hex(&Value::text("IETF").encode()), "6449455446");
        assert_eq!(hex(&Value::bytes(vec![1, 2, 3, 4]).encode()), "4401020304");
        assert_eq!(hex(&Value::Array(vec![Value::Uint(1), Value::Array(vec![Value::Uint(2), Value::Uint(3)])]).encode()), "8201820203");
        assert_eq!(hex(&Value::record([("a", Value::Uint(1))]).encode()), "a1616101");
        assert_eq!(hex(&Value::Null.encode()), "f6");
        assert_eq!(hex(&Value::Bool(true).encode()), "f5");
    }

    #[test]
    fn the_canonical_form_is_rfc_8949_4_2_1() {
        let c = |v: &Value| hex(&v.canonical());
        // RFC 8949 appendix A
        assert_eq!(c(&Value::Float(0.0)), "f90000");
        assert_eq!(c(&Value::Float(-0.0)), "f98000");
        assert_eq!(c(&Value::Float(1.5)), "f93e00");
        assert_eq!(c(&Value::Float(65504.0)), "f97bff");
        assert_eq!(c(&Value::Float(100000.0)), "fa47c35000");
        assert_eq!(c(&Value::Float(1.1)), "fb3ff199999999999a");
        assert_eq!(c(&Value::Float(5.960464477539063e-8)), "f90001");
        assert_eq!(c(&Value::Float(f64::INFINITY)), "f97c00");
        assert_eq!(c(&Value::Float(f64::NAN)), "f97e00");
        assert_eq!(c(&Value::int(-1000)), "3903e7");
        // keys sorted by their encoding: shorter first, then bytewise
        let m = Value::Map(vec![
            (Value::text("bb"), Value::Uint(2)),
            (Value::text("a"), Value::Uint(1)),
            (Value::text("b"), Value::Array(vec![Value::Float(1.0)])),
        ]);
        assert_eq!(c(&m), "a3616101616281f93c0062626202");
        // the wire keeps the order and the 64-bit floats
        assert_eq!(hex(&m.encode()), "a362626202616101616281fb3ff0000000000000");
    }

    #[test]
    fn decodes_what_it_encodes_and_other_float_widths() {
        let v = Value::record([
            ("n", Value::int(i64::MIN)),
            ("u", Value::Uint(u64::MAX)),
            ("f", Value::Float(-0.5)),
            ("b", Value::bytes(vec![0; 300])),
            ("t", Value::text("名前")),
            ("l", Value::Array(vec![Value::Null, Value::Bool(false)])),
        ]);
        assert_eq!(Value::decode(&v.encode()).unwrap(), v);
        assert_eq!(Value::decode(&[0xf9, 0x3c, 0x00]).unwrap(), Value::Float(1.0));
        assert_eq!(Value::decode(&[0xf9, 0xc4, 0x00]).unwrap(), Value::Float(-4.0));
        assert_eq!(Value::decode(&[0xfa, 0x47, 0xc3, 0x50, 0x00]).unwrap(), Value::Float(100000.0));
        assert_eq!(Value::int(i64::MIN).i64("x").unwrap(), i64::MIN);
    }

    #[test]
    fn refuses_bad_items_without_panicking() {
        for bad in [
            &[][..],
            &[0x18],                   // truncated argument
            &[0x5a, 0xff, 0xff, 0xff, 0xff], // a byte string longer than the frame
            &[0x9b, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff], // 2^64 items
            &[0x9f],                   // indefinite array
            &[0xc0, 0x00],             // a tag
            &[0x62, 0xff, 0xfe],       // text that is not UTF-8
            &[0x00, 0x00],             // something after the item
            &[0xf7],                   // undefined
        ] {
            assert!(Value::decode(bad).is_err(), "{bad:?}");
        }
        let deep: Vec<u8> = std::iter::repeat_n(0x81, 10_000).chain([0x00]).collect();
        assert!(Value::decode(&deep).unwrap_err().0.contains("deeper"));
    }

    #[test]
    fn typed_reads_say_what_is_wrong() {
        let v = Value::record([("opacity", Value::text("1")), ("n", Value::Uint(300))]);
        assert_eq!(v.field("layer", "opacity").unwrap().f64("layer.opacity").unwrap_err().0, "layer.opacity: expected a float, got a text string");
        assert_eq!(v.field("layer", "n").unwrap().u8("layer.n").unwrap_err().0, "layer.n: 300 is out of the range of u8");
        assert_eq!(v.field("layer", "id").unwrap_err().0, "layer: no field \"id\"");
        assert_eq!(Value::Uint(3).f64("x").unwrap(), 3.0);
        assert_eq!(Value::case("span", Value::Null).case_of("b").unwrap().0, "span");
        assert!(Value::ok(Value::Null).result("r").unwrap().is_ok());
    }
}
