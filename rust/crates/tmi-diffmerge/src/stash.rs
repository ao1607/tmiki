//! repo.py `stash_push` and `_working_tree_entries_for_stash`: every
//! tracked working file imported (tmi-import), the stash commit, its ref
//! and `.tmi/stash.json` entry, then the working tree put back to HEAD.

use crate::merge::{materialize_file, MergeCtx};
use std::collections::HashMap;
use tmi_repo::repo::{same_entry, Entries};
use tmi_repo::write::{committable, int, is_deleted, obj, s};
use tmi_repo::{fsio, RepoError, Result};
use tmi_store::json::{Map, Value};

/// `_entries_equal(left, right)`.
fn entries_equal(a: &Entries, b: &Entries) -> bool {
    a.len() == b.len() && a.iter().all(|(k, v)| b.get(k).is_some_and(|w| same_entry(Some(v), Some(w))))
}

/// `stash_push(message)`: the new stash.json entry.
pub fn stash_push(ctx: &mut MergeCtx, message: Option<&str>) -> Result<Value> {
    let repo = ctx.repo;
    let Some(current) = repo.current_commit()? else { return Err(RepoError::runtime("cannot stash without a commit")) };
    if repo.pending_merge_head()?.is_some() {
        return Err(RepoError::runtime("cannot stash during a merge"));
    }
    tmi_repo::write::object_timestamp()?; // a form Python may read differently goes now
    repo.prepare_identity()?;
    let head = repo.entries_for_commit(Some(&current))?;
    let index = repo.index_entries()?;
    // _working_tree_entries_for_stash
    let mut rels: Vec<&String> = head.keys().chain(index.keys()).collect();
    rels.sort();
    rels.dedup();
    let mut entries = Entries::new();
    // "stashing <path>" only: Python imports the files without on_progress
    let progress = ctx.progress.take();
    for rel in rels {
        let full = repo.abspath(rel)?;
        if full.exists() {
            tmi_import::report(progress, || format!("stashing {rel}"));
            let base = index.get(rel).filter(|e| e.truthy()).or_else(|| head.get(rel));
            let bytes = fsio::read_bytes(&full)?;
            let name = full.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let entry = materialize_file(ctx, rel, &name, &bytes, &HashMap::new(), base)?;
            entries.insert(rel.clone(), entry);
            continue;
        }
        if let Some(indexed) = index.get(rel).filter(|e| e.truthy() && !is_deleted(e)) {
            entries.insert(rel.clone(), committable(indexed)?);
        }
    }
    if entries_equal(&entries, &head) && index.is_empty() {
        return Err(RepoError::runtime("no local changes to save"));
    }
    let label = match message.filter(|m| !m.is_empty()) {
        Some(m) => m.to_string(),
        None => "WIP".to_string(),
    };
    tmi_repo::formats::refuse_dev_records(entries.iter().chain(index.iter().filter(|(_, e)| !is_deleted(e))))?;
    let tree_ref = repo.write_repository_tree(&entries)?;
    let (name, email) = repo.author()?;
    let timestamp = tmi_repo::write::object_timestamp()?;
    let thumbnail = entries.values().filter_map(|e| e.get("thumbnail_ref")).find(|t| t.truthy()).cloned().unwrap_or(Value::Null);
    let canvas = match entries.values().next() {
        Some(e) => e.get("canvas_meta").cloned().unwrap_or(Value::Null),
        None => Value::Object(Map::new()),
    };
    let commit = obj([
        ("version", int(1)),
        ("tree_ref", s(tree_ref)),
        ("index_entries", Value::Object(index.clone())),
        ("parents", Value::Array(vec![s(current.clone())])),
        ("author", obj([("name", s(name)), ("email", s(email))])),
        ("timestamp", s(timestamp.clone())),
        ("message", s(format!("stash: {label}"))),
        ("thumbnail_ref", thumbnail),
        ("canvas_meta", canvas),
    ]);
    fsio::mark_mutated();
    let oid = repo.store.write_json("commit", &commit)?;
    let stash_id = tmi_repo::clock::id_stamp(tmi_repo::clock::now());
    let ref_name = format!("refs/stash/{stash_id}");
    repo.write_ref(&ref_name, &oid)?;
    let mut stashes = repo.read_stashes()?;
    let entry = obj([
        ("id", s(stash_id)),
        ("ref", s(ref_name)),
        ("commit", s(oid.clone())),
        ("base", s(current.clone())),
        ("message", s(label)),
        ("timestamp", s(timestamp)),
    ]);
    stashes.insert(0, entry.clone());
    repo.write_stashes(&stashes)?;
    repo.restore_all(&current, Some(&oid), "stash push")?;
    repo.write_index_entries(Entries::new())?;
    Ok(entry)
}
