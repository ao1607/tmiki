//! tmi/merge.py `merge_entries` (the layer-aware three-way merge of one
//! file) and repo.py `merge`, `merge_abort`, `materialize_conflict`:
//! fast-forward, clean and layer merges, the conflict state (MERGE_HEAD,
//! MERGE_MSG, `.tmi/conflicts/<path>/` with OURS / THEIRS and per-layer
//! PNGs, conflict.json), the index and working files it leaves.

use crate::pix::{self, fwd, Op, Px};
use crate::restore::flatten_layers;
use crate::writeback::{self, WriteCtx};
use indexmap::IndexMap;
use std::collections::{HashMap, HashSet};
use tmi_import::{CompositeBase, LayerBlobs, Limits, Options, Warnings};
use tmi_repo::fsio;
use tmi_repo::hooks;
use tmi_repo::pyeq::py_eq;
use tmi_repo::repo::{prefix_chars, same_entry, Entries};
use tmi_repo::stage::{self, BlobReuse, ImportContext, Stager};
use tmi_repo::write::{deleted_entry, int, obj, s, safe_filename, source_blob_ref, str_of};
use tmi_repo::{Repo, RepoError, Result};
use tmi_store::json::{self, DumpOptions, Map, Value};

const MERGEABLE_FORMATS: [&str; 4] = ["clip", "mdp2", "png", "jpeg"];

/// merge.py `no_merge_writer_reason(source_format)` (issue #43).
fn no_merge_writer_reason(source_format: &str) -> String {
    let name = match source_format {
        "gif" => Some("GIF"),
        "bmp" => Some("BMP"),
        "tiff" => Some("TIFF"),
        "webp" => Some("WebP"),
        "psd" => Some("PSD"),
        _ => None,
    };
    match name {
        Some(n) => format!("{n} files cannot be merged automatically (tmiki cannot write them back); take one side with --ours or --theirs"),
        None => "this file cannot be merged automatically (tmiki does not read it as a drawing); take one side with --ours or --theirs".into(),
    }
}
const LOSSY_CONFLICT_REASON: &str =
    "JPEG changes every pixel slightly on each save, so it cannot be merged automatically; take one side with --ours or --theirs";

#[derive(Clone, Debug)]
pub struct LayerConflict {
    pub sid: String,
    pub name: String,
    pub path_name: Value,
    pub reason: String,
}

#[derive(Default)]
pub struct LayerMergeResult {
    pub entry: Option<Value>,
    pub conflicts: Vec<String>,
    pub notes: Vec<String>,
    pub layer_conflicts: Vec<LayerConflict>,
    pub partial_entry: Option<Value>,
    /// Layers whose two sides go into `.tmi/conflicts/<file>/` as PNGs
    /// although they are no layer conflict (a plugin format that cannot be
    /// written back: issue #47).
    pub export_layers: Vec<String>,
}

/// merge.py `_same(left, right)`: equal canonical JSON.
fn same(left: Option<&Value>, right: Option<&Value>) -> bool {
    same_entry(left, right)
}

/// merge.py `_same_entry`: identical source bytes, or equal records.
fn same_file_entry(left: Option<&Value>, right: Option<&Value>) -> bool {
    if let (Some(l), Some(r)) = (left, right) {
        if let Some(d) = l.get("source_digest").filter(|d| d.truthy()) {
            if r.get("source_digest").is_some_and(|e| py_eq(d, e)) {
                return true;
            }
        }
    }
    same(left, right)
}

fn without_keys(layer: &Value, keys: &[&str]) -> Value {
    match layer.as_object() {
        Some(m) => Value::Object(m.iter().filter(|(k, _)| !keys.contains(&k.as_str())).map(|(k, v)| (k.clone(), v.clone())).collect()),
        None => layer.clone(),
    }
}

/// `_without_pixel_source(layer)`.
fn without_pixel_source(layer: &Value) -> Value {
    let Some(map) = layer.as_object() else { return layer.clone() };
    match map.get("source_meta") {
        Some(Value::Object(meta)) if meta.contains_key("pixel_source") => {
            let mut out = map.clone();
            out.insert("source_meta".into(), without_keys(&Value::Object(meta.clone()), &["pixel_source"]));
            Value::Object(out)
        }
        _ => layer.clone(),
    }
}

/// `_same_layer(left, right)`.
fn same_layer(left: Option<&Value>, right: Option<&Value>) -> bool {
    if let (Some(l), Some(r)) = (left, right) {
        if l.get("pixel_sha256").is_some_and(Value::truthy) && r.get("pixel_sha256").is_some_and(Value::truthy) {
            return same(
                Some(&without_pixel_source(&without_keys(l, &["blob_ref"]))),
                Some(&without_pixel_source(&without_keys(r, &["blob_ref"]))),
            );
        }
    }
    same(left, right)
}

/// `_props(layer)`.
fn props(layer: &Value) -> Value {
    without_pixel_source(&without_keys(layer, &["blob_ref", "pixel_bbox", "pixel_sha256", "path_name"]))
}

/// `_props_changed(base, side)`.
fn props_changed(base: Option<&Value>, side: Option<&Value>) -> bool {
    match (base, side) {
        (Some(b), Some(s)) if b.truthy() && s.truthy() => !py_eq(&props(b), &props(s)),
        _ => true,
    }
}

/// `_pixel_changed(base, side)`.
fn pixel_changed(base: Option<&Value>, side: Option<&Value>) -> bool {
    let (Some(b), Some(s)) = (base.filter(|v| v.truthy()), side.filter(|v| v.truthy())) else { return false };
    let (bp, sp) = (b.get("pixel_sha256"), s.get("pixel_sha256"));
    if bp.is_some_and(Value::truthy) && sp.is_some_and(Value::truthy) {
        return !py_eq(bp.unwrap(), sp.unwrap());
    }
    let null = Value::Null;
    !py_eq(b.get("blob_ref").unwrap_or(&null), s.get("blob_ref").unwrap_or(&null))
}

/// `_layer_name(layer, fallback)`.
fn layer_name(layer: Option<&Value>, fallback: &str) -> Result<String> {
    match layer.filter(|l| l.truthy()) {
        Some(l) => match l.get("name") {
            None => Ok("None".into()),
            Some(v) => v.py_str().ok_or_else(|| fwd("str() of a container name")),
        },
        None => Ok(fallback.to_string()),
    }
}

