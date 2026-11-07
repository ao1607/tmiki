//! tmi/reflog.py, reading: `<old> <new> <name> <<email>> <ts>\t<reason>`.

use crate::error::Result;
use crate::py;
use std::path::Path;

pub const ZERO_OID: &str = "0000000000000000000000000000000000000000000000000000000000000000";

#[derive(Clone, Debug)]
pub struct ReflogEntry {
    pub old_oid: String,
    pub new_oid: String,
    pub name: String,
    pub email: String,
    pub timestamp: String,
    pub reason: String,
}

fn parse_line(raw: &str) -> Option<ReflogEntry> {
    if crate::repo::py_strip(raw).is_empty() {
        return None;
    }
    let (head, reason) = raw.split_once('\t')?;
    let parts: Vec<&str> = head.split(' ').collect();
    if parts.len() < 5 {
        return None;
    }
    let middle = &parts[2..];
    let timestamp = middle[middle.len() - 1];
    let email_tok = middle[middle.len() - 2];
    let name = middle[..middle.len() - 2].join(" ");
    Some(ReflogEntry {
        old_oid: parts[0].to_string(),
        new_oid: parts[1].to_string(),
        name,
        email: email_tok.trim_matches(|c| c == '<' || c == '>').to_string(),
        timestamp: timestamp.to_string(),
        reason: reason.to_string(),
    })
}

fn log_path(repo_dir: &Path, ref_name: &str) -> std::path::PathBuf {
    let mut path = repo_dir.join("logs");
    for part in ref_name.split('/') {
        path.push(part);
    }
    path
}

/// `append(repo_dir, ref_name, old, new, identity, reason)`: one line, in
/// text mode ("\r\n" on Windows), UTF-8.
pub fn append(repo_dir: &Path, ref_name: &str, old: Option<&str>, new: &str, identity: Option<(&str, &str)>, reason: &str) -> Result<()> {
    let path = log_path(repo_dir, ref_name);
    crate::fsio::mkdir_parent(&path)?;
    let (name, email) = identity.unwrap_or(("", ""));
    let name = if name.is_empty() { "tmi" } else { name };
    let old = old.filter(|o| !o.is_empty()).unwrap_or(ZERO_OID);
    let timestamp = crate::clock::reflog_stamp(crate::clock::now());
    let safe_reason = reason.replace(['\n', '\t'], " ");
    let line = format!("{old} {new} {name} <{email}> {timestamp}\t{safe_reason}\n");
    crate::fsio::append(&path, &crate::fsio::text_bytes(&line))
}

/// Every reflog file: (path, its lines after read_text + splitlines).
pub fn all_logs(repo_dir: &Path) -> Result<Vec<(std::path::PathBuf, Vec<String>)>> {
    let logs = repo_dir.join("logs");
    if !logs.is_dir() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for (_rel, path) in crate::repo::walk_files(&logs)? {
        let text = crate::repo::read_text(&path)?.unwrap_or_default();
        out.push((path, py::splitlines(&text).into_iter().map(str::to_string).collect()));
    }
    Ok(out)
}

/// One field of `_strptime`'s regex: its alternatives, tried in order.
/// Each alternative is a list of byte classes ('d' = an ASCII digit).
const FIELDS: [&[&[&str]]; 6] = [
    &[&["d", "d", "d", "d"]],
    &[&["1", "0-2"], &["0", "1-9"], &["1-9"]],
    &[&["3", "01"], &["12", "d"], &["0", "1-9"], &["1-9"], &[" ", "1-9"]],
    &[&["2", "0-3"], &["0-1", "d"], &["d"]],
    &[&["0-5", "d"], &["d"]],
    &[&["6", "0-1"], &["0-5", "d"], &["d"]],
];
/// The literal after each field (matched case-insensitively).
const SEPS: [u8; 6] = [b'-', b'-', b'T', b':', b':', b'Z'];

fn class_hit(class: &str, c: u8) -> bool {
    let k = class.as_bytes();
    match class {
        "d" => c.is_ascii_digit(),
        _ if k.len() == 3 && k[1] == b'-' => (k[0]..=k[2]).contains(&c),
        _ => k.contains(&c),
    }
}

/// `re.match` of the regex at `pos` for fields `idx..`: the end of the
/// first match in Python's backtracking order, with the field values.
fn match_from(b: &[u8], idx: usize, pos: usize, vals: &mut Vec<u32>) -> Option<usize> {
    if idx == FIELDS.len() {
        return Some(pos);
    }
    for alt in FIELDS[idx] {
        let mut j = pos;
        let mut val = 0u32;
        let mut ok = true;
        for class in alt.iter() {
            match b.get(j) {
                Some(&c) if class_hit(class, c) => {
                    if c.is_ascii_digit() {
                        val = val * 10 + (c - b'0') as u32;
                    }
                    j += 1;
                }
                _ => {
                    ok = false;
                    break;
                }
            }
        }
        if !ok || !b.get(j).is_some_and(|c| c.eq_ignore_ascii_case(&SEPS[idx])) {
            continue;
        }
        vals.push(val);
        if let Some(end) = match_from(b, idx + 1, j + 1, vals) {
            return Some(end);
        }
        vals.pop();
    }
    None
}

