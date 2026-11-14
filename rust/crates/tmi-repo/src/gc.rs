//! repo.py `gc(prune_age_days)`: prune the reflog and old backups, delete
//! objects nothing reaches, empty fan-out directories and scratch data.
//!
//! Everything is decided first (what Python would prune, keep and walk),
//! then written, so a case not reproduced forwards before any change.

use crate::error::{RepoError, Result};
use crate::fsio;
use crate::reflog::{self, ZERO_OID};
use crate::repo::Repo;
use crate::write::is_deleted;
use std::collections::{BTreeSet, HashSet};
use tmi_store::json::{self, Value};
use tmi_store::StoreError;

pub struct GcResult {
    pub kept: usize,
    pub removed: usize,
    pub reflog_entries_removed: usize,
    pub scratch_bytes_removed: u64,
}

fn refs_in(object_type: &str, obj: &Value) -> Vec<String> {
    let mut refs = Vec::new();
    let push_str = |refs: &mut Vec<String>, v: Option<&Value>| {
        if let Some(v) = v {
            if v.truthy() {
                if let Some(s) = v.as_str() {
                    refs.push(s.to_string());
                }
            }
        }
    };
    let entry_refs = |refs: &mut Vec<String>, entry: &Value| {
        for key in ["source_blob_ref", "tree_ref", "thumbnail_ref", "composite_ref"] {
            push_str(refs, entry.get(key));
        }
    };
    match object_type {
        "commit" => {
            if let Some(Value::Array(ps)) = obj.get("parents") {
                for p in ps {
                    if let Some(s) = p.as_str() {
                        refs.push(s.to_string());
                    }
                }
            }
            for key in ["tree_ref", "thumbnail_ref", "composite_ref"] {
                push_str(&mut refs, obj.get(key));
            }
            if let Some(Value::Object(ie)) = obj.get("index_entries") {
                for e in ie.values() {
                    if e.as_object().is_some() {
                        entry_refs(&mut refs, e);
                    }
                }
            }
        }
        "tag" => push_str(&mut refs, obj.get("object")),
        "chunked" => {
            if let Some(Value::Array(parts)) = obj.get("parts") {
                for p in parts {
                    if let Some(s) = p.as_str() {
                        refs.push(s.to_string());
                    }
                }
            }
            push_str(&mut refs, obj.get("glue"));
        }
        "tree" => match obj.get("kind").and_then(Value::as_str) {
            Some("repository") => {
                if let Some(Value::Array(es)) = obj.get("entries") {
                    for e in es {
                        entry_refs(&mut refs, e);
                    }
                }
            }
            Some("layer-tree") => {
                if let Some(Value::Array(ls)) = obj.get("layers") {
                    for l in ls {
                        for key in ["blob_ref", "subtree_ref", "mask_ref"] {
                            push_str(&mut refs, l.get(key));
                        }
                    }
                }
            }
            _ => {}
        },
        _ => {}
    }
    refs
}

/// Whether `refs_in` reads `obj` as Python's `_referenced_oids` would
/// (the shapes it handles without raising).
fn shapes_ok(object_type: &str, obj: &Value) -> bool {
    let list_ok = |v: Option<&Value>| match v {
        None | Some(Value::Null) => true,
        Some(v) if !v.truthy() => true,
        Some(Value::Array(_)) => true,
        _ => false,
    };
    let scalar_ok = |v: Option<&Value>| matches!(v, None | Some(Value::Null) | Some(Value::Str(_))) || v.is_some_and(|x| !x.truthy());
    if obj.as_object().is_none() {
        return false;
    }
    match object_type {
        "commit" => {
            list_ok(obj.get("parents"))
                && obj.get("parents").and_then(Value::as_array).is_none_or(|ps| ps.iter().all(|p| p.as_str().is_some()))
                && ["tree_ref", "thumbnail_ref", "composite_ref"].iter().all(|k| scalar_ok(obj.get(k)))
        }
        "tag" => scalar_ok(obj.get("object")),
        "chunked" => list_ok(obj.get("parts")) && scalar_ok(obj.get("glue")),
        "tree" => {
            let kind = obj.get("kind");
            match kind.and_then(Value::as_str) {
                Some("repository") => {
                    list_ok(obj.get("entries"))
                        && obj.get("entries").and_then(Value::as_array).is_none_or(|es| {
                            es.iter().all(|e| {
                                e.as_object().is_some()
                                    && ["source_blob_ref", "tree_ref", "thumbnail_ref", "composite_ref"]
                                        .iter()
                                        .all(|k| scalar_ok(e.get(k)))
                            })
                        })
                }
                Some("layer-tree") => {
                    list_ok(obj.get("layers"))
                        && obj.get("layers").and_then(Value::as_array).is_none_or(|ls| {
                            ls.iter().all(|l| {
                                l.as_object().is_some() && ["blob_ref", "subtree_ref", "mask_ref"].iter().all(|k| scalar_ok(l.get(k)))
                            })
                        })
                }
                _ => true,
            }
        }
        _ => true,
    }
}

