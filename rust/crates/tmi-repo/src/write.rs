//! tmi/repo.py `Repo`, the shared write side: the index, config, refs and
//! HEAD, the reflog, identities and object times, automatic backups of
//! overwritten work, putting a commit's files in the working tree, and the
//! merge / conflict state files.

use crate::config;
use crate::error::{RepoError, Result};
use crate::fsio;
use crate::lockfile::{self, Expect};
use crate::reflog;
use crate::repo::{read_text, same_entry, Entries, Repo};
use crate::status;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use tmi_store::json::{self, DumpOptions, Int, Map, Value};

pub const RESOLVED_CONFLICTS_DIR: &str = ".resolved";

pub const DIRTY_WORKTREE_MESSAGE: &str = "working tree has changes that are not committed; keep them first with \
     'tmiki commit -a' or 'tmiki stash', or pass --discard to overwrite them \
     (they are backed up: 'tmiki recover --files')";

/// A JSON object from (key, value) pairs, in this order.
pub fn obj<const N: usize>(pairs: [(&str, Value); N]) -> Value {
    let mut map = Map::new();
    for (k, v) in pairs {
        map.insert(k.to_string(), v);
    }
    Value::Object(map)
}

pub fn s(text: impl Into<String>) -> Value {
    Value::Str(text.into())
}

pub fn int(n: i64) -> Value {
    Value::Int(Int::Small(n))
}

/// `json.dumps(v, ensure_ascii=False, sort_keys=True, indent=2) + "\n"`.
pub fn dumps_sorted(value: &Value) -> String {
    json::dumps(value, &DumpOptions::INDENTED_SORTED) + "\n"
}

/// `_deleted_index_entry(rel)`.
pub fn deleted_entry(rel: &str) -> Value {
    obj([("path", s(rel)), ("deleted", Value::Bool(true))])
}

/// `_is_deleted_index_entry(entry)`.
pub fn is_deleted(entry: &Value) -> bool {
    entry.get("deleted").is_some_and(Value::truthy)
}

/// `entry.get(key)` as an optional str (Forward for other types).
pub fn str_of(entry: &Value, key: &str) -> Result<Option<String>> {
    match entry.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Str(v)) => Ok(Some(v.clone())),
        Some(_) => Err(RepoError::forward(format!("{key} is not a str"))),
    }
}

/// `(entry or {}).get("source_digest")`.
pub fn digest_of(entry: Option<&Value>) -> Result<Option<String>> {
    match entry {
        None => Ok(None),
        Some(e) => str_of(e, "source_digest"),
    }
}

/// `entry["source_blob_ref"]`: KeyError and non-str values are Python's.
pub fn source_blob_ref(entry: &Value) -> Result<String> {
    str_of(entry, "source_blob_ref")?.ok_or_else(|| RepoError::forward("entry without a source_blob_ref str"))
}

/// `_committable_entry(entry)`.
pub fn committable(entry: &Value) -> Result<Value> {
    let Some(map) = entry.as_object() else { return Err(RepoError::forward("entry is not an object")) };
    let mut cleaned = map.clone();
    let mut meta = match map.get("source_meta") {
        None | Some(Value::Null) => Map::new(),
        Some(v) if !v.truthy() => Map::new(),
        Some(Value::Object(m)) => m.clone(),
        Some(_) => return Err(RepoError::forward("source_meta is not a dict")),
    };
    for key in ["staged_layer_selector", "staged_layer_name", "staged_layer_stable_source_id"] {
        meta.shift_remove(key);
    }
    cleaned.insert("source_meta".into(), Value::Object(meta));
    Ok(Value::Object(cleaned))
}

