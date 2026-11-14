//! JSON exactly as CPython's `json` module reads and writes it.
//!
//! tmiki hashes `json.dumps(value, ensure_ascii=False, sort_keys=True,
//! separators=(",", ":"))` (objects.py `canonical_json_bytes`), so every
//! detail that reaches the bytes is Python's: ints of any size, floats as
//! `repr(float)` (`1e-05`, `1.0`, `1e+16`), `NaN` / `Infinity`, which
//! characters are escaped, key order, the `indent=` layout. The parser is
//! `json.loads`: duplicate keys keep their first position and their last
//! value, `NaN` / `Infinity` / `-Infinity` are accepted, control characters
//! inside strings are not.
//!
//! Strings Python can hold but Rust cannot (lone surrogates from `\ud800`)
//! are refused: the caller falls back to Python.

use indexmap::IndexMap;
use std::fmt::Write as _;

pub type Map = IndexMap<String, Value>;

/// A Python `int` read from JSON: an i64 when it fits, else its decimal
/// digits (as `repr(int)` prints them).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Int {
    Small(i64),
    Big(String),
}

#[derive(Clone, Debug)]
pub enum Value {
    Null,
    Bool(bool),
    Int(Int),
    Float(f64),
    Str(String),
    Array(Vec<Value>),
    Object(Map),
}

