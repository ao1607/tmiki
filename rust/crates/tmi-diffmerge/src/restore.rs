//! repo.py `restore` of one layer (`file:layer`: written back into the
//! working file with `--in-place`, else exported as a PNG) and
//! `resolve_conflict` of one layer (`restore --ours/--theirs file:layer`):
//! the layer pixels go through writeback.rs.

use crate::pix::{self, fwd};
use crate::writeback::{self, WriteCtx};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tmi_repo::fsio;
use tmi_repo::paths::split_layer_spec;
use tmi_repo::repo::{prefix_chars, read_text};
use tmi_repo::write::{is_deleted, safe_filename, source_blob_ref, str_of};
use tmi_repo::{Repo, RepoError, Result};
use tmi_store::json::{self, DumpOptions, Map, Value};

fn is_py_space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

/// `str.casefold()` where it is the identity or ASCII lowercasing (uncased
/// scripts such as kana and kanji); Forward for cased non-ASCII letters.
pub fn casefold(s: &str) -> Result<String> {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii() {
            out.push(c.to_ascii_lowercase());
        } else {
            let mut lower = c.to_lowercase();
            let mut upper = c.to_uppercase();
            let uncased = lower.next() == Some(c) && lower.next().is_none() && upper.next() == Some(c) && upper.next().is_none();
            if !uncased {
                return Err(fwd("casefold() of a cased non-ASCII letter"));
            }
            out.push(c);
        }
    }
    Ok(out)
}

/// repo.py `normalize_selector`: `re.sub(r"\s+", " ", value.strip()).casefold()`.
pub fn normalize_selector(value: &str) -> Result<String> {
    let stripped = value.trim_matches(is_py_space);
    let mut collapsed = String::with_capacity(stripped.len());
    let mut space = false;
    for c in stripped.chars() {
        if is_py_space(c) {
            if !space {
                collapsed.push(' ');
            }
            space = true;
        } else {
            collapsed.push(c);
            space = false;
        }
    }
    casefold(&collapsed)
}

/// `str(value)` of a JSON value (`str(layer.get(key, ""))`).
fn text(v: Option<&Value>) -> Result<String> {
    match v {
        None => Ok(String::new()),
        Some(v) => v.py_str().ok_or_else(|| fwd("str() of a container")),
    }
}

/// repo.py `flatten_layers(tree_ref)`: every stored layer with `path_name`.
pub fn flatten_layers(repo: &Repo, tree_ref: &str, prefix: &str, out: &mut Vec<Value>, depth: usize) -> Result<()> {
    if depth > 200 {
        return Err(fwd("layer tree too deep"));
    }
    let tree = repo.store.read_json(tree_ref, Some("tree"))?;
    let Some(layers) = tree.get("layers") else { return Ok(()) };
    for layer in layers.as_array().ok_or_else(|| fwd("tree layers not a list"))? {
        let Some(map) = layer.as_object() else { return Err(fwd("tree layer is not an object")) };
        let name = match map.get("name") {
            Some(Value::Str(n)) => n.clone(),
            _ => return Err(fwd("layer name is not a str")),
        };
        let path = if prefix.is_empty() { name } else { format!("{prefix}/{name}") };
        let mut item = map.clone();
        item.insert("path_name".into(), Value::Str(path.clone()));
        out.push(Value::Object(item));
        match map.get("subtree_ref") {
            Some(Value::Str(s)) if !s.is_empty() => flatten_layers(repo, s, &path, out, depth + 1)?,
            Some(v) if v.truthy() => return Err(fwd("subtree_ref is not a str")),
            _ => {}
        }
    }
    Ok(())
}

/// importers.py `_ambiguous_message(selector, candidates)`.
fn ambiguous_message(selector: &str, candidates: &[(String, String)]) -> Result<String> {
    let shown: Vec<String> = candidates.iter().take(8).map(|(p, id)| format!("{p} [{id}]")).collect();
    let more = if candidates.len() > 8 { format!("; +{} more", candidates.len() - 8) } else { String::new() };
    Ok(format!(
        "layer selector {} is ambiguous: {} layers share that name ({}{more}). Rename the layers so each name is unique, or select one by its folder path or id.",
        crate::pix::py_repr(selector)?,
        candidates.len(),
        shown.join("; ")
    ))
}

