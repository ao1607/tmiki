//! repo.py `status_items` in full (stage 3b-2): files the stat cache does
//! not vouch for are hashed and the cache rewritten, and a modified file's
//! layer changes are described by importing it (tmi-import, as
//! `_import_to_compare`: no composite, unchanged layers not decoded).

use crate::error::{RepoError, Result};
use crate::ignore;
use crate::layer_changes::layer_changes;
use crate::pyeq::py_eq;
use crate::repo::{Entries, Repo};
use crate::stage::{file_name, flatten_layers, is_deleted, layer_blob_reuse, reuse_decoded, reuse_pixels, ImportContext, StatCache};
use crate::status::{untracked_candidates, StatusItem};
use std::collections::HashSet;
use std::path::Path;
use tmi_import::{report, LayerBlobs, Options, Progress, Warnings};
use tmi_store::json::{Map, Value};

fn item(state: &'static str, path: &str, detail: impl Into<String>) -> StatusItem {
    StatusItem { state, path: path.to_string(), detail: detail.into() }
}

/// `_import_to_compare(full, entry, ...)` then `layer_changes(entry, ...)`:
/// the change lines, and whether the canvas_meta is unchanged.
fn describe(repo: &Repo, ctx: &ImportContext, rel: &str, full: &Path, entry: &Value, source: (Vec<u8>, String), warns: &mut Warnings) -> Result<(Vec<String>, bool)> {
    let reuse = layer_blob_reuse(repo, Some(entry))?;
    let mut legacy = HashSet::new();
    if let Some(Value::Str(tree_ref)) = entry.get("tree_ref").filter(|v| v.truthy()) {
        let mut layers = Vec::new();
        flatten_layers(repo, tree_ref, &mut layers, 0)?;
        for l in layers {
            if l.get("pixel_sha256").is_none_or(Value::is_null) {
                match l.get("stable_source_id") {
                    Some(Value::Str(s)) if !s.is_empty() => {
                        legacy.insert(s.clone());
                    }
                    Some(v) if v.truthy() => return Err(RepoError::forward("stable_source_id is not a str")),
                    _ => {}
                }
            }
        }
    }
    let pixels = reuse_pixels(&reuse);
    let decoded = reuse_decoded(&reuse);
    let blobs = LayerBlobs::Only(legacy);
    let opts = Options {
        render_composite: false,
        cache: None,
        reuse_pixels: (!pixels.is_empty()).then_some(&pixels),
        reuse_decoded: decoded.as_ref(),
        layer_blobs: &blobs,
        composite_base: None,
        limits: ctx.limits,
        verify_partial: false,
        progress: None,
    };
    let (bytes, digest) = source;
    let imported = ctx.formats.import(repo, rel, &file_name(full), &bytes, &digest, &opts, warns)?;
    let changes = layer_changes(repo, entry, &imported)?;
    let same_canvas = py_eq(entry.get("canvas_meta").unwrap_or(&Value::Null), &Value::Object(imported.canvas_meta.clone()));
    Ok((changes, same_canvas))
}

/// `describe`, reading the file when the stat cache answered; any failure
/// is Python's "layer scan failed: ..." (its exception text): Forward. A
/// file `.tmiattributes` maps to a plugin never goes to Python (which does
/// not run plugins): its plugin missing, the line says so (PLUGINS.md 4.4);
/// any other failure is the same "layer scan failed" line, said here.
fn describe_or_forward(
    repo: &Repo,
    ctx: &ImportContext,
    rel: &str,
    full: &Path,
    entry: &Value,
    source: Option<(Vec<u8>, String)>,
    warns: &mut Warnings,
) -> Result<(Vec<String>, bool)> {
    let plugin = ctx.formats.mapping(rel).plugin_id().map(str::to_string);
    if let Some(id) = &plugin {
        if ctx.formats.plugin(repo, id).is_err() {
            return Ok((vec![format!("(layer details need plugin '{id}')")], false));
        }
    }
    let source = match source {
        Some(s) => s,
        None => crate::stage::read_file_digest(full)?,
    };
    match describe(repo, ctx, rel, full, entry, source, warns) {
        Ok(d) => Ok(d),
        Err(RepoError::Forward(w)) if plugin.is_none() => Err(RepoError::Forward(w)),
        Err(e) if plugin.is_none() => Err(RepoError::forward(format!("layer scan failed: {e}"))),
        Err(e) => Ok((vec![format!("layer scan failed: {}", e.message().unwrap_or("a record tmi does not read"))], false)),
    }
}