impl Value {
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Object(map) => map.get(key),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_object(&self) -> Option<&Map> {
        match self {
            Value::Object(map) => Some(map),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&Vec<Value>> {
        match self {
            Value::Array(items) => Some(items),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Int(Int::Small(v)) => Some(*v),
            _ => None,
        }
    }

    /// Python truthiness.
    pub fn truthy(&self) -> bool {
        match self {
            Value::Null => false,
            Value::Bool(b) => *b,
            Value::Int(Int::Small(v)) => *v != 0,
            Value::Int(Int::Big(_)) => true,
            Value::Float(f) => *f != 0.0,
            Value::Str(s) => !s.is_empty(),
            Value::Array(items) => !items.is_empty(),
            Value::Object(map) => !map.is_empty(),
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// `str(value)` for the scalars (`print(value)` in tmiki): None, True,
    /// 3, 1.5, text. Containers are None here (callers fall back to Python).
    pub fn py_str(&self) -> Option<String> {
        Some(match self {
            Value::Null => "None".into(),
            Value::Bool(true) => "True".into(),
            Value::Bool(false) => "False".into(),
            Value::Int(Int::Small(v)) => v.to_string(),
            Value::Int(Int::Big(s)) => s.clone(),
            Value::Float(f) => float_repr(*f),
            Value::Str(s) => s.clone(),
            Value::Array(_) | Value::Object(_) => return None,
        })
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonError(pub String);

impl std::fmt::Display for JsonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for JsonError {}

impl JsonError {
    /// The document nests past MAX_DEPTH (rather than being malformed).
    pub fn is_too_deep(&self) -> bool {
        self.0.starts_with(TOO_DEEP)
    }
}

/// How deep arrays and objects may nest. An object received from a remote is
/// refused past this (`MAX_JSON_DEPTH` in objects.py is the same rule); real
/// tmiki objects nest a dozen levels.
pub const MAX_DEPTH: usize = 256;

/// The start of the error text for a document nested past `MAX_DEPTH`.
pub const TOO_DEEP: &str = "JSON nested deeper than 256 levels";

/// `json.loads(text)`.
pub fn loads(text: &str) -> Result<Value, JsonError> {
    let mut p = Parser { s: text.as_bytes(), i: 0, depth: 0 };
    p.ws();
    let value = p.value()?;
    p.ws();
    if p.i != p.s.len() {
        return Err(JsonError(format!("Extra data at {}", p.i)));
    }
    Ok(value)
}

/// `json.loads(payload.decode("utf-8"))`.
pub fn loads_bytes(payload: &[u8]) -> Result<Value, JsonError> {
    let text = std::str::from_utf8(payload).map_err(|e| JsonError(format!("invalid utf-8: {e}")))?;
    loads(text)
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
    depth: usize,
}

impl Parser<'_> {
    fn err<T>(&self, what: &str) -> Result<T, JsonError> {
        Err(JsonError(format!("{what} at {}", self.i)))
    }

    fn ws(&mut self) {
        while self.i < self.s.len() && matches!(self.s[self.i], b' ' | b'\t' | b'\n' | b'\r') {
            self.i += 1;
        }
    }

    fn eat(&mut self, lit: &[u8]) -> bool {
        if self.s[self.i..].starts_with(lit) {
            self.i += lit.len();
            true
        } else {
            false
        }
    }

    fn value(&mut self) -> Result<Value, JsonError> {
        let Some(&c) = self.s.get(self.i) else {
            return self.err("Expecting value");
        };
        match c {
            b'"' => Ok(Value::Str(self.string()?)),
            b'{' => self.nested(Self::object),
            b'[' => self.nested(Self::array),
            b'n' if self.eat(b"null") => Ok(Value::Null),
            b't' if self.eat(b"true") => Ok(Value::Bool(true)),
            b'f' if self.eat(b"false") => Ok(Value::Bool(false)),
            b'N' if self.eat(b"NaN") => Ok(Value::Float(f64::NAN)),
            b'I' if self.eat(b"Infinity") => Ok(Value::Float(f64::INFINITY)),
            b'-' if self.s[self.i..].starts_with(b"-Infinity") => {
                self.i += 9;
                Ok(Value::Float(f64::NEG_INFINITY))
            }
            b'-' | b'0'..=b'9' => self.number(),
            _ => self.err("Expecting value"),
        }
    }

    /// A container one level deeper: past MAX_DEPTH the parser stops with an
    /// error instead of recursing until the stack is gone.
    fn nested(&mut self, parse: fn(&mut Self) -> Result<Value, JsonError>) -> Result<Value, JsonError> {
        if self.depth >= MAX_DEPTH {
            return Err(JsonError(format!("{TOO_DEEP} at {}", self.i)));
        }
        self.depth += 1;
        let value = parse(self);
        self.depth -= 1;
        value
    }

    fn number(&mut self) -> Result<Value, JsonError> {
        // NUMBER_RE = r'(-?(?:0|[1-9][0-9]*))(\.[0-9]+)?([eE][-+]?[0-9]+)?'
        let start = self.i;
        let s = self.s;
        let mut i = self.i;
        if s.get(i) == Some(&b'-') {
            i += 1;
        }
        match s.get(i) {
            Some(b'0') => i += 1,
            Some(b'1'..=b'9') => {
                while matches!(s.get(i), Some(b'0'..=b'9')) {
                    i += 1;
                }
            }
            _ => return self.err("Expecting value"),
        }
        let int_end = i;
        let mut is_float = false;
        if s.get(i) == Some(&b'.') && matches!(s.get(i + 1), Some(b'0'..=b'9')) {
            i += 1;
            while matches!(s.get(i), Some(b'0'..=b'9')) {
                i += 1;
            }
            is_float = true;
        }
        if matches!(s.get(i), Some(b'e' | b'E')) {
            let mut j = i + 1;
            if matches!(s.get(j), Some(b'+' | b'-')) {
                j += 1;
            }
            if matches!(s.get(j), Some(b'0'..=b'9')) {
                while matches!(s.get(j), Some(b'0'..=b'9')) {
                    j += 1;
                }
                i = j;
                is_float = true;
            }
        }
        self.i = i;
        let text = std::str::from_utf8(&s[start..i]).unwrap();
        if is_float {
            return text
                .parse::<f64>()
                .map(Value::Float)
                .map_err(|_| JsonError(format!("bad float {text}")));
        }
        let digits = &s[start..int_end];
        if let Ok(v) = text.parse::<i64>() {
            return Ok(Value::Int(Int::Small(v)));
        }
        // CPython's int max str digits (4300) applies to json.loads too.
        let n_digits = digits.iter().filter(|b| b.is_ascii_digit()).count();
        if n_digits > 4300 {
            return self.err("Exceeds the limit for integer string conversion");
        }
        Ok(Value::Int(Int::Big(text.to_string())))
    }

    fn string(&mut self) -> Result<String, JsonError> {
        self.i += 1; // opening quote
        let mut out = String::new();
        loop {
            let start = self.i;
            while self.i < self.s.len() {
                let c = self.s[self.i];
                if c == b'"' || c == b'\\' || c < 0x20 {
                    break;
                }
                self.i += 1;
            }
            out.push_str(std::str::from_utf8(&self.s[start..self.i]).map_err(|e| JsonError(e.to_string()))?);
            let Some(&c) = self.s.get(self.i) else {
                return self.err("Unterminated string");
            };
            self.i += 1;
            match c {
                b'"' => return Ok(out),
                b'\\' => {
                    let Some(&e) = self.s.get(self.i) else {
                        return self.err("Unterminated string");
                    };
                    self.i += 1;
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let hi = self.hex4()?;
                            if (0xD800..0xDC00).contains(&hi) && self.s[self.i..].starts_with(b"\\u") {
                                let save = self.i;
                                self.i += 2;
                                let lo = self.hex4()?;
                                if (0xDC00..0xE000).contains(&lo) {
                                    let cp = 0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00);
                                    out.push(char::from_u32(cp).unwrap());
                                    continue;
                                }
                                self.i = save;
                            }
                            match char::from_u32(hi) {
                                Some(ch) => out.push(ch),
                                None => return self.err("lone surrogate (not representable)"),
                            }
                        }
                        _ => return self.err("Invalid \\escape"),
                    }
                }
                _ => return self.err("Invalid control character"),
            }
        }
    }

    fn hex4(&mut self) -> Result<u32, JsonError> {
        let Some(h) = self.s.get(self.i..self.i + 4) else {
            return self.err("Invalid \\uXXXX escape");
        };
        let text = std::str::from_utf8(h).map_err(|_| JsonError("Invalid \\uXXXX escape".into()))?;
        if !text.bytes().all(|b| b.is_ascii_hexdigit()) {
            return self.err("Invalid \\uXXXX escape");
        }
        self.i += 4;
        Ok(u32::from_str_radix(text, 16).unwrap())
    }

    fn array(&mut self) -> Result<Value, JsonError> {
        self.i += 1;
        let mut items = Vec::new();
        self.ws();
        if self.s.get(self.i) == Some(&b']') {
            self.i += 1;
            return Ok(Value::Array(items));
        }
        loop {
            self.ws();
            items.push(self.value()?);
            self.ws();
            match self.s.get(self.i) {
                Some(b',') => self.i += 1,
                Some(b']') => {
                    self.i += 1;
                    return Ok(Value::Array(items));
                }
                _ => return self.err("Expecting ',' delimiter"),
            }
        }
    }

    fn object(&mut self) -> Result<Value, JsonError> {
        self.i += 1;
        let mut map = Map::new();
        self.ws();
        if self.s.get(self.i) == Some(&b'}') {
            self.i += 1;
            return Ok(Value::Object(map));
        }
        loop {
            self.ws();
            if self.s.get(self.i) != Some(&b'"') {
                return self.err("Expecting property name enclosed in double quotes");
            }
            let key = self.string()?;
            self.ws();
            if self.s.get(self.i) != Some(&b':') {
                return self.err("Expecting ':' delimiter");
            }
            self.i += 1;
            self.ws();
            let value = self.value()?;
            // dict assignment: first position, last value
            map.insert(key, value);
            self.ws();
            match self.s.get(self.i) {
                Some(b',') => self.i += 1,
                Some(b'}') => {
                    self.i += 1;
                    return Ok(Value::Object(map));
                }
                _ => return self.err("Expecting ',' delimiter"),
            }
        }
    }
}

/// `json.dumps` options tmiki uses.
#[derive(Clone, Copy, Debug)]
pub struct DumpOptions<'a> {
    pub sort_keys: bool,
    pub ensure_ascii: bool,
    pub indent: Option<usize>,
    pub item_separator: &'a str,
    pub key_separator: &'a str,
}

impl DumpOptions<'static> {
    /// objects.py `canonical_json_bytes` (and importers' `_semantic_digest`).
    pub const CANONICAL: DumpOptions<'static> = DumpOptions {
        sort_keys: true,
        ensure_ascii: false,
        indent: None,
        item_separator: ",",
        key_separator: ":",
    };
    /// `json.dumps(v, ensure_ascii=False, sort_keys=True, indent=2)`: the
    /// index, config, stat cache and stash list (each then gets "\n").
    pub const INDENTED_SORTED: DumpOptions<'static> = DumpOptions {
        sort_keys: true,
        ensure_ascii: false,
        indent: Some(2),
        item_separator: ",",
        key_separator: ": ",
    };
    /// `json.dumps(v, ensure_ascii=False, indent=2)`: backups.json and
    /// conflict.json (insertion order).
    pub const INDENTED: DumpOptions<'static> = DumpOptions {
        sort_keys: false,
        ensure_ascii: false,
        indent: Some(2),
        item_separator: ",",
        key_separator: ": ",
    };
    /// `json.dumps(v)` with every default.
    pub const DEFAULT: DumpOptions<'static> = DumpOptions {
        sort_keys: false,
        ensure_ascii: true,
        indent: None,
        item_separator: ", ",
        key_separator: ": ",
    };
}

