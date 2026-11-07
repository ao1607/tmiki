//! CPython 3.13's `int(text, base)` on a str (bases 10 and 16) and
//! `str.strip()`, for the MDP2 reader's attribute parsing (mdp2.py
//! `_int_attr`, `_parse_argb`).
//!
//! `int()` first maps every non-ASCII character to ASCII
//! (`_PyUnicode_TransformDecimalAndSpaceToASCII`): a Unicode decimal digit
//! (category Nd, Unicode 15.1) to its ASCII digit, Unicode whitespace to a
//! space, anything else to an invalid character. ASCII stays as it is, so
//! the whitespace `PyLong_FromString` then skips is ASCII " \t\n\v\f\r" plus
//! the non-ASCII whitespace: exactly Rust's `char::is_whitespace`. (U+001C to
//! U+001F are whitespace to `str.isspace()` / `strip()`, not to `int()`.)

/// The first code point of every run of ten Unicode decimal digits (Nd) in
/// Unicode 15.1 (CPython 3.13's `unicodedata`), 0 to 9 in order.
const DECIMAL_ZEROS: [u32; 68] = [
    0x30, 0x660, 0x6f0, 0x7c0, 0x966, 0x9e6, 0xa66, 0xae6, 0xb66, 0xbe6, 0xc66, 0xce6, 0xd66, 0xde6, 0xe50, 0xed0,
    0xf20, 0x1040, 0x1090, 0x17e0, 0x1810, 0x1946, 0x19d0, 0x1a80, 0x1a90, 0x1b50, 0x1bb0, 0x1c40, 0x1c50, 0xa620,
    0xa8d0, 0xa900, 0xa9d0, 0xa9f0, 0xaa50, 0xabf0, 0xff10, 0x104a0, 0x10d30, 0x11066, 0x110f0, 0x11136, 0x111d0,
    0x112f0, 0x11450, 0x114d0, 0x11650, 0x116c0, 0x11730, 0x118e0, 0x11950, 0x11c50, 0x11d50, 0x11da0, 0x11f50,
    0x16a60, 0x16ac0, 0x16b50, 0x1d7ce, 0x1d7d8, 0x1d7e2, 0x1d7ec, 0x1d7f6, 0x1e140, 0x1e2f0, 0x1e4f0, 0x1e950,
    0x1fbf0,
];

/// `unicodedata.decimal(c)`.
pub fn decimal_value(c: char) -> Option<u32> {
    let cp = c as u32;
    let i = DECIMAL_ZEROS.partition_point(|&z| z <= cp);
    if i == 0 {
        return None;
    }
    let d = cp - DECIMAL_ZEROS[i - 1];
    (d < 10).then_some(d)
}

/// `str.isspace()` of one character (`Py_UNICODE_ISSPACE`).
pub fn is_py_space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

/// `str.strip()` with no argument.
pub fn py_strip(s: &str) -> &str {
    s.trim_matches(is_py_space)
}

/// The value `int(text, base)` gives, in parts: the sign, the magnitude
/// modulo 2^128 and whether the magnitude reached 2^127 (a Python int has no
/// such bound). None where Python raises ValueError.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PyInt {
    pub negative: bool,
    pub magnitude: u128,
    pub big: bool,
}

impl PyInt {
    /// The value as an i64, None when it does not fit.
    pub fn to_i64(self) -> Option<i64> {
        if self.big || self.magnitude > i64::MAX as u128 + self.negative as u128 {
            return None;
        }
        Some(if self.negative { (self.magnitude as i128).wrapping_neg() as i64 } else { self.magnitude as i64 })
    }

    /// The low 64 bits of the value's two's complement (what `>>` and `&`
    /// on a Python int see there).
    pub fn low_bits(self) -> u64 {
        let m = self.magnitude as u64;
        if self.negative {
            m.wrapping_neg()
        } else {
            m
        }
    }
}

/// `sys.get_int_max_str_digits()`'s default: longer base-10 strings raise.
const MAX_STR_DIGITS: usize = 4300;

/// `str(int(text))`: the value of a base-10 `int()` exactly, however long.
pub fn py_int_decimal(text: &str) -> Option<String> {
    let v = py_int(text, 10)?;
    let mut digits = String::new();
    for c in text.chars() {
        let d = if c.is_ascii_digit() { Some(c as u32 - '0' as u32) } else if c.is_ascii() { None } else { decimal_value(c) };
        if let Some(d) = d {
            if !(digits.is_empty() && d == 0) {
                digits.push(char::from(b'0' + d as u8));
            }
        }
    }
    if digits.is_empty() {
        return Some("0".into());
    }
    Some(if v.negative { format!("-{digits}") } else { digits })
}

