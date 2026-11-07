//! `tmiki log` filters and date forms beyond 3a: `--grep` (Python's `re`, for
//! the patterns whose language Rust's regex reads the same), `--since` /
//! `--until` (cli.py `_parse_log_date`, `_commit_datetime`), `--author`
//! casefolded as `str.casefold`, and `--date local`.

use crate::casefold_table::CASEFOLD;
use crate::cmd_log::fwd;
use tmi_repo::clock;
use tmi_repo::{RepoError, Result};
use tmi_store::json::Value;

/// `str.casefold()` (the table of the Python version recorded with it).
pub fn casefold(s: &str) -> Result<String> {
    if s.is_ascii() {
        return Ok(s.to_ascii_lowercase());
    }
    let version = crate::pyenv::python_version().ok_or_else(|| fwd("casefold: Python version unknown"))?;
    if version.split('.').take(2).collect::<Vec<_>>().join(".") != crate::casefold_table::PYTHON_VERSION {
        return Err(fwd("casefold table of another Python version"));
    }
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match CASEFOLD.binary_search_by_key(&(c as u32), |(cp, _)| *cp) {
            Ok(i) => out.push_str(CASEFOLD[i].1),
            Err(_) => out.push(c),
        }
    }
    Ok(out)
}

/// `re.compile(pattern)` for a pattern whose meaning in Rust's regex is the
/// same set of matches; Forward for anything else (escapes of letters,
/// `{`, `(?`, `$` other than at the very end, classes with `[`, `\`, `&&`,
/// `--`, `~~` or a leading `:`, stacked quantifiers, and what Rust refuses).
pub fn py_regex(pattern: &str) -> Result<regex::Regex> {
    let not_here = || fwd("--grep pattern not reproduced natively");
    let chars: Vec<char> = pattern.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    let is_quant = |c: char| matches!(c, '*' | '+' | '?');
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\\' => {
                let next = *chars.get(i + 1).ok_or_else(not_here)?;
                if !".^$*+?()[]{}|\\/-".contains(next) {
                    return Err(not_here());
                }
                out.push('\\');
                out.push(next);
                i += 2;
                continue;
            }
            '{' | '}' => return Err(not_here()),
            '(' if chars.get(i + 1) == Some(&'?') => return Err(not_here()),
            '$' => {
                if i + 1 != chars.len() || pattern.contains('|') {
                    return Err(not_here());
                }
                out.push_str(r"\n?\z");
                i += 1;
                continue;
            }
            '[' => {
                let start = i;
                let mut j = i + 1;
                if chars.get(j) == Some(&'^') {
                    j += 1;
                }
                if chars.get(j) == Some(&']') {
                    j += 1;
                }
                if chars.get(j) == Some(&':') {
                    return Err(not_here());
                }
                while j < chars.len() && chars[j] != ']' {
                    if chars[j] == '[' || chars[j] == '\\' {
                        return Err(not_here());
                    }
                    j += 1;
                }
                if j >= chars.len() {
                    return Err(not_here()); // unterminated: Python's error
                }
                let body: String = chars[start..=j].iter().collect();
                if body.contains("&&") || body.contains("--") || body.contains("~~") {
                    return Err(not_here());
                }
                out.push_str(&body);
                i = j + 1;
                continue;
            }
            c if is_quant(c) => {
                // a quantifier may be followed by one lazy '?', nothing else
                if let Some(&n) = chars.get(i + 1) {
                    if is_quant(n) {
                        if n != '?' || chars.get(i + 2).is_some_and(|&m| is_quant(m)) {
                            return Err(not_here());
                        }
                    }
                }

                out.push(c);
            }
            c => {
                if c.is_ascii_punctuation() && !".^*+?()|".contains(c) && !c.is_alphanumeric() {
                    // a literal; escape what Rust treats as meta
                    if "#&-~".contains(c) {
                        out.push('\\');
                    }
                }
                out.push(c);
            }
        }
        i += 1;
    }
    regex::Regex::new(&out).map_err(|_| not_here())
}

/// cli.py `_parse_log_date(value)`: the UTC instant in microseconds.
pub fn parse_log_date(value: &str) -> Result<i128> {
    let text = tmi_repo::repo::py_strip(value);
    match clock::parse_iso(text) {
        Some(iso) => Ok(iso.instant_utc()),
        None => {
            if text.is_ascii() && !text.bytes().any(|b| b.is_ascii_digit()) {
                return Err(RepoError::runtime(format!("could not parse date: {value}")));
            }
            Err(fwd("date fromisoformat may read differently"))
        }
    }
}

/// cli.py `_commit_datetime(commit)`: None when there is no str timestamp.
pub fn commit_instant(commit: &Value) -> Result<Option<i128>> {
    match commit.get("timestamp") {
        Some(Value::Str(raw)) if !raw.is_empty() => match clock::parse_iso(raw) {
            Some(iso) => Ok(Some(iso.instant_utc())),
            None => Err(fwd("commit timestamp fromisoformat may read differently")),
        },
        _ => Ok(None),
    }
}

/// The local zone's offset from UTC in seconds at `secs` (Unix time), as
/// CPython's `astimezone()` reads it: on POSIX `localtime_r(secs).tm_gmtoff`
/// (the same C library and TZ, so any zone; stage 4b), on Windows only a
/// zone without daylight saving rules and no TZ (the CRT's).
fn local_offset_secs(secs: i64) -> Option<i32> {
    #[cfg(unix)]
    {
        extern "C" {
            fn tzset();
        }
        let t = secs as libc::time_t;
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        // SAFETY: tzset reads TZ; localtime_r fills a zeroed out-struct.
        let ok = unsafe {
            tzset();
            !libc::localtime_r(&t, &mut tm).is_null()
        };
        ok.then_some(tm.tm_gmtoff as i32)
    }
    #[cfg(windows)]
    {
        let _ = secs;
        if std::env::var_os("TZ").is_some() {
            return None;
        }
        use windows_sys::Win32::System::Time::{GetTimeZoneInformation, TIME_ZONE_INFORMATION};
        let mut tz: TIME_ZONE_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a zeroed out-struct.
        let r = unsafe { GetTimeZoneInformation(&mut tz) };
        if r == u32::MAX || tz.StandardDate.wMonth != 0 || tz.DaylightDate.wMonth != 0 {
            return None;
        }
        Some(-tz.Bias * 60)
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = secs;
        None
    }
}

/// `dt.astimezone().isoformat(sep=" ", timespec="seconds")`.
pub fn local_date(commit: &Value) -> Result<Option<String>> {
    let Some(micros) = commit_instant(commit)? else { return Ok(None) };
    let secs = micros.div_euclid(1_000_000) as i64;
    if !(0..32_503_680_000).contains(&secs) {
        return Err(fwd("--date local: time outside localtime's range"));
    }
    let offset = local_offset_secs(secs).ok_or_else(|| fwd("--date local: local time zone not reproduced"))?;
    let f = clock::fields(clock::Utc { secs: secs + offset as i64, micros: 0 });
    let sign = if offset < 0 { '-' } else { '+' };
    let a = offset.unsigned_abs();
    let mut off = format!("{sign}{:02}:{:02}", a / 3600, a / 60 % 60);
    if a % 60 != 0 {
        off += &format!(":{:02}", a % 60);
    }
    Ok(Some(format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}{off}",
        f.year, f.month, f.day, f.hour, f.minute, f.second
    )))
}