/// `json.dumps(value, ...)`.
pub fn dumps(value: &Value, opts: &DumpOptions) -> String {
    let mut out = String::new();
    write_value(&mut out, value, opts, 0);
    out
}

/// `canonical_json_bytes(value)` (objects.py).
pub fn canonical_bytes(value: &Value) -> Vec<u8> {
    dumps(value, &DumpOptions::CANONICAL).into_bytes()
}

fn newline_indent(out: &mut String, opts: &DumpOptions, level: usize) {
    if let Some(n) = opts.indent {
        out.push('\n');
        for _ in 0..n * level {
            out.push(' ');
        }
    }
}

fn write_value(out: &mut String, value: &Value, opts: &DumpOptions, level: usize) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Int(Int::Small(v)) => {
            let _ = write!(out, "{v}");
        }
        Value::Int(Int::Big(s)) => out.push_str(&normalize_big_int(s)),
        Value::Float(f) => out.push_str(&json_float(*f)),
        Value::Str(s) => write_string(out, s, opts.ensure_ascii),
        Value::Array(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            newline_indent(out, opts, level + 1);
            for (idx, item) in items.iter().enumerate() {
                if idx > 0 {
                    out.push_str(opts.item_separator);
                    newline_indent(out, opts, level + 1);
                }
                write_value(out, item, opts, level + 1);
            }
            newline_indent(out, opts, level);
            out.push(']');
        }
        Value::Object(map) => {
            if map.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push('{');
            newline_indent(out, opts, level + 1);
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            if opts.sort_keys {
                // str ordering is code point order, which UTF-8 byte order keeps
                entries.sort_by(|a, b| a.0.cmp(b.0));
            }
            for (idx, (key, item)) in entries.into_iter().enumerate() {
                if idx > 0 {
                    out.push_str(opts.item_separator);
                    newline_indent(out, opts, level + 1);
                }
                write_string(out, key, opts.ensure_ascii);
                out.push_str(opts.key_separator);
                write_value(out, item, opts, level + 1);
            }
            newline_indent(out, opts, level);
            out.push('}');
        }
    }
}

