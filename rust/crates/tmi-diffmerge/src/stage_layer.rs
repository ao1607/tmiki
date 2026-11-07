//! `tmiki add <file>:<layer>` of a plugin-format file (PLUGINS.md P7):
//! repo.py `materialize_layer_stage` for a format only the executable
//! reads. The working file's layer is written into HEAD's version of the
//! file through the plugin's writer (with consent and the check after
//! writing), and that file is staged; the working file is not touched.
//! The built-in formats' layer stages stay Python's.

use crate::pix;
use std::collections::HashMap;
use tmi_import::{LayerBlobs, Options};
use tmi_repo::stage::{self, committable, Stager};
use tmi_repo::{RepoError, Result};
use tmi_store::json::{self, Value};

/// repo.py `PARTIAL_STAGE_LAYER_FIELDS`.
const LAYER_FIELDS: [&str; 8] = ["name", "kind", "visible", "opacity", "blend_mode", "clip_to_below", "offset_x", "offset_y"];

fn canon(v: &Value) -> String {
    json::dumps(v, &json::DumpOptions::CANONICAL)
}

/// The record minus what a layer's pixels decide (`pixel_source`).
fn meta_without_pixel_source(v: Option<&Value>) -> String {
    match v {
        Some(Value::Object(m)) => canon(&Value::Object(m.iter().filter(|(k, _)| k.as_str() != "pixel_source").map(|(k, v)| (k.clone(), v.clone())).collect())),
        Some(other) => canon(other),
        None => "null".into(),
    }
}

fn mark(entry: &mut Value, selector: &str, layer: &tmi_import::ImportedLayer) {
    if let Value::Object(e) = entry {
        let meta = e.entry("source_meta".to_string()).or_insert_with(|| Value::Object(json::Map::new()));
        if let Value::Object(m) = meta {
            m.insert("staged_layer_selector".into(), Value::Str(selector.into()));
            m.insert("staged_layer_name".into(), Value::Str(layer.name.clone()));
            m.insert("staged_layer_stable_source_id".into(), Value::Str(layer.stable_source_id.clone()));
        }
    }
}

