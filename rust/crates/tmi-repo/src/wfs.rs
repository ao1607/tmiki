//! Writing repository files as repo.py does (stage 3b-2 helpers for add,
//! commit and status): `_atomic_write_bytes`, the index and stat cache
//! JSON, refs, reflog lines, `_object_timestamp` and the author identity.

use crate::error::{RepoError, Result};
use crate::repo::Repo;
use std::io::Write;
use std::path::Path;
use tmi_store::json::{self, DumpOptions, Value};

fn io_err(path: &Path, e: std::io::Error) -> RepoError {
    RepoError::forward(format!("{}: {e}", path.display()))
}

/// `_atomic_write_bytes(path, data)`: a sibling temp file, then replace.
pub fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
    crate::fsio::mark_mutated();
    atomic_write_unmarked(path, data)
}

/// `atomic_write` without marking the run as having written (the stat
/// cache: see fsio::atomic_write_unmarked).
pub fn atomic_write_unmarked(path: &Path, data: &[u8]) -> Result<()> {
    let parent = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent).map_err(|e| io_err(parent, e))?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let mut n = 0u32;
    let (tmp, mut file) = loop {
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        let tmp = parent.join(format!("{name}.{:x}{:x}{n}.tmp", std::process::id(), stamp & 0xffff_ffff));
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp) {
            Ok(f) => break (tmp, f),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && n < 1000 => n += 1,
            Err(e) => return Err(io_err(&tmp, e)),
        }
    };
    let result = file.write_all(data).and_then(|_| file.flush());
    drop(file);
    if let Err(e) = result.and_then(|_| std::fs::rename(&tmp, path)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(io_err(path, e));
    }
    Ok(())
}

/// `json.dumps(v, ensure_ascii=False, sort_keys=True, indent=2) + "\n"`.
pub fn indented_json(v: &Value) -> Vec<u8> {
    let mut s = json::dumps(v, &DumpOptions::INDENTED_SORTED);
    s.push('\n');
    s.into_bytes()
}

/// `write_index(index)`.
pub fn write_index(repo: &Repo, index: &Value) -> Result<()> {
    repo.write_index(index)
}

/// `write_ref(ref, value)`.
pub fn write_ref(repo: &Repo, name: &str, value: &str) -> Result<()> {
    repo.write_ref(name, value)
}

/// `set_current_commit(commit, expected=...)`.
pub fn set_current_commit(repo: &Repo, commit: &str, expect: crate::lockfile::Expect<'_>) -> Result<()> {
    repo.set_current_commit_expect(commit, expect)
}

/// Days since 1970-01-01 to (year, month, day) (proleptic Gregorian).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
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

/// UTC now as (y, mo, d, h, mi, s, microseconds).
fn utc_now() -> (i64, u32, u32, u32, u32, u32, u32) {
    let d = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs() as i64;
    let (y, mo, da) = civil_from_days(secs.div_euclid(86_400));
    let rem = secs.rem_euclid(86_400) as u32;
    (y, mo, da, rem / 3600, rem / 60 % 60, rem % 60, d.subsec_micros())
}

/// `datetime.now(timezone.utc).isoformat()`.
pub fn now_isoformat() -> String {
    let (y, mo, d, h, mi, s, us) = utc_now();
    if us == 0 {
        format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}+00:00")
    } else {
        format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{us:06}+00:00")
    }
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        _ => 28,
    }
}

/// Whether `datetime.fromisoformat` surely accepts `s`, for the common
/// form `YYYY-MM-DD[THH:MM[:SS[.ffffff]]][Z|±HH:MM]`. None: not sure.
fn plain_isoformat(s: &str) -> Option<bool> {
    let b = s.as_bytes();
    let digits = |r: std::ops::Range<usize>| -> Option<u32> {
        let part = b.get(r)?;
        part.iter().all(u8::is_ascii_digit).then(|| std::str::from_utf8(part).unwrap().parse().unwrap())
    };
    if b.len() < 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let (y, mo, d) = (digits(0..4)? as i64, digits(5..7)?, digits(8..10)?);
    if y < 1 || !(1..=12).contains(&mo) || d < 1 || d > days_in_month(y, mo) {
        return Some(false);
    }
    let mut i = 10;
    if i == b.len() {
        return Some(true);
    }
    if b[i] != b'T' && b[i] != b' ' {
        return None;
    }
    i += 1;
    let h = digits(i..i + 2)?;
    if b.get(i + 2) != Some(&b':') {
        return None;
    }
    let mi = digits(i + 3..i + 5)?;
    i += 5;
    let mut sec = 0;
    if b.get(i) == Some(&b':') {
        sec = digits(i + 1..i + 3)?;
        i += 3;
        if b.get(i) == Some(&b'.') {
            let start = i + 1;
            let mut j = start;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            if j - start != 6 && j - start != 3 {
                return None;
            }
            i = j;
        }
    }
    if h > 23 || mi > 59 || sec > 59 {
        return Some(false);
    }
    match b.get(i) {
        None => Some(true),
        Some(b'Z') if i + 1 == b.len() => Some(true),
        Some(b'+') | Some(b'-') if b.len() == i + 6 && b[i + 3] == b':' => {
            let (th, tm) = (digits(i + 1..i + 3)?, digits(i + 4..i + 6)?);
            Some(th <= 23 && tm <= 59)
        }
        _ => None,
    }
}