fn normalize_big_int(s: &str) -> String {
    // json.loads gave int(text); repr(int) has no leading zeros or "-0"
    let (neg, digits) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s),
    };
    let trimmed = digits.trim_start_matches('0');
    if trimmed.is_empty() {
        return "0".into();
    }
    if neg {
        format!("-{trimmed}")
    } else {
        trimmed.to_string()
    }
}

/// `json.encoder` `floatstr`: NaN, Infinity, -Infinity, else `repr(x)`.
pub fn json_float(x: f64) -> String {
    if x.is_nan() {
        "NaN".into()
    } else if x == f64::INFINITY {
        "Infinity".into()
    } else if x == f64::NEG_INFINITY {
        "-Infinity".into()
    } else {
        float_repr(x)
    }
}

/// The shortest round-trip digits of finite `x` and the decimal point
/// position: x = 0.DIGITS * 10**decpt (CPython's `_Py_dg_dtoa` mode 0).
fn shortest_digits(x: f64) -> (String, i32) {
    // Rust's `{:e}` finds how many digits round-trip, but at 17 digits it
    // may not pick the closest string; dtoa mode 0 does. The correctly
    // rounded string of that length is the closest, so it round-trips too.
    let shortest = format!("{:e}", x.abs());
    let n = shortest.split_once('e').unwrap().0.chars().filter(|c| c.is_ascii_digit()).count();
    let text = format!("{:.*e}", n.saturating_sub(1), x.abs());
    let (mantissa, exp) = text.split_once('e').unwrap();
    let exp: i32 = exp.parse().unwrap();
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let digits = digits.trim_end_matches('0');
    if digits.is_empty() {
        return ("0".into(), 1);
    }
    (digits.to_string(), exp + 1)
}