fn join_changes(changes: &[String]) -> String {
    let mut detail = changes.iter().take(6).cloned().collect::<Vec<_>>().join("; ");
    if changes.len() > 6 {
        detail.push_str(&format!("; +{} more", changes.len() - 6));
    }
    detail
}

/// `status_items(with_layer_detail=...)`; the warnings imports printed go
/// to `warns`.
pub fn status_items(repo: &Repo, with_layer_detail: bool, warns: &mut Warnings, progress: Option<Progress>) -> Result<Vec<StatusItem>> {
    let root = repo.require_worktree()?.to_path_buf();
    report(progress, || "reading index".into());
    let index = repo.read_index()?;
    let index_entries: Map = match index.get("entries") {
        None => Map::new(),
        Some(Value::Object(m)) => m.clone(),
        Some(_) => return Err(RepoError::forward("index entries is not an object")),
    };
    report(progress, || "reading HEAD tree".into());
    let head_entries: Entries = repo.entries_for_commit(repo.current_commit()?.as_deref())?;
    let mut items = Vec::new();
    let mut cache = StatCache::load(repo)?;
    let ctx = if with_layer_detail { Some(ImportContext::for_repo(repo)?) } else { None };

    let mut index_sorted: Vec<(&String, &Value)> = index_entries.iter().collect();
    index_sorted.sort_by(|a, b| a.0.cmp(b.0));
    for (rel, entry) in index_sorted {
        if entry.as_object().is_none() {
            return Err(RepoError::forward("index entry is not an object"));
        }
        if is_deleted(entry) {
            items.push(item("staged", rel, "delete"));
            continue;
        }
        let selector = match entry.get("source_meta") {
            None => None,
            Some(Value::Object(m)) => m.get("staged_layer_selector"),
            Some(_) => return Err(RepoError::forward("source_meta is not an object")),
        };
        let detail = match selector {
            Some(v) if v.truthy() => format!("layer {}", v.py_str().ok_or_else(|| RepoError::forward("container selector"))?),
            _ => String::new(),
        };
        items.push(item("staged", rel, detail));
        let full = root.join(rel);
        let Ok(meta) = std::fs::metadata(&full) else {
            items.push(item("deleted", rel, ""));
            continue;
        };
        let (modified, source) = cache.changed(rel, &full, entry.get("source_digest"), &meta, with_layer_detail)?;
        if !modified {
            continue;
        }
        let mut unstaged = String::new();
        let mut semantically_same = false;
        if let Some(ctx) = &ctx {
            let (changes, same_canvas) = describe_or_forward(repo, ctx, rel, &full, entry, source, warns)?;
            if changes.is_empty() && same_canvas {
                semantically_same = true;
            } else {
                unstaged = join_changes(&changes);
            }
        }
        if !semantically_same {
            items.push(item("modified", rel, unstaged));
        }
    }

    let mut head_sorted: Vec<(&String, &Value)> = head_entries.iter().collect();
    head_sorted.sort_by(|a, b| a.0.cmp(b.0));
    let tracked_total = head_sorted.len();
    for (idx, (rel, entry)) in head_sorted.into_iter().enumerate() {
        if index_entries.contains_key(rel.as_str()) {
            continue;
        }
        let full = root.join(rel);
        let Ok(meta) = std::fs::metadata(&full) else {
            items.push(item("deleted", rel, ""));
            continue;
        };
        report(progress, || format!("hashing [{}/{tracked_total}] {rel}", idx + 1));
        let (modified, source) = cache.changed(rel, &full, entry.get("source_digest"), &meta, with_layer_detail)?;
        if modified {
            let mut detail = String::new();
            if let Some(ctx) = &ctx {
                report(progress, || format!("scanning layers [{}/{tracked_total}] {rel}", idx + 1));
                let (changes, _) = describe_or_forward(repo, ctx, rel, &full, entry, source, warns)?;
                detail = join_changes(&changes);
            }
            items.push(item("modified", rel, detail));
        }
    }

    report(progress, || "scanning working tree for untracked files".into());
    let patterns = ignore::repo_patterns(repo.worktree.as_deref())?;
    let tracked: HashSet<&str> = head_entries.keys().chain(index_entries.keys()).map(String::as_str).collect();
    for rel in untracked_candidates(&root, &patterns)? {
        if !tracked.contains(rel.as_str()) {
            items.push(item("untracked", &rel, ""));
        }
    }
    cache.flush(repo)?;
    Ok(items)
}
