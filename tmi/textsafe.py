"""Text from a repository (a commit message, an author, a path, a ref name) is
written by other people: before it reaches a terminal its control characters
are escaped (tmi-repo's ``textsafe.rs`` is the same function).

Escaped: C0 controls (U+0000-U+001F) except TAB (and LF when the caller keeps
newlines), DEL, C1 controls (U+0080-U+009F) and ESC, each as ``\\xNN`` (so ESC
is ``\\x1b``); bidi controls and marks (U+200E / U+200F, U+202A-U+202E,
U+2066-U+2069) as ``\\uNNNN``. A backslash is left as it is.
"""

from __future__ import annotations

from .repo import _FORMAT_CHARS


def escape(text: str, keep_newline: bool = False) -> str:
    out: list[str] = []
    for ch in text:
        c = ord(ch)
        if ch == "\t" or (ch == "\n" and keep_newline):
            out.append(ch)
        elif c < 32 or c == 127 or 0x80 <= c <= 0x9F:
            out.append(f"\\x{c:02x}")
        elif ch in _FORMAT_CHARS:
            out.append(f"\\u{c:04x}")
        else:
            out.append(ch)
    return "".join(out)