/// repo.py `find_committed_layer(entry, selector)`.
pub fn find_committed_layer(repo: &Repo, entry: &Value, selector: &str) -> Result<Option<Value>> {
    let normalized = normalize_selector(selector)?;
    let tree_ref = str_of(entry, "tree_ref")?.ok_or_else(|| fwd("entry without a tree_ref"))?;
    let mut layers = Vec::new();
    flatten_layers(repo, &tree_ref, "", &mut layers, 0)?;
    let (mut ids, mut paths, mut names) = (Vec::new(), Vec::new(), Vec::new());
    for layer in layers {
        if normalize_selector(&text(layer.get("layer_id"))?)? == normalized
            || normalize_selector(&text(layer.get("stable_source_id"))?)? == normalized
        {
            ids.push(layer);
        } else if normalize_selector(&text(layer.get("path_name"))?)? == normalized {
            paths.push(layer);
        } else if normalize_selector(&text(layer.get("name"))?)? == normalized {
            names.push(layer);
        }
    }
    if let Some(first) = ids.into_iter().next() {
        return Ok(Some(first));
    }
    for matches in [paths, names] {
        if matches.len() > 1 {
            let mut cands = Vec::new();
            for m in &matches {
                let shown = match m.get("path_name").filter(|v| v.truthy()) {
                    Some(v) => text(Some(v))?,
                    None => text(m.get("name"))?,
                };
                cands.push((shown, text(m.get("layer_id"))?));
            }
            return Err(RepoError::runtime(ambiguous_message(selector, &cands)?));
        }
        if let Some(first) = matches.into_iter().next() {
            return Ok(Some(first));
        }
    }
    Ok(None)
}

/// A layer of the plugin-format file `rel` written back through plugin
/// `id` (PLUGINS.md P7). A plugin without a writer says so, with `instead`.
#[allow(clippy::too_many_arguments)]
fn plugin_write(
    repo: &Repo,
    ctx: &mut WriteCtx,
    rel: &str,
    name: &str,
    current: &[u8],
    id: &str,
    layers: &[(String, Vec<u8>)],
    instead: &str,
) -> Result<Vec<u8>> {
    let formats = tmi_repo::formats::Formats::for_repo(repo);
    let loaded = formats.plugin(repo, id).map_err(|u| RepoError::runtime(tmi_repo::formats::not_available_message(rel, id, &u)))?;
    if !loaded.can_write() {
        return Err(RepoError::runtime(format!("{rel}: {} cannot write layers back; {instead}", loaded.label())));
    }
    writeback::plugin_replace(repo, &formats, ctx.limits, ctx.warns, rel, name, current, id, layers)
}

/// `Path(rel).stem`.
fn rel_stem(rel: &str) -> String {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    match name.rfind('.') {
        Some(i) if i > 0 && i + 1 < name.len() => name[..i].to_string(),
        _ => name.to_string(),
    }
}

/// What a layer restore wrote: the path to print.
pub enum Restored {
    /// The working file (`_rewrote`).
    InPlace(PathBuf),
    /// An exported PNG.
    Exported(PathBuf),
}