/// `safe_filename(value)`.
pub fn safe_filename(value: &str) -> String {
    let slashed = value.replace('\\', "／").replace('/', "／");
    let unsafe_char = |c: char| "<>:\"|?*".contains(c) || (c as u32) < 0x20;
    let replaced: String = slashed.chars().map(|c| if unsafe_char(c) { '_' } else { c }).collect();
    let cleaned = replaced.trim_matches(|c| c == ' ' || c == '.').to_string();
    let lossy = cleaned != slashed || value.contains('／');
    let mut cleaned = cleaned;
    if lossy || cleaned.is_empty() {
        let digest = &tmi_store::oid::sha256_hex(value.as_bytes())[..8];
        cleaned = if cleaned.is_empty() { digest.to_string() } else { format!("{cleaned}-{digest}") };
    }
    if cleaned.is_empty() {
        "layer".into()
    } else {
        cleaned
    }
}

/// `os.path.samefile(a, b)` (Windows: same volume and file index).
pub fn samefile(a: &Path, b: &Path) -> std::io::Result<bool> {
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION};
        let info = |p: &Path| -> std::io::Result<(u32, u64)> {
            use std::os::windows::fs::OpenOptionsExt;
            // FILE_FLAG_BACKUP_SEMANTICS opens directories too
            let f = std::fs::OpenOptions::new().read(true).custom_flags(0x0200_0000).open(p)?;
            let mut bi: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
            // SAFETY: a valid handle and a zeroed out-struct.
            if unsafe { GetFileInformationByHandle(f.as_raw_handle() as _, &mut bi) } == 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok((bi.dwVolumeSerialNumber, ((bi.nFileIndexHigh as u64) << 32) | bi.nFileIndexLow as u64))
        };
        Ok(info(a)? == info(b)?)
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let (x, y) = (std::fs::metadata(a)?, std::fs::metadata(b)?);
        Ok(x.dev() == y.dev() && x.ino() == y.ino())
    }
}

impl Repo {
    /// The file a recorded path names in the working tree (`abspath(rel)`),
    /// after `recpath`'s rule, inside the working tree and outside `.tmi`,
    /// with no symbolic link or junction on the way. Every write, delete and
    /// existence check of a recorded path goes through this.
    pub fn abspath(&self, rel: &str) -> Result<PathBuf> {
        crate::recpath::worktree_path(self.require_worktree()?, &self.repo_dir, rel)
    }

    /// `_rewrote(path)`.
    pub fn rewrote(&self, path: &Path) {
        let mut list = self.rewritten.lock().unwrap();
        if !list.iter().any(|p| p == path) {
            list.push(path.to_path_buf());
        }
    }

    /// `read_index().get("entries", {})`.
    pub fn index_entries(&self) -> Result<Entries> {
        entries_of(&self.read_index()?)
    }

    /// `lock_index()`: take `index.lock` (O_CREAT | O_EXCL) and keep it until
    /// the command ends (issue #35). A command that reads the index in order
    /// to change it calls this before the read. Errors at once when another
    /// process holds it; false when this process already does.
    pub fn lock_index(&self) -> Result<bool> {
        let path = self.repo_dir.join("index.lock");
        lockfile::hold(&path, &lockfile::display(&self.repo_dir, &path))
    }

    pub fn unlock_index(&self) {
        lockfile::release(&self.repo_dir.join("index.lock"));
    }

    /// `write_index(index)`, under `index.lock` (taken for this one write
    /// when the command does not already hold it).
    pub fn write_index(&self, index: &Value) -> Result<()> {
        let taken = self.lock_index()?;
        let result = fsio::atomic_write(&self.repo_dir.join("index"), dumps_sorted(index).as_bytes());
        if taken {
            self.unlock_index();
        }
        result
    }

    /// `write_index({"version": 1, "entries": entries})`.
    pub fn write_index_entries(&self, entries: Entries) -> Result<()> {
        self.write_index(&obj([("version", int(1)), ("entries", Value::Object(entries))]))
    }

    pub fn write_config(&self, config: &Value) -> Result<()> {
        fsio::atomic_write(&self.repo_dir.join("config"), dumps_sorted(config).as_bytes())
    }

    /// `write_ref(ref, value)` through `<ref path>.lock`.
    pub fn write_ref(&self, name: &str, value: &str) -> Result<()> {
        self.write_ref_expect(name, value, Expect::Any)
    }