/// Stages layer `selector` of the plugin-format working file `rel` (plugin
/// `id`): (rel, changed).
pub fn stage_plugin_layer(stager: &mut Stager, rel: &str, full: &std::path::Path, selector: &str, id: &str) -> Result<(String, bool)> {
    let repo = stager.repo;
    let (bytes, digest) = stage::read_file_digest(full)?;
    let name = stage::file_name(full);
    tmi_import::report(stager.progress, || format!("parsing {rel}"));
    if let Some((id, u)) = stager.ctx.formats.unavailable(repo, rel) {
        return Err(RepoError::runtime(tmi_repo::formats::not_available_message(rel, &id, &u)));
    }
    let all = LayerBlobs::All;
    let opts = Options {
        render_composite: true,
        cache: Some(&stager.ctx.render_cache),
        reuse_pixels: None,
        reuse_decoded: None,
        layer_blobs: &all,
        composite_base: None,
        limits: stager.ctx.limits,
        verify_partial: false,
        progress: stager.progress,
    };
    let mut warns = std::mem::take(&mut stager.warns);
    let imported = stager.ctx.formats.import(repo, rel, &name, &bytes, &digest, &opts, &mut warns);
    stager.warns = warns;
    let imported = imported?;
    let Some(layer) = crate::restore::find_imported_layer(&imported.layers, selector)? else {
        return Err(RepoError::runtime(format!("layer not found in {rel}: {selector}")));
    };
    let Some(head_entry) = stager.head_entries.get(rel).cloned() else {
        // nothing to stage it into: the whole file, marked as Python marks it
        let mut entry = stager.materialize_import(rel, &imported, &bytes, None, None)?;
        mark(&mut entry, selector, layer);
        let changed = stager.put_staged(rel, entry)?;
        return Ok((rel.to_string(), changed));
    };
    let head_format = head_entry.get("source_format").and_then(Value::py_str).unwrap_or_default().to_lowercase();
    if head_format != imported.source_format {
        return Err(RepoError::runtime(format!(
            "cannot stage a single layer after file format changed ({head_format} -> {}); stage the whole file",
            imported.source_format
        )));
    }
    // _ensure_partial_layer_stage_safe, for a plugin's records: the canvas,
    // the layer's properties and its whole source_meta but the pixel source
    if canon(head_entry.get("canvas_meta").unwrap_or(&Value::Null)) != canon(&Value::Object(imported.canvas_meta.clone())) {
        return Err(RepoError::runtime(format!("cannot stage a single layer while canvas/global settings changed: {selector}; stage the whole file")));
    }
    let tree_ref = head_entry.get("tree_ref").and_then(Value::as_str).ok_or_else(|| pix::fwd("entry without a tree_ref"))?.to_string();
    let mut old_layers = Vec::new();
    stage::flatten_layers(repo, &tree_ref, &mut old_layers, 0)?;
    let Some(old) = old_layers.iter().find(|l| l.get("stable_source_id").and_then(Value::as_str) == Some(layer.stable_source_id.as_str())) else {
        return Err(RepoError::runtime(format!("cannot stage a newly added layer separately: {selector}; stage the whole file")));
    };
    let changed_props = LAYER_FIELDS.iter().any(|k| canon(old.get(k).unwrap_or(&Value::Null)) != canon(&layer.field(k)))
        || meta_without_pixel_source(old.get("source_meta")) != meta_without_pixel_source(Some(&Value::Object(layer.source_meta.clone())));
    if changed_props {
        return Err(RepoError::runtime(format!("layer metadata changed for {selector}; stage the whole file for metadata-only/global changes")));
    }
    let Some(blob) = &layer.blob_bytes else {
        let hint = match layer.source_meta.get("write_capability").and_then(Value::as_str) {
            Some("metadata-only") => "; stage the whole file for metadata-only changes",
            _ => "",
        };
        return Err(RepoError::runtime(format!("layer has no stageable pixel blob: {selector}{hint}")));
    };
    let loaded = stager.ctx.formats.plugin(repo, id).map_err(|u| RepoError::runtime(tmi_repo::formats::not_available_message(rel, id, &u)))?;
    if !loaded.can_write() {
        return Err(RepoError::runtime(format!(
            "{rel}: {} cannot write layers back, so one layer cannot be staged alone; stage the whole file",
            loaded.label()
        )));
    }
    tmi_import::report(stager.progress, || format!("staging layer {rel}:{}", layer.name));
    let base = repo.store.read_blob(head_entry.get("source_blob_ref").and_then(Value::as_str).ok_or_else(|| pix::fwd("entry without a source blob"))?)?;
    let layers = [(layer.stable_source_id.clone(), blob.clone())];
    let limits = stager.ctx.limits;
    let mut warns = std::mem::take(&mut stager.warns);
    let written = crate::writeback::plugin_replace(repo, &stager.ctx.formats, limits, &mut warns, rel, &name, &base, id, &layers);
    stager.warns = warns;
    let written = written?;
    // materialize the merged bytes, reusing HEAD's layer blobs
    let reuse: HashMap<String, stage::BlobReuse> = stage::layer_blob_reuse(repo, Some(&head_entry))?;
    let pixels = stage::reuse_pixels(&reuse);
    let decoded = stage::reuse_decoded(&reuse);
    let opts = Options {
        render_composite: true,
        cache: Some(&stager.ctx.render_cache),
        reuse_pixels: (!pixels.is_empty()).then_some(&pixels),
        reuse_decoded: decoded.as_ref(),
        layer_blobs: &all,
        composite_base: None,
        limits,
        verify_partial: false,
        progress: stager.progress,
    };
    let merged_digest = tmi_store::oid::sha256_hex(&written);
    let mut warns = std::mem::take(&mut stager.warns);
    let merged = stager.ctx.formats.import(repo, rel, &name, &written, &merged_digest, &opts, &mut warns);
    stager.warns = warns;
    let merged = merged?;
    let mut entry = stager.materialize_import(rel, &merged, &written, Some(&reuse), None)?;
    if tmi_repo::repo::same_entry(Some(&committable(&entry)), Some(&head_entry)) {
        return Err(RepoError::runtime(format!("layer has no pixel changes to stage separately: {selector}; stage the whole file for metadata-only changes")));
    }
    mark(&mut entry, selector, layer);
    let changed = stager.put_staged(rel, entry)?;
    Ok((rel.to_string(), changed))
}