/// `repr(x)` of a Python float (`float_repr_style == "short"`).
pub fn float_repr(x: f64) -> String {
    if x.is_nan() {
        return "nan".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "inf".into() } else { "-inf".into() };
    }
    let sign = if x.is_sign_negative() { "-" } else { "" };
    if x == 0.0 {
        return format!("{sign}0.0");
    }
    let (digits, decpt) = shortest_digits(x);
    let n = digits.len() as i32;
    let body = if decpt <= -4 || decpt > 16 {
        let exp = decpt - 1;
        let mantissa = if n > 1 { format!("{}.{}", &digits[..1], &digits[1..]) } else { digits.clone() };
        let esign = if exp < 0 { '-' } else { '+' };
        format!("{mantissa}e{esign}{:02}", exp.abs())
    } else if decpt <= 0 {
        format!("0.{}{}", "0".repeat((-decpt) as usize), digits)
    } else if decpt >= n {
        format!("{}{}.0", digits, "0".repeat((decpt - n) as usize))
    } else {
        format!("{}.{}", &digits[..decpt as usize], &digits[decpt as usize..])
    };
    format!("{sign}{body}")
}

/// `round(x, ndigits)` of a Python float for ndigits >= 0 (tmiki rounds
/// opacities with ndigits 6): CPython's `double_round` takes the correctly
/// rounded decimal of the exact binary value, ties to even, and reads it
/// back. Rust's fixed-precision formatting is that same exact rounding.
pub fn py_round(x: f64, ndigits: u32) -> f64 {
    if !x.is_finite() || x == 0.0 || ndigits > 323 {
        return x;
    }
    let y: f64 = format!("{:.*}", ndigits as usize, x).parse().unwrap();
    if y == 0.0 {
        0.0f64.copysign(x)
    } else {
        y
    }
}