/// merge.py `_layers(repo, entry)`: stable id -> layer (subtree_ref
/// replaced by the folder's child ids).
fn layers_of(repo: &Repo, entry: &Value) -> Result<IndexMap<String, Value>> {
    let tree_ref = str_of(entry, "tree_ref")?.ok_or_else(|| fwd("entry without a tree_ref"))?;
    let mut flat = Vec::new();
    flatten_layers(repo, &tree_ref, "", &mut flat, 0)?;
    let mut out: IndexMap<String, Value> = IndexMap::new();
    for layer in flat {
        let sid = match layer.get("stable_source_id") {
            Some(Value::Str(s)) => s.clone(),
            _ => return Err(fwd("stable_source_id is not a str")),
        };
        out.insert(sid, layer);
    }
    for layer in out.values_mut() {
        let Value::Object(map) = layer else { unreachable!() };
        let subtree = map.shift_remove("subtree_ref");
        if let Some(sub) = subtree.filter(Value::truthy) {
            let sub = sub.as_str().ok_or_else(|| fwd("subtree_ref is not a str"))?.to_string();
            let tree = repo.store.read_json(&sub, Some("tree"))?;
            let mut ids = Vec::new();
            if let Some(layers) = tree.get("layers") {
                for child in layers.as_array().ok_or_else(|| fwd("tree layers not a list"))? {
                    ids.push(child.get("stable_source_id").cloned().ok_or_else(|| fwd("child without a stable_source_id"))?);
                }
            }
            map.insert("child_ids".into(), Value::Array(ids));
        }
    }
    Ok(out)
}

/// `_blob_reuse_info(layer)`.
fn blob_reuse_info(layer: Option<&Value>) -> Option<BlobReuse> {
    let layer = layer.filter(|l| l.truthy())?;
    let pixel = layer.get("pixel_sha256").filter(|v| !v.is_null())?.clone();
    let blob = layer.get("blob_ref").filter(|v| !v.is_null())?.clone();
    Some(BlobReuse {
        pixel_sha256: pixel,
        blob_ref: blob,
        pixel_bbox: layer.get("pixel_bbox").cloned().unwrap_or(Value::Null),
        pixel_source: layer.get("source_meta").and_then(|m| m.get("pixel_source")).cloned().unwrap_or(Value::Null),
    })
}

/// `_merged_blob_reuse(start_layers, other_layers, local_layers, replacements)`.
fn merged_blob_reuse(
    start: &IndexMap<String, Value>,
    other: &IndexMap<String, Value>,
    local: &IndexMap<String, Value>,
    replacements: &IndexMap<String, Vec<u8>>,
) -> HashMap<String, BlobReuse> {
    let mut reuse = HashMap::new();
    for (sid, layer) in start {
        if let Some(info) = blob_reuse_info(Some(layer)) {
            reuse.insert(sid.clone(), info);
        }
    }
    for sid in replacements.keys() {
        match blob_reuse_info(other.get(sid)).or_else(|| blob_reuse_info(local.get(sid))) {
            Some(mut info) => {
                info.pixel_source = Value::Null;
                reuse.insert(sid.clone(), info);
            }
            None => {
                reuse.remove(sid);
            }
        }
    }
    reuse
}

/// What the merge reads and accumulates besides the repository.
pub struct MergeCtx<'a> {
    pub repo: &'a Repo,
    pub import: ImportContext,
    pub warns: Warnings,
    /// repo.py `merge`'s `on_progress`.
    pub progress: Option<tmi_import::Progress<'a>>,
    /// `.tmiattributes` or a `.tmiplugins/` file conflicts in this merge:
    /// no plugin-format file is merged layer by layer (PLUGINS.md 4.5, Q7).
    pub plugin_setup_conflict: bool,
}

impl MergeCtx<'_> {
    fn limits(&self) -> Limits {
        self.import.limits
    }
}

struct StoredBase<'a> {
    repo: &'a Repo,
    source_ref: String,
    composite_ref: String,
}

impl CompositeBase for StoredBase<'_> {
    fn load_composite_png(&self) -> std::result::Result<Vec<u8>, String> {
        self.repo.store.read_blob(&self.composite_ref).map_err(|e| e.to_string())
    }

    fn load_source(&self) -> std::result::Result<Vec<u8>, String> {
        self.repo.store.read_blob(&self.source_ref).map_err(|e| e.to_string())
    }
}

/// repo.py `materialize_file_from_path(relpath, path, blob_reuse=,
/// base_entry=)` of a file whose bytes are `source` and name `name`.
pub fn materialize_file(
    ctx: &mut MergeCtx,
    rel: &str,
    name: &str,
    source: &[u8],
    blob_reuse: &HashMap<String, BlobReuse>,
    base_entry: Option<&Value>,
) -> Result<Value> {
    let repo = ctx.repo;
    let digest = tmi_store::oid::sha256_hex(source);
    let pixels = stage::reuse_pixels(blob_reuse);
    let decoded = stage::reuse_decoded(blob_reuse);
    let base = crate::diff::composite_base_refs(base_entry).map(|(s, c)| StoredBase { repo, source_ref: s, composite_ref: c });
    let all = LayerBlobs::All;
    let opts = Options {
        render_composite: true,
        cache: Some(&ctx.import.render_cache),
        reuse_pixels: (!pixels.is_empty()).then_some(&pixels),
        reuse_decoded: decoded.as_ref(),
        layer_blobs: &all,
        composite_base: base.as_ref().map(|b| b as &dyn CompositeBase),
        limits: ctx.import.limits,
        verify_partial: ctx.import.verify_partial,
        progress: ctx.progress,
    };
    let imported = ctx.import.formats.import(repo, rel, name, source, &digest, &opts, &mut ctx.warns)?;
    let mut stager = Stager::new(repo)?;
    stager.progress = ctx.progress;
    let reuse = (!blob_reuse.is_empty()).then_some(blob_reuse);
    let mut entry = stager.materialize_import(rel, &imported, source, reuse, None)?;
    if let Value::Object(m) = &mut entry {
        m.insert("path".into(), Value::Str(rel.to_string()));
    }
    Ok(entry)
}

/// `_write_merged(repo, path, start, source_format, replacements, blob_reuse)`.
fn write_merged(
    ctx: &mut MergeCtx,
    path: &str,
    start: &Value,
    source_format: &str,
    replacements: &IndexMap<String, Vec<u8>>,
    blob_reuse: &HashMap<String, BlobReuse>,
) -> Result<Value> {
    let repo = ctx.repo;
    // _write_temp_source: .tmi/tmp/merge/<name> (the bytes stay in memory;
    // the directory is what Python leaves behind)
    let tmp_dir = repo.repo_dir.join("tmp").join("merge");
    std::fs::create_dir_all(&tmp_dir).map_err(|e| fwd(format!("{}: {e}", tmp_dir.display())))?;
    let name = path.rsplit('/').next().unwrap_or(path).to_string();
    let mut source = repo.store.read_blob(&source_blob_ref(start)?)?;
    let limits = ctx.limits();
    if let Some(id) = source_format.strip_prefix("plugin:") {
        // every layer in one call of the plugin's writer, checked after
        // writing (PLUGINS.md P7; the consent was asked before)
        tmi_import::report(ctx.progress, || format!("writing merged layers through plugin {id}"));
        let layers: Vec<(String, Vec<u8>)> = replacements.iter().map(|(s, b)| (s.clone(), b.clone())).collect();
        source = writeback::plugin_replace(repo, &ctx.import.formats, limits, &mut ctx.warns, path, &name, &source, id, &layers)?;
        tmi_import::report(ctx.progress, || format!("parsing merged {path}"));
        return materialize_file(ctx, path, &name, &source, blob_reuse, Some(start));
    }
    for (idx, (sid, png)) in replacements.iter().enumerate() {
        tmi_import::report(ctx.progress, || format!("writing merged layers {}/{}", idx + 1, replacements.len()));
        let mut wctx = WriteCtx { limits, warns: &mut ctx.warns };
        source = writeback::replace_layer_pixels(&mut wctx, &name, Some(&source), source_format, sid, png, false)?;
    }
    tmi_import::report(ctx.progress, || format!("parsing merged {path}"));
    materialize_file(ctx, path, &name, &source, blob_reuse, Some(start))
}

