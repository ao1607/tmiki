//! repo.py `commit` and `commit_amend` (stage 3b-2) with the pre- and
//! post-commit hooks (hooks.rs, 3b-1), without plugins or a pending
//! merge: the CLI hands those cases to Python.

use crate::error::{RepoError, Result};
use crate::hooks;
use crate::py;
use crate::repo::{parents_of, same_entry, Entries, Repo};
use crate::stage::{committable, is_deleted};
use crate::wfs;
use tmi_store::json::{Int, Map, Value};

fn index_parts(repo: &Repo) -> Result<(Vec<(String, Value)>, Vec<String>)> {
    let index = repo.read_index()?;
    let entries = match index.get("entries") {
        None => Map::new(),
        Some(Value::Object(m)) => m.clone(),
        Some(_) => return Err(RepoError::forward("index entries is not an object")),
    };
    let mut staged = Vec::new();
    let mut deleted = Vec::new();
    for (path, entry) in entries {
        if entry.as_object().is_none() {
            return Err(RepoError::forward("index entry is not an object"));
        }
        if is_deleted(&entry) {
            deleted.push(path);
        } else {
            staged.push((path, committable(&entry)));
        }
    }
    Ok((staged, deleted))
}

/// `entries.update(staged)` then pop the deleted (dict order kept).
fn apply(mut entries: Entries, staged: &[(String, Value)], deleted: &[String]) -> Entries {
    for (p, e) in staged {
        entries.insert(p.clone(), e.clone());
    }
    for p in deleted {
        entries.shift_remove(p);
    }
    entries
}

fn entries_equal(a: &Entries, b: &Entries) -> bool {
    a.len() == b.len() && a.iter().all(|(k, v)| b.get(k).is_some_and(|w| same_entry(Some(v), Some(w))))
}

/// `write_repository_tree(entries)`.
fn write_repository_tree(repo: &Repo, entries: &Entries) -> Result<String> {
    let mut keys: Vec<&String> = entries.keys().collect();
    keys.sort();
    let ordered: Vec<Value> = keys.into_iter().map(|k| entries[k].clone()).collect();
    let mut tree = Map::new();
    tree.insert("version".into(), Value::Int(Int::Small(1)));
    tree.insert("kind".into(), Value::Str("repository".into()));
    tree.insert("entries".into(), Value::Array(ordered));
    Ok(repo.store.write_json("tree", &Value::Object(tree))?)
}

fn first_thumbnail<'a>(entries: impl Iterator<Item = &'a Value>) -> Value {
    entries
        .filter_map(|e| e.get("thumbnail_ref"))
        .find(|t| t.truthy())
        .cloned()
        .unwrap_or(Value::Null)
}

fn write_commit(
    repo: &Repo,
    entries: &Entries,
    staged: &[(String, Value)],
    parents: Vec<String>,
    message: &str,
) -> Result<String> {
    let tree_ref = write_repository_tree(repo, entries)?;
    let thumbnail = match first_thumbnail(staged.iter().map(|(_, e)| e)) {
        Value::Null => first_thumbnail(entries.values()),
        t => t,
    };
    let (name, email) = wfs::resolve_identity(repo)?;
    let mut author = Map::new();
    author.insert("name".into(), Value::Str(name));
    author.insert("email".into(), Value::Str(email));
    let canvas = match entries.values().next() {
        Some(e) => e.get("canvas_meta").cloned().unwrap_or(Value::Null),
        None => Value::Object(Map::new()),
    };
    let mut c = Map::new();
    c.insert("version".into(), Value::Int(Int::Small(1)));
    c.insert("tree_ref".into(), Value::Str(tree_ref));
    c.insert("parents".into(), Value::Array(parents.into_iter().map(Value::Str).collect()));
    c.insert("author".into(), Value::Object(author));
    c.insert("timestamp".into(), Value::Str(wfs::object_timestamp()?));
    c.insert("message".into(), Value::Str(message.to_string()));
    c.insert("thumbnail_ref".into(), thumbnail);
    c.insert("canvas_meta".into(), canvas);
    Ok(repo.store.write_json("commit", &Value::Object(c))?)
}

fn finish(repo: &Repo, current: Option<&str>, oid: &str, reason: &str) -> Result<()> {
    wfs::set_current_commit(repo, oid, crate::lockfile::Expect::Is(current))?;
    let mut empty = Map::new();
    empty.insert("version".into(), Value::Int(Int::Small(1)));
    empty.insert("entries".into(), Value::Object(Map::new()));
    wfs::write_index(repo, &Value::Object(empty))?;
    let identity = wfs::resolve_identity(repo).ok();
    wfs::reflog_append(repo, "HEAD", current, oid, identity.as_ref(), reason);
    let target = repo.head_target()?;
    if target.starts_with("refs/heads/") {
        wfs::reflog_append(repo, &target, current, oid, identity.as_ref(), reason);
    }
    Ok(())
}