    /// `write_ref(ref, value, expected=...)`: the ref is re-read under its
    /// lock and compared (compare-and-swap); an error if it differs.
    pub fn write_ref_expect(&self, name: &str, value: &str, expect: Expect<'_>) -> Result<()> {
        let path = self.ref_path(name)?;
        self.locked_write(&path, format!("{value}\n").as_bytes(), name, expect)
    }

    /// The compare-and-swap of `write_ref_expect`, but a moved ref is an
    /// `Ok(Some(actual))` for the caller to word (the push of a local remote).
    pub fn try_write_ref(&self, name: &str, value: &str, expect: Expect<'_>) -> Result<Option<Option<String>>> {
        let path = self.ref_path(name)?;
        lockfile::write_locked(&self.repo_dir, &path, format!("{value}\n").as_bytes(), expect)
    }

    fn locked_write(&self, path: &Path, data: &[u8], name: &str, expect: Expect<'_>) -> Result<()> {
        match lockfile::write_locked(&self.repo_dir, path, data, expect)? {
            None => Ok(()),
            Some(actual) => {
                let want = if let Expect::Is(w) = expect { w } else { None };
                Err(RepoError::runtime(lockfile::ref_moved_message(name, want, actual.as_deref())))
            }
        }
    }

    /// `delete_ref(ref, expected=...)`.
    pub fn delete_ref(&self, name: &str, expect: Expect<'_>) -> Result<()> {
        let path = self.ref_path_lenient(name)?;
        match lockfile::delete_locked(&self.repo_dir, &path, expect)? {
            None => Ok(()),
            Some(actual) => {
                let want = if let Expect::Is(w) = expect { w } else { None };
                Err(RepoError::runtime(lockfile::ref_moved_message(name, want, actual.as_deref())))
            }
        }
    }

    pub fn set_head_ref(&self, name: &str) -> Result<()> {
        self.ref_path(name)?;
        self.locked_write(&self.repo_dir.join("HEAD"), format!("ref: {name}\n").as_bytes(), "HEAD", Expect::Any)
    }

    pub fn set_current_commit(&self, commit: &str) -> Result<()> {
        self.set_current_commit_expect(commit, Expect::Any)
    }

    /// `set_current_commit(commit, expected=...)`.
    pub fn set_current_commit_expect(&self, commit: &str, expect: Expect<'_>) -> Result<()> {
        let target = self.head_target()?;
        if target.starts_with("refs/") {
            self.write_ref_expect(&target, commit, expect)
        } else {
            self.locked_write(&self.repo_dir.join("HEAD"), format!("{commit}\n").as_bytes(), "HEAD", expect)
        }
    }

    pub fn detach_head(&self, commit: &str) -> Result<()> {
        self.locked_write(&self.repo_dir.join("HEAD"), format!("{commit}\n").as_bytes(), "HEAD", Expect::Any)
    }

    /// Read `_safe_identity()` now (before anything is written), so a
    /// configuration Python would read differently forwards early.
    pub fn prepare_identity(&self) -> Result<Option<(String, String)>> {
        if let Some(v) = self.identity.get() {
            return Ok(v.clone());
        }
        let v = match self.author() {
            Ok(a) => Some(a),
            Err(RepoError::Runtime(_)) => None,
            Err(e) => return Err(e),
        };
        let _ = self.identity.set(v.clone());
        Ok(v)
    }

    /// `_author()`: config.resolve_identity(self.read_config()).
    pub fn author(&self) -> Result<(String, String)> {
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        let repo_cfg = self.read_config()?;
        let user_cfg = match config::read_user_config() {
            Ok(c) => c,
            // Python raises RuntimeError("invalid config file ...") here.
            Err(e) => return Err(e),
        };
        let merged = |key: &str| -> Result<Option<Value>> {
            if let Some(v) = config::get_value(&repo_cfg, key)? {
                if !v.is_null() {
                    return Ok(Some(v.clone()));
                }
            }
            Ok(config::get_value(&user_cfg, key)?.filter(|v| !v.is_null()).cloned())
        };
        let name = match env("TMI_AUTHOR_NAME") {
            Some(n) => Some(Value::Str(n)),
            None => merged("user.name")?,
        };
        let email = match env("TMI_AUTHOR_EMAIL") {
            Some(e) => Some(Value::Str(e)),
            None => merged("user.email")?,
        };
        match (name, email) {
            (Some(n), Some(e)) if n.truthy() && e.truthy() => {
                let text = |v: &Value| v.py_str().ok_or_else(|| RepoError::forward("container identity"));
                Ok((text(&n)?, text(&e)?))
            }
            _ => Err(RepoError::runtime(
                "author identity is not configured.\n  Set it with:\n    tmiki config --global user.name \"Your Name\"\n    \
                 tmiki config --global user.email \"you@example.com\"\n  Or per-repo by omitting --global, or via env vars \
                 TMI_AUTHOR_NAME / TMI_AUTHOR_EMAIL.",
            )),
        }
    }