/// `_object_timestamp()`.
pub fn object_timestamp() -> Result<String> {
    match std::env::var("TMI_COMMIT_TIMESTAMP") {
        Ok(fixed) if !fixed.is_empty() => match plain_isoformat(&fixed) {
            Some(true) => Ok(fixed),
            _ => Err(RepoError::forward("TMI_COMMIT_TIMESTAMP in a form fromisoformat decides")),
        },
        _ => Ok(now_isoformat()),
    }
}

/// `reflog.append(repo_dir, ref_name, old, new, identity, reason)`: text
/// mode, so "\r\n" on Windows.
pub fn reflog_append(repo: &Repo, ref_name: &str, old: Option<&str>, new: &str, identity: Option<&(String, String)>, reason: &str) {
    let path = if ref_name == "HEAD" { repo.repo_dir.join("logs").join("HEAD") } else { repo.repo_dir.join("logs").join(ref_name) };
    let name = identity.map(|i| i.0.as_str()).filter(|n| !n.is_empty()).unwrap_or("tmiki");
    let email = identity.map(|i| i.1.as_str()).unwrap_or("");
    let old = old.unwrap_or(crate::reflog::ZERO_OID);
    let (y, mo, d, h, mi, s, _) = utc_now();
    let reason = reason.replace(['\n', '\t'], " ");
    let nl = if cfg!(windows) { "\r\n" } else { "\n" };
    let line = format!("{old} {new} {name} <{email}> {y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z\t{reason}{nl}");
    if let Some(parent) = path.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = f.write_all(line.as_bytes());
    }
}

/// `config.resolve_identity(repo_config)`: (name, email).
pub fn resolve_identity(repo: &Repo) -> Result<(String, String)> {
    let repo_cfg = repo.read_config()?;
    let user_cfg = crate::config::read_user_config()?;
    let pick = |env: &str, key: &str| -> Result<Value> {
        if let Ok(v) = std::env::var(env) {
            if !v.is_empty() {
                return Ok(Value::Str(v));
            }
        }
        let repo_v = crate::config::get_value(&repo_cfg, key)?;
        Ok(match repo_v {
            Some(v) if !v.is_null() => v.clone(),
            _ => crate::config::get_value(&user_cfg, key)?.cloned().unwrap_or(Value::Null),
        })
    };
    let name = pick("TMI_AUTHOR_NAME", "user.name")?;
    let email = pick("TMI_AUTHOR_EMAIL", "user.email")?;
    if !name.truthy() || !email.truthy() {
        return Err(RepoError::runtime(
            "author identity is not configured.\n  Set it with:\n    tmiki config --global user.name \"Your Name\"\n    \
             tmiki config --global user.email \"you@example.com\"\n  Or per-repo by omitting --global, or via env vars \
             TMI_AUTHOR_NAME / TMI_AUTHOR_EMAIL.",
        ));
    }
    let s = |v: &Value| v.py_str().ok_or_else(|| RepoError::forward("identity is a container"));
    Ok((s(&name)?, s(&email)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_738), (2026, 10, 12));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
    }

    #[test]
    fn isoformat_forms() {
        assert_eq!(plain_isoformat("2026-01-01T09:00:00+00:00"), Some(true));
        assert_eq!(plain_isoformat("2026-01-01"), Some(true));
        assert_eq!(plain_isoformat("2026-02-30T09:00:00"), Some(false));
        assert_eq!(plain_isoformat("2026-01-01T09:00:00.123456Z"), Some(true));
        assert_eq!(plain_isoformat("yesterday"), None);
    }
}