/// `_merge_non_overlapping_pixels(repo, base, local, remote)`.
fn merge_non_overlapping(ctx: &MergeCtx, base: &Value, local: &Value, remote: &Value) -> Result<Option<Vec<u8>>> {
    let blob = |v: &Value| str_of(v, "blob_ref").map(|b| b.filter(|b| !b.is_empty()));
    let (Some(bb), Some(lb), Some(rb)) = (blob(base)?, blob(local)?, blob(remote)?) else { return Ok(None) };
    let repo = ctx.repo;
    let limits = ctx.limits();
    let base_img = pix::pixels_from_bytes(&repo.store.read_blob(&bb)?, &limits)?;
    let local_img = pix::pixels_from_bytes(&repo.store.read_blob(&lb)?, &limits)?;
    let remote_img = pix::pixels_from_bytes(&repo.store.read_blob(&rb)?, &limits)?;
    let local_bbox = pix::diff_bbox(&base_img, &local_img);
    let remote_bbox = pix::diff_bbox(&base_img, &remote_img);
    let Some(lbox) = local_bbox else { return Ok(Some(repo.store.read_blob(&rb)?)) };
    let Some(rbox) = remote_bbox else { return Ok(Some(repo.store.read_blob(&lb)?)) };
    let overlaps = !(lbox.2 <= rbox.0 || rbox.2 <= lbox.0 || lbox.3 <= rbox.1 || rbox.3 <= lbox.1);
    if overlaps {
        return Ok(None);
    }
    let base_full = base_img.into_dense();
    let (lr, rr) = (local_img.region(lbox), remote_img.region(rbox));
    let merged = pix::compose(
        base_full.width,
        base_full.height,
        &[Op::Paste(&lr, lbox.0, lbox.1), Op::Paste(&rr, rbox.0, rbox.1)],
        [0, 0, 0, 0],
        Some(&base_full),
    );
    Ok(Some(pix::png(&merged)?))
}

/// A Python exception's message (what `except Exception` catches); a
/// Forward stays a Forward.
fn caught(e: RepoError) -> Result<String> {
    match e {
        RepoError::Forward(w) => Err(RepoError::Forward(w)),
        other => Ok(other.message().unwrap_or_default().to_string()),
    }
}

/// How a plugin-format file's three sides go into the layer merge.
enum PluginSides {
    /// Decided already (a conflict, or one side's record).
    Done(Box<LayerMergeResult>),
    /// Merge these records layer by layer (None: the entries as they are;
    /// Some: re-imported with the merge result's plugin); `no_writer`: why
    /// nothing can be written back (the plugin has no writer or cannot be
    /// used), None when its writer can write the merged layers (P7).
    Records { sides: Option<(Value, Value, Value)>, no_writer: Option<String> },
}

/// Whether two records hold the same canvas and layers (PLUGINS.md 4.5:
/// what is compared once all three sides come from the same plugin).
fn same_records(repo: &Repo, a: &Value, b: &Value) -> Result<bool> {
    if !py_eq(a.get("canvas_meta").unwrap_or(&Value::Null), b.get("canvas_meta").unwrap_or(&Value::Null)) {
        return Ok(false);
    }
    let (la, lb) = (layers_of(repo, a)?, layers_of(repo, b)?);
    Ok(la.len() == lb.len() && la.iter().all(|(sid, l)| same_layer(Some(l), lb.get(sid))))
}

/// A plugin-format file changed on both sides (PLUGINS.md 4.4, 4.5): when
/// `.tmiattributes` or `.tmiplugins/` conflict, a conflict (Q7); when the
/// sides' `importer`s differ, the three sources re-imported with the merge
/// result's plugin (or a conflict when it cannot be used); then, when one
/// side's record is the base's, the other side.
fn plugin_sides(ctx: &mut MergeCtx, path: &str, id: &str, base: &Value, local: &Value, remote: &Value) -> Result<PluginSides> {
    let done = |reason: String| Ok(PluginSides::Done(Box::new(LayerMergeResult { conflicts: vec![format!("{path}: {reason}")], ..Default::default() })));
    if ctx.plugin_setup_conflict {
        return done(format!(
            "{} or {}/ conflict in this merge, so its layers are not merged; take one side with --ours or --theirs",
            tmi_repo::attributes::FILE,
            tmi_repo::attributes::PLUGIN_DIR
        ));
    }
    let repo = ctx.repo;
    let importer = |e: &Value| e.get("importer").cloned().unwrap_or(Value::Null);
    let same_importer = py_eq(&importer(base), &importer(local)) && py_eq(&importer(base), &importer(remote));
    let loaded = ctx.import.formats.plugin(repo, id);
    let no_writer = match &loaded {
        Ok(l) if l.can_write() => None,
        Ok(l) => Some(format!("{} cannot write layers back, so the two sides could not be combined", l.label())),
        Err(u) => Some(format!("plugin '{id}' is not available ({}), so the two sides could not be combined", u.why(id))),
    };
    let sides = if same_importer {
        None
    } else {
        if let Err(u) = &loaded {
            return done(format!("plugin '{id}' is not available: cannot write merged layers ({})", u.why(id)));
        }
        tmi_import::report(ctx.progress, || format!("re-importing {path} with plugin {id}"));
        let name = path.rsplit('/').next().unwrap_or(path).to_string();
        let mut again = Vec::new();
        for side in [base, local, remote] {
            let source = repo.store.read_blob(&source_blob_ref(side)?)?;
            let reuse = stage::layer_blob_reuse(repo, Some(side))?;
            again.push(materialize_file(ctx, path, &name, &source, &reuse, None)?);
        }
        let remote = again.pop().unwrap();
        let local = again.pop().unwrap();
        let base = again.pop().unwrap();
        Some((base, local, remote))
    };
    let (b, l, r) = match &sides {
        Some((b, l, r)) => (b, l, r),
        None => (base, local, remote),
    };
    if same_records(repo, b, l)? {
        return Ok(PluginSides::Done(Box::new(LayerMergeResult { entry: Some(r.clone()), ..Default::default() })));
    }
    if same_records(repo, b, r)? {
        return Ok(PluginSides::Done(Box::new(LayerMergeResult { entry: Some(l.clone()), ..Default::default() })));
    }
    Ok(PluginSides::Records { sides, no_writer })
}

