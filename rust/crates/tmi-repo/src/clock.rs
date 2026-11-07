//! Times as tmi writes and reads them: `datetime.now(timezone.utc)` in its
//! `isoformat()` / `strftime` forms, `time.time_ns()`, and the ISO 8601
//! texts `datetime.fromisoformat` reads (the forms tmi itself writes, and
//! the plain ones a user types; anything else is left to Python).

use std::time::{SystemTime, UNIX_EPOCH};

/// A UTC instant with microseconds, as `datetime.now(timezone.utc)` holds it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Utc {
    pub secs: i64,
    pub micros: u32,
}

pub fn now() -> Utc {
    let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    Utc { secs: d.as_secs() as i64, micros: d.subsec_micros() }
}

/// `time.time_ns()`.
pub fn time_ns() -> i128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos() as i128)
}

/// Days since 1970-01-01 to (year, month, day) (Howard Hinnant's algorithm).
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let m = m as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

pub struct Fields {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
    pub micros: u32,
}

pub fn fields(t: Utc) -> Fields {
    let days = t.secs.div_euclid(86_400);
    let rem = t.secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    Fields {
        year,
        month,
        day,
        hour: (rem / 3600) as u32,
        minute: (rem / 60 % 60) as u32,
        second: (rem % 60) as u32,
        micros: t.micros,
    }
}

/// `dt.isoformat()` of an aware UTC datetime.
pub fn isoformat_utc(t: Utc) -> String {
    let f = fields(t);
    let frac = if f.micros == 0 { String::new() } else { format!(".{:06}", f.micros) };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}{frac}+00:00",
        f.year, f.month, f.day, f.hour, f.minute, f.second
    )
}

/// `strftime("%Y%m%d%H%M%S%f")`.
pub fn id_stamp(t: Utc) -> String {
    let f = fields(t);
    format!(
        "{:04}{:02}{:02}{:02}{:02}{:02}{:06}",
        f.year, f.month, f.day, f.hour, f.minute, f.second, f.micros
    )
}

/// `strftime("%Y-%m-%dT%H:%M:%SZ")` (the reflog).
pub fn reflog_stamp(t: Utc) -> String {
    let f = fields(t);
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", f.year, f.month, f.day, f.hour, f.minute, f.second)
}

/// What `datetime.fromisoformat(text)` gives, when it is certain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Iso {
    /// An aware time: the UTC instant in microseconds, and the offset.
    Aware { micros: i128, offset_secs: i32 },
    /// A naive time: its fields as microseconds since 1970 read as UTC.
    Naive { micros: i128 },
}

impl Iso {
    /// `dt if dt.tzinfo else dt.replace(tzinfo=timezone.utc)`, as an instant.
    pub fn instant_utc(self) -> i128 {
        match self {
            Iso::Aware { micros, .. } | Iso::Naive { micros } => micros,
        }
    }
}

fn digits(s: &[u8], n: usize) -> Option<u32> {
    if s.len() < n || !s[..n].iter().all(u8::is_ascii_digit) {
        return None;
    }
    Some(s[..n].iter().fold(0u32, |a, b| a * 10 + (b - b'0') as u32))
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        _ => 28,
    }
}

/// `datetime.fromisoformat(text)` for `YYYY-MM-DD` optionally followed by
/// one separator character and `HH:MM[:SS[.f{1,6}]]` with an optional `Z`
/// or `±HH:MM` offset. None for every other text, valid or not (the caller
/// leaves those to Python).
pub fn parse_iso(text: &str) -> Option<Iso> {
    if !text.is_ascii() {
        return None;
    }
    let b = text.as_bytes();
    if b.len() < 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let year = digits(b, 4)? as i64;
    let month = digits(&b[5..], 2)?;
    let day = digits(&b[8..], 2)?;
    if year < 1 || !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) {
        return None;
    }
    let days = days_from_civil(year, month, day);
    if b.len() == 10 {
        return Some(Iso::Naive { micros: days as i128 * 86_400_000_000 });
    }
    let t = &b[11..];
    let hour = digits(t, 2)?;
    if t.len() < 5 || t[2] != b':' {
        return None;
    }
    let minute = digits(&t[3..], 2)?;
    let mut i = 5;
    let mut second = 0;
    let mut micros = 0u32;
    if t.len() > i && t[i] == b':' {
        second = digits(&t[i + 1..], 2)?;
        i += 3;
        if t.len() > i && t[i] == b'.' {
            let start = i + 1;
            let mut end = start;
            while end < t.len() && t[end].is_ascii_digit() {
                end += 1;
            }
            let n = end - start;
            if n == 0 || n > 6 {
                return None;
            }
            micros = digits(&t[start..], n)? * 10u32.pow((6 - n) as u32);
            i = end;
        }
    }
    if hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let local = (days as i128 * 86_400 + hour as i128 * 3600 + minute as i128 * 60 + second as i128) * 1_000_000
        + micros as i128;
    let rest = &t[i..];
    if rest.is_empty() {
        return Some(Iso::Naive { micros: local });
    }
    if rest == b"Z" {
        return Some(Iso::Aware { micros: local, offset_secs: 0 });
    }
    if rest.len() == 6 && (rest[0] == b'+' || rest[0] == b'-') && rest[3] == b':' {
        let oh = digits(&rest[1..], 2)?;
        let om = digits(&rest[4..], 2)?;
        if oh > 23 || om > 59 {
            return None;
        }
        let off = (oh * 3600 + om * 60) as i32 * if rest[0] == b'-' { -1 } else { 1 };
        return Some(Iso::Aware { micros: local - off as i128 * 1_000_000, offset_secs: off });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        let t = Utc { secs: 1_767_258_000, micros: 0 }; // 2026-01-01T09:00:00Z
        assert_eq!(isoformat_utc(t), "2026-01-01T09:00:00+00:00");
        assert_eq!(isoformat_utc(Utc { micros: 120, ..t }), "2026-01-01T09:00:00.000120+00:00");
        assert_eq!(id_stamp(Utc { micros: 5, ..t }), "20260101090000000005");
        assert_eq!(reflog_stamp(t), "2026-01-01T09:00:00Z");
        for d in [-1_000_000i64, -1, 0, 1, 59, 60, 365, 10_957, 20_000, 2_000_000] {
            let (y, m, dd) = civil_from_days(d);
            assert_eq!(days_from_civil(y, m, dd), d);
        }
    }

    #[test]
    fn parse() {
        let base = 1_767_258_000i128 * 1_000_000;
        assert_eq!(parse_iso("2026-01-01T09:00:00+00:00"), Some(Iso::Aware { micros: base, offset_secs: 0 }));
        assert_eq!(parse_iso("2026-01-01T18:00:00+09:00"), Some(Iso::Aware { micros: base, offset_secs: 32_400 }));
        assert_eq!(parse_iso("2026-01-01 09:00"), Some(Iso::Naive { micros: base }));
        assert_eq!(parse_iso("2026-01-01T09:00:00.5Z"), Some(Iso::Aware { micros: base + 500_000, offset_secs: 0 }));
        assert_eq!(parse_iso("2026-01-01"), Some(Iso::Naive { micros: base - 9 * 3_600_000_000 }));
        assert_eq!(parse_iso("2026-02-30"), None);
        assert_eq!(parse_iso("20260101"), None);
        assert_eq!(parse_iso("2026-01-01T09"), None);
    }
}