    /// `_log_ref_move(ref, old, new, reason)`; best effort like Python's.
    pub fn log_ref_move(&self, ref_name: &str, old: Option<&str>, new: &str, reason: &str) -> Result<()> {
        let identity = self.prepare_identity().unwrap_or(None);
        let id = identity.as_ref().map(|(n, e)| (n.as_str(), e.as_str()));
        match reflog::append(&self.repo_dir, ref_name, old, new, id, reason) {
            Ok(()) => Ok(()),
            Err(_) => Ok(()), // OSError: never block the operation
        }
    }

    // -------- automatic backups of overwritten work --------

    pub fn read_backups(&self) -> Result<Vec<Value>> {
        let Some(text) = read_text(&self.repo_dir.join("backups.json"))? else {
            return Ok(Vec::new());
        };
        match json::loads(&text) {
            Ok(Value::Array(items)) => Ok(items.into_iter().filter(|i| i.as_object().is_some()).collect()),
            Ok(_) => Ok(Vec::new()),
            Err(_) => Ok(Vec::new()), // ValueError: no backups
        }
    }

    pub fn write_backups(&self, backups: &[Value]) -> Result<()> {
        let text = json::dumps(&Value::Array(backups.to_vec()), &DumpOptions::INDENTED) + "\n";
        fsio::atomic_write(&self.repo_dir.join("backups.json"), text.as_bytes())
    }

    /// `backup_worktree_file(rel, reason, known_digests=, check_head=)`.
    pub fn backup_worktree_file(
        &self,
        rel: &str,
        reason: &str,
        known: &[Option<String>],
        check_head: bool,
    ) -> Result<Option<String>> {
        let out = self.abspath(rel)?;
        if !out.is_file() {
            return Ok(None);
        }
        let digest = fsio::sha256_file(&out)?;
        let mut safe: Vec<Option<String>> = known.to_vec();
        if check_head {
            let head = self.entries_for_commit(self.current_commit()?.as_deref())?;
            safe.push(digest_of(head.get(rel))?);
            safe.push(digest_of(self.index_entries()?.get(rel))?);
        }
        if safe.iter().any(|d| d.as_deref() == Some(digest.as_str())) {
            return Ok(Some(digest));
        }
        let data = fsio::read_bytes(&out)?;
        fsio::mark_mutated();
        let oid = self.store.write_large_blob(&data).map_err(|e| RepoError::runtime(e.to_string()))?;
        let mut backups = self.read_backups()?;
        let size = std::fs::metadata(&out).map_err(|e| fsio::io_err(&out, e))?.len();
        let id = crate::clock::id_stamp(crate::clock::now());
        let timestamp = crate::clock::isoformat_utc(crate::clock::now());
        backups.insert(
            0,
            obj([
                ("id", s(id)),
                ("path", s(rel)),
                ("blob", s(oid)),
                ("digest", s(digest.clone())),
                ("size", int(size as i64)),
                ("reason", s(reason)),
                ("timestamp", s(timestamp)),
            ]),
        );
        self.write_backups(&backups)?;
        Ok(Some(digest))
    }