/// The names of the layers of `side` that differ from `base` (a short
/// list for a message).
fn changed_layer_names(base: &IndexMap<String, Value>, side: &IndexMap<String, Value>) -> Result<String> {
    let mut sids: Vec<&String> = base.keys().chain(side.keys()).collect();
    sids.sort();
    sids.dedup();
    let mut names = Vec::new();
    for sid in sids {
        if same_layer(base.get(sid), side.get(sid)) {
            continue;
        }
        let layer = side.get(sid).or_else(|| base.get(sid));
        let shown = match layer.and_then(|l| l.get("path_name")).filter(|v| v.truthy()) {
            Some(v) => v.py_str().ok_or_else(|| fwd("str() of a container name"))?,
            None => layer_name(layer, sid)?,
        };
        names.push(if side.contains_key(sid) { shown } else { format!("{shown} (deleted)") });
    }
    Ok(match names.len() {
        0 => "no layer".into(),
        n if n > 6 => format!("{} and {} more", names[..5].join(", "), n - 5),
        _ => names.join(", "),
    })
}

/// A plugin-format file whose sides need a write the plugin cannot make
/// (issue #47): one conflict of the whole file, saying why, which side
/// changed which layers, and what to do; both sides of every changed
/// layer go into the conflicts directory as PNGs.
fn no_writer_conflict(
    path: &str,
    reason: &str,
    base: &IndexMap<String, Value>,
    local: &IndexMap<String, Value>,
    remote: &IndexMap<String, Value>,
) -> Result<LayerMergeResult> {
    let mut export: Vec<String> = Vec::new();
    for sid in base.keys().chain(local.keys()).chain(remote.keys()) {
        let changed = !same_layer(base.get(sid), local.get(sid)) || !same_layer(base.get(sid), remote.get(sid));
        if changed && !export.contains(sid) {
            export.push(sid.clone());
        }
    }
    let dir = format!(".tmi/conflicts/{}/", safe_filename(path));
    let conflicts = vec![
        format!("{path}: {reason}"),
        format!("{path}: ours changed {}; theirs changed {}", changed_layer_names(base, local)?, changed_layer_names(base, remote)?),
        format!(
            "{path}: take one side with 'tmiki restore --ours {path}' or 'tmiki restore --theirs {path}' and redo the other side's changes in your drawing app; both versions are in {dir}"
        ),
    ];
    Ok(LayerMergeResult { conflicts, export_layers: export, ..Default::default() })
}

/// merge.py `merge_entries(repo, path, base, local, remote)`.
pub fn merge_entries(ctx: &mut MergeCtx, path: &str, base: Option<&Value>, local: Option<&Value>, remote: Option<&Value>) -> Result<LayerMergeResult> {
    if same_file_entry(local, remote) {
        return Ok(LayerMergeResult { entry: local.cloned(), ..Default::default() });
    }
    if same_file_entry(base, local) {
        return Ok(LayerMergeResult { entry: remote.cloned(), ..Default::default() });
    }
    if same_file_entry(base, remote) {
        return Ok(LayerMergeResult { entry: local.cloned(), ..Default::default() });
    }
    let (Some(base), Some(local), Some(remote)) = (base.filter(|v| v.truthy()), local.filter(|v| v.truthy()), remote.filter(|v| v.truthy())) else {
        return Ok(LayerMergeResult { conflicts: vec![format!("{path}: add/delete conflict")], ..Default::default() });
    };
    let fmt = |e: &Value| e.get("source_format").cloned().unwrap_or(Value::Null);
    if !py_eq(&fmt(local), &fmt(remote)) || !py_eq(&fmt(base), &fmt(local)) {
        return Ok(LayerMergeResult { conflicts: vec![format!("{path}: source format changed on both sides")], ..Default::default() });
    }
    let source_format = match local.get("source_format").filter(|v| v.truthy()) {
        Some(v) => v.py_str().ok_or_else(|| fwd("source_format"))?.to_lowercase(),
        None => String::new(),
    };
    if !source_format.is_ascii() {
        return Err(fwd("a non-ASCII source format"));
    }
    let plugin_id = source_format.strip_prefix("plugin:").map(str::to_string);
    let reimported: (Value, Value, Value);
    // A plugin format without a writer (or whose plugin cannot be used)
    // cannot take a merge that needs writing: a conflict of the whole file
    // with this reason (issue #47).
    let mut no_writer: Option<String> = None;
    let (base, local, remote) = match &plugin_id {
        Some(id) => match plugin_sides(ctx, path, id, base, local, remote)? {
            PluginSides::Done(result) => return Ok(*result),
            PluginSides::Records { sides, no_writer: why } => {
                no_writer = why;
                match sides {
                    Some(s) => {
                        reimported = s;
                        (&reimported.0, &reimported.1, &reimported.2)
                    }
                    None => (base, local, remote),
                }
            }
        },
        None if !MERGEABLE_FORMATS.contains(&source_format.as_str()) => {
            return Ok(LayerMergeResult { conflicts: vec![format!("{path}: {}", no_merge_writer_reason(&source_format))], ..Default::default() });
        }
        None => (base, local, remote),
    };
    let repo = ctx.repo;
    let base_layers = layers_of(repo, base)?;
    let local_layers = layers_of(repo, local)?;
    let remote_layers = layers_of(repo, remote)?;
    let mut all: Vec<&String> = base_layers.keys().chain(local_layers.keys()).chain(remote_layers.keys()).collect();
    all.sort();
    all.dedup();
    let differing: Vec<String> =
        all.into_iter().filter(|sid| !same_layer(local_layers.get(*sid), remote_layers.get(*sid))).cloned().collect();
    let structural = |side: &IndexMap<String, Value>| -> Vec<String> {
        differing
            .iter()
            .filter(|sid| !same_layer(base_layers.get(*sid), side.get(*sid)) && props_changed(base_layers.get(*sid), side.get(*sid)))
            .cloned()
            .collect()
    };
    let local_structural = structural(&local_layers);
    let remote_structural = structural(&remote_layers);
    let start_is_remote = !remote_structural.is_empty() && local_structural.is_empty();
    let (start, start_layers, other_layers) =
        if start_is_remote { (remote, &remote_layers, &local_layers) } else { (local, &local_layers, &remote_layers) };

    let mut replacements: IndexMap<String, Vec<u8>> = IndexMap::new();
    let mut layer_conflicts: Vec<LayerConflict> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    let conflict = |sid: &str, reason: &str, out: &mut Vec<LayerConflict>| -> Result<()> {
        let layer = [local_layers.get(sid), remote_layers.get(sid), base_layers.get(sid)].into_iter().flatten().find(|l| l.truthy());
        out.push(LayerConflict {
            sid: sid.to_string(),
            name: layer_name(layer, sid)?,
            path_name: layer.and_then(|l| l.get("path_name")).cloned().unwrap_or(Value::Null),
            reason: reason.to_string(),
        });
        Ok(())
    };
    if !local_structural.is_empty() && !remote_structural.is_empty() {
        let mut both: Vec<&String> = local_structural.iter().chain(remote_structural.iter()).collect();
        both.sort();
        both.dedup();
        for sid in both {
            conflict(sid, "layer structure/properties changed on both sides", &mut layer_conflicts)?;
        }
    }
    for sid in &differing {
        let b = base_layers.get(sid);
        let st = start_layers.get(sid);
        let o = other_layers.get(sid);
        if layer_conflicts.iter().any(|c| &c.sid == sid) {
            continue;
        }
        if same_layer(b, o) {
            continue;
        }
        if props_changed(b, o) {
            continue;
        }
        match st {
            None => conflict(sid, "deleted on one side, edited on the other", &mut layer_conflicts)?,
            Some(st) if !pixel_changed(b, Some(st)) => {
                let blob = str_of(o.unwrap(), "blob_ref")?.ok_or_else(|| fwd("read_blob(None)"))?;
                replacements.insert(sid.clone(), repo.store.read_blob(&blob)?);
            }
            Some(st) => match merge_non_overlapping(ctx, b.unwrap(), st, o.unwrap())? {
                None => conflict(
                    sid,
                    if source_format == "jpeg" { LOSSY_CONFLICT_REASON } else { "overlapping pixel edits" },
                    &mut layer_conflicts,
                )?,
                Some(merged) => {
                    replacements.insert(sid.clone(), merged);
                    notes.push(format!("{path}:{} merged non-overlapping pixel edits", layer_name(b, sid)?));
                }
            },
        }
    }
    if let Some(reason) = &no_writer {
        // nothing can be written: not a layer conflict, the whole file is
        if replacements.is_empty() && layer_conflicts.is_empty() {
            return Ok(LayerMergeResult { entry: Some(start.clone()), notes, ..Default::default() });
        }
        return no_writer_conflict(path, reason, &base_layers, &local_layers, &remote_layers);
    }
    if let Some(id) = &plugin_id {
        // the writer runs below (the merged layers, or the layers of a
        // partial merge): the user's consent first, which stops the merge
        // before it changes anything when it is refused (PLUGINS.md Q1)
        if !replacements.is_empty() || (start_is_remote && !layer_conflicts.is_empty()) {
            let loaded = ctx.import.formats.plugin(repo, id).map_err(|u| RepoError::runtime(tmi_repo::formats::not_available_message(path, id, &u)))?;
            tmi_repo::trust::require_write_consent(repo, &loaded.component(), path)?;
        }
    }
    let conflicts: Vec<String> = layer_conflicts.iter().map(|c| format!("{path}:{} {}", c.name, c.reason)).collect();
    if !layer_conflicts.is_empty() {
        if start_is_remote {
            for c in &layer_conflicts {
                let ours = local_layers.get(&c.sid);
                let in_start = start_layers.get(&c.sid);
                if ours.is_none() || in_start.is_none() || props_changed(in_start, ours) {
                    return Ok(LayerMergeResult { conflicts, notes, layer_conflicts, ..Default::default() });
                }
                let blob = str_of(ours.unwrap(), "blob_ref")?.ok_or_else(|| fwd("read_blob(None)"))?;
                replacements.insert(c.sid.clone(), repo.store.read_blob(&blob)?);
            }
        }
        if replacements.is_empty() {
            return Ok(LayerMergeResult { conflicts, notes, layer_conflicts, ..Default::default() });
        }
        let reuse = merged_blob_reuse(start_layers, other_layers, &local_layers, &replacements);
        let partial = match write_merged(ctx, path, start, &source_format, &replacements, &reuse) {
            Ok(e) => Some(e),
            Err(e) => {
                caught(e)?;
                None
            }
        };
        return Ok(LayerMergeResult { conflicts, notes, layer_conflicts, partial_entry: partial, ..Default::default() });
    }
    if replacements.is_empty() {
        return Ok(LayerMergeResult { entry: Some(start.clone()), notes, ..Default::default() });
    }
    let reuse = merged_blob_reuse(start_layers, other_layers, &local_layers, &replacements);
    match write_merged(ctx, path, start, &source_format, &replacements, &reuse) {
        Ok(e) => Ok(LayerMergeResult { entry: Some(e), notes, ..Default::default() }),
        Err(e) => {
            let msg = caught(e)?;
            let line = match plugin_id {
                // "<path>: plugin 'x' 1.0.0 could not write layers back: ..."
                Some(_) => format!("{msg}; take one side with --ours or --theirs"),
                None => format!("{path}: native merge failed: {msg}"),
            };
            Ok(LayerMergeResult { conflicts: vec![line], notes, ..Default::default() })
        }
    }
}