/// repo.py `restore(spec, source=, in_place=, output=)` for `file:layer`
/// specs. `output` is the user's text.
pub fn restore_layer(
    repo: &Repo,
    ctx: &mut WriteCtx,
    spec: &str,
    source: Option<&str>,
    in_place: bool,
    output: Option<&str>,
    warnings: &mut Vec<String>,
) -> Result<Restored> {
    if source == Some("WORKTREE") {
        return Err(fwd("restore --source WORKTREE exports from the working file"));
    }
    let commit = repo.resolve_rev(source, warnings)?.ok_or_else(|| RepoError::runtime("no commit to restore from"))?;
    let (file_part, layer_selector) = split_layer_spec(repo, spec)?;
    let Some(layer_selector) = layer_selector else { return Err(fwd("a whole-file restore")) };
    let mut entries = repo.entries_for_commit(Some(&commit))?;
    let mut label = prefix_chars(&commit, 12);
    if source.is_none() {
        for (path, entry) in repo.index_entries()? {
            if is_deleted(&entry) {
                entries.shift_remove(&path);
            } else {
                entries.insert(path, entry);
            }
        }
        label = "index".into();
    }
    let rel = repo.pathspec_relpath(&file_part, &|p| entries.contains_key(p))?;
    let Some(entry) = entries.get(&rel) else {
        return Err(RepoError::runtime(format!("path not found in {label}: {rel}")));
    };
    let layer = find_committed_layer(repo, entry, &layer_selector)?
        .ok_or_else(|| RepoError::runtime(format!("layer not found in {rel}: {layer_selector}")))?;
    let blob = str_of(&layer, "blob_ref")?.filter(|b| !b.is_empty());
    let Some(blob) = blob else {
        if layer.get("subtree_ref").is_some_and(Value::truthy) && !in_place {
            return export_folder(repo, ctx, &rel, entry, &layer, output).map(Restored::Exported);
        }
        return Err(RepoError::runtime(format!("layer has no restorable pixel blob: {layer_selector}")));
    };
    let png_bytes = repo.store.read_blob(&blob)?;
    let native_selector = match layer.get("stable_source_id").filter(|v| v.truthy()) {
        Some(v) => text(Some(v))?,
        None => layer_selector.clone(),
    };
    if in_place {
        let out = repo.abspath(&rel)?;
        let committed = repo.store.read_blob(&source_blob_ref(entry)?)?;
        let existing = if out.exists() { Some(fsio::read_bytes(&out)?) } else { None };
        let current = existing.clone().unwrap_or_else(|| committed.clone());
        let invalidate = current != committed;
        let format = match entry.get("source_format") {
            None => String::new(),
            Some(v) => text(Some(v))?,
        };
        let name = out.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if let Some(id) = format.to_ascii_lowercase().strip_prefix("plugin:") {
            // PLUGINS.md P7: through the plugin's writer, with consent and the
            // check after writing; the working file is replaced only after
            // both, and backed up first when it holds unsaved work
            let layers = [(native_selector.clone(), png_bytes)];
            let written = plugin_write(repo, ctx, &rel, &name, &current, id, &layers, "export the layer with --output instead")?;
            fsio::mkdir_parent(&out)?;
            repo.backup_worktree_file(&rel, "restore --in-place", &[], true)?;
            let tmp = PathBuf::from(format!("{}.tmp", out.display()));
            fsio::write_bytes(&tmp, &written)?;
            fsio::replace(&tmp, &out)?;
            repo.rewrote(&out);
            return Ok(Restored::InPlace(out));
        }
        let result = writeback::replace_layer_pixels(ctx, &name, Some(&current), &format, &native_selector, &png_bytes, invalidate);
        if let Err(RepoError::Forward(w)) = result {
            return Err(RepoError::Forward(w));
        }
        fsio::mkdir_parent(&out)?;
        if existing.is_none() {
            fsio::write_bytes(&out, &committed)?;
        }
        let native_bytes = result?;
        let tmp = PathBuf::from(format!("{}.tmp", out.display()));
        fsio::write_bytes(&tmp, &native_bytes)?;
        fsio::replace(&tmp, &out)?;
        repo.rewrote(&out);
        return Ok(Restored::InPlace(out));
    }
    let name = match layer.get("name") {
        Some(Value::Str(n)) => n.clone(),
        _ => return Err(fwd("layer name is not a str")),
    };
    let out = match output {
        Some(o) => PathBuf::from(o),
        None => repo.abspath(&rel)?.with_file_name(format!("{}__{}.png", rel_stem(&rel), safe_filename(&name))),
    };
    let png = pix::layer_blob_to_png(&png_bytes, &ctx.limits)?;
    if let Some(parent) = out.parent().filter(|p| !p.as_os_str().is_empty()) {
        fsio::mkdir_p(parent)?;
    }
    fsio::write_bytes(&out, &png)?;
    Ok(Restored::Exported(out))
}

