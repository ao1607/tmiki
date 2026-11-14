//! The rule for the paths a tree records (tmi/recpath.py, same rule, same
//! words), and the one way a recorded path becomes a file system path.
//!
//! A recorded path comes from data other people write (a clone, a fetch), so
//! it is never joined to the working tree before `RelPath::new` has
//! accepted it. The rule is the same on every OS: a repository made on Linux
//! must check out on Windows. What it refuses (each with its reason):
//!
//! - an empty path, an absolute path (`/x`, `//server/share`), a drive
//!   (`c:/x`, any `:`, which on NTFS is also a stream), a backslash (a
//!   record's separator is `/` only);
//! - control characters and the characters Windows cannot hold (`<>:"|?*`);
//! - a part that is empty, `.` or `..` (it would leave its directory);
//! - a part that ends with `.` or a space (Windows drops them: `.tmi.` is
//!   `.tmi`);
//! - a part that is a Windows device name (`CON`, `NUL`, `COM1`, `LPT1`, ...,
//!   with or without an extension);
//! - a part that is `.tmi` or `.git` in any letter case, or their NTFS short
//!   names (`TMI~1`, `GIT~1`), at any depth: `.tmi/hooks/` runs programs, and
//!   a `.git` directory is read by Git the same way.

use crate::error::{RepoError, Result};
use std::path::{Path, PathBuf};

const RESERVED_DEVICES: [&str; 6] = ["CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$"];

pub(crate) fn is_reserved_device(part: &str) -> bool {
    // Windows looks at the name before the first '.', without trailing spaces
    let stem = part.split('.').next().unwrap_or("").trim_end_matches(' ');
    let up = stem.to_ascii_uppercase();
    if RESERVED_DEVICES.contains(&up.as_str()) {
        return true;
    }
    for prefix in ["COM", "LPT"] {
        if let Some(rest) = up.strip_prefix(prefix) {
            let mut it = rest.chars();
            if let (Some(c), None) = (it.next(), it.next()) {
                if ('1'..='9').contains(&c) || matches!(c, '\u{b9}' | '\u{b2}' | '\u{b3}') {
                    return true;
                }
            }
        }
    }
    false
}

fn is_protected_dir(part: &str) -> bool {
    let low = part.to_ascii_lowercase();
    if low == ".tmi" || low == ".git" {
        return true;
    }
    for short in ["tmi~", "git~"] {
        if let Some(rest) = low.strip_prefix(short) {
            if !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()) {
                return true;
            }
        }
    }
    false
}

/// Why `path` cannot be a recorded path, or None. Same order and texts as
/// `tmi.recpath.record_path_problem`.
pub fn record_path_problem(path: &str) -> Option<String> {
    if path.is_empty() {
        return Some("it is empty".into());
    }
    if path.starts_with('/') {
        return Some("it is absolute".into());
    }
    for ch in path.chars() {
        if (ch as u32) < 32 || ch as u32 == 127 {
            return Some("it contains a control character".into());
        }
        if ch == '\\' {
            return Some("it contains a backslash".into());
        }
        if "<>:\"|?*".contains(ch) {
            return Some(format!("it contains '{ch}'"));
        }
    }
    let parts: Vec<&str> = path.split('/').collect();
    if parts.iter().any(|p| p.is_empty()) {
        return Some("it has an empty part (a doubled or trailing '/')".into());
    }
    if parts.iter().any(|p| *p == "." || *p == "..") {
        return Some("a part of it is '.' or '..'".into());
    }
    if parts.iter().any(|p| p.ends_with('.') || p.ends_with(' ')) {
        return Some("a part of it ends with '.' or a space".into());
    }
    if parts.iter().any(|p| is_reserved_device(p)) {
        return Some("a part of it is a reserved Windows device name".into());
    }
    if parts.iter().any(|p| is_protected_dir(p)) {
        return Some("a part of it is '.tmi' or '.git'".into());
    }
    None
}

/// `path` as a message shows it: controls and invisible characters
/// escaped (a path from a remote must not move the terminal's cursor),
/// long ones cut.
pub fn display_path(path: &str) -> String {
    let mut out = String::new();
    for (i, ch) in path.chars().enumerate() {
        if i == 120 {
            out.push_str("...");
            break;
        }
        let c = ch as u32;
        let hide = c < 0x20
            || (0x7f..=0x9f).contains(&c)
            || (0x200b..=0x200f).contains(&c)
            || (0x2028..=0x202e).contains(&c)
            || (0x2066..=0x2069).contains(&c)
            || c == 0xfeff;
        if !hide {
            out.push(ch);
        } else if c < 0x100 {
            out.push_str(&format!("\\x{c:02x}"));
        } else {
            out.push_str(&format!("\\u{c:04x}"));
        }
    }
    out
}

/// The words for a path a tree records that the rule refuses.
pub fn invalid_path_message(path: &str, problem: &str) -> String {
    format!("invalid path in a tree: {} ({problem})", display_path(path))
}

