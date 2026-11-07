//! repo.py: creating and deleting branches and tags, remotes in the repo
//! config (`create_branch`, `delete_branch`, `create_tag`, `delete_tag`,
//! `add_remote`, `remove_remote`, `rename_remote`, `set_remote_url`).

use crate::error::{RepoError, Result};
use crate::fsio;
use crate::lockfile::Expect;
use crate::repo::Repo;
use crate::write::{int, obj, s, samefile};
use std::path::{Path, PathBuf};
use tmi_store::json::{Map, Value};

impl Repo {
    /// Forward when writing the ref file would hit a file where Python
    /// needs a directory (its OSError text is not reproduced).
    fn check_ref_parents(&self, path: &Path) -> Result<()> {
        let mut cur = path.parent();
        while let Some(dir) = cur {
            if dir == self.repo_dir {
                break;
            }
            if dir.exists() && !dir.is_dir() {
                return Err(RepoError::forward("a ref file where a directory is needed"));
            }
            cur = dir.parent();
        }
        Ok(())
    }

    /// `create_branch(name, start)`.
    pub fn create_branch(&self, name: &str, start: Option<&str>, warnings: &mut Vec<String>) -> Result<String> {
        crate::repo::check_ref_short_name(name)?;
        let commit = self
            .resolve_rev(start, warnings)?
            .ok_or_else(|| RepoError::runtime("cannot create branch without a commit"))?;
        let ref_name = format!("refs/heads/{name}");
        let path = self.ref_path(&ref_name)?;
        if path.exists() {
            return Err(RepoError::runtime(format!("branch already exists: {name}")));
        }
        self.check_ref_parents(&path)?;
        self.prepare_identity()?;
        self.write_ref_expect(&ref_name, &commit, Expect::Is(None))?;
        let from = start.filter(|s| !s.is_empty()).unwrap_or("HEAD");
        self.log_ref_move(&ref_name, None, &commit, &format!("branch: created from {from}"))?;
        Ok(commit)
    }

    /// `delete_branch(name, force)`.
    pub fn delete_branch(&self, name: &str, force: bool) -> Result<String> {
        let ref_name = format!("refs/heads/{name}");
        let target = self.read_ref(&ref_name)?.ok_or_else(|| RepoError::runtime(format!("branch not found: {name}")))?;
        let current = self.current_branch()?;
        if current.as_deref() == Some(name) {
            return Err(RepoError::runtime(format!("cannot delete the current branch: {name}")));
        }
        if let Some(cur) = &current {
            let a = self.ref_path_lenient(&ref_name)?;
            let b = self.ref_path_lenient(&format!("refs/heads/{cur}"))?;
            if let Ok(true) = samefile(&a, &b) {
                return Err(RepoError::runtime(format!("cannot delete the current branch: {name}")));
            }
        }
        if !force {
            let head = self.current_commit()?;
            let merged = match &head {
                Some(h) if !h.is_empty() => target == *h || self.is_ancestor(&target, h)?,
                _ => false,
            };
            if !merged {
                return Err(RepoError::runtime(format!(
                    "branch {name} is not fully merged into HEAD; use -D to delete it anyway (recover via 'tmiki reflog')"
                )));
            }
        }
        self.delete_ref(&ref_name, Expect::Is(Some(&target)))?;
        Ok(target)
    }

    /// `delete_tag(name)`.
    pub fn delete_tag(&self, name: &str) -> Result<String> {
        let ref_name = format!("refs/tags/{name}");
        let target = self.read_ref(&ref_name)?.ok_or_else(|| RepoError::runtime(format!("tag not found: {name}")))?;
        self.delete_ref(&ref_name, Expect::Is(Some(&target)))?;
        Ok(target)
    }

    /// `create_tag(name, target, message)`.
    pub fn create_tag(&self, name: &str, target: Option<&str>, message: Option<&str>, warnings: &mut Vec<String>) -> Result<String> {
        crate::repo::check_ref_short_name(name)?;
        let commit = self
            .resolve_rev(target, warnings)?
            .ok_or_else(|| RepoError::runtime("cannot tag without a commit"))?;
        let ref_name = format!("refs/tags/{name}");
        let path = self.ref_path(&ref_name)?;
        if path.exists() {
            return Err(RepoError::runtime(format!("tag already exists: {name}")));
        }
        self.check_ref_parents(&path)?;
        let Some(message) = message else {
            self.write_ref_expect(&ref_name, &commit, Expect::Is(None))?;
            return Ok(commit);
        };
        let (tagger_name, tagger_email) = self.author()?;
        let timestamp = self.object_timestamp()?;
        let tag = obj([
            ("version", int(1)),
            ("object", s(commit.clone())),
            ("type", s("commit")),
            ("tag", s(name)),
            ("tagger", obj([("name", s(tagger_name)), ("email", s(tagger_email))])),
            ("timestamp", s(timestamp)),
            ("message", s(message)),
        ]);
        fsio::mark_mutated();
        let oid = self.store.write_json("tag", &tag)?;
        self.write_ref_expect(&ref_name, &oid, Expect::Is(None))?;
        Ok(oid)
    }