/// What `merge` did.
pub enum MergeOutcome {
    UpToDate,
    FastForward(String),
    Merged(String),
    /// (paths, per path the conflict details, the conflicts directory)
    Conflict(Vec<String>, Vec<Vec<Value>>, String),
}

/// `_build_conflict_meta(path, per_side_layers, current, other, suffix)`.
fn build_conflict_meta(path: &str, per_side: &[(&str, Vec<Value>)], current: Option<&Value>, other: Option<&Value>, suffix: &str) -> Result<Map> {
    let mut rows: IndexMap<String, Map> = IndexMap::new();
    for (label, layers) in per_side {
        for layer in layers {
            let sid = match layer.get("stable_source_id").filter(|v| v.truthy()) {
                Some(Value::Str(s)) => s.clone(),
                Some(_) => return Err(fwd("stable_source_id is not a str")),
                None => continue,
            };
            let key_src = [layer.get("path_name"), layer.get("name"), layer.get("layer_id")].into_iter().flatten().find(|v| v.truthy());
            let key = match key_src {
                Some(Value::Str(v)) => safe_filename(v),
                _ => return Err(fwd("a layer key that is not a str")),
            };
            let row = rows.entry(sid.clone()).or_insert_with(|| {
                let mut m = Map::new();
                m.insert("stable_source_id".into(), Value::Str(sid.clone()));
                m.insert("name".into(), layer.get("name").cloned().unwrap_or(Value::Null));
                m.insert("path_name".into(), layer.get("path_name").cloned().unwrap_or(Value::Null));
                m.insert("file_key".into(), Value::Str(key.clone()));
                m.insert("ours_blob_ref".into(), Value::Null);
                m.insert("theirs_blob_ref".into(), Value::Null);
                m
            });
            row.insert(format!("{}_blob_ref", label.to_lowercase()), layer.get("blob_ref").cloned().unwrap_or(Value::Null));
            if !row.get("name").is_some_and(Value::truthy) {
                row.insert("name".into(), layer.get("name").cloned().unwrap_or(Value::Null));
            }
            if !row.get("path_name").is_some_and(Value::truthy) {
                row.insert("path_name".into(), layer.get("path_name").cloned().unwrap_or(Value::Null));
            }
        }
    }
    let mut list: Vec<Map> = rows.into_values().collect();
    for row in &mut list {
        let differs = !py_eq(row.get("ours_blob_ref").unwrap_or(&Value::Null), row.get("theirs_blob_ref").unwrap_or(&Value::Null));
        row.insert("differs".into(), Value::Bool(differs));
    }
    let mut keyed = Vec::new();
    for row in list {
        let k = [row.get("path_name"), row.get("name")].into_iter().flatten().find(|v| v.truthy());
        let key = match k {
            None => String::new(),
            Some(Value::Str(s)) => s.clone(),
            Some(_) => return Err(fwd("a sort key that is not a str")),
        };
        keyed.push((key, row));
    }
    keyed.sort_by(|a, b| a.0.cmp(&b.0));
    let mut meta = Map::new();
    meta.insert("version".into(), int(1));
    meta.insert("path".into(), s(path));
    meta.insert("suffix".into(), s(suffix));
    let fmt = [current, other].into_iter().flatten().find(|e| e.truthy()).and_then(|e| e.get("source_format")).cloned().unwrap_or(Value::Null);
    meta.insert("source_format".into(), fmt);
    meta.insert("has_ours".into(), Value::Bool(current.is_some_and(Value::truthy)));
    meta.insert("has_theirs".into(), Value::Bool(other.is_some_and(Value::truthy)));
    meta.insert("layers".into(), Value::Array(keyed.into_iter().map(|(_, r)| Value::Object(r)).collect()));
    Ok(meta)
}