/// native.py `render_folder(path, source_format, selector)`: a folder's
/// contents composited on a transparent canvas.
fn render_folder(ctx: &mut WriteCtx, name: &str, raw: &[u8], source_format: &str, selector: &str) -> Result<tmi_import::Rgba> {
    let suffix = tmi_import::path_suffix(name).to_lowercase();
    let fmt = source_format.to_lowercase();
    let not_folder = || RepoError::Value(format!("not a folder: {selector}"));
    if fmt == "clip" || suffix == ".clip" {
        let main_id = crate::writeback::clip_node_id(raw, selector)?.ok_or_else(not_folder)?;
        let (node_id, is_folder) = main_id;
        if !is_folder {
            return Err(not_folder());
        }
        let doc = tmi_clip::ClipDoc::open(raw).map_err(|e| RepoError::runtime(format!("the CLIP renderer cannot draw this file (unreadable: {e})")))?;
        let i = doc.nodes.iter().position(|n| n.main_id == node_id).ok_or_else(|| fwd("folder node not in the renderer's tree"))?;
        if !doc.nodes[i].folder {
            return Err(RepoError::runtime("the CLIP renderer cannot draw this file (not a folder)"));
        }
        let mut out = vec![0u8; doc.width * doc.height * 4];
        let report = tmi_render::clip::band::render(&doc, raw, [0, 0, 0, 0], &tmi_render::clip::band::Target::Folder(i), false, &mut out)
            .map_err(|e| RepoError::runtime(format!("the CLIP renderer cannot draw this file ({e})")))?;
        tmi_import::clip::replay(&report, ctx.warns);
        return Ok(tmi_import::Rgba { width: doc.width, height: doc.height, data: out });
    }
    if fmt == "mdp2" || suffix == ".mdp" {
        let doc = tmi_clip::mdp2::parse(raw).map_err(RepoError::Value)?;
        let Some(fi) = crate::writeback::find_mdp2_layer(&doc, selector)? else { return Err(not_folder()) };
        let folder = &doc.layers[fi];
        if folder.children.is_empty() {
            return Err(not_folder());
        }
        let by_id: HashMap<&tmi_clip::mdp2::IntKey, usize> = doc.layers.iter().enumerate().map(|(i, l)| (&l.id_key, i)).collect();
        let inside = |layer: &tmi_clip::mdp2::Layer| -> bool {
            let mut seen = std::collections::HashSet::new();
            let mut cur = by_id.get(&layer.parent_key).copied();
            while let Some(c) = cur {
                let l = &doc.layers[c];
                if seen.contains(&l.id_key) {
                    break;
                }
                if l.id_key == folder.id_key {
                    return true;
                }
                seen.insert(&l.id_key);
                cur = by_id.get(&l.parent_key).copied();
            }
            false
        };
        let subset: Vec<usize> = doc.layers.iter().enumerate().filter(|(_, l)| inside(l)).map(|(i, _)| i).collect();
        let (w, h) = tmi_render::mdp2::canvas_size(&doc).map_err(RepoError::Value)?;
        let data = tmi_render::mdp2::render(&doc, raw, [0, 0, 0, 0], Some(&subset)).map_err(RepoError::Value)?;
        return Ok(tmi_import::Rgba { width: w, height: h, data });
    }
    Err(RepoError::Value(format!("folders are not supported for {source_format}")))
}

/// importers.py `find_layer(layers, selector)`.
pub(crate) fn find_imported_layer<'a>(layers: &'a [tmi_import::ImportedLayer], selector: &str) -> Result<Option<&'a tmi_import::ImportedLayer>> {
    fn walk<'a>(layers: &'a [tmi_import::ImportedLayer], prefix: &str, out: &mut Vec<(String, &'a tmi_import::ImportedLayer)>) {
        for l in layers {
            let path = if prefix.is_empty() { l.name.clone() } else { format!("{prefix}/{}", l.name) };
            out.push((path.clone(), l));
            walk(&l.children, &path, out);
        }
    }
    let normalized = normalize_selector(selector)?;
    let mut all = Vec::new();
    walk(layers, "", &mut all);
    let (mut ids, mut paths, mut names) = (Vec::new(), Vec::new(), Vec::new());
    for (path, layer) in all {
        if normalize_selector(&layer.layer_id)? == normalized || normalize_selector(&layer.stable_source_id)? == normalized {
            ids.push(layer);
        } else if normalize_selector(&path)? == normalized {
            paths.push(layer);
        } else if normalize_selector(&layer.name)? == normalized {
            names.push((path, layer));
        }
    }
    if let Some(first) = ids.first() {
        return Ok(Some(*first));
    }
    if !paths.is_empty() {
        if paths.len() > 1 {
            let cands: Vec<(String, String)> = paths.iter().map(|l| (selector.to_string(), l.layer_id.clone())).collect();
            return Err(RepoError::runtime(ambiguous_message(selector, &cands)?));
        }
        return Ok(Some(paths[0]));
    }
    if names.len() > 1 {
        let cands: Vec<(String, String)> = names.iter().map(|(p, l)| (p.clone(), l.layer_id.clone())).collect();
        return Err(RepoError::runtime(ambiguous_message(selector, &cands)?));
    }
    Ok(names.first().map(|(_, l)| *l))
}