impl Repo {
    /// `_walk_reachable(roots)`: the same set as Python's depth-first walk,
    /// read a level at a time on all cores (every object is decompressed
    /// and its id checked, as `store.read` does).
    fn walk_reachable(&self, roots: &BTreeSet<String>) -> Result<HashSet<String>> {
        use rayon::prelude::*;
        let mut reachable: HashSet<String> = HashSet::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut frontier: Vec<String> = roots.iter().filter(|r| !r.is_empty()).cloned().collect();
        while !frontier.is_empty() {
            frontier.retain(|o| seen.insert(o.clone()));
            // None: skipped (missing, damaged); Some(refs): reachable
            let store = &self.store;
            let read: Vec<Result<Option<Vec<String>>>> = frontier
                .par_iter()
                .map(|oid| {
                    let (object_type, payload) = match store.read(oid) {
                        Ok(v) => v,
                        Err(StoreError::Missing(_)) | Err(StoreError::HashMismatch { .. }) => return Ok(None),
                        Err(StoreError::Other(msg)) if msg.contains("has no type header") || msg.contains("non-ASCII type") => {
                            return Ok(None)
                        }
                        Err(e) => return Err(RepoError::forward(format!("gc reading {oid}: {e}"))),
                    };
                    if object_type == "blob" {
                        return Ok(Some(Vec::new()));
                    }
                    let Ok(obj) = json::loads_bytes(&payload) else { return Ok(Some(Vec::new())) };
                    if !shapes_ok(&object_type, &obj) {
                        return Err(RepoError::forward("object shape _referenced_oids may read differently"));
                    }
                    Ok(Some(refs_in(&object_type, &obj)))
                })
                .collect();
            let mut next = Vec::new();
            for (oid, r) in frontier.iter().zip(read) {
                if let Some(refs) = r? {
                    reachable.insert(oid.clone());
                    next.extend(refs.into_iter().filter(|o| !seen.contains(o)));
                }
            }
            frontier = next;
        }
        Ok(reachable)
    }

    /// The objects `roots` reach (by `refs_in`, the links `gc` keeps), that
    /// are in the store: what a push of `roots` must send. Only commits,
    /// trees, tags and chunk manifests are read; blobs are known by their
    /// type header. Objects nothing pushed reaches (other branches, stashes,
    /// files added and never committed) stay here.
    pub fn closure(&self, roots: &[String]) -> Result<HashSet<String>> {
        use rayon::prelude::*;
        let mut found: HashSet<String> = HashSet::new();
        let mut frontier: Vec<String> = roots.iter().filter(|r| !r.is_empty()).cloned().collect();
        while !frontier.is_empty() {
            frontier.retain(|o| found.insert(o.clone()));
            let store = &self.store;
            let read: Vec<std::result::Result<Vec<String>, String>> = frontier
                .par_iter()
                .map(|oid| {
                    let object_type = match store.peek_type(oid) {
                        Ok(t) => t,
                        Err(StoreError::Missing(_)) => return Err(oid.clone()),
                        Err(_) => return Ok(Vec::new()), // damaged: sent as it is, as before
                    };
                    if !tmi_store::limits::JSON_TYPES.contains(&object_type.as_str()) {
                        return Ok(Vec::new());
                    }
                    Ok(store.read_json(oid, None).map(|obj| refs_in(&object_type, &obj)).unwrap_or_default())
                })
                .collect();
            let mut next = Vec::new();
            for r in read {
                match r {
                    Ok(refs) => next.extend(refs.into_iter().filter(|o| !found.contains(o))),
                    // not here (the remote has it, or it never was): nothing to send
                    Err(missing) => {
                        found.remove(&missing);
                    }
                }
            }
            frontier = next;
        }
        Ok(found)
    }

