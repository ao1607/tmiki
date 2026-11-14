//! repo.py stash entries (`.tmi/stash.json` and `refs/stash/<id>`):
//! reading, applying, dropping, clearing. Saving one imports every tracked
//! file (`_working_tree_entries_for_stash`): that stays Python's.

use crate::error::{RepoError, Result};
use crate::fsio;
use crate::repo::{read_text, same_entry, Entries, Repo};
use crate::write::{dumps_sorted, source_blob_ref};
use std::collections::BTreeSet;
use tmi_store::json::{self, Map, Value};

impl Repo {
    /// `read_stashes()`.
    pub fn read_stashes(&self) -> Result<Vec<Value>> {
        let Some(text) = read_text(&self.repo_dir.join("stash.json"))? else {
            return Ok(Vec::new());
        };
        match json::loads(&text) {
            Ok(Value::Array(items)) => Ok(items.into_iter().filter(|i| i.as_object().is_some()).collect()),
            Ok(_) => Ok(Vec::new()),
            Err(_) => Err(RepoError::forward("stash.json does not parse (Python's message)")),
        }
    }

    pub fn write_stashes(&self, stashes: &[Value]) -> Result<()> {
        fsio::atomic_write(&self.repo_dir.join("stash.json"), dumps_sorted(&Value::Array(stashes.to_vec())).as_bytes())
    }

    /// `resolve_stash(spec)`: (index, entry).
    pub fn resolve_stash(&self, spec: Option<&str>) -> Result<(usize, Value)> {
        let stashes = self.read_stashes()?;
        if stashes.is_empty() {
            return Err(RepoError::runtime("no stash entries"));
        }
        let Some(spec) = spec.filter(|s| !s.is_empty()) else {
            return Ok((0, stashes[0].clone()));
        };
        if !spec.is_ascii() {
            return Err(RepoError::forward("non-ASCII stash spec (\\d, isdigit)"));
        }
        let digits = spec.strip_prefix("stash@{").and_then(|r| r.strip_suffix('}')).unwrap_or(spec);
        let numeric = !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit());
        if !numeric {
            return Err(RepoError::runtime(format!("unknown stash spec: {spec}")));
        }
        let idx = digits.parse::<usize>().unwrap_or(usize::MAX);
        if idx >= stashes.len() {
            return Err(RepoError::runtime(format!("stash not found: {spec}")));
        }
        Ok((idx, stashes[idx].clone()))
    }

    /// `stash_apply(spec)`.
    pub fn stash_apply(&self, spec: Option<&str>) -> Result<Value> {
        let (_idx, stash) = self.resolve_stash(spec)?;
        let commit = match stash.get("commit") {
            Some(Value::Str(c)) if !c.is_empty() => c.clone(),
            _ => return Err(RepoError::runtime("corrupt stash entry")),
        };
        let current = self.current_commit()?;
        self.ensure_clean_tracked("stash apply")?;
        let base_entries = match stash.get("base") {
            Some(Value::Str(b)) if !b.is_empty() => self.entries_for_commit(Some(b))?,
            _ => Entries::new(),
        };
        let stash_entries = self.entries_for_checkout(&commit)?;
        let changed: BTreeSet<String> = base_entries
            .keys()
            .chain(stash_entries.keys())
            .filter(|rel| !same_entry(base_entries.get(rel.as_str()), stash_entries.get(rel.as_str())))
            .cloned()
            .collect();
        let mut touched = Entries::new();
        for rel in &changed {
            if let Some(e) = stash_entries.get(rel) {
                touched.insert(rel.clone(), e.clone());
            }
        }
        self.ensure_no_untracked_overwrite(&touched, current.as_deref(), "stash apply")?;
        let stash_commit = self.read_commit(&commit)?;
        let restored: Map = match stash_commit.get("index_entries") {
            Some(Value::Object(m)) => m.iter().filter(|(_, e)| e.as_object().is_some()).map(|(k, e)| (k.clone(), e.clone())).collect(),
            _ => Map::new(),
        };
        for rel in changed.iter().rev() {
            if stash_entries.contains_key(rel) {
                continue;
            }
            let out = self.abspath(rel)?;
            if out.exists() && out.is_file() {
                // it may be a file made since (untracked now): kept in the backups
                self.backup_worktree_file(rel, "stash apply", &[], true)?;
                fsio::unlink(&out)?;
                if let Some(parent) = out.parent() {
                    self.remove_empty_parent_dirs(parent)?;
                }
            }
        }
        for (rel, entry) in &touched {
            let out = self.abspath(rel)?;
            fsio::mkdir_parent(&out)?;
            let data = self.store.read_blob(&source_blob_ref(entry)?)?;
            fsio::write_bytes(&out, &data)?;
            self.rewrote(&out);
        }
        self.write_index_entries(restored)?;
        Ok(stash)
    }

    /// `stash_drop(spec)`: the dropped entry.
    pub fn stash_drop(&self, spec: Option<&str>) -> Result<Value> {
        let (idx, _) = self.resolve_stash(spec)?;
        let mut stashes = self.read_stashes()?;
        let dropped = stashes.remove(idx);
        if let Some(Value::Str(r)) = dropped.get("ref") {
            fsio::unlink_missing_ok(&self.ref_path(r)?)?;
        }
        self.write_stashes(&stashes)?;
        Ok(dropped)
    }

    /// `stash_clear()`: how many were dropped.
    pub fn stash_clear(&self) -> Result<usize> {
        let stashes = self.read_stashes()?;
        let mut paths = Vec::new();
        for st in &stashes {
            if let Some(Value::Str(r)) = st.get("ref") {
                paths.push(self.ref_path(r)?);
            }
        }
        for p in paths {
            fsio::unlink_missing_ok(&p)?;
        }
        self.write_stashes(&[])?;
        Ok(stashes.len())
    }
}