    /// `entries_for_checkout(commit)`: the entries of `commit` that are
    /// about to be written to the working tree, each path checked against
    /// `recpath`'s rule first (an `Err` before any ref moves or file is
    /// touched: a tree can hold what tmi's own commands never record).
    pub fn entries_for_checkout(&self, commit: &str) -> Result<Entries> {
        let entries = self.entries_for_commit(Some(commit))?;
        for rel in entries.keys() {
            crate::recpath::check_record_path(rel)?;
        }
        check_paths_fit_together(entries.keys().map(String::as_str))?;
        Ok(entries)
    }

    /// `restore_all(commit, remove_missing_from=, reason=)`.
    pub fn restore_all(&self, commit: &str, remove_missing_from: Option<&str>, reason: &str) -> Result<Vec<PathBuf>> {
        let target = self.entries_for_checkout(commit)?;
        let old = match remove_missing_from {
            Some(c) if !c.is_empty() => self.entries_for_commit(Some(c))?,
            _ => Entries::new(),
        };
        // nothing is written or deleted before every path is known to be safe
        for rel in old.keys().filter(|k| !target.contains_key(k.as_str())) {
            self.abspath(rel)?;
        }
        for rel in target.keys() {
            self.abspath(rel)?;
        }
        let head = self.entries_for_commit(self.current_commit()?.as_deref())?;
        let known = |rel: &str| -> Result<Vec<Option<String>>> {
            Ok(vec![digest_of(target.get(rel))?, digest_of(old.get(rel))?, digest_of(head.get(rel))?])
        };
        let mut restored = Vec::new();
        let mut gone: Vec<&String> = old.keys().filter(|k| !target.contains_key(k.as_str())).collect();
        gone.sort();
        gone.reverse();
        for rel in gone {
            let out = self.abspath(rel)?;
            if out.exists() && out.is_file() {
                self.backup_worktree_file(rel, reason, &known(rel)?, false)?;
                fsio::unlink(&out)?;
                if let Some(parent) = out.parent() {
                    self.remove_empty_parent_dirs(parent)?;
                }
                restored.push(out);
            }
        }
        for (rel, entry) in &target {
            let out = self.abspath(rel)?;
            if out.is_file() && self.backup_worktree_file(rel, reason, &known(rel)?, false)? == digest_of(Some(entry))? {
                continue;
            }
            fsio::mkdir_parent(&out)?;
            let data = self.store.read_blob(&source_blob_ref(entry)?)?;
            fsio::write_bytes(&out, &data)?;
            self.rewrote(&out);
            restored.push(out);
        }
        Ok(restored)
    }

    /// `_remove_empty_parent_dirs(start)`.
    pub fn remove_empty_parent_dirs(&self, start: &Path) -> Result<()> {
        let root = self.require_worktree()?.to_path_buf();
        let mut cur = crate::py::realpath(start);
        while cur != root && cur.starts_with(&root) && cur != root {
            if fsio::rmdir(&cur).is_err() {
                return Ok(());
            }
            match cur.parent() {
                Some(p) => cur = p.to_path_buf(),
                None => return Ok(()),
            }
        }
        Ok(())
    }

    /// `_ensure_no_untracked_overwrite(target_entries, current_commit, op)`.
    pub fn ensure_no_untracked_overwrite(&self, target: &Entries, current: Option<&str>, operation: &str) -> Result<()> {
        if self.worktree.is_none() {
            return Ok(());
        }
        let tracked = match current {
            Some(c) if !c.is_empty() => self.entries_for_commit(Some(c))?,
            _ => Entries::new(),
        };
        let staged = self.index_entries()?;
        let mut collisions = Vec::new();
        let mut names: Vec<&String> =
            target.keys().filter(|k| !tracked.contains_key(k.as_str()) && !staged.contains_key(k.as_str())).collect();
        names.sort();
        for rel in names {
            if self.abspath(rel)?.exists() {
                collisions.push(rel.clone());
            }
        }
        if collisions.is_empty() {
            return Ok(());
        }
        let mut summary = collisions.iter().take(5).cloned().collect::<Vec<_>>().join(", ");
        if collisions.len() > 5 {
            summary += &format!(", +{} more", collisions.len() - 5);
        }
        Err(RepoError::runtime(format!("untracked files would be overwritten by {operation}: {summary}")))
    }

