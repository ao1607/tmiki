//! repo.py `_apply_layer_ignore` (stage 4b): `.tmiignore` lines
//! `<file glob>:<layer glob>` keep an ignored layer's committed pixels when
//! a file is staged: the new layer tree takes the HEAD layer's `blob_ref`
//! and `pixel_sha256` for every ignored layer that HEAD has (by stable id),
//! and the entry records `ignored_layer_sids`.

use crate::error::{RepoError, Result};
use crate::ignore::{self, IgnorePattern};
use crate::repo::{Entries, Repo};
use indexmap::IndexMap;
use tmi_import::Warnings;
use tmi_store::json::{Int, Map, Value};

fn fwd(why: &str) -> RepoError {
    RepoError::forward(why)
}

/// `layer_patterns_for(rel, patterns)`.
pub fn layer_patterns_for(rel: &str, patterns: &[IgnorePattern]) -> Result<Vec<IgnorePattern>> {
    let mut out = Vec::new();
    for p in patterns.iter().filter(|p| p.layer) {
        if ignore::match_path(rel, &p.file_glob)? {
            out.push(p.clone());
        }
    }
    Ok(out)
}

/// `is_layer_ignored(rel, name, patterns)`: the last matching pattern decides.
fn is_layer_ignored(rel: &str, name: &str, patterns: &[IgnorePattern]) -> Result<bool> {
    let mut decided = false;
    for p in patterns {
        let Some(glob) = &p.layer_glob else { continue };
        if !p.layer || !ignore::match_path(rel, &p.file_glob)? {
            continue;
        }
        if crate::py::fnmatch(name, glob).map_err(|_| fwd("layer glob Python's re rejects"))? {
            decided = !p.negate;
        }
    }
    Ok(decided)
}

/// `repr(text)` where every character is one Python prints as itself or
/// escapes in the plain ways; Forward otherwise (`str.isprintable` of other
/// characters is not reproduced).
pub fn py_repr(text: &str) -> Result<String> {
    let quote = if text.contains('\'') && !text.contains('"') { '"' } else { '\'' };
    let mut out = String::new();
    out.push(quote);
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => out.push_str(&format!("\\x{:02x}", c as u32)),
            c if c.is_ascii() || c.is_alphanumeric() => out.push(c),
            _ => return Err(fwd("repr of a character str.isprintable decides")),
        }
    }
    out.push(quote);
    Ok(out)
}

/// `flatten_layers(tree_ref)` with each layer's `path_name`.
fn flatten(repo: &Repo, tree_ref: &str, prefix: &str, out: &mut Vec<(Value, String)>, depth: usize) -> Result<()> {
    if depth > 200 {
        return Err(fwd("layer tree too deep"));
    }
    let tree = repo.store.read_json(tree_ref, Some("tree"))?;
    let layers = match tree.get("layers") {
        None => return Ok(()),
        Some(Value::Array(items)) => items.clone(),
        Some(_) => return Err(fwd("tree layers is not a list")),
    };
    for layer in layers {
        let name = match layer.get("name") {
            Some(Value::Str(s)) => s.clone(),
            _ => return Err(fwd("layer name is not a str")),
        };
        let path_name = if prefix.is_empty() { name } else { format!("{prefix}/{name}") };
        let sub = match layer.get("subtree_ref") {
            Some(Value::Str(s)) if !s.is_empty() => Some(s.clone()),
            Some(v) if v.truthy() => return Err(fwd("subtree_ref is not a str")),
            _ => None,
        };
        out.push((layer, path_name.clone()));
        if let Some(s) = sub {
            flatten(repo, &s, &path_name, out, depth + 1)?;
        }
    }
    Ok(())
}

fn sid_of(layer: &Value) -> Result<Option<String>> {
    match layer.get("stable_source_id") {
        Some(Value::Str(s)) if !s.is_empty() => Ok(Some(s.clone())),
        Some(v) if v.truthy() => Err(fwd("stable_source_id is not a str")),
        _ => Ok(None),
    }
}

