//! transport/local.py `LocalTransport`: a repository on a filesystem path
//! (bare or with a working tree), and objects.py `copy_missing_from`.

use crate::{rawobj, Expect, Progress, Refs, Updated};
use rayon::prelude::*;
use std::path::{Path, PathBuf};
use tmi_repo::repo::TMI_DIR;
use tmi_repo::{Repo, RepoError, Result};
use tmi_store::ObjectStore;

pub struct LocalRemote {
    pub path: PathBuf,
    pub repo: Repo,
}

/// `urllib.parse.unquote(s)`: %XX as UTF-8 (Forward when that is not
/// valid UTF-8: Python's "replace" is not reproduced).
fn unquote(s: &str) -> Result<String> {
    if !s.contains('%') {
        return Ok(s.to_string());
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    let hexval = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hexval(b[i + 1]), hexval(b[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8(out).map_err(|_| RepoError::forward("percent-escapes that are not UTF-8"))
}

/// local.py `_resolve_local_path(url)`.
pub fn resolve_local_path(url: &str) -> Result<PathBuf> {
    let raw = if url.to_lowercase().starts_with("file://") {
        let rest = &url[7..];
        if rest.contains(['?', '#', ';']) {
            return Err(RepoError::forward("file:// URL with a query, fragment or parameters"));
        }
        // urlparse: the netloc runs to the next "/", the path is the rest
        let path = match rest.find('/') {
            Some(i) => &rest[i..],
            None => "",
        };
        let mut raw = unquote(path)?;
        let chars: Vec<char> = raw.chars().collect();
        if raw.starts_with('/') && chars.len() > 3 && chars[2] == ':' {
            raw = raw[1..].to_string();
        }
        raw
    } else {
        url.to_string()
    };
    if raw.starts_with('~') {
        return Err(RepoError::forward("~ in a remote path (expanduser)"));
    }
    let p = if raw.is_empty() { Path::new(".") } else { Path::new(&raw) };
    Ok(tmi_repo::py::realpath(p))
}

/// `Repo.open_path(path)` of a resolved path.
pub fn open_repo(path: &Path) -> Result<Repo> {
    if path.join(TMI_DIR).is_dir() {
        return Ok(Repo::new(Some(path.to_path_buf()), path.join(TMI_DIR)));
    }
    if path.join("objects").is_dir() && path.join("refs").is_dir() {
        return Ok(Repo::new(None, path.to_path_buf()));
    }
    Err(RepoError::runtime(format!("not a tmi repository: {}", path.display())))
}

impl LocalRemote {
    pub fn open(url: &str) -> Result<LocalRemote> {
        let path = resolve_local_path(url)?;
        let repo = open_repo(&path)?;
        Ok(LocalRemote { path, repo })
    }

    /// `all_refs()` (sorted: Python's order is rglob's, which only `clone`
    /// could tell apart; see `ops::clone`).
    pub fn list_refs(&self) -> Result<Refs> {
        Ok(self.repo.all_refs_sorted()?.into_iter().collect())
    }

    pub fn pull_objects(&self, into: &ObjectStore, progress: Progress) -> Result<usize> {
        copy_missing(into, &self.repo.store, progress)
    }

    pub fn push_objects(&self, from: &ObjectStore, progress: Progress) -> Result<usize> {
        copy_missing(&self.repo.store, from, progress)
    }

    pub fn update_ref(&self, name: &str, value: &str, expect: Expect<'_>) -> Result<Updated> {
        if self.repo.read_ref(name)?.as_deref() == Some(value) {
            return Ok(Updated::Done);
        }
        if self.repo.worktree.is_some() && self.repo.head_target()? == name {
            return Err(RepoError::runtime(format!(
                "refusing to update checked-out branch in non-bare repository: {name}"
            )));
        }
        // Compare-and-swap under <ref>.lock in the remote repository: a push
        // that raced another one fails here instead of overwriting it (#36).
        if let Some(actual) = self.repo.try_write_ref(name, value, expect)? {
            return Ok(Updated::Moved(actual));
        }
        if self.repo.current_branch()?.is_none() && name.starts_with("refs/heads/") {
            self.repo.set_head_ref(name)?;
        }
        Ok(Updated::Done)
    }

    pub fn head_branch(&self) -> Result<Option<String>> {
        self.repo.current_branch()
    }
}

/// How many objects one parallel round reads and checks at most.
const ROUND: usize = 256;

/// `dst.copy_missing_from(src, on_progress)`: every object of `src` that
/// `dst` lacks, in oid order, each checked (`write_raw`) before it lands.
/// Objects are read and checked in parallel; when one fails, exactly the
/// ones before it (in oid order) are stored, as Python's loop leaves it.
pub fn copy_missing(dst: &ObjectStore, src: &ObjectStore, progress: Progress) -> Result<usize> {
    progress("enumerating remote objects");
    let oids = src.iter_object_ids();
    let total = oids.len();
    let missing: Vec<(usize, &String)> =
        oids.iter().enumerate().filter(|(_, oid)| !dst.path_for(oid).exists()).collect();
    let mut copied = 0usize;
    for round in missing.chunks(ROUND) {
        let checked: Vec<std::result::Result<Vec<u8>, RepoError>> = round
            .par_iter()
            .map(|(_, oid)| {
                let path = src.path_for(oid);
                let raw = std::fs::read(&path).map_err(|e| RepoError::runtime(read_error(&path, &e)))?;
                rawobj::verify(oid, &raw).map_err(|e| {
                    if e.starts_with(rawobj::REFUSED_TREE) {
                        RepoError::runtime(e)
                    } else {
                        RepoError::Value(e)
                    }
                })?;
                Ok(raw)
            })
            .collect();
        let good = checked.iter().position(|r| r.is_err()).unwrap_or(checked.len());
        // the messages in Python's order (TMI_PROGRESS lines show the first)
        for (idx, oid) in &round[..good] {
            progress(&format!("copying object [{}/{total}] {}", idx + 1, &oid[..oid.len().min(12)]));
        }
        let stored: Vec<std::result::Result<(), RepoError>> = round[..good]
            .par_iter()
            .zip(&checked[..good])
            .map(|((_, oid), raw)| rawobj::store(dst, oid, raw.as_ref().unwrap()).map_err(RepoError::runtime))
            .collect();
        for r in stored {
            r?;
            copied += 1;
        }
        if good < checked.len() {
            let (idx, oid) = round[good];
            progress(&format!("copying object [{}/{total}] {}", idx + 1, &oid[..oid.len().min(12)]));
            return Err(checked.into_iter().nth(good).unwrap().unwrap_err());
        }
    }
    Ok(copied)
}

/// `str(OSError)` of a failed `read_bytes()`.
fn read_error(path: &Path, e: &std::io::Error) -> String {
    format!("{}: '{}'", tmi_repo::hooks::os_error_str(e), path.display())
}