/// `datetime.strptime(ts, "%Y-%m-%dT%H:%M:%SZ")` as UTC seconds; None for
/// a ValueError; Err(()) for text whose reading is not reproduced.
fn strptime_reflog(ts: &str) -> std::result::Result<Option<i64>, ()> {
    if !ts.is_ascii() {
        return Err(()); // \d and IGNORECASE reach beyond ASCII
    }
    let b = ts.as_bytes();
    let mut vals = Vec::new();
    let Some(end) = match_from(b, 0, 0, &mut vals) else { return Ok(None) };
    if end != b.len() {
        return Ok(None); // "unconverted data remains"
    }
    let (y, mo, d, h, mi, s) = (vals[0] as i64, vals[1], vals[2], vals[3], vals[4], vals[5]);
    let dim = match mo {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        _ => 28,
    };
    if y < 1 || d > dim || s > 59 {
        return Ok(None);
    }
    Ok(Some(crate::clock::days_from_civil(y, mo, d) * 86_400 + (h * 3600 + mi * 60 + s) as i64))
}
/// What `prune(repo_dir, max_age_days)` would do: per log file, the lines
/// it keeps (None: the file is deleted), and the number of entries removed.
/// Err(Forward) for timestamps whose reading is not reproduced.
pub struct PrunePlan {
    pub files: Vec<(std::path::PathBuf, Option<Vec<String>>)>,
    pub removed: usize,
}

pub fn plan_prune(repo_dir: &Path, max_age_days: i64) -> Result<PrunePlan> {
    let now = crate::clock::now();
    let now_f = now.secs as f64 + now.micros as f64 / 1e6;
    let cutoff = now_f - (max_age_days * 86_400) as f64;
    let mut plan = PrunePlan { files: Vec::new(), removed: 0 };
    for (path, lines) in all_logs(repo_dir)? {
        let mut kept = Vec::new();
        for raw in lines {
            let Some(entry) = parse_line(&raw) else { continue };
            match strptime_reflog(&entry.timestamp) {
                Err(()) => return Err(crate::RepoError::forward("reflog timestamp strptime is not reproduced")),
                Ok(None) => kept.push(raw),
                Ok(Some(ts)) => {
                    if ts as f64 >= cutoff {
                        kept.push(raw);
                    } else {
                        plan.removed += 1;
                    }
                }
            }
        }
        plan.files.push((path, (!kept.is_empty()).then_some(kept)));
    }
    Ok(plan)
}

impl PrunePlan {
    /// The oids `iter_all_reachable_oids` yields after the prune.
    pub fn reachable_oids(&self) -> Vec<String> {
        let mut out = Vec::new();
        for (_, kept) in &self.files {
            for raw in kept.iter().flatten() {
                if let Some(e) = parse_line(raw) {
                    if e.old_oid != ZERO_OID {
                        out.push(e.old_oid);
                    }
                    out.push(e.new_oid);
                }
            }
        }
        out
    }

    pub fn apply(&self) -> Result<()> {
        for (path, kept) in &self.files {
            match kept {
                Some(lines) => crate::fsio::write_text(path, &(lines.join("\n") + "\n"))?,
                None => crate::fsio::unlink(path)?,
            }
        }
        Ok(())
    }
}

/// `resolve_at_spec(repo_dir, spec)`: HEAD@{N}.
pub fn resolve_at_spec(repo_dir: &Path, spec: &str) -> Result<Option<String>> {
    let Some(inner) = spec.strip_prefix("HEAD@{").and_then(|s| s.strip_suffix('}')) else {
        return Ok(None);
    };
    // int(): plain ASCII digits with an optional sign; other texts Python's
    // int() might read are left to Python.
    let digits = inner.strip_prefix(['-', '+']).unwrap_or(inner);
    if digits.is_empty() || !digits.bytes().all(|c| c.is_ascii_digit()) {
        if inner.is_ascii() && !inner.contains(|c: char| c.is_ascii_whitespace() || c == '_') {
            return Ok(None);
        }
        return Err(crate::RepoError::forward("HEAD@{...} that int() may read"));
    }
    let Ok(index) = inner.parse::<i64>() else { return Ok(None) };
    let entries = read(repo_dir, "HEAD")?;
    if index < 0 || index as usize >= entries.len() {
        return Ok(None);
    }
    Ok(Some(entries[entries.len() - 1 - index as usize].new_oid.clone()))
}

/// `read(repo_dir, ref_name)`.
pub fn read(repo_dir: &Path, ref_name: &str) -> Result<Vec<ReflogEntry>> {
    let path = log_path(repo_dir, ref_name);
    let Some(text) = crate::repo::read_text(&path)? else {
        return Ok(Vec::new());
    };
    // read_text's universal newlines, then splitlines(): splitlines alone
    // already breaks at "\r" and "\r\n".
    Ok(py::splitlines(&text).into_iter().filter_map(parse_line).collect())
}
