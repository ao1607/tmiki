//! What a native command prints, held until it is done, then written the
//! way CPython's stdio would: text through the stream's encoding with
//! "\n" as "\r\n" on Windows, raw bytes (`sys.stdout.buffer`) as they are.
//!
//! Holding everything lets a command change its mind: when it finds
//! something it does not reproduce, nothing has been written and the
//! whole command goes to Python. Progress lines (`TMI_PROGRESS=1`) are
//! held the same way and written in their place among these.

use std::io::{IsTerminal, Write};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stream {
    Stdout,
    Stderr,
}

enum Chunk {
    Text(String),
    Bytes(Vec<u8>),
}

#[derive(Default)]
pub struct Out {
    /// (place in the process's output, stream, what)
    events: Vec<(u64, Stream, Chunk)>,
}

/// The events with the progress lines held so far, in the order they were
/// made.
fn with_progress_lines(mut events: Vec<(u64, Stream, Chunk)>) -> Vec<(u64, Stream, Chunk)> {
    let lines = crate::progress::take_held();
    if !lines.is_empty() {
        events.extend(lines.into_iter().map(|(seq, line)| (seq, Stream::Stderr, Chunk::Text(line))));
        events.sort_by_key(|(seq, _, _)| *seq);
    }
    events
}

/// How Python would encode a text stream.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Encoding {
    /// UTF-8 (or a console, which Python writes as UTF-16 via WriteConsoleW).
    Utf8,
    /// An ASCII-compatible legacy code page: ASCII text is safe, anything
    /// else is left to Python (its codec and backslashreplace).
    LegacyAscii,
    /// Python's cp932 codec (the Japanese ANSI code page), with the
    /// backslashreplace tmi.cli.main sets (stage 4b).
    Cp932,
    /// Not reproduced (e.g. PYTHONIOENCODING=utf-16): Python runs it all.
    Unknown,
}

impl Out {
    pub fn println(&mut self, line: impl Into<String>) {
        let mut s = line.into();
        s.push('\n');
        self.events.push((crate::progress::next_seq(), Stream::Stdout, Chunk::Text(s)));
    }

    pub fn eprintln(&mut self, line: impl Into<String>) {
        let mut s = line.into();
        s.push('\n');
        self.events.push((crate::progress::next_seq(), Stream::Stderr, Chunk::Text(s)));
    }

    pub fn write_bytes(&mut self, data: Vec<u8>) {
        self.events.push((crate::progress::next_seq(), Stream::Stdout, Chunk::Bytes(data)));
    }

    fn has_non_ascii(&self, stream: Stream) -> bool {
        self.events
            .iter()
            .any(|(_, s, c)| *s == stream && matches!(c, Chunk::Text(t) if !t.is_ascii()))
            || (stream == Stream::Stderr && crate::progress::held_non_ascii())
    }

    fn has_bytes(&self) -> bool {
        self.events.iter().any(|(_, _, c)| matches!(c, Chunk::Bytes(_)))
    }

    /// Whether this output can be written exactly as Python would.
    pub fn writable(&self) -> bool {
        for stream in [Stream::Stdout, Stream::Stderr] {
            match encoding(stream) {
                Encoding::Utf8 | Encoding::Cp932 => {}
                Encoding::LegacyAscii if !self.has_non_ascii(stream) => {}
                _ => return false,
            }
        }
        // Raw bytes on a console: Python's console writer would decode them.
        !(self.has_bytes() && is_console(Stream::Stdout))
    }

    /// Before a child process (a hook) writes to the same streams: write
    /// what Python would already have written. Its stderr is line-buffered
    /// (everything so far); its stdout only on a console (else it is still
    /// in Python's buffer and comes out after the child's).
    pub fn flush_pending(&mut self) {
        crate::progress::clear_active();
        let stdout_console = is_console(Stream::Stdout);
        let events = with_progress_lines(std::mem::take(&mut self.events));
        let (now, later): (Vec<_>, Vec<_>) =
            events.into_iter().partition(|(_, s, _)| *s == Stream::Stderr || stdout_console);
        self.events = later;
        Out { events: now }.flush_in_order();
    }

    fn flush_in_order(self) {
        let mut out = std::io::stdout().lock();
        let mut err = std::io::stderr().lock();
        for (_, stream, chunk) in &self.events {
            let bytes: std::borrow::Cow<[u8]> = match chunk {
                Chunk::Text(t) => std::borrow::Cow::Owned(text_bytes(*stream, t)),
                Chunk::Bytes(b) => std::borrow::Cow::Borrowed(b),
            };
            let _ = match stream {
                Stream::Stdout => out.write_all(&bytes),
                Stream::Stderr => err.write_all(&bytes),
            };
        }
        let _ = out.flush();
        let _ = err.flush();
    }

    /// Write everything. Python block-buffers a stdout that is not a
    /// console, so there its stderr lines come out first.
    pub fn flush(self) {
        crate::progress::clear_active();
        let stdout_console = is_console(Stream::Stdout);
        let events = with_progress_lines(self.events);
        let mut order: Vec<&(u64, Stream, Chunk)> = events.iter().collect();
        if !stdout_console {
            order.sort_by_key(|(_, s, _)| (*s == Stream::Stdout) as u8);
        }
        let mut out = std::io::stdout().lock();
        let mut err = std::io::stderr().lock();
        for (_, stream, chunk) in order {
            let bytes: std::borrow::Cow<[u8]> = match chunk {
                Chunk::Text(t) => std::borrow::Cow::Owned(text_bytes(*stream, t)),
                Chunk::Bytes(b) => std::borrow::Cow::Borrowed(b),
            };
            let _ = match stream {
                Stream::Stdout => out.write_all(&bytes),
                Stream::Stderr => err.write_all(&bytes),
            };
        }
        let _ = out.flush();
        let _ = err.flush();
    }
}