/// repo.py `_export_from_worktree(spec, in_place=, output=)`: a layer (or
/// a folder's composite) of the working file as it is now.
pub fn export_from_worktree(repo: &Repo, ctx: &mut WriteCtx, spec: &str, in_place: bool, output: Option<&str>) -> Result<PathBuf> {
    let Some(output) = output.filter(|_| !in_place) else {
        return Err(RepoError::runtime(
            "--source WORKTREE only exports from the working file; use it with --output <file>",
        ));
    };
    let out = PathBuf::from(output);
    let (file_part, layer_selector) = split_layer_spec(repo, spec)?;
    let full = repo.pathspec_full_path(&file_part)?;
    if !full.is_file() {
        return Err(RepoError::runtime(format!("no such file in the working tree: {file_part}")));
    }
    if layer_selector.is_none() && tmi_repo::worktree::whole_file_output_is_png(&file_part, output)? {
        // the working file's image, not its bytes (issue #30)
        let bytes = fsio::read_bytes(&full)?;
        let import = tmi_repo::stage::ImportContext::for_repo(repo)?;
        let all = tmi_import::LayerBlobs::All;
        let opts = tmi_import::Options {
            render_composite: true,
            cache: Some(&import.render_cache),
            reuse_pixels: None,
            reuse_decoded: None,
            layer_blobs: &all,
            composite_base: None,
            limits: ctx.limits,
            verify_partial: import.verify_partial,
            progress: None,
        };
        let name = full.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let digest = tmi_store::oid::sha256_hex(&bytes);
        let rel = repo.relpath(&full)?;
        let imported = import.formats.import(repo, &rel, &name, &bytes, &digest, &opts, ctx.warns)?;
        if imported.composite_png.is_empty() {
            return Err(RepoError::runtime(tmi_repo::worktree::no_image_message(&file_part)?));
        }
        if let Some(parent) = out.parent().filter(|p| !p.as_os_str().is_empty()) {
            fsio::mkdir_p(parent)?;
        }
        fsio::write_bytes(&out, &imported.composite_png)?;
        return Ok(out);
    }
    if let Some(parent) = out.parent().filter(|p| !p.as_os_str().is_empty()) {
        fsio::mkdir_p(parent)?;
    }
    let bytes = fsio::read_bytes(&full)?;
    let Some(layer_selector) = layer_selector else {
        fsio::write_bytes(&out, &bytes)?;
        return Ok(out);
    };
    let rel = repo.relpath(&full)?;
    let head = repo.entries_for_commit(repo.current_commit()?.as_deref())?;
    let mut reuse = tmi_repo::stage::layer_blob_reuse(repo, head.get(&rel))?;
    reuse.extend(tmi_repo::stage::layer_blob_reuse(repo, repo.index_entries()?.get(&rel))?);
    let pixels = tmi_repo::stage::reuse_pixels(&reuse);
    let decoded = tmi_repo::stage::reuse_decoded(&reuse);
    let all = tmi_import::LayerBlobs::All;
    let opts = tmi_import::Options {
        render_composite: false,
        cache: None,
        reuse_pixels: (!pixels.is_empty()).then_some(&pixels),
        reuse_decoded: decoded.as_ref(),
        layer_blobs: &all,
        composite_base: None,
        limits: ctx.limits,
        verify_partial: false,
        progress: None,
    };
    let name = full.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let digest = tmi_store::oid::sha256_hex(&bytes);
    let imported = tmi_repo::formats::Formats::for_repo(repo).import(repo, &rel, &name, &bytes, &digest, &opts, ctx.warns)?;
    let Some(layer) = find_imported_layer(&imported.layers, &layer_selector)? else {
        return Err(RepoError::runtime(format!("layer not found in {file_part}: {layer_selector}")));
    };
    if !layer.children.is_empty() || layer.kind == "folder" {
        let selector = if layer.stable_source_id.is_empty() { layer.name.clone() } else { layer.stable_source_id.clone() };
        let image = render_folder(ctx, &name, &bytes, &imported.source_format, &selector)?;
        fsio::write_bytes(&out, &pix::png(&image)?)?;
        return Ok(out);
    }
    let mut blob = layer.blob_bytes.clone();
    if blob.is_none() {
        if let (Some(r), Some(p)) = (reuse.get(&layer.stable_source_id), &layer.pixel_sha256) {
            if p.truthy() && tmi_repo::pyeq::py_eq(&r.pixel_sha256, p) {
                let oid = r.blob_ref.as_str().ok_or_else(|| fwd("blob_ref is not a str"))?;
                blob = Some(repo.store.read_blob(oid)?);
            }
        }
    }
    let Some(blob) = blob else {
        return Err(RepoError::runtime(format!("layer has no restorable pixel blob: {layer_selector}")));
    };
    fsio::write_bytes(&out, &pix::layer_blob_to_png(&blob, &ctx.limits)?)?;
    Ok(out)
}