/// `commit(message, allow_empty)`, the commit that concludes a pending
/// merge included (stage 4a). `before_hook` runs right before a hook
/// starts (see hooks::run).
pub fn commit(repo: &Repo, message: &str, allow_empty: bool, before_hook: &mut dyn FnMut()) -> Result<String> {
    let (staged, deleted) = index_parts(repo)?;
    let current = repo.current_commit()?;
    let merge_other = repo.pending_merge_head()?;
    if staged.is_empty() && deleted.is_empty() && !allow_empty && merge_other.is_none() {
        return Err(RepoError::runtime("nothing to commit"));
    }
    if merge_other.is_some() {
        let conflicts = repo.pending_conflict_paths()?;
        if !conflicts.is_empty() {
            let mut summary = conflicts.iter().take(5).cloned().collect::<Vec<_>>().join(", ");
            if conflicts.len() > 5 {
                summary += &format!(", +{} more", conflicts.len() - 5);
            }
            return Err(RepoError::runtime(format!("unresolved merge conflicts: {summary}")));
        }
    }
    let before = repo.entries_for_commit(current.as_deref())?;
    let entries = apply(before.clone(), &staged, &deleted);
    if entries_equal(&entries, &before) && !allow_empty && merge_other.is_none() {
        return Err(RepoError::runtime("nothing to commit"));
    }
    crate::formats::refuse_dev_records(&entries)?;
    hooks::run(&repo.repo_dir, "pre-commit", &[("TMI_COMMIT_MSG", message)], true, before_hook)?;
    // Like git, index changes the pre-commit hook made belong in this
    // commit: recompute from the live index.
    let (staged, deleted) = index_parts(repo)?;
    let entries = apply(repo.entries_for_commit(current.as_deref())?, &staged, &deleted);
    if entries_equal(&entries, &before) && !allow_empty && merge_other.is_none() {
        return Err(RepoError::runtime("nothing to commit"));
    }
    crate::formats::refuse_dev_records(&entries)?;
    let mut parents: Vec<String> = current.iter().cloned().collect();
    if let Some(other) = &merge_other {
        if !parents.contains(other) {
            parents.push(other.clone());
        }
    }
    let oid = write_commit(repo, &entries, &staged, parents, message)?;
    let first = py::first_line(message);
    let reason = if merge_other.is_some() { format!("merge: {first}") } else { format!("commit: {first}") };
    finish(repo, current.as_deref(), &oid, &reason)?;
    if merge_other.is_some() {
        repo.clear_pending_merge()?;
        repo.clear_all_conflict_files()?;
    }
    hooks::run(&repo.repo_dir, "post-commit", &[("TMI_COMMIT", &oid), ("TMI_COMMIT_MSG", message)], false, before_hook)?;
    Ok(oid)
}

/// `commit_amend(message)`.
pub fn commit_amend(repo: &Repo, message: Option<&str>, before_hook: &mut dyn FnMut()) -> Result<String> {
    if repo.pending_merge_head()?.is_some() {
        return Err(RepoError::runtime("cannot amend during a merge; finish the merge with 'tmiki commit' or discard it"));
    }
    let Some(current) = repo.current_commit()? else {
        return Err(RepoError::runtime("nothing to amend (no commits yet)"));
    };
    let previous = repo.read_commit(&current)?;
    let (staged, deleted) = index_parts(repo)?;
    let entries = apply(repo.entries_for_commit(Some(&current))?, &staged, &deleted);
    let new_message = match message {
        Some(m) => m.to_string(),
        None => match previous.get("message") {
            None => String::new(),
            Some(Value::Str(s)) => s.clone(),
            Some(_) => return Err(RepoError::forward("commit message is not a str")),
        },
    };
    let parents = parents_of(&previous)?;
    crate::formats::refuse_dev_records(&entries)?;
    hooks::run(&repo.repo_dir, "pre-commit", &[("TMI_COMMIT_MSG", &new_message)], true, before_hook)?;
    let oid = write_commit(repo, &entries, &staged, parents, &new_message)?;
    let first = py::first_line(&new_message);
    finish(repo, Some(&current), &oid, &format!("commit (amend): {first}"))?;
    hooks::run(&repo.repo_dir, "post-commit", &[("TMI_COMMIT", &oid), ("TMI_COMMIT_MSG", &new_message)], false, before_hook)?;
    Ok(oid)
}
