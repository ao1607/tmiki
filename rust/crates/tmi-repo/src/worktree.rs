//! repo.py operations that move HEAD and rewrite working files without
//! importing them: `switch`, `checkout_detached`, `reset`, `reset_path`,
//! `unstage`, `restore` of whole files, `remove_path`, taking a side of a
//! conflicted file, putting a backup back.

use crate::error::{RepoError, Result};
use crate::fsio;
use crate::hooks;
use crate::paths::split_pathspec;
use crate::repo::{prefix_chars, same_entry, Entries, Repo};
use crate::write::{
    committable, deleted_entry, digest_of, entries_of, index_diff_to_entries, is_deleted, source_blob_ref, str_of,
    DIRTY_WORKTREE_MESSAGE,
};
use std::path::PathBuf;
use tmi_store::json::{self, DumpOptions, Map, Value};

/// repo.py `_UNCONVERTED_IMAGE_SUFFIXES`.
const UNCONVERTED_IMAGE_SUFFIXES: [&str; 13] =
    [".jpg", ".jpeg", ".gif", ".bmp", ".tif", ".tiff", ".webp", ".psd", ".clip", ".mdp", ".apng", ".avif", ".ico"];

/// `Path(text).suffix.lower()` of a path's last component.
fn suffix_lower(text: &str) -> Result<String> {
    let name = std::path::Path::new(text).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let suffix = tmi_import::path_suffix(&name);
    if !suffix.is_ascii() {
        if !crate::py::python_fallback() {
            return Ok(suffix.to_lowercase());
        }
        return Err(RepoError::forward("a non-ASCII extension (str.lower())"));
    }
    Ok(suffix.to_ascii_lowercase())
}

/// What `restore --output <output> <rel>` of a whole file writes (repo.py
/// `_whole_file_output`, issue #30): true for the file's image as a PNG,
/// false for the file as it is; another image format is refused.
pub fn whole_file_output_is_png(rel: &str, output: &str) -> Result<bool> {
    let src = suffix_lower(rel)?;
    let dst = suffix_lower(output)?;
    if dst.is_empty() || dst.replace(".jpeg", ".jpg") == src.replace(".jpeg", ".jpg") {
        return Ok(false);
    }
    if dst == ".png" {
        return Ok(true);
    }
    if UNCONVERTED_IMAGE_SUFFIXES.contains(&dst.as_str()) {
        let hint = if src.is_empty() {
            ".png for its image".to_string()
        } else {
            format!("{src} for the file as it is, or .png for its image")
        };
        return Err(RepoError::runtime(format!("cannot convert {rel} to {dst}; give --output a path ending in {hint}")));
    }
    Ok(false)
}

/// repo.py `_no_image_message(rel)`.
pub fn no_image_message(rel: &str) -> Result<String> {
    let src = suffix_lower(rel)?;
    let keep = if src.is_empty() { "a path without an extension".to_string() } else { format!("a path ending in {src}") };
    Ok(format!("{rel} has no image to write as .png; give --output {keep} for the file as it is"))
}

/// The index as a mutable object, `index.setdefault("entries", {})`.
fn index_entries_mut(index: &mut Value) -> Result<&mut Map> {
    let Value::Object(map) = index else { return Err(RepoError::forward("index is not an object")) };
    match map.entry("entries".to_string()).or_insert_with(|| Value::Object(Map::new())) {
        Value::Object(m) => Ok(m),
        _ => Err(RepoError::forward("index entries is not an object")),
    }
}

