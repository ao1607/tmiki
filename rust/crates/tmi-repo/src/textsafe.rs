//! Text from a repository (a commit message, an author, a path, a ref name)
//! is written by other people: before it reaches a terminal its control
//! characters are escaped (tmi/textsafe.py is the same function).
//!
//! Escaped: C0 controls (U+0000-U+001F) except TAB (and LF when the caller
//! keeps newlines), DEL, C1 controls (U+0080-U+009F) and ESC, each as
//! `\xNN` (so ESC is `\x1b`); bidi controls and marks (U+200E / U+200F,
//! U+202A-U+202E, U+2066-U+2069) as `\uNNNN`. A backslash is left as it is.

use crate::repo::is_format_char;

/// `textsafe.escape(text, keep_newline)`.
pub fn escape(text: &str, keep_newline: bool) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        let c = ch as u32;
        if ch == '\t' || (ch == '\n' && keep_newline) {
            out.push(ch);
        } else if c < 32 || c == 127 || (0x80..=0x9f).contains(&c) {
            out.push_str(&format!("\\x{c:02x}"));
        } else if is_format_char(ch) {
            out.push_str(&format!("\\u{c:04x}"));
        } else {
            out.push(ch);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::escape;

    #[test]
    fn escapes_controls() {
        assert_eq!(escape("a\x1b]0;x\x07b", false), "a\\x1b]0;x\\x07b");
        assert_eq!(escape("a\tb\nc", false), "a\tb\\x0ac");
        assert_eq!(escape("a\tb\nc", true), "a\tb\nc");
        assert_eq!(escape("\x7f\u{9b}\u{202e}\u{2066}", true), "\\x7f\\x9b\\u202e\\u2066");
        assert_eq!(escape("日本語 ok\\", false), "日本語 ok\\");
    }
}