    /// The non-untracked items of `status_items(with_layer_detail=False)`.
    pub fn dirty_items(&self) -> Result<Vec<status::StatusItem>> {
        let mut warns = tmi_import::Warnings::default();
        Ok(crate::worktree_status::status_items(self, false, &mut warns, None)?.into_iter().filter(|i| i.state != "untracked").collect())
    }

    /// `_ensure_clean_tracked(operation)`.
    pub fn ensure_clean_tracked(&self, operation: &str) -> Result<()> {
        let dirty = self.dirty_items()?;
        if dirty.is_empty() {
            return Ok(());
        }
        let mut summary = dirty.iter().take(5).map(|i| format!("{} {}", i.state, i.path)).collect::<Vec<_>>().join(", ");
        if dirty.len() > 5 {
            summary += &format!(", +{} more", dirty.len() - 5);
        }
        Err(RepoError::runtime(format!("working tree has changes; commit or restore them before {operation}: {summary}")))
    }

    // -------- merge state and conflict records --------

    pub fn clear_pending_merge(&self) -> Result<()> {
        for name in ["MERGE_HEAD", "MERGE_MSG"] {
            fsio::unlink_missing_ok(&self.repo_dir.join(name))?;
        }
        Ok(())
    }

    pub fn conflict_dir_for(&self, path: &str) -> PathBuf {
        self.repo_dir.join("conflicts").join(safe_filename(path))
    }

    pub fn resolved_conflict_dir_for(&self, path: &str) -> PathBuf {
        self.repo_dir.join("conflicts").join(RESOLVED_CONFLICTS_DIR).join(safe_filename(path))
    }

    pub fn conflict_record_dir(&self, path: &str) -> PathBuf {
        let live = self.conflict_dir_for(path);
        if live.join("conflict.json").exists() {
            return live;
        }
        let resolved = self.resolved_conflict_dir_for(path);
        if resolved.join("conflict.json").exists() {
            return resolved;
        }
        live
    }

    pub fn pending_conflict_paths(&self) -> Result<Vec<String>> {
        conflict_paths_in(&self.repo_dir.join("conflicts"))
    }

    pub fn resolved_conflict_paths(&self) -> Result<Vec<String>> {
        conflict_paths_in(&self.repo_dir.join("conflicts").join(RESOLVED_CONFLICTS_DIR))
    }

    /// `read_conflict_meta(path)`.
    pub fn read_conflict_meta(&self, path: &str) -> Result<Option<Value>> {
        let meta = self.conflict_record_dir(path).join("conflict.json");
        match read_text(&meta)? {
            None => Ok(None),
            Some(text) => json::loads(&text).map(Some).map_err(|_| RepoError::forward("conflict.json does not parse")),
        }
    }

    /// `conflict_choices(rel)`: each conflict of `rel` with the side taken
    /// (`taken`) and whether the file changed since (`edited_since`).
    pub fn conflict_choices(&self, rel: &str) -> Result<Vec<(Value, Option<Value>, bool)>> {
        let meta = self.read_conflict_meta(rel)?.filter(Value::truthy).unwrap_or(Value::Object(Map::new()));
        if meta.as_object().is_none() {
            return Err(RepoError::forward("conflict.json is not an object"));
        }
        let taken = match meta.get("taken") {
            Some(v) if v.truthy() => v.as_object().cloned().ok_or_else(|| RepoError::forward("taken is not a dict"))?,
            _ => Map::new(),
        };
        let full = self.abspath(rel)?;
        let digest = if full.is_file() { Some(fsio::sha256_file(&full)?) } else { None };
        let as_opt_str = |v: Option<&Value>| -> Result<Option<String>> {
            match v {
                None | Some(Value::Null) => Ok(None),
                Some(Value::Str(s)) => Ok(Some(s.clone())),
                Some(_) => Err(RepoError::forward("digest is not a str")),
            }
        };
        let edited_since = if !taken.is_empty() {
            digest != as_opt_str(meta.get("taken_digest"))?
        } else {
            let wd = meta.get("worktree_digest");
            wd.is_some_and(Value::truthy) && digest != as_opt_str(wd)?
        };
        let mut out = Vec::new();
        let conflicts = match meta.get("conflicts") {
            Some(v) if v.truthy() => v.as_array().cloned().ok_or_else(|| RepoError::forward("conflicts is not a list"))?,
            _ => Vec::new(),
        };
        for item in conflicts {
            if item.as_object().is_none() {
                return Err(RepoError::forward("conflict item is not a dict"));
            }
            let sid = match item.get("stable_source_id") {
                Some(v) if v.truthy() => v.as_str().ok_or_else(|| RepoError::forward("stable_source_id is not a str"))?.to_string(),
                _ => String::new(),
            };
            let side = taken.get(sid.as_str()).filter(|v| v.truthy()).or_else(|| taken.get("*")).cloned();
            out.push((item, side, edited_since));
        }
        Ok(out)
    }