/// Text as the stream's TextIOWrapper writes it: "\n" as "\r\n" on
/// Windows, then the encoding (cp932 with backslashreplace when Python
/// would use it).
pub fn text_bytes(stream: Stream, t: &str) -> Vec<u8> {
    let t: std::borrow::Cow<str> = if cfg!(windows) { t.replace('\n', "\r\n").into() } else { t.into() };
    if t.is_ascii() || encoding(stream) != Encoding::Cp932 {
        return t.into_owned().into_bytes();
    }
    cp932_encode(&t)
}

static CP932: &[u8] = include_bytes!("cp932.bin");

/// Python's `text.encode("cp932", "backslashreplace")` (the table is
/// tools/gen_cp932.py's).
pub fn cp932_encode(text: &str) -> Vec<u8> {
    let n = CP932.len() / 6;
    let record = |i: usize| {
        let r = &CP932[i * 6..i * 6 + 6];
        (u32::from_le_bytes([r[0], r[1], r[2], r[3]]), u16::from_le_bytes([r[4], r[5]]))
    };
    let mut out = Vec::with_capacity(text.len());
    for c in text.chars() {
        let cp = c as u32;
        if cp < 0x80 {
            out.push(cp as u8);
            continue;
        }
        let (mut lo, mut hi) = (0usize, n);
        let mut found = None;
        while lo < hi {
            let mid = (lo + hi) / 2;
            let (k, v) = record(mid);
            match k.cmp(&cp) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => {
                    found = Some(v);
                    break;
                }
            }
        }
        match found {
            Some(v) if v < 0x100 => out.push(v as u8),
            Some(v) => out.extend_from_slice(&v.to_be_bytes()),
            None if cp < 0x100 => out.extend_from_slice(format!("\\x{cp:02x}").as_bytes()),
            None if cp < 0x10000 => out.extend_from_slice(format!("\\u{cp:04x}").as_bytes()),
            None => out.extend_from_slice(format!("\\U{cp:08x}").as_bytes()),
        }
    }
    out
}

/// Whether the stream is a Windows console (Python's `_WindowsConsoleIO`)
/// or, elsewhere, a terminal.
pub fn is_console(stream: Stream) -> bool {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Console::{GetConsoleMode, GetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE};
        let which = if stream == Stream::Stdout { STD_OUTPUT_HANDLE } else { STD_ERROR_HANDLE };
        let mut mode = 0u32;
        // SAFETY: querying the mode of a standard handle.
        unsafe { GetConsoleMode(GetStdHandle(which), &mut mode) != 0 }
    }
    #[cfg(not(windows))]
    {
        match stream {
            Stream::Stdout => std::io::stdout().is_terminal(),
            Stream::Stderr => std::io::stderr().is_terminal(),
        }
    }
}

pub fn is_tty(stream: Stream) -> bool {
    match stream {
        Stream::Stdout => std::io::stdout().is_terminal(),
        Stream::Stderr => std::io::stderr().is_terminal(),
    }
}

fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// The names Python's codec registry takes for cp932.
fn is_cp932_name(name: &str) -> bool {
    let n: String = name.trim().to_ascii_lowercase().chars().map(|c| if c == '-' || c == ' ' { '_' } else { c }).collect();
    matches!(n.as_str(), "cp932" | "932" | "ms932" | "mskanji" | "ms_kanji")
}

fn is_utf8_name(name: &str) -> bool {
    let n: String = name.trim().to_ascii_lowercase().chars().filter(|c| *c != '-' && *c != '_').collect();
    matches!(n.as_str(), "utf8" | "u8" | "utf" | "cp65001")
}

/// The encoding CPython 3.13 gives `sys.stdout` / `sys.stderr`.
pub fn encoding(stream: Stream) -> Encoding {
    if env_nonempty("PYTHONLEGACYWINDOWSSTDIO").is_some() {
        return Encoding::Unknown;
    }
    if cfg!(windows) && is_console(stream) {
        return Encoding::Utf8;
    }
    if let Some(spec) = env_nonempty("PYTHONIOENCODING") {
        let enc = spec.split(':').next().unwrap_or("");
        if !enc.is_empty() {
            return if is_utf8_name(enc) {
                Encoding::Utf8
            } else if is_cp932_name(enc) {
                Encoding::Cp932
            } else {
                Encoding::Unknown
            };
        }
    }
    match std::env::var("PYTHONUTF8").ok().as_deref() {
        Some("1") => return Encoding::Utf8,
        Some("0") | None | Some("") => {}
        Some(_) => return Encoding::Unknown,
    }
    locale_encoding()
}

#[cfg(windows)]
fn locale_encoding() -> Encoding {
    // SAFETY: GetACP takes no arguments.
    let acp = unsafe { windows_sys::Win32::Globalization::GetACP() };
    match acp {
        65001 => Encoding::Utf8,
        932 => Encoding::Cp932,
        _ => Encoding::LegacyAscii,
    }
}

#[cfg(not(windows))]
fn locale_encoding() -> Encoding {
    let value = ["LC_ALL", "LC_CTYPE", "LANG"].iter().find_map(|k| env_nonempty(k));
    match value.as_deref() {
        // the C locale: Python coerces it and enables UTF-8 mode
        None | Some("C") | Some("POSIX") => Encoding::Utf8,
        Some(v) => {
            let lower = v.to_ascii_lowercase();
            if lower.contains("utf-8") || lower.contains("utf8") {
                Encoding::Utf8
            } else {
                Encoding::LegacyAscii
            }
        }
    }
}