/// `materialize_conflict(path, current, other, layer_merge=, worktree_entry=)`:
/// the details.
fn materialize_conflict(
    ctx: &MergeCtx,
    path: &str,
    current: Option<&Value>,
    other: Option<&Value>,
    lm: &LayerMergeResult,
    worktree_entry: Option<&Value>,
) -> Result<Vec<Value>> {
    let repo = ctx.repo;
    let target = repo.conflict_dir_for(path);
    if target.exists() {
        fsio::rmtree(&target)?;
    }
    fsio::mkdir_p(&target)?;
    let suffix = match tmi_repo::worktree::py_suffix(path) {
        s if s.is_empty() => ".bin".to_string(),
        s => s,
    };
    let wanted: HashSet<&str> = lm.layer_conflicts.iter().map(|c| c.sid.as_str()).chain(lm.export_layers.iter().map(String::as_str)).collect();
    let mut per_side: Vec<(&str, Vec<Value>)> = Vec::new();
    for (label, entry) in [("OURS", current), ("THEIRS", other)] {
        let Some(entry) = entry.filter(|e| e.truthy()) else {
            per_side.push((label, Vec::new()));
            continue;
        };
        fsio::write_bytes(&target.join(format!("{label}{suffix}")), &repo.store.read_blob(&source_blob_ref(entry)?)?)?;
        let tree_ref = str_of(entry, "tree_ref")?.ok_or_else(|| fwd("entry without a tree_ref"))?;
        let mut layers = Vec::new();
        flatten_layers(repo, &tree_ref, "", &mut layers, 0)?;
        for layer in &layers {
            let blob = str_of(layer, "blob_ref")?.filter(|b| !b.is_empty());
            let sid = layer.get("stable_source_id").and_then(Value::as_str).unwrap_or("");
            let Some(blob) = blob else { continue };
            if !wanted.contains(sid) {
                continue;
            }
            let key_src = [layer.get("path_name"), layer.get("name"), layer.get("layer_id")].into_iter().flatten().find(|v| v.truthy());
            let name = match key_src {
                Some(Value::Str(v)) => safe_filename(v),
                _ => return Err(fwd("a layer key that is not a str")),
            };
            let png = pix::layer_blob_to_png(&repo.store.read_blob(&blob)?, &ctx.limits())?;
            fsio::write_bytes(&target.join(format!("{name}__{label}.png")), &png)?;
        }
        per_side.push((label, layers));
    }
    let mut meta = build_conflict_meta(path, &per_side, current, other, &suffix)?;
    let mut details: Vec<Value> = lm
        .layer_conflicts
        .iter()
        .map(|c| {
            obj([
                ("stable_source_id", s(c.sid.clone())),
                ("name", s(c.name.clone())),
                ("path_name", c.path_name.clone()),
                ("reason", s(c.reason.clone())),
            ])
        })
        .collect();
    if details.is_empty() {
        // a conflict of the whole file: one line per reason (a plugin format
        // without a writer has three, issue #47; every other case one)
        let fallback = [format!("{path}: conflict")];
        let reasons = if lm.conflicts.is_empty() { &fallback[..] } else { &lm.conflicts[..] };
        for reason in reasons {
            let tail = reason.split_once(": ").map_or(reason.as_str(), |x| x.1).to_string();
            details.push(obj([("stable_source_id", Value::Null), ("name", Value::Null), ("path_name", Value::Null), ("reason", s(tail))]));
        }
    }
    meta.insert("conflicts".into(), Value::Array(details.clone()));
    let in_worktree = worktree_entry.or(current);
    meta.insert("worktree_digest".into(), in_worktree.and_then(|e| e.get("source_digest")).cloned().unwrap_or(Value::Null));
    meta.insert("worktree_is_partial_merge".into(), Value::Bool(worktree_entry.is_some()));
    fsio::write_text(&target.join("conflict.json"), &json::dumps(&Value::Object(meta), &DumpOptions::INDENTED))?;
    Ok(details)
}

/// `merge_base(left, right)`.
fn merge_base(repo: &Repo, left: &str, right: &str) -> Result<Option<String>> {
    let left_anc = repo.ancestor_set(left)?;
    let mut queue = std::collections::VecDeque::from([right.to_string()]);
    let mut seen = HashSet::new();
    while let Some(cur) = queue.pop_front() {
        if seen.contains(&cur) {
            continue;
        }
        if left_anc.contains(&cur) {
            return Ok(Some(cur));
        }
        seen.insert(cur.clone());
        match repo.read_commit(&cur) {
            Ok(c) => {
                for p in tmi_repo::repo::parents_of(&c)? {
                    queue.push_back(p);
                }
            }
            Err(RepoError::Forward(w)) => return Err(RepoError::Forward(w)),
            Err(_) => {}
        }
    }
    Ok(None)
}

/// `repo.merge(other_rev, message)`. `before_hook` writes what Python
/// would already have written before a hook starts.
pub fn merge(
    ctx: &mut MergeCtx,
    other_rev: &str,
    message: Option<&str>,
    warnings: &mut Vec<String>,
    before_hook: &mut dyn FnMut(),
) -> Result<MergeOutcome> {
    merge_as(ctx, other_rev, message, None, warnings, before_hook)
}