    /// `reopen_conflict(path)`.
    pub fn reopen_conflict(&self, path: &str) -> Result<bool> {
        let resolved = self.resolved_conflict_dir_for(path);
        if self.pending_merge_head()?.is_none() || !resolved.join("conflict.json").exists() {
            return Ok(false);
        }
        let live = self.conflict_dir_for(path);
        if live.exists() {
            fsio::rmtree(&live)?;
        }
        fsio::mkdir_parent(&live)?;
        fsio::replace(&resolved, &live)?;
        Ok(true)
    }

    /// `clear_conflict_files(path)`.
    pub fn clear_conflict_files(&self, path: &str) -> Result<()> {
        let sub = self.conflict_dir_for(path);
        if !sub.exists() {
            return Ok(());
        }
        if self.pending_merge_head()?.is_some() && sub.join("conflict.json").exists() {
            let kept = self.resolved_conflict_dir_for(path);
            if kept.exists() {
                fsio::rmtree_ignore(&kept);
            }
            fsio::mkdir_parent(&kept)?;
            if std::fs::rename(&sub, &kept).is_ok() {
                fsio::mark_mutated();
                return Ok(());
            }
        }
        fsio::rmtree_ignore(&sub);
        Ok(())
    }

    /// `clear_all_conflict_files()`.
    pub fn clear_all_conflict_files(&self) -> Result<()> {
        let root = self.repo_dir.join("conflicts");
        let Ok(dir) = std::fs::read_dir(&root) else { return Ok(()) };
        for sub in dir.flatten() {
            let p = sub.path();
            if p.is_dir() {
                fsio::rmtree_ignore(&p);
            } else {
                fsio::unlink_missing_ok(&p)?;
            }
        }
        Ok(())
    }

    /// `_object_timestamp()`.
    pub fn object_timestamp(&self) -> Result<String> {
        object_timestamp()
    }

    /// `write_repository_tree(entries)`.
    pub fn write_repository_tree(&self, entries: &Entries) -> Result<String> {
        let mut keys: Vec<&String> = entries.keys().collect();
        keys.sort();
        let ordered: Vec<Value> = keys.into_iter().map(|k| entries[k.as_str()].clone()).collect();
        let tree = obj([("version", int(1)), ("kind", s("repository")), ("entries", Value::Array(ordered))]);
        fsio::mark_mutated();
        Ok(self.store.write_json("tree", &tree)?)
    }
}

/// `_object_timestamp()`: TMI_COMMIT_TIMESTAMP as given, else now.
pub fn object_timestamp() -> Result<String> {
    if let Some(fixed) = std::env::var("TMI_COMMIT_TIMESTAMP").ok().filter(|v| !v.is_empty()) {
        if crate::clock::parse_iso(&fixed).is_none() {
            return Err(RepoError::forward("TMI_COMMIT_TIMESTAMP that fromisoformat may read"));
        }
        return Ok(fixed);
    }
    Ok(crate::clock::isoformat_utc(crate::clock::now()))
}

