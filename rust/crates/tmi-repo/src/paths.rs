//! repo.py's pathspec helpers: `relpath`, `pathspec_full_path`,
//! `pathspec_relpath`, `split_pathspec`, and `str(Path(text))`.

use crate::error::{RepoError, Result};
use crate::py;
use crate::repo::Repo;
use std::path::{Path, PathBuf};

fn cwd() -> Result<PathBuf> {
    std::env::current_dir().map_err(|e| RepoError::forward(e.to_string()))
}

impl Repo {
    /// `relpath(path)`: posix path of `path.resolve()` under the worktree.
    pub fn relpath(&self, path: &Path) -> Result<String> {
        let root = self.require_worktree()?;
        let full = py::realpath(path);
        match py::relative_posix(&full, root) {
            Some(rel) => Ok(if rel.is_empty() { ".".into() } else { rel }),
            None => Err(RepoError::runtime(format!("path is outside working tree: {}", full.display()))),
        }
    }

    /// `pathspec_full_path(spec)`.
    pub fn pathspec_full_path(&self, spec: &str) -> Result<PathBuf> {
        let path = Path::new(spec);
        if path.is_absolute() {
            return Ok(py::realpath(path));
        }
        let cwd_path = py::realpath(&cwd()?.join(path));
        if cwd_path.exists() {
            return Ok(cwd_path);
        }
        let root_path = py::realpath(&self.require_worktree()?.join(path));
        if root_path.exists() {
            return Ok(root_path);
        }
        Ok(cwd_path)
    }

    /// repo.py `pathspec_link(spec)`: the worktree-relative path of the
    /// symbolic link or junction `spec` is, or lies beyond (links are not
    /// followed), when there is one (issue #44).
    pub fn pathspec_link(&self, spec: &str) -> Result<Option<String>> {
        let root = self.require_worktree()?.to_path_buf();
        let path = Path::new(spec);
        let candidates = if path.is_absolute() { vec![path.to_path_buf()] } else { vec![cwd()?.join(path), root.join(path)] };
        for cand in candidates {
            let cand = py::abspath(&cand);
            if std::fs::symlink_metadata(&cand).is_err() {
                continue;
            }
            let Some(rel) = py::relative_posix(&cand, &root) else { continue };
            let mut cur = root.clone();
            let mut seen: Vec<&str> = Vec::new();
            for part in rel.split('/').filter(|p| !p.is_empty()) {
                cur.push(part);
                seen.push(part);
                if std::fs::symlink_metadata(&cur).is_ok_and(|m| m.file_type().is_symlink()) {
                    return Ok(Some(seen.join("/")));
                }
            }
            return Ok(None);
        }
        Ok(None)
    }

    /// `pathspec_relpath(spec, known_paths)`.
    pub fn pathspec_relpath(&self, spec: &str, known: &dyn Fn(&str) -> bool) -> Result<String> {
        let path = Path::new(spec);
        if path.is_absolute() {
            return self.relpath(path);
        }
        let root = self.require_worktree()?.to_path_buf();
        let mut candidates: Vec<String> = Vec::new();
        for candidate in [py::realpath(&cwd()?.join(path)), py::realpath(&root.join(path))] {
            if let Some(rel) = py::relative_posix(&candidate, &root) {
                let rel = if rel.is_empty() { ".".to_string() } else { rel };
                if !candidates.contains(&rel) {
                    candidates.push(rel);
                }
            }
        }
        if let Some(hit) = candidates.iter().find(|c| known(c)) {
            return Ok(hit.clone());
        }
        match candidates.into_iter().next() {
            Some(c) => Ok(c),
            None => Ok(posix_str(spec)?),
        }
    }

    /// cli.py `_pathspec_exists_or_tracked(repo, spec)`.
    pub fn pathspec_exists_or_tracked(&self, spec: &str) -> Result<bool> {
        let full = self.pathspec_full_path(spec)?;
        if full.exists() {
            return Ok(true);
        }
        let rel = match self.relpath(&full) {
            Ok(r) => r,
            Err(RepoError::Forward(w)) => return Err(RepoError::Forward(w)),
            Err(_) => return Ok(false),
        };
        Ok(self.entries_for_commit(self.current_commit()?.as_deref())?.contains_key(&rel))
    }
}