/// `int(text, base)` for base 10 or 16.
pub fn py_int(text: &str, base: u32) -> Option<PyInt> {
    debug_assert!(base == 10 || base == 16);
    // The ASCII form `int()` parses; None stands for an invalid character.
    let mut chars: Vec<u8> = Vec::with_capacity(text.len());
    for c in text.chars() {
        let b = if c.is_ascii() {
            c as u8
        } else if let Some(d) = decimal_value(c) {
            b'0' + d as u8
        } else if c.is_whitespace() {
            b' '
        } else {
            0xff
        };
        chars.push(b);
    }
    let is_space = |b: u8| matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r');
    let mut i = 0;
    while i < chars.len() && is_space(chars[i]) {
        i += 1;
    }
    let mut negative = false;
    if i < chars.len() && (chars[i] == b'+' || chars[i] == b'-') {
        negative = chars[i] == b'-';
        i += 1;
    }
    let mut prefixed = false;
    if base == 16 && i + 1 < chars.len() && chars[i] == b'0' && (chars[i + 1] == b'x' || chars[i + 1] == b'X') {
        i += 2;
        prefixed = true;
    }
    let digit = |b: u8| -> Option<u32> { (b as char).to_digit(base) };
    let mut magnitude: u128 = 0;
    let mut big = false;
    let mut ndigits = 0usize;
    // After a base prefix one underscore may come first ("0x_ff").
    let mut prev_underscore = false;
    if prefixed && i < chars.len() && chars[i] == b'_' {
        i += 1;
        prev_underscore = true;
    }
    while i < chars.len() {
        let b = chars[i];
        if b == b'_' {
            if prev_underscore || ndigits == 0 {
                return None;
            }
            prev_underscore = true;
            i += 1;
            continue;
        }
        let Some(d) = digit(b) else { break };
        let (m, o1) = magnitude.overflowing_mul(base as u128);
        let (m, o2) = m.overflowing_add(d as u128);
        big |= o1 || o2 || m >= 1u128 << 127;
        magnitude = m;
        ndigits += 1;
        prev_underscore = false;
        i += 1;
    }
    if ndigits == 0 || prev_underscore {
        return None;
    }
    if base == 10 && ndigits > MAX_STR_DIGITS {
        return None;
    }
    while i < chars.len() && is_space(chars[i]) {
        i += 1;
    }
    if i != chars.len() {
        return None;
    }
    Some(PyInt { negative, magnitude, big })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn int10(s: &str) -> Option<i64> {
        py_int(s, 10).and_then(PyInt::to_i64)
    }

    #[test]
    fn decimal_grammar() {
        assert_eq!(int10("12"), Some(12));
        assert_eq!(int10(" -7 "), Some(-7));
        assert_eq!(int10("+5"), Some(5));
        assert_eq!(int10("1_0"), Some(10));
        assert_eq!(int10("0010"), Some(10));
        assert_eq!(int10("\u{ff15}"), Some(5));
        assert_eq!(int10("\u{3000}3\u{a0}"), Some(3));
        assert_eq!(py_int("_1", 10), None);
        assert_eq!(py_int("1_", 10), None);
        assert_eq!(py_int("1__0", 10), None);
        assert_eq!(py_int("0x10", 10), None);
        assert_eq!(py_int("\u{1c}5", 10), None);
        assert_eq!(py_int("", 10), None);
        assert_eq!(py_int("-", 10), None);
        assert_eq!(py_int("1 2", 10), None);
        assert_eq!(int10("-9223372036854775808"), Some(i64::MIN));
        assert_eq!(int10("9223372036854775808"), None);
        assert!(py_int("99999999999999999999999999999999999999999999", 10).unwrap().big);
    }

    #[test]
    fn hex_grammar() {
        let v = |s: &str| py_int(s, 16).map(|p| p.low_bits());
        assert_eq!(v("ff"), Some(255));
        assert_eq!(v(" 0x_ff "), Some(255));
        assert_eq!(v("0X1"), Some(1));
        assert_eq!(v("0_1"), Some(1));
        assert_eq!(v("0x"), None);
        assert_eq!(v("\u{663}"), Some(3));
        assert_eq!(v("-1"), Some(u64::MAX));
        assert_eq!(v("1FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF"), Some(u64::MAX));
    }

    #[test]
    fn strip_and_digits() {
        assert_eq!(py_strip("\u{1c} a \u{85}"), "a");
        assert_eq!(decimal_value('9'), Some(9));
        assert_eq!(decimal_value('\u{1d7ff}'), Some(9));
        assert_eq!(decimal_value('a'), None);
        assert_eq!(decimal_value('\u{2160}'), None);
    }
}