/// Paths a tree records that one working tree cannot hold together: two
/// that differ only in letter case (one file on Windows and macOS, the last
/// written wins), or a file that is also a directory of another (`a` and
/// `a/b`: the checkout stops half way, after HEAD has moved). `Err` before
/// anything is written.
pub fn check_paths_fit_together<'a>(paths: impl Iterator<Item = &'a str>) -> Result<()> {
    let mut files: std::collections::HashMap<String, &str> = std::collections::HashMap::new();
    let paths: Vec<&str> = paths.collect();
    for rel in &paths {
        if let Some(other) = files.insert(rel.to_lowercase(), rel) {
            return Err(RepoError::runtime(crate::recpath::invalid_path_message(
                rel,
                &format!("it differs from {} only in letter case", crate::recpath::display_path(other)),
            )));
        }
    }
    for rel in &paths {
        let folded = rel.to_lowercase();
        for (i, _) in folded.match_indices('/') {
            if let Some(file) = files.get(&folded[..i]) {
                return Err(RepoError::runtime(crate::recpath::invalid_path_message(
                    rel,
                    &format!("{} is a file in the same tree", crate::recpath::display_path(file)),
                )));
            }
        }
    }
    Ok(())
}

/// `index.get("entries", {})` of a read index.
pub fn entries_of(index: &Value) -> Result<Entries> {
    match index.get("entries") {
        None => Ok(Entries::new()),
        Some(Value::Object(map)) => {
            if map.values().any(|v| v.as_object().is_none()) {
                return Err(RepoError::forward("index entry is not an object"));
            }
            Ok(map.clone())
        }
        Some(_) => Err(RepoError::forward("index entries is not an object")),
    }
}

/// `_conflict_paths_in(root)`.
pub fn conflict_paths_in(root: &Path) -> Result<Vec<String>> {
    if !root.is_dir() {
        return Ok(Vec::new());
    }
    let mut subs: Vec<PathBuf> = std::fs::read_dir(root)
        .map_err(|e| fsio::io_err(root, e))?
        .flatten()
        .map(|d| d.path())
        .collect();
    sort_paths(&mut subs);
    let mut out = Vec::new();
    for sub in subs {
        let meta = sub.join("conflict.json");
        if !meta.exists() {
            continue;
        }
        let Ok(Some(text)) = read_text(&meta) else { continue };
        let Ok(value) = json::loads(&text) else { continue };
        if let Some(Value::Str(p)) = value.get("path") {
            out.push(p.clone());
        }
    }
    Ok(out)
}

/// `sorted(paths)` of Path objects (case-insensitive on Windows).
pub fn sort_paths(paths: &mut [PathBuf]) {
    if cfg!(windows) {
        paths.sort_by_cached_key(|p| {
            p.components().map(|c| c.as_os_str().to_string_lossy().to_lowercase()).collect::<Vec<_>>()
        });
    } else {
        paths.sort();
    }
}

/// `_index_diff_to_entries(base, target_entries)`.
pub fn index_diff_to_entries(base: &Entries, target: &Entries) -> Value {
    let keys: BTreeSet<&String> = base.keys().chain(target.keys()).collect();
    let mut entries = Map::new();
    for rel in keys {
        let before = base.get(rel.as_str());
        let after = target.get(rel.as_str());
        if same_entry(before, after) {
            continue;
        }
        entries.insert(rel.clone(), after.cloned().unwrap_or_else(|| deleted_entry(rel)));
    }
    obj([("version", int(1)), ("entries", Value::Object(entries))])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A received tree with `a` and `a/b`, or with `A.png` and `a.png`: the
    /// first stopped a switch / merge / clone half way after HEAD had moved,
    /// the second wrote one file over the other on Windows.
    #[test]
    fn a_tree_with_a_file_inside_a_file_or_case_twins_is_refused_before_checkout() {
        let check = |paths: &[&str]| check_paths_fit_together(paths.iter().copied());
        assert!(check(&["a", "a/b"]).is_err());
        assert!(check(&["Dir/x.png", "dir"]).is_err());
        assert!(check(&["A.png", "a.png"]).is_err());
        assert!(check(&["art/É.png", "ART/é.png"]).is_err());
        assert!(check(&["a.png", "a/b.png", "ab/c.png", "b"]).is_ok());
    }
}