/// repo.py `_export_folder(rel, entry, layer, output)`.
fn export_folder(repo: &Repo, ctx: &mut WriteCtx, rel: &str, entry: &Value, layer: &Value, output: Option<&str>) -> Result<PathBuf> {
    let tmp_dir = repo.repo_dir.join("tmp").join("export");
    fsio::mkdir_p(&tmp_dir)?;
    let suffix = tmi_repo::worktree::py_suffix(rel);
    let name = format!("{}{suffix}", safe_filename(&rel_stem(rel)));
    let raw = repo.store.read_blob(&source_blob_ref(entry)?)?;
    let format = match entry.get("source_format") {
        None => String::new(),
        Some(v) => text(Some(v))?,
    };
    let layer_name = match layer.get("name") {
        Some(Value::Str(n)) => n.clone(),
        _ => return Err(fwd("layer name is not a str")),
    };
    let selector = match layer.get("stable_source_id").filter(|v| v.truthy()) {
        Some(v) => text(Some(v))?,
        None => layer_name.clone(),
    };
    let image = render_folder(ctx, &name, &raw, &format, &selector)?;
    let out = match output {
        Some(o) => PathBuf::from(o),
        None => repo.abspath(rel)?.with_file_name(format!("{}__{}.png", rel_stem(rel), safe_filename(&layer_name))),
    };
    let png = pix::png(&image)?;
    if let Some(parent) = out.parent().filter(|p| !p.as_os_str().is_empty()) {
        fsio::mkdir_p(parent)?;
    }
    fsio::write_bytes(&out, &png)?;
    Ok(out)
}

/// `_record_conflict_choice(rel, stable_source_id, side)`.
pub fn record_conflict_choice(repo: &Repo, rel: &str, sid: Option<&str>, side: &str) -> Result<()> {
    let meta_path = repo.conflict_record_dir(rel).join("conflict.json");
    let Ok(bytes) = std::fs::read(&meta_path) else { return Ok(()) };
    let Ok(text) = String::from_utf8(bytes) else { return Ok(()) };
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let Ok(Value::Object(mut meta)) = json::loads(&text) else { return Ok(()) };
    let mut taken = match sid {
        None => Map::new(),
        Some(_) => match meta.get("taken") {
            Some(Value::Object(m)) => m.clone(),
            Some(v) if v.truthy() => return Err(fwd("taken is not a dict")),
            _ => Map::new(),
        },
    };
    taken.insert(sid.unwrap_or("*").to_string(), Value::Str(side.to_string()));
    meta.insert("taken".into(), Value::Object(taken));
    let full = repo.abspath(rel)?;
    let digest = if full.is_file() { Value::Str(fsio::sha256_file(&full)?) } else { Value::Null };
    meta.insert("taken_digest".into(), digest);
    fsio::write_text(&meta_path, &json::dumps(&Value::Object(meta), &DumpOptions::INDENTED))
}

