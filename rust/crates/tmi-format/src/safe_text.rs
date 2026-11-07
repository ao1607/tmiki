//! Text a plugin supplied (its id, version, names, messages, errors), made
//! safe to print on a terminal (PLUGINS.md 11, plugin threat model): a
//! component is somebody else's code, so a CR, an ANSI escape or a bidi
//! override in what it says must not rewrite what the user sees ("trusted",
//! "signed", ...). Every place that prints plugin-made text goes through
//! [`line`] (one line: newlines escaped too) or [`message`] (newlines and
//! tabs kept, so a multi-line panic stays readable).

use std::borrow::Cow;
use std::fmt::Write;

fn dangerous(c: char, keep_layout: bool) -> bool {
    match c {
        '\n' | '\t' => !keep_layout,
        c if c.is_control() => true,
        // line / paragraph separators and the bidi controls and isolates
        '\u{2028}' | '\u{2029}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{200E}' | '\u{200F}' | '\u{061C}' | '\u{FEFF}' => true,
        _ => false,
    }
}

fn escape(s: &str, keep_layout: bool) -> Cow<'_, str> {
    if !s.chars().any(|c| dangerous(c, keep_layout)) {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        if dangerous(c, keep_layout) {
            match c {
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                '\x1b' => out.push_str("\\x1b"),
                c if (c as u32) <= 0xff => {
                    let _ = write!(out, "\\x{:02x}", c as u32);
                }
                c => {
                    let _ = write!(out, "\\u{{{:04x}}}", c as u32);
                }
            }
        } else {
            out.push(c);
        }
    }
    Cow::Owned(out)
}

/// `s` for one line of terminal output: controls (newline and tab too),
/// the C1 range and bidi controls become `\x1b`, `\n`, `\u{202e}` ...
pub fn line(s: &str) -> Cow<'_, str> {
    escape(s, false)
}

/// `s` for a multi-line message: as [`line`], but `\n` and `\t` stay.
pub fn message(s: &str) -> Cow<'_, str> {
    escape(s.trim_end_matches('\r'), true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_terminal_tricks() {
        assert_eq!(line("1.0.0-ok"), "1.0.0-ok");
        assert_eq!(line("1.0.0\r\x1b[2Ksigned\n"), "1.0.0\\r\\x1b[2Ksigned\\n");
        assert_eq!(line("a\u{202e}b\u{85}c"), "a\\u{202e}b\\x85c");
        assert_eq!(message("a\nb\tc\x1b[31m"), "a\nb\tc\\x1b[31m");
        assert!(matches!(line("plain"), Cow::Borrowed(_)));
    }
}