/// `repo.merge(other_rev, message, operation=)`: `operation` names the
/// command in the backups of overwritten files (pull, stage 4b).
pub fn merge_as(
    ctx: &mut MergeCtx,
    other_rev: &str,
    message: Option<&str>,
    operation: Option<&str>,
    warnings: &mut Vec<String>,
    before_hook: &mut dyn FnMut(),
) -> Result<MergeOutcome> {
    let repo = ctx.repo;
    let backup_reason = operation.map_or_else(|| format!("merge {other_rev}"), str::to_string);
    if repo.pending_merge_head()?.is_some() {
        return Err(RepoError::runtime(
            "merge already in progress; resolve it with 'tmiki add' and 'tmiki commit' or use 'tmiki switch --discard'",
        ));
    }
    let current = repo.current_commit()?;
    let other = repo
        .resolve_rev(Some(other_rev), warnings)?
        .ok_or_else(|| RepoError::runtime(format!("unknown merge target: {other_rev}")))?;
    if current.as_deref() == Some(other.as_str()) {
        return Ok(MergeOutcome::UpToDate);
    }
    if let Some(c) = &current {
        if repo.is_ancestor(&other, c)? {
            return Ok(MergeOutcome::UpToDate);
        }
    }
    let message_text = match message.filter(|m| !m.is_empty()) {
        Some(m) => m.to_string(),
        None => format!("Merge {}", prefix_chars(&other, 12)),
    };
    let fast_forward = match &current {
        None => true,
        Some(c) => repo.is_ancestor(c, &other)?,
    };
    if fast_forward {
        repo.prepare_identity()?;
        repo.ensure_clean_tracked("merge")?;
        repo.ensure_no_untracked_overwrite(&repo.entries_for_checkout(&other)?, current.as_deref(), "merge")?;
        repo.set_current_commit(&other)?;
        if operation.is_none() {
            // (pull's fast-forward is tmi-transport's, without this phase)
            tmi_import::report(ctx.progress, || "updating working tree".into());
        }
        repo.restore_all(&other, current.as_deref(), &backup_reason)?;
        let reason = format!("merge ff: {other_rev}");
        repo.log_ref_move("HEAD", current.as_deref(), &other, &reason)?;
        let target = repo.head_target()?;
        if target.starts_with("refs/heads/") {
            repo.log_ref_move(&target, current.as_deref(), &other, &reason)?;
        }
        hooks::run(&repo.repo_dir, "post-merge", &[("TMI_COMMIT", &other), ("TMI_MERGED_FROM", &other)], false, before_hook)?;
        return Ok(MergeOutcome::FastForward(other));
    }
    let current = current.unwrap();
    tmi_repo::write::object_timestamp()?; // a form Python may read differently goes now
    repo.prepare_identity()?;
    repo.ensure_clean_tracked("merge")?;
    let base = merge_base(repo, &current, &other)?;
    let base_entries = match &base {
        Some(b) => repo.entries_for_commit(Some(b))?,
        None => Entries::new(),
    };
    let current_entries = repo.entries_for_commit(Some(&current))?;
    let other_entries = repo.entries_for_checkout(&other)?;
    let mut paths: Vec<&String> = base_entries.keys().chain(current_entries.keys()).chain(other_entries.keys()).collect();
    paths.sort();
    paths.dedup();
    // The plugins of the merge result (PLUGINS.md 4.5): `.tmiattributes` and
    // `.tmiplugins/` as they merge cleanly, ours where they conflict (and
    // then no plugin format is merged layer by layer: Q7).
    let mut setup = Entries::new();
    for path in paths.iter().filter(|p| tmi_repo::attributes::is_plugin_setup_path(p)) {
        let (b, c, o) = (base_entries.get(*path), current_entries.get(*path), other_entries.get(*path));
        let side = if same_entry(c, o) || same_entry(b, o) {
            c
        } else if same_entry(b, c) {
            o
        } else {
            ctx.plugin_setup_conflict = true;
            c
        };
        if let Some(e) = side {
            setup.insert((*path).clone(), e.clone());
        }
    }
    if !setup.is_empty() || base_entries.keys().chain(current_entries.keys()).chain(other_entries.keys()).any(|p| tmi_repo::attributes::is_plugin_setup_path(p)) {
        ctx.import.formats = tmi_repo::formats::Formats::from_entries(repo, &setup)?;
    }
    let mut merged = Entries::new();
    let mut conflicts: Vec<String> = Vec::new();
    let mut conflict_sides: Vec<(String, Option<Value>, Option<Value>, LayerMergeResult)> = Vec::new();
    let mut conflict_worktree: HashMap<String, Value> = HashMap::new();
    for path in paths {
        let (b, c, o) = (base_entries.get(path), current_entries.get(path), other_entries.get(path));
        if same_entry(c, o) {
            if let Some(c) = c {
                merged.insert(path.clone(), c.clone());
            }
        } else if same_entry(b, c) {
            if let Some(o) = o {
                merged.insert(path.clone(), o.clone());
            }
        } else if same_entry(b, o) {
            if let Some(c) = c {
                merged.insert(path.clone(), c.clone());
            }
        } else {
            tmi_import::report(ctx.progress, || format!("merging layers of {path}"));
            let lm = merge_entries(ctx, path, b, c, o)?;
            if let Some(e) = lm.entry.clone() {
                merged.insert(path.clone(), e);
            } else {
                conflicts.push(path.clone());
                if let Some(c) = c {
                    merged.insert(path.clone(), c.clone());
                }
                if let Some(p) = lm.partial_entry.clone() {
                    conflict_worktree.insert(path.clone(), p);
                }
                conflict_sides.push((path.clone(), c.cloned(), o.cloned(), lm));
            }
        }
    }
    if !conflicts.is_empty() {
        let conflict_set: HashSet<&String> = conflicts.iter().collect();
        repo.ensure_no_untracked_overwrite(&merged, Some(&current), "merge")?;
        let mut details = Vec::new();
        for (path, c, o, lm) in &conflict_sides {
            let wt = conflict_worktree.get(path);
            details.push(materialize_conflict(ctx, path, c.as_ref(), o.as_ref(), lm, wt)?);
            if let Some(wt) = wt {
                let out = repo.abspath(path)?;
                fsio::mkdir_parent(&out)?;
                fsio::write_bytes(&out, &repo.store.read_blob(&source_blob_ref(wt)?)?)?;
                repo.rewrote(&out);
            }
        }
        stage_merge_result(repo, &current_entries, &merged, &conflict_set)?;
        restore_merge_result(repo, &current_entries, &merged, &conflict_set)?;
        fsio::write_text(&repo.repo_dir.join("MERGE_HEAD"), &other)?;
        fsio::write_text(&repo.repo_dir.join("MERGE_MSG"), &message_text)?;
        let dir = repo.repo_dir.join("conflicts").display().to_string();
        return Ok(MergeOutcome::Conflict(conflicts, details, dir));
    }
    repo.ensure_no_untracked_overwrite(&merged, Some(&current), "merge")?;
    tmi_repo::formats::refuse_dev_records(&merged)?;
    let tree_ref = repo.write_repository_tree(&merged)?;
    let (name, email) = repo.author()?;
    let first = merged.values().next();
    let commit = obj([
        ("version", int(1)),
        ("tree_ref", s(tree_ref)),
        ("parents", Value::Array(vec![s(current.clone()), s(other.clone())])),
        ("author", obj([("name", s(name)), ("email", s(email))])),
        ("timestamp", s(tmi_repo::write::object_timestamp()?)),
        ("message", s(message_text.clone())),
        ("thumbnail_ref", first.and_then(|e| e.get("thumbnail_ref")).cloned().unwrap_or(Value::Null)),
        ("canvas_meta", match first {
            Some(e) => e.get("canvas_meta").cloned().unwrap_or(Value::Null),
            None => Value::Object(Map::new()),
        }),
    ]);
    fsio::mark_mutated();
    let oid = repo.store.write_json("commit", &commit)?;
    repo.set_current_commit(&oid)?;
    tmi_import::report(ctx.progress, || "updating working tree".into());
    repo.restore_all(&oid, Some(&current), &backup_reason)?;
    repo.clear_pending_merge()?;
    repo.clear_all_conflict_files()?;
    let reason = format!("merge: {}", tmi_repo::py::first_line(&message_text));
    repo.log_ref_move("HEAD", Some(&current), &oid, &reason)?;
    let target = repo.head_target()?;
    if target.starts_with("refs/heads/") {
        repo.log_ref_move(&target, Some(&current), &oid, &reason)?;
    }
    hooks::run(&repo.repo_dir, "post-merge", &[("TMI_COMMIT", &oid), ("TMI_MERGED_FROM", &other)], false, before_hook)?;
    Ok(MergeOutcome::Merged(oid))
}