/// `split_pathspec(spec)`: (file, layer selector).
pub fn split_pathspec(spec: &str) -> Result<(String, Option<String>)> {
    if Path::new(spec).exists() {
        return Ok((spec.to_string(), None));
    }
    if spec.contains('\n') || spec.contains(['\u{130}', '\u{131}', '\u{17f}', '\u{212a}']) {
        return Err(RepoError::forward("pathspec the layer regex may read differently"));
    }
    // r"^(.+\.(?:png|jpg|jpeg|mdp|clip)):(.+)$" (IGNORECASE): the greedy
    // first group ends at the last ':' that follows an extension.
    let exts = [".png", ".jpg", ".jpeg", ".mdp", ".clip"];
    for (i, _) in spec.char_indices().rev().filter(|(_, c)| *c == ':') {
        let (head, tail) = (&spec[..i], &spec[i + 1..]);
        if tail.is_empty() {
            continue;
        }
        let lower = head.to_ascii_lowercase();
        if exts.iter().any(|e| lower.ends_with(e) && lower.len() > e.len()) {
            return Ok((head.to_string(), Some(tail.to_string())));
        }
    }
    Ok((spec.to_string(), None))
}

/// [`split_pathspec`], and also `file:layer` where `.tmiattributes` maps
/// the file to a format plugin (PLUGINS.md P7): the regex knows only tmi's
/// own layered extensions, so `art.ora:影` would otherwise be a file named
/// so (issue #47). As in the regex, the last `:` that ends such a file name
/// wins. Python never sees these specs: it does not run plugins.
pub fn split_layer_spec(repo: &Repo, spec: &str) -> Result<(String, Option<String>)> {
    let (file, layer) = split_pathspec(spec)?;
    if layer.is_some() || !spec.contains(':') || Path::new(spec).exists() || repo.worktree.is_none() {
        return Ok((file, layer));
    }
    let attrs = crate::attributes::Attributes::load(repo.worktree.as_deref()).unwrap_or_default();
    for (i, _) in spec.char_indices().rev().filter(|(_, c)| *c == ':') {
        let (head, tail) = (&spec[..i], &spec[i + 1..]);
        if head.is_empty() || tail.is_empty() {
            continue;
        }
        let Ok(rel) = repo.pathspec_relpath(head, &|_| false) else { continue };
        if attrs.lookup(&rel).plugin_id().is_some() {
            return Ok((head.to_string(), Some(tail.to_string())));
        }
    }
    Ok((file, layer))
}

/// `PurePath(text).as_posix()` for ordinary relative texts.
fn posix_str(text: &str) -> Result<String> {
    let parts = path_parts(text)?;
    Ok(if parts.is_empty() { ".".into() } else { parts.join("/") })
}

fn path_parts(text: &str) -> Result<Vec<&str>> {
    if cfg!(windows) && (text.starts_with(['/', '\\']) || text.as_bytes().get(1) == Some(&b':')) {
        return Err(RepoError::forward("a drive or root in a path text"));
    }
    let seps: &[char] = if cfg!(windows) { &['/', '\\'] } else { &['/'] };
    if !cfg!(windows) && text.starts_with('/') {
        return Err(RepoError::forward("a root in a path text"));
    }
    Ok(text.split(seps).filter(|p| !p.is_empty() && *p != ".").collect())
}

/// `str(Path(text))` for a relative text (what Python prints for a path
/// the user gave, e.g. `--output`).
pub fn path_str(text: &str) -> Result<String> {
    let b = text.as_bytes();
    if cfg!(windows) && b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/') {
        if text[3..].starts_with(['/', '\\']) {
            return Err(RepoError::forward("doubled separator after a drive"));
        }
        let rest = path_parts(&text[3..])?;
        return Ok(format!("{}\\{}", &text[..2], rest.join("\\")));
    }
    if !cfg!(windows) && text.starts_with('/') && !text.starts_with("//") {
        let rest = path_parts(text.trim_start_matches('/'))?;
        return Ok(format!("/{}", rest.join("/")));
    }
    let parts = path_parts(text)?;
    let sep = if cfg!(windows) { "\\" } else { "/" };
    Ok(if parts.is_empty() { ".".into() } else { parts.join(sep) })
}