/// `Err` with `invalid_path_message` when `path` breaks the rule.
pub fn check_record_path(path: &str) -> Result<()> {
    match record_path_problem(path) {
        Some(problem) => Err(RepoError::runtime(invalid_path_message(path, &problem))),
        None => Ok(()),
    }
}

/// `check_record_path` for a file about to be recorded (`add`): the words
/// say what cannot be done, not where a tree came from.
pub fn check_new_record_path(path: &str) -> Result<()> {
    match record_path_problem(path) {
        Some(problem) => Err(RepoError::runtime(format!(
            "cannot record {}: its path is not allowed in a tree ({problem})",
            display_path(path)
        ))),
        None => Ok(()),
    }
}

/// A recorded path the rule accepted: the only thing `join_rel` takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelPath(String);

impl RelPath {
    pub fn new(path: &str) -> Result<RelPath> {
        check_record_path(path)?;
        Ok(RelPath(path.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// `root` and the parts of `rel`, joined with the platform separator.
pub fn join_rel(root: &Path, rel: &RelPath) -> PathBuf {
    let mut out = root.to_path_buf();
    for part in rel.0.split('/') {
        // RelPath holds no empty, '.' or '..' part; a second line of defence
        assert!(!part.is_empty() && part != "." && part != "..", "RelPath held a bad part");
        out.push(part);
    }
    out
}

/// `join_rel` of a text: `Err` when the rule refuses it.
pub fn join_checked(root: &Path, rel: &str) -> Result<PathBuf> {
    Ok(join_rel(root, &RelPath::new(rel)?))
}

/// For the names tmiki itself makes (the directories `init` creates, the
/// refs a rename moves): `rel`'s parts joined with the platform separator.
/// Never for a path a tree records.
pub fn join_trusted(root: &Path, rel: &str) -> PathBuf {
    let mut out = root.to_path_buf();
    for part in rel.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        out.push(part);
    }
    out
}

fn is_link(path: &Path) -> bool {
    match std::fs::symlink_metadata(path) {
        Ok(m) => m.file_type().is_symlink(),
        Err(_) => false,
    }
}

/// The file `rel` names under `worktree`: the rule, the place (inside the
/// working tree, outside `repo_dir`), and no symbolic link or junction on the
/// way (tmiki does not track links; writing through one would leave the working
/// tree). `Err` with a sentence otherwise.
pub fn worktree_path(worktree: &Path, repo_dir: &Path, rel: &str) -> Result<PathBuf> {
    let rel_path = RelPath::new(rel)?;
    let mut cur = worktree.to_path_buf();
    for part in rel_path.0.split('/') {
        cur.push(part);
        if is_link(&cur) {
            return Err(RepoError::runtime(format!(
                "refusing to use {}: it goes through a symbolic link or junction ({})",
                display_path(rel),
                display_path(&cur.strip_prefix(worktree).unwrap_or(&cur).to_string_lossy().replace('\\', "/"))
            )));
        }
    }
    if !cur.starts_with(worktree) || cur.starts_with(repo_dir) {
        return Err(RepoError::runtime(invalid_path_message(rel, "it is outside the working tree")));
    }
    Ok(cur)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn good_paths() {
        for p in [
            "a.clip",
            "dir/sub/x.png",
            "日本語/ファイル.mdp",
            "a b/c d.png",
            ".tmiattributes",
            ".tmiplugins/ora.wasm",
            ".gitignore",
            ".tmiignore",
            "console.png",
            "com10.png",
            "tmi~x/y",
            "a~1/b",
        ] {
            assert_eq!(record_path_problem(p), None, "{p}");
        }
    }

    #[test]
    fn bad_paths() {
        let bad = [
            "",
            "/abs",
            "//server/share/x",
            "c:/x",
            "c:x",
            "a\\b",
            "a//b",
            "a/",
            "../x",
            "a/../../x",
            "a/./b",
            ".",
            "a/..",
            ".tmi/hooks/post-commit.bat",
            ".TMI/hooks/x",
            ".Tmi./x",
            "sub/.tmi/x",
            "TMI~1/hooks/x",
            "tmi~12/x",
            ".git/config",
            ".GIT/config",
            "GIT~1/config",
            "a/CON",
            "con.txt",
            "NUL.tar.gz",
            "a/com1",
            "LPT9.x",
            "COM\u{b9}",
            "CONIN$",
            "x/aux .png",
            "a.",
            "a /b",
            "a:b",
            "x/f.txt:stream",
            "a<b",
            "a>b",
            "a\"b",
            "a|b",
            "a?b",
            "a*b",
            "a\u{1}b",
            "a\u{7f}b",
            "a\nb",
        ];
        for p in bad {
            assert!(record_path_problem(p).is_some(), "{p:?}");
        }
    }

    #[test]
    fn join_refuses() {
        assert!(join_checked(Path::new("r"), "../x").is_err());
        assert!(join_checked(Path::new("r"), "a/b").is_ok());
    }

    #[test]
    fn display_escapes() {
        assert_eq!(display_path("a\u{1b}[31mb"), "a\\x1b[31mb");
        assert_eq!(display_path(&"x".repeat(130)), format!("{}...", "x".repeat(120)));
    }
}