    fn remotes_mut(config: &mut Value) -> Result<&mut Map> {
        let Value::Object(map) = config else { return Err(RepoError::forward("config is not an object")) };
        // config.setdefault("remotes", {})
        let entry = map.entry("remotes".to_string()).or_insert_with(|| Value::Object(Map::new()));
        match entry {
            Value::Object(m) => Ok(m),
            _ => Err(RepoError::forward("remotes is not a dict")),
        }
    }

    /// `add_remote(name, url)`.
    pub fn add_remote(&self, name: &str, url: &str) -> Result<()> {
        crate::repo::check_ref_short_name(name)?;
        let mut config = self.read_config()?;
        let canonical = canonical_url(url)?;
        let remotes = Repo::remotes_mut(&mut config)?;
        if remotes.contains_key(name) {
            return Err(RepoError::runtime(format!("remote {name} already exists")));
        }
        remotes.insert(name.to_string(), obj([("url", s(canonical))]));
        self.write_config(&config)
    }

    /// `remove_remote(name)`.
    pub fn remove_remote(&self, name: &str) -> Result<()> {
        let mut config = self.read_config()?;
        let remotes = Repo::remotes_mut(&mut config)?;
        if remotes.shift_remove(name).is_none() {
            return Err(RepoError::runtime(format!("no such remote: {name}")));
        }
        self.write_config(&config)
    }

    /// `set_remote_url(name, url)`: the stored URL.
    pub fn set_remote_url(&self, name: &str, url: &str) -> Result<String> {
        let mut config = self.read_config()?;
        let canonical = canonical_url(url)?;
        let remotes = Repo::remotes_mut(&mut config)?;
        match remotes.get_mut(name) {
            None => return Err(RepoError::runtime(format!("no such remote: {name}"))),
            Some(Value::Object(remote)) => {
                remote.insert("url".into(), s(canonical.clone()));
            }
            Some(_) => return Err(RepoError::forward("remote entry is not a dict")),
        }
        self.write_config(&config)?;
        Ok(canonical)
    }

    /// `rename_remote(old, new)`.
    pub fn rename_remote(&self, old: &str, new: &str) -> Result<()> {
        let mut config = self.read_config()?;
        let remotes = Repo::remotes_mut(&mut config)?;
        if !remotes.contains_key(old) {
            return Err(RepoError::runtime(format!("no such remote: {old}")));
        }
        if remotes.contains_key(new) {
            return Err(RepoError::runtime(format!("remote {new} already exists")));
        }
        crate::repo::check_ref_short_name(new)?;
        self.ref_path(&format!("refs/remotes/{new}/placeholder"))?;
        let value = remotes.shift_remove(old).unwrap();
        remotes.insert(new.to_string(), value);
        let old_dir = self.repo_dir.join("refs").join("remotes").join(old);
        let new_dir = self.repo_dir.join("refs").join("remotes").join(new);
        if old_dir.exists() && new_dir.exists() && !new_dir.is_dir() {
            return Err(RepoError::forward("a remote's refs path is a file"));
        }
        self.write_config(&config)?;
        if old_dir.exists() {
            if new_dir.exists() {
                let mut files: Vec<(String, PathBuf)> = crate::repo::walk_files(&old_dir)?;
                files.sort_by(|a, b| a.0.cmp(&b.0));
                for (rel, path) in files {
                    let target = crate::repo::join_trusted(&new_dir, &rel);
                    fsio::mkdir_parent(&target)?;
                    fsio::replace(&path, &target)?;
                }
                fsio::rmtree_ignore(&old_dir);
            } else {
                fsio::mkdir_parent(&new_dir)?;
                fsio::replace(&old_dir, &new_dir)?;
            }
        }
        Ok(())
    }
}

/// transport `canonical_url(url)`: a local path resolved to an absolute
/// one, network URLs unchanged. `file://` URLs, `~` and URLs urlsplit
/// cleans up first are left to Python.
pub fn canonical_url(url: &str) -> Result<String> {
    if url.starts_with(|c: char| c <= ' ') || url.contains(['\t', '\r', '\n']) {
        return Err(RepoError::forward("URL urlsplit would clean"));
    }
    // urlsplit's scheme: up to the first ':', ASCII letter first, then
    // letters, digits, "+-."
    let scheme = match url.find(':') {
        Some(i) if i > 0 => {
            let cand = &url[..i];
            let first = cand.chars().next().unwrap();
            if first.is_ascii_alphabetic() && cand.chars().all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c)) {
                cand.to_ascii_lowercase()
            } else {
                String::new()
            }
        }
        _ => String::new(),
    };
    let local = scheme.is_empty() || scheme == "file" || scheme.chars().count() == 1;
    if !local {
        return Ok(url.to_string());
    }
    if url.to_lowercase().starts_with("file://") || url.starts_with('~') {
        return Err(RepoError::forward("file:// URL or ~ path"));
    }
    let b = url.as_bytes();
    if cfg!(windows) && (b.get(1) == Some(&b':') && !matches!(b.get(2), Some(b'\\' | b'/')) || url.starts_with(r"\\")) {
        return Err(RepoError::forward("drive-relative or UNC path"));
    }
    let path = if url.is_empty() { Path::new(".") } else { Path::new(url) };
    Ok(crate::py::realpath(path).to_string_lossy().into_owned())
}