/// repo.py `resolve_conflict(spec, side)` for `file:layer`: that layer's
/// pixels from `side` written into the working file.
pub fn resolve_conflict_layer(repo: &Repo, ctx: &mut WriteCtx, spec: &str, side: &str) -> Result<PathBuf> {
    let (file_part, layer_selector) = split_layer_spec(repo, spec)?;
    let Some(layer_selector) = layer_selector else { return Err(fwd("a whole-file side")) };
    if repo.pending_merge_head()?.is_none() {
        return Err(RepoError::runtime(format!(
            "no merge in progress; --{side} only applies to a file that conflicted in a merge"
        )));
    }
    let known: Vec<String> = repo.pending_conflict_paths()?.into_iter().chain(repo.resolved_conflict_paths()?).collect();
    let rel = repo.pathspec_relpath(&file_part, &|p| known.iter().any(|k| k == p))?;
    let Some(meta) = repo.read_conflict_meta(&rel)?.filter(Value::truthy) else {
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
    let wanted = normalize_selector(&layer_selector)?;
    let rows = match meta.get("layers") {
        None => Vec::new(),
        Some(Value::Array(a)) => a.clone(),
        Some(_) => return Err(fwd("conflict layers is not a list")),
    };
    let mut matched = Vec::new();
    for row in rows {
        let mut keys = Vec::new();
        for k in ["name", "path_name", "stable_source_id"] {
            let v = row.get(k).filter(|v| v.truthy());
            keys.push(normalize_selector(&text(v)?)?);
        }
        if keys.contains(&wanted) {
            matched.push(row);
        }
    }
    if matched.is_empty() {
        return Err(RepoError::runtime(format!("layer not found in {rel}: {layer_selector}")));
    }
    if matched.len() > 1 {
        let mut choices = Vec::new();
        for row in &matched {
            let shown = match row.get("path_name").filter(|v| v.truthy()) {
                Some(v) => text(Some(v))?,
                None => text(row.get("name"))?,
            };
            choices.push(format!("{shown} [{}]", text(row.get("stable_source_id"))?));
        }
        return Err(RepoError::runtime(format!(
            "layer selector {} is ambiguous in {rel}: {}",
            crate::pix::py_repr(&layer_selector)?,
            choices.join(", ")
        )));
    }
    let row = &matched[0];
    let Some(blob_ref) = str_of(row, &format!("{side}_blob_ref"))?.filter(|b| !b.is_empty()) else {
        return Err(RepoError::runtime(format!(
            "{} has no pixels for layer {}",
            side.to_uppercase(),
            crate::pix::py_repr(&layer_selector)?
        )));
    };
    let sid = str_of(row, "stable_source_id")?.ok_or_else(|| fwd("conflict row without a stable_source_id"))?;
    let target = repo.abspath(&rel)?;
    let mut created = None;
    let current = if target.exists() {
        fsio::read_bytes(&target)?
    } else {
        let suffix = match meta.get("suffix").filter(|v| v.truthy()) {
            Some(v) => text(Some(v))?,
            None => tmi_repo::worktree::py_suffix(&rel),
        };
        let ours = fsio::read_bytes(&repo.conflict_record_dir(&rel).join(format!("OURS{suffix}")))?;
        created = Some(ours.clone());
        ours
    };
    let format = match meta.get("source_format").filter(|v| v.truthy()) {
        Some(v) => text(Some(v))?,
        None => String::new(),
    };
    let png = repo.store.read_blob(&blob_ref)?;
    let name = target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    if let Some(id) = format.to_ascii_lowercase().strip_prefix("plugin:") {
        // PLUGINS.md P7 (as `restore --in-place` above)
        let instead = format!("take the whole file with 'tmiki restore --{side} {rel}'");
        let written = plugin_write(repo, ctx, &rel, &name, &current, id, &[(sid.clone(), png)], &instead)?;
        if created.is_none() {
            // what the merge left there is known (as the whole-file side's)
            let left = meta.get("worktree_digest").and_then(Value::as_str).map(str::to_string);
            repo.backup_worktree_file(&rel, &format!("restore --{side}"), &[left], true)?;
        }
        fsio::mkdir_parent(&target)?;
        fsio::write_bytes(&target, &written)?;
        record_conflict_choice(repo, &rel, Some(&sid), side)?;
        repo.rewrote(&target);
        return Ok(target);
    }
    let result = writeback::replace_layer_pixels(ctx, &name, Some(&current), &format, &sid, &png, false);
    if let Err(RepoError::Forward(w)) = result {
        return Err(RepoError::Forward(w));
    }
    if let Some(ours) = created {
        fsio::write_bytes(&target, &ours)?;
    }
    let merged = result?;
    fsio::write_bytes(&target, &merged)?;
    record_conflict_choice(repo, &rel, Some(&sid), side)?;
    repo.rewrote(&target);
    Ok(target)
}

/// The write-back context of a command.
pub fn ctx_limits(repo: &Repo) -> Result<tmi_import::Limits> {
    Ok(tmi_repo::stage::ImportContext::for_repo(repo)?.limits)
}

pub fn read_text_opt(path: &Path) -> Result<Option<String>> {
    read_text(path)
}