/// `_rewrite_layer_tree(tree_ref, subs)`.
fn rewrite(repo: &Repo, tree_ref: &str, subs: &IndexMap<String, (Value, Value)>, depth: usize) -> Result<String> {
    if depth > 200 {
        return Err(fwd("layer tree too deep"));
    }
    let tree = repo.store.read_json(tree_ref, Some("tree"))?;
    if tree.get("kind").and_then(Value::as_str) != Some("layer-tree") {
        return Ok(tree_ref.to_string());
    }
    let layers = match tree.get("layers") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items.clone(),
        Some(v) if !v.truthy() => Vec::new(),
        Some(_) => return Err(fwd("tree layers is not a list")),
    };
    let mut changed = false;
    let mut rewritten = Vec::new();
    for layer in layers {
        let Value::Object(mut m) = layer else { return Err(fwd("tree layer is not an object")) };
        let sid = match m.get("stable_source_id") {
            Some(Value::Str(s)) => Some(s.clone()),
            None | Some(Value::Null) => None,
            Some(_) => return Err(fwd("stable_source_id is not a str")),
        };
        if let Some((blob, pixels)) = sid.as_ref().and_then(|s| subs.get(s)) {
            let null = Value::Null;
            let differs = !crate::pyeq::py_eq(m.get("blob_ref").unwrap_or(&null), blob)
                || !crate::pyeq::py_eq(m.get("pixel_sha256").unwrap_or(&null), pixels);
            if differs {
                m.insert("blob_ref".into(), blob.clone());
                m.insert("pixel_sha256".into(), pixels.clone());
                changed = true;
            }
        }
        match m.get("subtree_ref").cloned() {
            Some(Value::Str(s)) if !s.is_empty() => {
                let new_sub = rewrite(repo, &s, subs, depth + 1)?;
                if new_sub != s {
                    m.insert("subtree_ref".into(), Value::Str(new_sub));
                    changed = true;
                }
            }
            Some(v) if v.truthy() => return Err(fwd("subtree_ref is not a str")),
            _ => {}
        }
        rewritten.push(Value::Object(m));
    }
    if !changed {
        return Ok(tree_ref.to_string());
    }
    let mut out = Map::new();
    out.insert("version".into(), tree.get("version").cloned().unwrap_or(Value::Int(Int::Small(1))));
    out.insert("kind".into(), Value::Str("layer-tree".into()));
    out.insert("layers".into(), Value::Array(rewritten));
    Ok(repo.store.write_json("tree", &Value::Object(out))?)
}

/// `_apply_layer_ignore(rel, entry)`. `head_entries` is HEAD's tree (the
/// entries of `current_commit()`); notes Python prints go to `warns`
/// (stdout lines).
pub fn apply(
    repo: &Repo,
    head_entries: &Entries,
    patterns: &[IgnorePattern],
    rel: &str,
    entry: &mut Value,
    warns: &mut Warnings,
    progress: Option<tmi_import::Progress>,
) -> Result<()> {
    let pats = layer_patterns_for(rel, patterns)?;
    if pats.is_empty() {
        return Ok(());
    }
    let mut head_by_sid: IndexMap<String, Value> = IndexMap::new();
    if let Some(head) = head_entries.get(rel).filter(|e| e.truthy()) {
        let tree_ref = match head.get("tree_ref") {
            Some(Value::Str(s)) => s.clone(),
            _ => return Err(fwd("HEAD entry without a tree_ref str")),
        };
        let mut flat = Vec::new();
        flatten(repo, &tree_ref, "", &mut flat, 0)?;
        for (layer, _) in flat {
            if let Some(sid) = sid_of(&layer)? {
                head_by_sid.insert(sid, layer);
            }
        }
    }
    if head_by_sid.is_empty() {
        for p in &pats {
            let glob = py_repr(p.layer_glob.as_deref().unwrap_or(""))?;
            warns.stdout(format!(
                "tmiki: warning: ignore pattern {rel}:{glob} skipped (no matching prior commit to inherit from)"
            ));
        }
        return Ok(());
    }
    let tree_ref = match entry.get("tree_ref") {
        Some(Value::Str(s)) => s.clone(),
        _ => return Err(fwd("entry without a tree_ref str")),
    };
    let mut flat = Vec::new();
    flatten(repo, &tree_ref, "", &mut flat, 0)?;
    let mut subs: IndexMap<String, (Value, Value)> = IndexMap::new();
    for (layer, path_name) in flat {
        let name = match layer.get("name") {
            Some(Value::Str(s)) if !s.is_empty() => s.clone(),
            _ => path_name,
        };
        let Some(sid) = sid_of(&layer)? else { continue };
        if !is_layer_ignored(rel, &name, &pats)? {
            continue;
        }
        let Some(prior) = head_by_sid.get(&sid) else {
            let short: String = sid.chars().take(12).collect();
            warns.stdout(format!("tmiki: warning: ignore layer {rel}:{name} skipped (no prior layer with sid {short})"));
            continue;
        };
        let get = |k: &str| prior.get(k).cloned().unwrap_or(Value::Null);
        subs.insert(sid, (get("blob_ref"), get("pixel_sha256")));
    }
    if subs.is_empty() {
        return Ok(());
    }
    tmi_import::report(progress, || format!("applying .tmiignore layer substitutions ({})", subs.len()));
    let new_tree = rewrite(repo, &tree_ref, &subs, 0)?;
    let Value::Object(map) = entry else { return Err(fwd("entry is not an object")) };
    map.insert("tree_ref".into(), Value::Str(new_tree));
    let mut sids: Vec<String> = subs.keys().cloned().collect();
    sids.sort();
    let meta = map.entry("source_meta".to_string()).or_insert_with(|| Value::Object(Map::new()));
    let Value::Object(meta) = meta else { return Err(fwd("source_meta is not a dict")) };
    meta.insert("ignored_layer_sids".into(), Value::Array(sids.into_iter().map(Value::Str).collect()));
    Ok(())
}
