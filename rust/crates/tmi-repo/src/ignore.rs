//! tmi/ignore.py: `.tmiignore` patterns (file and file:layer) and matching.

use crate::error::{RepoError, Result};
use crate::py;
use std::path::Path;

#[derive(Clone, Debug)]
pub struct IgnorePattern {
    pub layer: bool,
    pub negate: bool,
    pub file_glob: String,
    pub layer_glob: Option<String>,
}

fn split_layer(line: &str) -> (String, Option<String>) {
    match line.rfind(':') {
        Some(pos) if pos > 0 && pos + 1 < line.len() => (line[..pos].to_string(), Some(line[pos + 1..].to_string())),
        _ => (line.to_string(), None),
    }
}

/// `parse_lines(lines)`.
pub fn parse_lines<'a>(lines: impl IntoIterator<Item = &'a str>) -> Vec<IgnorePattern> {
    let mut out = Vec::new();
    for raw in lines {
        let line = crate::repo::py_strip(raw);
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (negate, line) = match line.strip_prefix('!') {
            Some(rest) => (true, rest),
            None => (false, line),
        };
        let (file_glob, layer_glob) = split_layer(line);
        out.push(IgnorePattern { layer: layer_glob.is_some(), negate, file_glob, layer_glob });
    }
    out
}

/// `parse_file(path)`.
pub fn parse_file(path: &Path) -> Result<Vec<IgnorePattern>> {
    match crate::repo::read_text(path)? {
        None => Ok(Vec::new()),
        Some(text) => Ok(parse_lines(py::splitlines(&text))),
    }
}

/// `_match_path(rel, pattern)`.
pub fn match_path(rel: &str, pattern: &str) -> Result<bool> {
    let fail = |_| RepoError::forward("ignore pattern Python would reject");
    let rel_posix = rel.replace('\\', "/");
    let pat = pattern.replace('\\', "/");
    if py::pure_posix_match(&rel_posix, &pat).map_err(fail)? {
        return Ok(true);
    }
    if py::fnmatch(&rel_posix, &pat).map_err(fail)? {
        return Ok(true);
    }
    let name = rel_posix.rsplit('/').next().unwrap_or("");
    py::fnmatch(name, &pat).map_err(fail)
}

/// `is_file_ignored(rel, patterns)`: the last matching file pattern decides.
pub fn is_file_ignored(rel: &str, patterns: &[IgnorePattern]) -> Result<bool> {
    let mut decided = false;
    for pat in patterns {
        if pat.layer {
            continue;
        }
        if match_path(rel, &pat.file_glob)? {
            decided = !pat.negate;
        }
    }
    Ok(decided)
}

/// repo.py `parsed_ignore_patterns()`: the baseline plus `.tmiignore`.
pub fn repo_patterns(worktree: Option<&Path>) -> Result<Vec<IgnorePattern>> {
    let mut patterns = parse_lines([".tmi/*", ".git/*", ".tmi-diff/*", ".*-smoke-*/*", "__pycache__/*", "*.pyc", "*.pyo"]);
    if let Some(root) = worktree {
        patterns.extend(parse_file(&root.join(".tmiignore"))?);
    }
    Ok(patterns)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn baseline_and_layers() {
        let pats = parse_lines(["*.psd", "!keep.psd", "art/*.clip:下書き*", "# c", "  ", "a:"]);
        assert_eq!(pats.len(), 4);
        assert!(pats[2].layer && pats[2].layer_glob.as_deref() == Some("下書き*"));
        assert!(!pats[3].layer && pats[3].file_glob == "a:");
        assert!(is_file_ignored("x/y.psd", &pats).unwrap());
        assert!(!is_file_ignored("keep.psd", &pats).unwrap());
        let base = repo_patterns(None).unwrap();
        assert!(is_file_ignored(".tmi/objects", &base).unwrap());
        assert!(is_file_ignored("a/b.pyc", &base).unwrap());
        assert!(!is_file_ignored("a.png", &base).unwrap());
        #[cfg(windows)]
        assert!(is_file_ignored("A/B.PYC", &base).unwrap());
    }
}