fn write_string(out: &mut String, s: &str, ensure_ascii: bool) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c if ensure_ascii && (c as u32) > 0x7e => {
                let cp = c as u32;
                if cp >= 0x10000 {
                    let v = cp - 0x10000;
                    let _ = write!(out, "\\u{:04x}\\u{:04x}", 0xD800 + (v >> 10), 0xDC00 + (v & 0x3FF));
                } else {
                    let _ = write!(out, "\\u{:04x}", cp);
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn float_repr_matches_python() {
        let cases = [
            (1.0, "1.0"),
            (0.1, "0.1"),
            (1e-05, "1e-05"),
            (0.0001, "0.0001"),
            (1e16, "1e+16"),
            (1e15, "1000000000000000.0"),
            (1234567890123456.0, "1234567890123456.0"),
            (12345678901234567.0, "1.2345678901234568e+16"),
            (-0.0, "-0.0"),
            (0.5, "0.5"),
            (1.5e300, "1.5e+300"),
            (5e-324, "5e-324"),
            (0.3333333333333333, "0.3333333333333333"),
            (100.0, "100.0"),
            (123.456, "123.456"),
            (2.5e-05, "2.5e-05"),
            (25398418981342.062, "25398418981342.062"),
            (594923428183362.2, "594923428183362.2"),
        ];
        for (x, want) in cases {
            assert_eq!(float_repr(x), want, "{x:?}");
        }
        assert_eq!(json_float(f64::NAN), "NaN");
        assert_eq!(json_float(f64::NEG_INFINITY), "-Infinity");
    }

    #[test]
    fn round_half_even_on_exact_value() {
        assert_eq!(py_round(0.125, 2), 0.12);
        assert_eq!(py_round(0.375, 2), 0.38);
        assert_eq!(py_round(2.675, 2), 2.67); // 2.67499999...
        assert_eq!(py_round(128.0 / 255.0, 6), 0.501961);
        assert_eq!(py_round(1.0, 6), 1.0);
    }

    #[test]
    fn nesting_past_the_limit_is_an_error_not_a_stack_overflow() {
        let ok = format!("{}{}", "[".repeat(MAX_DEPTH), "]".repeat(MAX_DEPTH));
        assert!(loads(&ok).is_ok());
        let deep = format!("{}{}", "[".repeat(MAX_DEPTH + 1), "]".repeat(MAX_DEPTH + 1));
        assert!(loads(&deep).unwrap_err().is_too_deep());
        assert!(loads(&"[".repeat(200_000)).unwrap_err().is_too_deep());
        assert!(loads(&"{\"a\":".repeat(1000)).unwrap_err().is_too_deep());
    }

    #[test]
    fn roundtrip_canonical() {
        let text = r#"{"b":[1,2.5,"é\n",null,true],"a":{"z":1e-05,"y":-0.0},"c":123456789012345678901234567890}"#;
        let v = loads(text).unwrap();
        let out = String::from_utf8(canonical_bytes(&v)).unwrap();
        assert_eq!(out, "{\"a\":{\"y\":-0.0,\"z\":1e-05},\"b\":[1,2.5,\"é\\n\",null,true],\"c\":123456789012345678901234567890}");
    }

    #[test]
    fn indent_layout() {
        let v = loads(r#"{"version":1,"entries":{},"l":[1,[]]}"#).unwrap();
        assert_eq!(
            dumps(&v, &DumpOptions::INDENTED_SORTED),
            "{\n  \"entries\": {},\n  \"l\": [\n    1,\n    []\n  ],\n  \"version\": 1\n}"
        );
    }

    #[test]
    fn duplicate_keys_keep_first_position_last_value() {
        let v = loads(r#"{"a":1,"b":2,"a":3}"#).unwrap();
        assert_eq!(dumps(&v, &DumpOptions::DEFAULT), "{\"a\": 3, \"b\": 2}");
    }

    #[test]
    fn ensure_ascii_escapes() {
        let v = Value::Str("é😀\u{7f}".into());
        assert_eq!(dumps(&v, &DumpOptions::DEFAULT), "\"\\u00e9\\ud83d\\ude00\\u007f\"");
        assert_eq!(dumps(&v, &DumpOptions::CANONICAL), "\"é😀\u{7f}\"");
    }
}