impl Repo {
    fn known_fn<'a>(sets: &'a [&'a Entries], extra: &'a [String]) -> impl Fn(&str) -> bool + 'a {
        move |p: &str| sets.iter().any(|e| e.contains_key(p)) || extra.iter().any(|x| x == p)
    }

    /// `switch(name, discard)`.
    pub fn switch(&self, name: &str, discard: bool, before_hook: &mut dyn FnMut()) -> Result<Option<String>> {
        let ref_name = format!("refs/heads/{name}");
        let commit = self.read_ref(&ref_name)?.ok_or_else(|| RepoError::runtime(format!("branch not found: {name}")))?;
        self.entries_for_checkout(&commit)?;
        if self.pending_merge_head()?.is_some() && !discard {
            return Err(RepoError::runtime(
                "merge in progress; finish it (tmiki add + tmiki commit) or give it up with 'tmiki merge --abort' before switching",
            ));
        }
        self.prepare_identity()?;
        let dirty = self.dirty_items()?;
        if !discard && !dirty.is_empty() {
            return Err(RepoError::runtime(DIRTY_WORKTREE_MESSAGE));
        }
        let previous_branch = self.current_branch()?;
        let previous_commit = self.current_commit()?;
        if !discard {
            self.ensure_no_untracked_overwrite(&self.entries_for_commit(Some(&commit))?, previous_commit.as_deref(), "switch")?;
        }
        let from = previous_branch.clone().unwrap_or_default();
        hooks::run(&self.repo_dir, "pre-switch", &[("TMI_BRANCH_FROM", &from), ("TMI_BRANCH_TO", name)], true, before_hook)?;
        self.set_head_ref(&ref_name)?;
        let backup_reason = if discard { format!("switch --discard {name}") } else { format!("switch {name}") };
        self.restore_all(&commit, previous_commit.as_deref(), &backup_reason)?;
        if discard {
            let target = self.entries_for_commit(Some(&commit))?;
            let index = self.index_entries()?;
            let mut extra: Vec<&String> = index.keys().filter(|k| !target.contains_key(k.as_str())).collect();
            extra.sort();
            extra.reverse();
            for rel in extra {
                let out = self.abspath(rel)?;
                if out.exists() && out.is_file() {
                    self.backup_worktree_file(rel, &backup_reason, &[], false)?;
                    fsio::unlink(&out)?;
                    if let Some(parent) = out.parent() {
                        self.remove_empty_parent_dirs(parent)?;
                    }
                }
            }
            self.write_index_entries(Entries::new())?;
            self.clear_pending_merge()?;
            self.clear_all_conflict_files()?;
        }
        let reason = format!("switch: {} -> {name}", previous_branch.as_deref().unwrap_or("(detached)"));
        self.log_ref_move("HEAD", previous_commit.as_deref(), &commit, &reason)?;
        hooks::run(
            &self.repo_dir,
            "post-switch",
            &[("TMI_BRANCH_FROM", &from), ("TMI_BRANCH_TO", name), ("TMI_COMMIT", &commit)],
            false,
            before_hook,
        )?;
        Ok(Some(commit))
    }

    /// `checkout_detached(rev, discard)`.
    pub fn checkout_detached(&self, rev: &str, discard: bool, warnings: &mut Vec<String>) -> Result<String> {
        let commit = self
            .resolve_rev(Some(rev), warnings)?
            .ok_or_else(|| RepoError::runtime(format!("unknown revision: {rev}")))?;
        self.entries_for_checkout(&commit)?;
        if self.pending_merge_head()?.is_some() && !discard {
            return Err(RepoError::runtime(
                "merge in progress; finish it (tmiki add + tmiki commit) or give it up with 'tmiki merge --abort' before checkout",
            ));
        }
        self.prepare_identity()?;
        let dirty = self.dirty_items()?;
        if !dirty.is_empty() && !discard {
            return Err(RepoError::runtime(DIRTY_WORKTREE_MESSAGE));
        }
        let previous = self.current_commit()?;
        if !discard {
            self.ensure_no_untracked_overwrite(&self.entries_for_commit(Some(&commit))?, previous.as_deref(), "checkout")?;
        }
        self.detach_head(&commit)?;
        let reason = if discard { format!("checkout --discard {rev}") } else { format!("checkout {rev}") };
        self.restore_all(&commit, previous.as_deref(), &reason)?;
        if discard {
            self.write_index_entries(Entries::new())?;
            self.clear_pending_merge()?;
            self.clear_all_conflict_files()?;
        }
        self.log_ref_move("HEAD", previous.as_deref(), &commit, &format!("checkout: moving to {rev}"))?;
        Ok(commit)
    }

    /// `restore(spec, source=, output=, operation=)` of a whole file (a
    /// `file:layer` spec is Forward). `output` is the user's text.
    pub fn restore_file(&self, spec: &str, source: Option<&str>, output: Option<&str>, operation: &str, warnings: &mut Vec<String>) -> Result<PathBuf> {
        if source == Some("WORKTREE") {
            return Err(RepoError::forward("restore --source WORKTREE exports layers"));
        }
        let commit = self
            .resolve_rev(source, warnings)?
            .ok_or_else(|| RepoError::runtime("no commit to restore from"))?;
        let (file_part, layer) = split_pathspec(spec)?;
        if layer.is_some() {
            return Err(RepoError::forward("layer restore"));
        }
        let mut entries = self.entries_for_commit(Some(&commit))?;
        let mut label = prefix_chars(&commit, 12);
        if source.is_none() {
            for (path, entry) in self.index_entries()? {
                if is_deleted(&entry) {
                    entries.shift_remove(&path);
                } else {
                    entries.insert(path, entry);
                }
            }
            label = "index".into();
        }
        let rel = self.pathspec_relpath(&file_part, &|p| entries.contains_key(p))?;
        let Some(entry) = entries.get(&rel) else {
            return Err(RepoError::runtime(format!("path not found in {label}: {rel}")));
        };
        if let Some(o) = output {
            if whole_file_output_is_png(&rel, o)? {
                // the file's image, not its bytes (issue #30)
                let composite = match entry.get("composite_ref") {
                    Some(v) if v.truthy() => v.as_str().ok_or_else(|| RepoError::forward("composite_ref is not a str"))?.to_string(),
                    _ => return Err(RepoError::runtime(no_image_message(&rel)?)),
                };
                let out = PathBuf::from(o);
                if let Some(parent) = out.parent().filter(|p| !p.as_os_str().is_empty()) {
                    fsio::mkdir_p(parent)?;
                }
                fsio::write_bytes(&out, &self.store.read_blob(&composite)?)?;
                return Ok(out);
            }
        }
        let blob = source_blob_ref(entry)?;
        let out = match output {
            Some(o) => PathBuf::from(o),
            None => self.abspath(&rel)?,
        };
        if output.is_none() {
            self.prepare_identity()?;
        }
        if let Some(parent) = out.parent().filter(|p| !p.as_os_str().is_empty()) {
            fsio::mkdir_p(parent)?;
        }
        if output.is_none() {
            self.backup_worktree_file(&rel, &format!("{operation} {rel}"), &[digest_of(Some(entry))?], true)?;
        }
        let data = self.store.read_blob(&blob)?;
        fsio::write_bytes(&out, &data)?;
        if output.is_none() {
            self.rewrote(&out);
        }
        Ok(out)
    }

    /// `unstage(spec)`: (rel, was_staged).
    pub fn unstage(&self, spec: &str) -> Result<(String, bool)> {
        let (file_part, _) = split_pathspec(spec)?;
        let mut index = self.read_index()?;
        let entries = entries_of(&index)?;
        let head = self.entries_for_commit(self.current_commit()?.as_deref())?;
        let resolved = if self.pending_merge_head()?.is_some() { self.resolved_conflict_paths()? } else { Vec::new() };
        let rel = self.pathspec_relpath(&file_part, &Repo::known_fn(&[&entries, &head], &resolved))?;
        if resolved.contains(&rel) {
            if entries.contains_key(&rel) {
                index_entries_mut(&mut index)?.shift_remove(&rel);
                self.write_index(&index)?;
            }
            self.reopen_conflict(&rel)?;
            return Ok((rel, true));
        }
        if entries.contains_key(&rel) {
            index_entries_mut(&mut index)?.shift_remove(&rel);
            self.write_index(&index)?;
            return Ok((rel, true));
        }
        if head.contains_key(&rel) {
            return Ok((rel, false));
        }
        Err(RepoError::runtime(format!("pathspec did not match any staged or tracked file: {rel}")))
    }

    /// `reset_path(spec, source)`: (rel, changed).
    pub fn reset_path(&self, spec: &str, source: Option<&str>, warnings: &mut Vec<String>) -> Result<(String, bool)> {
        let commit = self.resolve_rev(source, warnings)?;
        let target = self.entries_for_commit(commit.as_deref())?;
        let mut index = self.read_index()?;
        let current = self.entries_for_commit(self.current_commit()?.as_deref())?;
        let entries = index_entries_mut(&mut index)?;
        let snapshot: Entries = entries.clone();
        let rel = self.pathspec_relpath(spec, &Repo::known_fn(&[&snapshot, &target, &current], &[]))?;
        let mut changed = false;
        if let Some(target_entry) = target.get(&rel) {
            let before = entries.get(&rel).cloned();
            if same_entry(Some(target_entry), current.get(&rel)) {
                if entries.shift_remove(&rel).is_some() {
                    changed = true;
                }
            } else if !same_entry(before.as_ref(), Some(target_entry)) {
                entries.insert(rel.clone(), target_entry.clone());
                changed = true;
            }
        } else if entries.contains_key(&rel) || current.contains_key(&rel) {
            if entries.shift_remove(&rel).is_some() {
                changed = true;
            }
            if current.contains_key(&rel) {
                entries.insert(rel.clone(), deleted_entry(&rel));
                changed = true;
            }
        } else {
            return Err(RepoError::runtime(format!("pathspec did not match any file: {rel}")));
        }
        self.write_index(&index)?;
        Ok((rel, changed))
    }

    /// `reset(rev, mode)`.
    pub fn reset(&self, rev: Option<&str>, mode: &str, warnings: &mut Vec<String>) -> Result<String> {
        let commit = self
            .resolve_rev(rev, warnings)?
            .ok_or_else(|| RepoError::runtime("no commit to reset to"))?;
        let previous = self.current_commit()?;
        if mode == "hard" {
            self.entries_for_checkout(&commit)?;
            self.ensure_no_untracked_overwrite(&self.entries_for_commit(Some(&commit))?, previous.as_deref(), "reset --hard")?;
        }
        let mut soft_target = Entries::new();
        if mode == "soft" {
            soft_target = self.entries_for_commit(previous.as_deref())?;
            for (path, entry) in self.index_entries()? {
                if is_deleted(&entry) {
                    soft_target.shift_remove(&path);
                } else {
                    soft_target.insert(path, committable(&entry)?);
                }
            }
        }
        self.prepare_identity()?;
        let rev_text = rev.filter(|r| !r.is_empty()).unwrap_or("HEAD");
        self.set_current_commit(&commit)?;
        if mode == "soft" {
            let base = self.entries_for_commit(Some(&commit))?;
            self.write_index(&index_diff_to_entries(&base, &soft_target))?;
        } else {
            self.write_index_entries(Entries::new())?;
        }
        if mode == "hard" {
            self.restore_all(&commit, previous.as_deref(), &format!("reset --hard {rev_text}"))?;
            self.clear_pending_merge()?;
            self.clear_all_conflict_files()?;
        }
        let reason = format!("reset --{mode}: {rev_text}");
        self.log_ref_move("HEAD", previous.as_deref(), &commit, &reason)?;
        let target = self.head_target()?;
        if target.starts_with("refs/heads/") {
            self.log_ref_move(&target, previous.as_deref(), &commit, &reason)?;
        }
        Ok(commit)
    }

    /// `stage_delete(rel)`.
    pub fn stage_delete(&self, rel: &str) -> Result<()> {
        let mut index = self.read_index()?;
        index_entries_mut(&mut index)?.insert(rel.to_string(), deleted_entry(rel));
        self.write_index(&index)?;
        self.clear_conflict_files(rel)
    }

    /// `remove_path(spec, cached, force)`.
    pub fn remove_path(&self, spec: &str, cached: bool, force: bool) -> Result<String> {
        let head = self.entries_for_commit(self.current_commit()?.as_deref())?;
        let mut index = self.read_index()?;
        let index_entries = entries_of(&index)?;
        let rel = self.pathspec_relpath(spec, &Repo::known_fn(&[&head, &index_entries], &[]))?;
        if !head.contains_key(&rel) && !index_entries.contains_key(&rel) {
            return Err(RepoError::runtime(format!("pathspec did not match any tracked file: {rel}")));
        }
        let full = self.abspath(&rel)?;
        let index_entry = index_entries.get(&rel);
        if !cached && !force && !head.contains_key(&rel) && index_entry.is_some_and(|e| !is_deleted(e)) {
            return Err(RepoError::runtime(format!(
                "{rel} has changes staged in the index but no commit; use --cached to keep the file or -f to delete it anyway"
            )));
        }
        if !cached && full.exists() && !force {
            if let Some(h) = head.get(&rel) {
                let digest = fsio::sha256_file(&full)?;
                if Some(digest) != str_of(h, "source_digest")? {
                    return Err(RepoError::runtime(format!("{rel} has local modifications; use -f to remove")));
                }
            }
        }
        if !head.contains_key(&rel) && index_entries.contains_key(&rel) {
            index_entries_mut(&mut index)?.shift_remove(&rel);
            self.write_index(&index)?;
            self.clear_conflict_files(&rel)?;
        } else {
            self.stage_delete(&rel)?;
        }
        if !cached && full.exists() {
            if full.is_dir() {
                return Err(RepoError::runtime(format!("not removing directory: {rel}")));
            }
            fsio::unlink(&full)?;
            if let Some(parent) = full.parent() {
                self.remove_empty_parent_dirs(parent)?;
            }
        }
        Ok(rel)
    }

    /// `resolve_conflict(spec, side)` for a whole file (`file:layer` is
    /// Forward: it writes layer pixels into the file).
    pub fn resolve_conflict_file(&self, spec: &str, side: &str) -> Result<PathBuf> {
        let (file_part, layer) = split_pathspec(spec)?;
        if self.pending_merge_head()?.is_none() {
            return Err(RepoError::runtime(format!(
                "no merge in progress; --{side} only applies to a file that conflicted in a merge"
            )));
        }
        let known: Vec<String> = self.pending_conflict_paths()?.into_iter().chain(self.resolved_conflict_paths()?).collect();
        let rel = self.pathspec_relpath(&file_part, &|p| known.iter().any(|k| k == p))?;
        let meta = self.read_conflict_meta(&rel)?;
        let Some(meta) = meta.filter(Value::truthy) else {
            let flag = "--source MERGE_HEAD";
            return Err(RepoError::runtime(if side == "theirs" {
                format!(
                    "{rel} has no merge conflict to take --{side} from; to copy their version anyway use \
                     'tmiki restore {flag} {rel}' (whole file) or 'tmiki restore {flag} --in-place {rel}:<layer>' (one layer)"
                )
            } else {
                format!("{rel} has no merge conflict to take --{side} from; our version is HEAD: 'tmiki restore --source HEAD {rel}'")
            }));
        };
        if layer.is_some() {
            return Err(RepoError::forward("restore --ours/--theirs of one layer"));
        }
        let suffix = match meta.get("suffix") {
            Some(v) if v.truthy() => v.as_str().ok_or_else(|| RepoError::forward("suffix is not a str"))?.to_string(),
            _ => {
                let s = py_suffix(&rel);
                if s.is_empty() {
                    ".bin".into()
                } else {
                    s
                }
            }
        };
        let side_file = self.conflict_record_dir(&rel).join(format!("{}{suffix}", side.to_uppercase()));
        if !side_file.exists() {
            return Err(RepoError::runtime(format!("missing conflict source for {side}: {}", side_file.display())));
        }
        let worktree_digest = match meta.get("worktree_digest") {
            None | Some(Value::Null) => None,
            Some(Value::Str(d)) => Some(d.clone()),
            Some(_) => return Err(RepoError::forward("worktree_digest is not a str")),
        };
        let target = self.abspath(&rel)?;
        let data = fsio::read_bytes(&side_file)?;
        fsio::mkdir_parent(&target)?;
        self.backup_worktree_file(&rel, &format!("restore --{side}"), &[worktree_digest], true)?;
        fsio::write_bytes(&target, &data)?;
        self.rewrote(&target);
        self.record_conflict_choice(&rel, side)?;
        Ok(target)
    }

    /// `_record_conflict_choice(rel, None, side)`.
    fn record_conflict_choice(&self, rel: &str, side: &str) -> Result<()> {
        let meta_path = self.conflict_record_dir(rel).join("conflict.json");
        let Ok(bytes) = std::fs::read(&meta_path) else { return Ok(()) };
        let Ok(text) = String::from_utf8(bytes) else { return Ok(()) };
        // read_text's universal newlines
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        let Ok(Value::Object(mut meta)) = json::loads(&text) else { return Ok(()) };
        let mut taken = Map::new();
        taken.insert("*".into(), Value::Str(side.to_string()));
        meta.insert("taken".into(), Value::Object(taken));
        let full = self.abspath(rel)?;
        let digest = if full.is_file() { Value::Str(fsio::sha256_file(&full)?) } else { Value::Null };
        meta.insert("taken_digest".into(), digest);
        fsio::write_text(&meta_path, &json::dumps(&Value::Object(meta), &DumpOptions::INDENTED))
    }

    /// `restore_backup(spec, output)`: (the backup, where it went).
    pub fn restore_backup(&self, spec: &str, output: Option<&str>) -> Result<(Value, PathBuf)> {
        let backups = self.read_backups()?;
        let found = match crate::py::ascii_isdigit(spec) {
            None => return Err(RepoError::forward("non-ASCII backup spec")),
            Some(true) if spec.len() < 19 && (spec.parse::<usize>().unwrap() < backups.len()) => {
                Some(backups[spec.parse::<usize>().unwrap()].clone())
            }
            _ => {
                let mut matches = Vec::new();
                for b in &backups {
                    let id = match b.get("id") {
                        None => String::new(),
                        Some(v) => v.py_str().ok_or_else(|| RepoError::forward("container backup id"))?,
                    };
                    if id.starts_with(spec) {
                        matches.push(b.clone());
                    }
                }
                if matches.len() == 1 {
                    matches.pop()
                } else {
                    None
                }
            }
        };
        let Some(found) = found else { return Err(RepoError::runtime(format!("no such backup: {spec}"))) };
        let path = str_of(&found, "path")?.ok_or_else(|| RepoError::forward("backup without a path"))?;
        let blob = str_of(&found, "blob")?.ok_or_else(|| RepoError::forward("backup without a blob"))?;
        let id = found.get("id").and_then(Value::py_str).ok_or_else(|| RepoError::forward("backup id"))?;
        let data = self.store.read_blob(&blob)?;
        let out = match output {
            Some(o) => PathBuf::from(o),
            None => self.abspath(&path)?,
        };
        if output.is_none() {
            self.prepare_identity()?;
            let digest = match found.get("digest") {
                None | Some(Value::Null) => None,
                Some(Value::Str(d)) => Some(d.clone()),
                Some(_) => return Err(RepoError::forward("backup digest is not a str")),
            };
            self.backup_worktree_file(&path, &format!("recover {id}"), &[digest], true)?;
        }
        if let Some(parent) = out.parent().filter(|p| !p.as_os_str().is_empty()) {
            fsio::mkdir_p(parent)?;
        }
        fsio::write_bytes(&out, &data)?;
        if output.is_none() {
            self.rewrote(&out);
        }
        Ok((found, out))
    }
}

/// `PurePath(p).suffix`.
pub fn py_suffix(p: &str) -> String {
    let name = p.rsplit(['/', '\\']).next().unwrap_or("");
    match name.rfind('.') {
        Some(i) if i > 0 && i + 1 < name.len() => name[i..].to_string(),
        _ => String::new(),
    }
}