/// `_stage_merge_result(current_entries, merged_entries, conflict_paths)`.
fn stage_merge_result(repo: &Repo, current: &Entries, merged: &Entries, conflicts: &HashSet<&String>) -> Result<()> {
    let mut paths: Vec<&String> = current.keys().chain(merged.keys()).collect();
    paths.sort();
    paths.dedup();
    let mut staged = Entries::new();
    for path in paths {
        if conflicts.contains(path) {
            continue;
        }
        let (before, after) = (current.get(path), merged.get(path));
        if same_entry(before, after) {
            continue;
        }
        staged.insert(path.clone(), after.cloned().unwrap_or_else(|| deleted_entry(path)));
    }
    repo.write_index_entries(staged)
}

/// `_restore_merge_result(current_entries, merged_entries, conflict_paths)`.
fn restore_merge_result(repo: &Repo, current: &Entries, merged: &Entries, conflicts: &HashSet<&String>) -> Result<()> {
    let mut paths: Vec<&String> = current.keys().chain(merged.keys()).collect();
    paths.sort();
    paths.dedup();
    paths.reverse();
    for path in paths {
        if conflicts.contains(path) {
            continue;
        }
        let (before, after) = (current.get(path), merged.get(path));
        if same_entry(before, after) {
            continue;
        }
        let out = repo.abspath(path)?;
        match after {
            None => {
                if out.exists() && out.is_file() {
                    fsio::unlink(&out)?;
                    if let Some(parent) = out.parent() {
                        repo.remove_empty_parent_dirs(parent)?;
                    }
                }
            }
            Some(a) => {
                fsio::mkdir_parent(&out)?;
                fsio::write_bytes(&out, &repo.store.read_blob(&source_blob_ref(a)?)?)?;
                repo.rewrote(&out);
            }
        }
    }
    Ok(())
}

/// `repo.merge_abort()`: the paths rewritten.
pub fn merge_abort(repo: &Repo) -> Result<Vec<String>> {
    if repo.pending_merge_head()?.is_none() {
        return Err(RepoError::runtime("no merge in progress"));
    }
    repo.prepare_identity()?;
    let head = repo.entries_for_commit(repo.current_commit()?.as_deref())?;
    let mut candidates: Vec<String> = head.keys().cloned().collect();
    candidates.extend(repo.index_entries()?.keys().cloned());
    candidates.extend(repo.pending_conflict_paths()?);
    candidates.extend(repo.resolved_conflict_paths()?);
    candidates.sort();
    candidates.dedup();
    let mut rewritten = Vec::new();
    let mut failed: Vec<String> = Vec::new();
    for rel in &candidates {
        let entry = head.get(rel);
        let want = match entry {
            Some(e) => str_of(e, "source_digest")?,
            None => None,
        };
        let out = repo.abspath(rel)?;
        let digest = repo.backup_worktree_file(rel, "merge --abort", &[want.clone()], false)?;
        let Some(entry) = entry else {
            if out.is_file() {
                fsio::mark_mutated();
                if let Err(e) = std::fs::remove_file(&out) {
                    failed.push(format!("{rel} ({})", py_strerror(&e)));
                    continue;
                }
                if let Some(parent) = out.parent() {
                    repo.remove_empty_parent_dirs(parent)?;
                }
                rewritten.push(rel.clone());
            }
            continue;
        };
        if digest == want {
            continue;
        }
        fsio::mkdir_parent(&out)?;
        let data = repo.store.read_blob(&source_blob_ref(entry)?)?;
        fsio::mark_mutated();
        // except OSError as exc: failed.append(f"{rel} ({exc.strerror or exc})")
        if let Err(e) = std::fs::write(&out, &data) {
            failed.push(format!("{rel} ({})", py_strerror(&e)));
            continue;
        }
        repo.rewrote(&out);
        rewritten.push(rel.clone());
        if Some(fsio::sha256_file(&out)?) != want {
            failed.push(format!("{rel} (content does not match HEAD after writing)"));
        }
    }
    if !failed.is_empty() {
        let mut summary = failed.iter().take(5).cloned().collect::<Vec<_>>().join(", ");
        if failed.len() > 5 {
            summary += &format!(", +{} more", failed.len() - 5);
        }
        return Err(RepoError::runtime(format!(
            "merge --abort could not put {} file(s) back to HEAD: {summary}; the merge is still in progress. Close the file(s) in your drawing app and run 'tmiki merge --abort' again (overwritten work is in 'tmiki recover --files')",
            failed.len()
        )));
    }
    repo.write_index_entries(Entries::new())?;
    repo.clear_pending_merge()?;
    repo.clear_all_conflict_files()?;
    Ok(rewritten)
}

/// `OSError.strerror` of a failed `open()` / `unlink()`: the C runtime's
/// text for the errno the OS error maps to.
fn py_strerror(e: &std::io::Error) -> String {
    #[cfg(windows)]
    let mapped = match e.raw_os_error() {
        Some(5 | 19 | 32 | 33 | 65) => Some("Permission denied"),
        Some(2 | 3 | 15 | 161) => Some("No such file or directory"),
        Some(39 | 112) => Some("No space left on device"),
        _ => None,
    };
    #[cfg(not(windows))]
    let mapped = match e.kind() {
        std::io::ErrorKind::PermissionDenied => Some("Permission denied"),
        std::io::ErrorKind::NotFound => Some("No such file or directory"),
        _ => None,
    };
    mapped.map_or_else(|| e.to_string(), str::to_string)
}

pub fn new_ctx(repo: &Repo) -> Result<MergeCtx<'_>> {
    Ok(MergeCtx { repo, import: ImportContext::for_repo(repo)?, warns: Warnings::default(), progress: None, plugin_setup_conflict: false })
}

/// The pixels of a stored blob (for callers that compare layers).
pub fn load(repo: &Repo, oid: &str, limits: &Limits) -> Result<Px> {
    pix::pixels_from_bytes(&repo.store.read_blob(oid)?, limits)
}