    /// `gc(prune_age_days)`.
    pub fn gc(&self, prune_age_days: i64) -> Result<GcResult> {
        let plan = reflog::plan_prune(&self.repo_dir, prune_age_days)?;
        let mut roots: BTreeSet<String> = self.all_refs_sorted()?.into_iter().map(|(_, v)| v).collect();
        if let Some(head) = self.current_commit()? {
            roots.insert(head);
        }
        roots.extend(plan.reachable_oids().into_iter().filter(|o| o != ZERO_OID));
        // _prune_backups
        let backups = self.read_backups()?;
        let now = crate::clock::now();
        let cutoff = now.secs as f64 + now.micros as f64 / 1e6 - (prune_age_days.max(0) * 86_400) as f64;
        let mut kept_backups = Vec::new();
        for b in &backups {
            let when = match b.get("timestamp") {
                None => 0.0,
                Some(Value::Str(ts)) => match crate::clock::parse_iso(ts) {
                    Some(crate::clock::Iso::Aware { micros, .. }) => micros as f64 / 1e6,
                    _ => return Err(RepoError::forward("backup timestamp fromisoformat may read differently")),
                },
                Some(_) => return Err(RepoError::forward("backup timestamp is not a str")),
            };
            if prune_age_days > 0 && when >= cutoff {
                kept_backups.push(b.clone());
            }
        }
        for b in &kept_backups {
            match b.get("blob") {
                Some(v) if v.truthy() => {
                    roots.insert(v.as_str().ok_or_else(|| RepoError::forward("backup blob is not a str"))?.to_string());
                }
                _ => {}
            }
        }
        for entry in self.index_entries()?.values() {
            if is_deleted(entry) {
                continue;
            }
            for key in ["source_blob_ref", "tree_ref", "thumbnail_ref", "composite_ref"] {
                match entry.get(key) {
                    Some(v) if v.truthy() => {
                        roots.insert(v.as_str().ok_or_else(|| RepoError::forward("index ref is not a str"))?.to_string());
                    }
                    _ => {}
                }
            }
        }
        roots.remove("");
        let reachable = self.walk_reachable(&roots)?;
        let existing = self.store.iter_object_ids();
        let unreachable: Vec<&String> = existing.iter().filter(|o| !reachable.contains(*o)).collect();

        // Now write, in Python's order.
        plan.apply()?;
        if kept_backups.len() != backups.len() {
            self.write_backups(&kept_backups)?;
        }
        for oid in &unreachable {
            let _ = fsio::unlink(&self.store.path_for(oid));
        }
        if let Ok(dir) = std::fs::read_dir(&self.store.objects_dir) {
            for sub in dir.flatten() {
                let p = sub.path();
                if p.is_dir() && std::fs::read_dir(&p).map(|mut d| d.next().is_none()).unwrap_or(false) {
                    fsio::rmdir(&p).map_err(|e| fsio::io_err(&p, e))?;
                }
            }
        }
        let mut scratch_bytes = 0u64;
        for name in ["diff", "cache", "tmp"] {
            let scratch = self.repo_dir.join(name);
            if !scratch.is_dir() {
                continue;
            }
            let Ok(children) = std::fs::read_dir(&scratch) else { continue };
            for child in children.flatten() {
                let p = child.path();
                if p.is_dir() {
                    let Ok(files) = crate::repo::walk_files(&p) else { continue };
                    let size: u64 = files.iter().filter_map(|(_, f)| std::fs::metadata(f).ok()).map(|m| m.len()).sum();
                    scratch_bytes += size;
                    fsio::rmtree_ignore(&p);
                } else if let Ok(m) = std::fs::metadata(&p) {
                    scratch_bytes += m.len();
                    let _ = fsio::unlink(&p);
                }
            }
        }
        Ok(GcResult {
            kept: reachable.len(),
            removed: unreachable.len(),
            reflog_entries_removed: plan.removed,
            scratch_bytes_removed: scratch_bytes,
        })
    }
}
