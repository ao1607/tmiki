//! tmi/diff.py `diff_sides`: which files and layers differ between two
//! sides (a commit, the index, the working tree), their changed rectangles,
//! and the preview images of each mode (side by side, overlay, per-layer,
//! the HTML report's WebP previews). The files written and the results are
//! Python's, byte for byte.

use crate::pix::{self, fwd, Px, Rect};
use crate::prof;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tmi_import::{CompositeBase, ImportedFile, LayerBlobs, Limits, Options, Rgba, Warnings};
use tmi_repo::layer_changes::{self, PropChange, Props};
use tmi_repo::pyeq::py_eq;
use tmi_repo::repo::Entries;
use tmi_repo::stage::{self, BlobReuse, ImportContext, StatCache};
use tmi_repo::write::{is_deleted, safe_filename};
use tmi_repo::{Repo, RepoError, Result};
use tmi_store::json::Value;

pub const HTML_COMPOSITE_EDGE: usize = 2048;
pub const HTML_LAYER_EDGE: usize = 1024;
pub const HTML_PREVIEW_QUALITY: f32 = 82.0;
pub const HTML_PREVIEW_METHOD: i32 = 2;
const RASTER_LAYER_ID: &str = "image:0";

/// `DiffSide`: a commit (`Some(oid)`), the index or the working tree.
#[derive(Clone, Debug)]
pub enum Side {
    Commit(String),
    Index,
    Worktree,
}

#[derive(Clone, Debug)]
pub struct DiffSide {
    pub side: Side,
    pub label: String,
}

impl DiffSide {
    pub fn worktree() -> DiffSide {
        DiffSide { side: Side::Worktree, label: "working tree".into() }
    }

    pub fn index() -> DiffSide {
        DiffSide { side: Side::Index, label: "index".into() }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Names,
    Summary,
    Html,
    Overlay,
    PerLayer,
    SideBySide,
}

pub struct CompositeHtml {
    pub old_png: Vec<u8>,
    pub new_png: Vec<u8>,
    pub side_by_side_png: Vec<u8>,
    pub overlay_png: Vec<u8>,
    pub diff_preview_png: Vec<u8>,
    pub size: (usize, usize),
    pub full_old: String,
    pub full_new: String,
}

pub struct LayerResult {
    pub name: String,
    pub state: &'static str,
    pub prop_changes: Vec<PropChange>,
    pub bbox: Option<Rect>,
    /// (side_by_side_png, overlay_png) of the HTML report.
    pub html: Option<(Vec<u8>, Vec<u8>)>,
}

pub struct FileResult {
    pub path: String,
    pub state: &'static str,
    pub binary: bool,
    pub raster: bool,
    pub layers: Vec<LayerResult>,
    pub outputs: Vec<String>,
    pub bbox: Option<Rect>,
    pub html: Option<CompositeHtml>,
    /// What a format plugin adds (PLUGINS.md 4.4, 4.5): "importer changed
    /// ora 0.1.0 -> 0.2.0", or why the working file is compared by its bytes
    /// only ("layer details need plugin 'ora' ...").
    pub notes: Vec<String>,
}

impl FileResult {
    fn new(path: &str, state: &'static str) -> FileResult {
        FileResult { path: path.into(), state, binary: false, raster: false, layers: Vec::new(), outputs: Vec::new(), bbox: None, html: None, notes: Vec::new() }
    }
}

/// An entry's `importer` as "id version" (None: a built-in's record).
fn importer_text(importer: Option<&Value>) -> Option<String> {
    let i = importer.filter(|v| v.truthy())?;
    let field = |k: &str| i.get(k).and_then(Value::as_str).unwrap_or("?").to_string();
    Some(format!("{} {}", field("id"), field("version")))
}

/// "importer changed ..." when the two records were made by different
/// plugins (or one by a plugin, one by tmiki itself).
fn importer_note(old: Option<&Value>, new: Option<&Value>) -> Option<String> {
    let (old, new) = (old.filter(|v| v.truthy()), new.filter(|v| v.truthy()));
    let sha = |v: Option<&Value>| v.and_then(|i| i.get("component_sha256")).and_then(Value::as_str).map(str::to_string);
    if sha(old) == sha(new) {
        return None;
    }
    let name = |t: Option<String>| t.unwrap_or_else(|| "built-in".into());
    Some(format!("importer changed {} -> {}", name(importer_text(old)), name(importer_text(new))))
}

/// Where previews go: the directory as Python's `Path` prints it.
pub struct OutDir {
    pub path: PathBuf,
    pub text: String,
}

impl OutDir {
    /// `str(out_dir / name)`.
    pub fn join_text(&self, name: &str) -> String {
        let sep = if cfg!(windows) { '\\' } else { '/' };
        if self.text == "." {
            name.to_string()
        } else if self.text.ends_with(['\\', '/']) {
            format!("{}{name}", self.text)
        } else {
            format!("{}{sep}{name}", self.text)
        }
    }

    fn mkdir(&self) -> Result<()> {
        std::fs::create_dir_all(&self.path).map_err(|e| RepoError::runtime(format!("{}: {e}", self.path.display())))
    }

    fn write(&self, name: &str, data: &[u8]) -> Result<String> {
        let target = self.path.join(name);
        std::fs::write(&target, data).map_err(|e| RepoError::runtime(format!("{}: {e}", target.display())))?;
        Ok(self.join_text(name))
    }
}

/// One side's layer (`_LayerView`).
struct LayerView<'a> {
    name: String,
    pixel: Value,
    props: Props,
    load: Option<Load<'a>>,
    metadata: Option<(Value, Value)>,
}

enum Load<'a> {
    Stored(String),
    Bytes(&'a [u8]),
}

struct Ctx<'a> {
    repo: &'a Repo,
    limits: Limits,
}

impl Ctx<'_> {
    fn load(&self, load: &Load) -> Result<Px> {
        match load {
            Load::Stored(oid) => pix::pixels_from_bytes(&self.repo.store.read_blob(oid)?, &self.limits),
            Load::Bytes(b) => pix::pixels_from_bytes(b, &self.limits),
        }
    }
}

/// `_side_entries(repo, side)`.
pub fn side_entries(repo: &Repo, side: &DiffSide) -> Result<Entries> {
    match &side.side {
        Side::Commit(oid) => repo.entries_for_commit(Some(oid)),
        Side::Index | Side::Worktree => {
            let mut entries = repo.entries_for_commit(repo.current_commit()?.as_deref())?;
            for (path, entry) in repo.index_entries()? {
                if is_deleted(&entry) {
                    entries.shift_remove(&path);
                } else {
                    entries.insert(path, entry);
                }
            }
            Ok(entries)
        }
    }
}

fn get<'a>(v: &'a Value, key: &str) -> &'a Value {
    static NULL: Value = Value::Null;
    v.get(key).unwrap_or(&NULL)
}

fn str_field(v: &Value, key: &str) -> Result<Option<String>> {
    match v.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Str(s)) => Ok(Some(s.clone())),
        Some(_) => Err(fwd(format!("{key} is not a str"))),
    }
}

/// `_semantic_key(source_meta)`.
fn semantic_key(meta: &Value) -> Option<(Value, Value)> {
    let digest = get(meta, "semantic_digest");
    if digest.is_null() {
        return None;
    }
    Some((get(meta, "semantic_digest_version").clone(), digest.clone()))
}

/// `_entry_layers(repo, entry)`.
fn entry_layers<'a>(repo: &Repo, entry: &Value) -> Result<(Vec<String>, HashMap<String, LayerView<'a>>)> {
    let tree_ref = str_field(entry, "tree_ref")?.ok_or_else(|| fwd("entry without a tree_ref"))?;
    let mut walked = Vec::new();
    layer_changes::walk_tree(repo, &tree_ref, (Value::Null, String::new()), &mut walked, 0)?;
    let mut order = Vec::new();
    let mut views = HashMap::new();
    for (layer, parent) in walked {
        let sid = match layer.get("stable_source_id") {
            Some(Value::Str(s)) => s.clone(),
            _ => return Err(fwd("stable_source_id is not a str")),
        };
        let blob = str_field(&layer, "blob_ref")?.filter(|b| !b.is_empty());
        let pixel = get(&layer, "pixel_sha256").clone();
        if pixel.is_null() && blob.is_some() {
            return Err(fwd("legacy layer without a pixel digest: Python decodes its blob"));
        }
        let name = match layer.get("name") {
            Some(v) if v.truthy() => v.py_str().ok_or_else(|| fwd("layer name is not printable"))?,
            _ => sid.clone(),
        };
        let view = LayerView {
            name,
            pixel,
            props: layer_changes::stored_props(&layer, &parent)?,
            load: blob.map(Load::Stored),
            metadata: semantic_key(get(&layer, "source_meta")),
        };
        if !views.contains_key(&sid) {
            order.push(sid.clone());
        }
        views.insert(sid, view);
    }
    Ok((order, views))
}

/// `_imported_layers(imported, repo, blob_reuse)`.
fn imported_layers<'a>(imported: &'a ImportedFile, reuse: &HashMap<String, BlobReuse>) -> Result<(Vec<String>, HashMap<String, LayerView<'a>>)> {
    let mut walked = Vec::new();
    layer_changes::walk_imported(&imported.layers, (Value::Null, String::new()), &mut walked);
    let mut order = Vec::new();
    let mut views = HashMap::new();
    for (layer, parent) in walked {
        let mut load = layer.blob_bytes.as_deref().map(Load::Bytes);
        if load.is_none() {
            if let (Some(r), Some(p)) = (reuse.get(&layer.stable_source_id), &layer.pixel_sha256) {
                if py_eq(&r.pixel_sha256, p) {
                    load = Some(Load::Stored(r.blob_ref.as_str().ok_or_else(|| fwd("blob_ref is not a str"))?.to_string()));
                }
            }
        }
        let view = LayerView {
            name: layer.name.clone(),
            pixel: layer.pixel_sha256.clone().unwrap_or(Value::Null),
            props: layer_changes::imported_props(layer, &parent),
            load,
            metadata: semantic_key(&Value::Object(layer.source_meta.clone())),
        };
        if !views.contains_key(&layer.stable_source_id) {
            order.push(layer.stable_source_id.clone());
        }
        views.insert(layer.stable_source_id.clone(), view);
    }
    Ok((order, views))
}

/// `_working_file_entries(repo, rel, left)`.
fn working_file_entries(repo: &Repo, rel: &str, left: Option<&Value>, head: &Entries, index: &Entries) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for entry in [left, head.get(rel), index.get(rel)].into_iter().flatten() {
        if entry.truthy() && !is_deleted(entry) && !out.iter().any(|e| py_eq(e, entry)) {
            out.push(entry.clone());
        }
    }
    let _ = repo;
    out
}

/// clip_partial `CompositeBase` over two stored blobs.
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

/// `repo._composite_base(entry)` as the two refs.
pub fn composite_base_refs(entry: Option<&Value>) -> Option<(String, String)> {
    let entry = entry?;
    if is_deleted(entry) || entry.get("source_format").and_then(Value::as_str) != Some("clip") {
        return None;
    }
    let s = entry.get("source_blob_ref").filter(|v| v.truthy())?.as_str()?.to_string();
    let c = entry.get("composite_ref").filter(|v| v.truthy())?.as_str()?.to_string();
    Some((s, c))
}

/// `Path(rel).as_posix()` through `safe_filename`: the whole path, extension
/// included, so art.clip and its export art.png keep their own preview files
/// (issue #38).
fn safe_rel_stem(rel: &str) -> String {
    safe_filename(rel)
}

fn is_png(data: Option<&[u8]>) -> bool {
    data.is_some_and(|d| d.starts_with(pix::PNG_SIGNATURE))
}

/// `_fit(size, edge)`.
pub fn fit(size: (usize, usize), edge: usize) -> (usize, usize) {
    let scale = edge as f64 / size.0.max(size.1).max(1) as f64;
    if scale >= 1.0 {
        return size;
    }
    let r = |v: usize| ((v as f64 * scale).round_ties_even() as usize).max(1);
    (r(size.0), r(size.1))
}

/// `_shrink(image, edge)`.
fn shrink(img: &Rgba, edge: usize) -> Result<Rgba> {
    pix::resize(img, fit((img.width, img.height), edge), None)
}

/// `_resize_all(jobs)` (side by side on the rayon pool).
fn resize_all(jobs: Vec<(&Rgba, (usize, usize), Option<[f64; 4]>)>) -> Result<Vec<Rgba>> {
    use rayon::prelude::*;
    jobs.into_par_iter().map(|(img, size, bx)| pix::resize(img, size, bx)).collect()
}

fn webp(img: &Rgba) -> Result<Vec<u8>> {
    pix::webp(img, HTML_PREVIEW_QUALITY, HTML_PREVIEW_METHOD)
}

/// `_encode_previews(images)`.
fn encode_previews(images: Vec<&Rgba>) -> Result<Vec<Vec<u8>>> {
    use rayon::prelude::*;
    images.into_par_iter().map(webp).collect()
}

/// `_composite_preview_images(old, new, bbox)`: old, new, side by side,
/// overlay, diff preview.
fn composite_preview_images(old: &Rgba, new: &Rgba, bbox: Option<Rect>) -> Result<[Rgba; 5]> {
    let edge = HTML_COMPOSITE_EDGE;
    let (pold, pnew) = (Px::Dense(old.clone()), Px::Dense(new.clone()));
    if (old.width, old.height) != (new.width, new.height) {
        return Ok([
            shrink(old, edge)?,
            shrink(new, edge)?,
            shrink(&pix::side_by_side(&pold, &pnew), edge)?,
            shrink(&pix::overlay(&pold, &pnew), edge)?,
            shrink(&pix::diff_preview(&pold, &pnew, Some(bbox)), edge)?,
        ]);
    }
    let (width, height) = (new.width, new.height);
    let small = fit((width, height), edge);
    let mut jobs: Vec<(&Rgba, (usize, usize), Option<[f64; 4]>)> = vec![(old, small, None), (new, small, None)];
    let gutter = 16usize;
    let pair = (2 * width + gutter, height);
    let mut pair_size = Some(fit(pair, edge));
    let (mut step, mut left_w, mut right_x) = (0.0f64, 0i64, 0i64);
    if pair_size != Some(pair) {
        let ps = pair_size.unwrap();
        step = pair.0 as f64 / ps.0 as f64;
        left_w = (width as f64 / step).floor() as i64;
        right_x = ((width + gutter) as f64 / step).ceil() as i64;
        let right_w = ps.0 as i64 - right_x;
        if left_w < 1 || right_w < 1 {
            pair_size = None;
        } else {
            let lb = (width as f64).min(left_w as f64 * step);
            jobs.push((old, (left_w as usize, ps.1), Some([0.0, 0.0, lb, height as f64])));
            let rb = (right_x as f64 * step - width as f64 - gutter as f64).max(0.0);
            jobs.push((new, (right_w as usize, ps.1), Some([rb, 0.0, width as f64, height as f64])));
        }
    }
    let mut resized = prof::time("composite: resample (parallel)", || resize_all(jobs))?.into_iter();
    let small_old = resized.next().unwrap();
    let small_new = resized.next().unwrap();
    let halves: Vec<Rgba> = resized.collect();
    let pair_image = match pair_size {
        None => shrink(&pix::side_by_side(&pold, &pnew), edge)?,
        Some(ps) if ps == pair => pix::side_by_side(&Px::Dense(small_old.clone()), &Px::Dense(small_new.clone())),
        Some(ps) => {
            let (w, g) = (width as i64, gutter as i64);
            let c0 = (left_w - 3).max(0);
            let c1 = (ps.0 as i64).min(right_x + 3);
            let x0 = (((c0 - 4) as f64 * step).floor() as i64).max(0);
            let x1 = (pair.0 as i64).min(((c1 + 4) as f64 * step).ceil() as i64);
            let mut crops: Vec<(Rgba, i64)> = Vec::new();
            if x0 < w {
                crops.push((pix::crop(old, (x0, 0, w, height as i64)).into_owned(), 0));
            }
            if x1 > w + g {
                let at = (w + g - x0).max(0);
                crops.push((pix::crop(new, (x0 + at - w - g, 0, x1 - w - g, height as i64)).into_owned(), at));
            }
            let ops: Vec<pix::Op> = crops.iter().map(|(c, at)| pix::Op::Over(c, *at, 0)).collect();
            let strip = pix::compose((x1 - x0) as usize, height, &ops, [32, 32, 32, 255], None);
            let bx = [c0 as f64 * step - x0 as f64, 0.0, ((x1 - x0) as f64).min(c1 as f64 * step - x0 as f64), height as f64];
            let strip_small = prof::time("composite: gutter strip", || pix::resize(&strip, ((c1 - c0) as usize, ps.1), Some(bx)))?;
            pix::compose(
                ps.0,
                ps.1,
                &[pix::Op::Over(&halves[0], 0, 0), pix::Op::Over(&halves[1], right_x, 0), pix::Op::Paste(&strip_small, c0, 0)],
                [32, 32, 32, 255],
                None,
            )
        }
    };
    let outline = bbox.map(|b| {
        let sx = small_new.width as f64 / width as f64;
        let sy = small_new.height as f64 / height as f64;
        (
            (b.0 as f64 * sx).floor() as i64,
            (b.1 as f64 * sy).floor() as i64,
            (b.2 as f64 * sx).ceil() as i64,
            (b.3 as f64 * sy).ceil() as i64,
        )
    });
    let (so, sn) = (Px::Dense(small_old), Px::Dense(small_new));
    let overlay = pix::overlay(&so, &sn);
    let preview = pix::diff_preview(&so, &sn, Some(outline));
    let (Px::Dense(small_old), Px::Dense(small_new)) = (so, sn) else { unreachable!() };
    Ok([small_old, small_new, pair_image, overlay, preview])
}

pub struct DiffRun<'a> {
    pub repo: &'a Repo,
    pub mode: Mode,
    pub out: &'a OutDir,
    pub warns: Warnings,
    /// diff.py's `on_progress`.
    pub progress: Option<tmi_import::Progress<'a>>,
}

/// The layers' "comparing layers" messages wait until the composite's
/// preview phase has been said: Python compares the composites first and
/// the layers after, while here they run side by side.
struct Gate<'a> {
    progress: Option<tmi_import::Progress<'a>>,
    state: std::sync::Mutex<(bool, Vec<String>)>,
}

impl<'a> Gate<'a> {
    fn new(progress: Option<tmi_import::Progress<'a>>, open: bool) -> Gate<'a> {
        Gate { progress, state: std::sync::Mutex::new((open, Vec::new())) }
    }

    fn report(&self, msg: String) {
        let Some(p) = self.progress else { return };
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if s.0 {
            p(&msg);
        } else {
            s.1.push(msg);
        }
    }

    /// Say `first` (if any), then what waited.
    fn open(&self, first: Option<String>) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if s.0 {
            return;
        }
        if let Some(p) = self.progress {
            if let Some(m) = first {
                p(&m);
            }
            for m in s.1.drain(..) {
                p(&m);
            }
        }
        s.0 = true;
    }
}

/// What comparing the composites adds to a file's result.
#[derive(Default)]
struct CompositeOut {
    bbox: Option<Rect>,
    html: Option<CompositeHtml>,
    output: Option<String>,
}

/// The composites' bbox and `_emit_composite(...)`; `on_bbox(found)` once
/// the bbox is known.
fn compare_composites(
    run: &DiffRun,
    path: &str,
    old: &Rgba,
    new: &Rgba,
    old_png: Option<&[u8]>,
    new_png: Option<&[u8]>,
    on_bbox: &dyn Fn(bool),
) -> Result<CompositeOut> {
    let bbox = prof::time("composite: bbox", || pix::diff_bbox_rgba(old, new));
    on_bbox(bbox.is_some());
    let Some(bbox) = bbox else { return Ok(CompositeOut::default()) };
    let out = run.out;
    out.mkdir()?;
    let stem = safe_rel_stem(path);
    if run.mode == Mode::Html {
        let (old_name, new_name) = (format!("{stem}-old.png"), format!("{stem}-new.png"));
        prof::time("composite: write full PNGs", || -> Result<()> {
            let old_bytes = if is_png(old_png) { old_png.unwrap().to_vec() } else { pix::png(old)? };
            out.write(&old_name, &old_bytes)?;
            let new_bytes = if is_png(new_png) { new_png.unwrap().to_vec() } else { pix::png(new)? };
            out.write(&new_name, &new_bytes)?;
            Ok(())
        })?;
        let images = prof::time("composite: preview images", || composite_preview_images(old, new, Some(bbox)))?;
        let mut enc = prof::time("composite: webp x5", || encode_previews(images.iter().collect()))?.into_iter();
        let html = CompositeHtml {
            old_png: enc.next().unwrap(),
            new_png: enc.next().unwrap(),
            side_by_side_png: enc.next().unwrap(),
            overlay_png: enc.next().unwrap(),
            diff_preview_png: enc.next().unwrap(),
            size: (old.width.max(new.width), old.height.max(new.height)),
            full_old: old_name,
            full_new: new_name,
        };
        return Ok(CompositeOut { bbox: Some(bbox), html: Some(html), output: None });
    }
    let (image, name) = match run.mode {
        Mode::Overlay => (pix::overlay_rgba(old, new), format!("{stem}-overlay.png")),
        Mode::PerLayer => (pix::diff_preview_rgba(old, new, Some(Some(bbox))), format!("{stem}-composite-diff.png")),
        _ => (pix::side_by_side_rgba(old, new), format!("{stem}-side-by-side.png")),
    };
    let png = prof::time("composite: png encode", || pix::png(&image))?;
    let text = out.write(&name, &png)?;
    Ok(CompositeOut { bbox: Some(bbox), html: None, output: Some(text) })
}

/// What one layer's pixels add to its result.
#[derive(Default)]
struct LayerOut {
    bbox: Option<Rect>,
    html: Option<(Vec<u8>, Vec<u8>)>,
    /// (file name, PNG): written in the layers' order once the chunk is
    /// done, so a name two layers share ends up as Python leaves it.
    file: Option<(String, Vec<u8>)>,
    /// What wrote prints for it.
    output: Option<String>,
}

/// The region of a layer the report previews: `bbox` padded by 16, within
/// the larger side (as `diff_bbox` takes it). When the canvas grew, the
/// change can lie wholly outside the smaller side.
fn html_layer_box(bbox: Rect, left: &Px, right: &Px) -> Rect {
    let pad = 16i64;
    (
        (bbox.0 - pad).max(0),
        (bbox.1 - pad).max(0),
        (left.width().max(right.width()) as i64).min(bbox.2 + pad),
        (left.height().max(right.height()) as i64).min(bbox.3 + pad),
    )
}

/// One changed layer's pixels: both sides loaded, their bbox and
/// `_emit_layer_preview(...)`.
fn layer_pixels(run: &DiffRun, pctx: &Ctx, path: &str, name: &str, raster: bool, before: Option<&LayerView>, after: Option<&LayerView>) -> Result<LayerOut> {
    let (left_img, right_img) = prof::time("layer: load blobs", || -> Result<_> {
        let left_img = match before.and_then(|b| b.load.as_ref()) {
            Some(l) => Some(pctx.load(l)?),
            None => None,
        };
        let right_img = match after.and_then(|a| a.load.as_ref()) {
            Some(l) => Some(pctx.load(l)?),
            None => None,
        };
        Ok((left_img, right_img))
    })?;
    if left_img.is_none() && right_img.is_none() {
        return Ok(LayerOut::default());
    }
    let left = left_img.unwrap_or_else(|| Px::Sparse(pix::Sparse::empty(right_img.as_ref().unwrap().size())));
    let right = right_img.unwrap_or_else(|| Px::Sparse(pix::Sparse::empty(left.size())));
    let bbox = prof::time("layer: bbox", || pix::diff_bbox(&left, &right));
    let Some(bbox) = bbox else { return Ok(LayerOut::default()) };
    if run.mode == Mode::Html {
        if raster {
            // the report shows a PNG / JPEG once, as the composite
            return Ok(LayerOut { bbox: Some(bbox), ..Default::default() });
        }
        let bx = html_layer_box(bbox, &left, &right);
        // region(bx) of each side, shrunk (_shrink_both): only the painted
        // pixels inside bx are resampled, the region itself is never built.
        let size = fit(((bx.2 - bx.0) as usize, (bx.3 - bx.1) as usize), HTML_LAYER_EDGE);
        let (sl, sr) = prof::time("layer: resample", || {
            rayon::join(|| pix::resize_region(&left, bx, size), || pix::resize_region(&right, bx, size))
        });
        let (sl, sr) = (sl?, sr?);
        let (sbs, ovl) = prof::time("layer: compose + webp x2", || {
            rayon::join(|| webp(&pix::side_by_side_rgba(&sl, &sr)), || webp(&pix::overlay_rgba(&sl, &sr)))
        });
        return Ok(LayerOut { bbox: Some(bbox), html: Some((sbs?, ovl?)), ..Default::default() });
    }
    run.out.mkdir()?;
    // The full-size images, drawing only the sides' painted pixels.
    let (image, suffix) = prof::time("layer: full-size image", || match run.mode {
        Mode::Overlay => (pix::overlay_sparse(&left, &right), "overlay"),
        Mode::PerLayer => (pix::diff_preview_sparse(&left, &right, Some(bbox)), "layer-diff"),
        _ => (pix::side_by_side_sparse(&left, &right), "side-by-side"),
    });
    // An image's one layer is named after the file: "art.png-art-overlay"
    // says "art" twice, and "art.png-overlay" is the composite's (#38).
    let basename = path.rsplit('/').next().unwrap_or(path);
    let file = if name == tmi_import::path_stem(basename) {
        format!("{}-layer-{}.png", safe_rel_stem(path), suffix.strip_prefix("layer-").unwrap_or(suffix))
    } else {
        format!("{}-{}-{suffix}.png", safe_rel_stem(path), safe_filename(name))
    };
    let png = prof::time("layer: png encode", || pix::png(&image))?;
    Ok(LayerOut { bbox: Some(bbox), file: Some((file, png)), ..Default::default() })
}

/// How many changed layers are compared at once (each holds its two
/// sides' pixels; the resampling inside is parallel too).
const LAYERS_AT_ONCE: usize = 4;

/// `f` of each item, at most `n` at a time, each worker taking the next item
/// as it finishes one. Once an item fails no later one is started (the
/// earlier ones still run: the first failure in order is the one reported).
fn in_flight<T: Sync, R: Send>(items: &[T], n: usize, f: impl Fn(&T) -> Result<R> + Sync) -> Vec<Result<R>> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    let next = AtomicUsize::new(0);
    let stop = AtomicUsize::new(usize::MAX);
    let slots: Vec<Mutex<Option<Result<R>>>> = items.iter().map(|_| Mutex::new(None)).collect();
    rayon::scope(|s| {
        for _ in 0..n.min(items.len()) {
            s.spawn(|_| loop {
                let i = next.fetch_add(1, Ordering::SeqCst);
                if i >= items.len() || i > stop.load(Ordering::SeqCst) {
                    break;
                }
                let r = f(&items[i]);
                if r.is_err() {
                    stop.fetch_min(i, Ordering::SeqCst);
                }
                *slots[i].lock().unwrap() = Some(r);
            });
        }
    });
    let mut out = Vec::with_capacity(items.len());
    for slot in slots {
        match slot.into_inner().unwrap() {
            Some(r) => {
                let failed = r.is_err();
                out.push(r);
                if failed {
                    break;
                }
            }
            None => break,
        }
    }
    out
}

/// `diff_sides(repo, left, right, path_filters, mode, out_dir)`.
pub fn diff_sides(run: &mut DiffRun, left: &DiffSide, right: &DiffSide, path_filters: &[String]) -> Result<Vec<FileResult>> {
    let repo = run.repo;
    let left_entries = side_entries(repo, left)?;
    let right_entries = side_entries(repo, right)?;
    let mut paths: Vec<String> = left_entries.keys().chain(right_entries.keys()).cloned().collect();
    paths.sort();
    paths.dedup();
    if !path_filters.is_empty() {
        let known: std::collections::HashSet<String> = paths.iter().cloned().collect();
        let mut wanted = std::collections::HashSet::new();
        for spec in path_filters {
            wanted.insert(repo.pathspec_relpath(spec, &|p| known.contains(p))?);
        }
        let mut missing: Vec<&String> = wanted.iter().filter(|w| !known.contains(*w)).collect();
        missing.sort();
        if let Some(m) = missing.first() {
            return Err(RepoError::runtime(format!("path is not tracked in {} or {}: {m}", left.label, right.label)));
        }
        paths.retain(|p| wanted.contains(p));
    }
    let names_only = run.mode == Mode::Names;
    let summary = matches!(run.mode, Mode::Summary | Mode::Names);
    let worktree = matches!(right.side, Side::Worktree);
    let mut stat_cache = if worktree { Some(StatCache::load(repo)?) } else { None };
    let ctx = ImportContext::for_repo(repo)?;
    let pctx = Ctx { repo, limits: ctx.limits };
    let head_entries = repo.entries_for_commit(repo.current_commit()?.as_deref())?;
    let index_raw = tmi_repo::write::entries_of(&repo.read_index()?)?;
    let mut results = Vec::new();

    for rel in &paths {
        let old = left_entries.get(rel);
        let new = right_entries.get(rel);
        let full = if worktree { Some(repo.abspath(rel)?) } else { None };
        let mut source: Option<(Vec<u8>, String)> = None;
        if let Some(full) = &full {
            if !full.is_file() {
                if old.is_some() {
                    results.push(FileResult::new(rel, "deleted"));
                }
                continue;
            }
            if let Some(o) = old {
                let meta = std::fs::metadata(full).map_err(|e| fwd(e.to_string()))?;
                let (modified, src) = stat_cache.as_mut().unwrap().changed(rel, full, o.get("source_digest"), &meta, !names_only)?;
                if !modified {
                    continue;
                }
                source = src;
            }
        } else if let (Some(o), Some(n)) = (old, new) {
            // the same bytes, read by the same plugin (or both by tmiki itself;
            // PLUGINS.md 4.5: an importer change alone is shown)
            if py_eq(get(o, "source_digest"), get(n, "source_digest")) && importer_note(o.get("importer"), n.get("importer")).is_none() {
                continue;
            }
        }
        if new.is_none() && !worktree {
            results.push(FileResult::new(rel, "deleted"));
            continue;
        }
        let added_state = if old.is_none() { "added" } else { "modified" };
        if names_only {
            results.push(FileResult::new(rel, added_state));
            continue;
        }
        if full.is_some() {
            // A working file whose plugin cannot be used is compared by its
            // bytes alone (PLUGINS.md 4.4): the digests differ.
            if let Some((id, u)) = ctx.formats.unavailable(repo, rel) {
                let mut fr = FileResult::new(rel, added_state);
                fr.binary = true;
                fr.notes.push(format!("layer details need plugin '{id}' ({})", u.why(&id)));
                results.push(fr);
                continue;
            }
            tmi_import::report(run.progress, || format!("parsing {rel}"));
        }

        // The left composite is read and decoded beside the working file's
        // import (as Python's thread does).
        let old_composite_job = || -> Result<Option<(Vec<u8>, Rgba)>> {
            if summary {
                return Ok(None);
            }
            let Some(r) = old.map(|o| str_field(o, "composite_ref")).transpose()?.flatten().filter(|r| !r.is_empty()) else {
                return Ok(None);
            };
            let png = prof::time("old composite: read", || repo.store.read_blob(&r))?;
            let image = prof::time("old composite: decode", || pix::pixels_from_bytes(&png, &pctx.limits))?.into_dense();
            Ok(Some((png, image)))
        };
        let mut blob_reuse: HashMap<String, BlobReuse> = HashMap::new();
        let (imported, old_composite) = match &full {
            Some(full) => {
                let entries = working_file_entries(repo, rel, old, &head_entries, &index_raw);
                for e in &entries {
                    blob_reuse.extend(stage::layer_blob_reuse(repo, Some(e))?);
                }
                let base = if summary { None } else { entries.iter().rev().find_map(|e| composite_base_refs(Some(e))) };
                let (bytes, digest) = match source.take() {
                    Some(s) => s,
                    None => stage::read_file_digest(full)?,
                };
                let pixels = stage::reuse_pixels(&blob_reuse);
                let decoded = stage::reuse_decoded(&blob_reuse);
                let stored_base = base.map(|(s, c)| StoredBase { repo, source_ref: s, composite_ref: c });
                let all = LayerBlobs::All;
                let opts = Options {
                    render_composite: !summary,
                    cache: Some(&ctx.render_cache),
                    reuse_pixels: (!pixels.is_empty()).then_some(&pixels),
                    reuse_decoded: decoded.as_ref(),
                    layer_blobs: &all,
                    composite_base: stored_base.as_ref().map(|b| b as &dyn CompositeBase),
                    limits: ctx.limits,
                    verify_partial: ctx.verify_partial,
                    progress: run.progress,
                };
                let name = stage::file_name(full);
                let warns = &mut run.warns;
                let formats = &ctx.formats;
                // The left composite is read on a thread with a small pool of
                // its own: in the shared pool its chunk reads waited behind
                // the import's work (a worker waiting for them takes import
                // jobs meanwhile) and it often finished after the import.
                let (old_composite, imported) = std::thread::scope(|s| {
                    let left = s.spawn(|| match rayon::ThreadPoolBuilder::new().num_threads(4).stack_size(8 << 20).build() {
                        Ok(pool) => pool.install(old_composite_job),
                        Err(_) => old_composite_job(),
                    });
                    let imported =
                        prof::time("import working file", || formats.import(repo, rel, &name, &bytes, &digest, &opts, warns));
                    let old = left.join().unwrap_or_else(|p| std::panic::resume_unwind(p));
                    (old, imported)
                });
                (Some(imported?), old_composite)
            }
            None => (None, old_composite_job()),
        };
        let new_format = match &imported {
            Some(i) => Value::Str(i.source_format.clone()),
            None => get(new.unwrap(), "source_format").clone(),
        };
        let old_binary = old.is_some_and(|o| get(o, "source_format").as_str() == Some("binary"));
        let new_importer = match &imported {
            Some(i) => i.importer.clone().map(Value::Object),
            None => new.and_then(|n| n.get("importer")).cloned(),
        };
        let note = importer_note(old.and_then(|o| o.get("importer")), new_importer.as_ref());
        if new_format.as_str() == Some("binary") || old_binary {
            let mut fr = FileResult::new(rel, added_state);
            fr.binary = true;
            fr.notes.extend(note);
            results.push(fr);
            continue;
        }
        let mut fr = FileResult::new(rel, added_state);
        fr.notes.extend(note);
        let (old_order, old_layers) = match old {
            Some(o) => entry_layers(repo, o)?,
            None => (Vec::new(), HashMap::new()),
        };
        let (new_order, new_layers) = match &imported {
            Some(i) => imported_layers(i, &blob_reuse)?,
            None => entry_layers(repo, new.unwrap())?,
        };

        // The composites (both sides decoded).
        let mut composites: Option<(Option<Vec<u8>>, Rgba, Option<Vec<u8>>, Rgba)> = None;
        if !summary {
            tmi_import::report(run.progress, || format!("comparing composites of {rel}"));
            let new_png: Option<Vec<u8>> = match &imported {
                Some(i) => (!i.composite_png.is_empty()).then(|| i.composite_png.clone()),
                None => match str_field(new.unwrap(), "composite_ref")?.filter(|r| !r.is_empty()) {
                    Some(r) => Some(repo.store.read_blob(&r)?),
                    None => None,
                },
            };
            let new_composite = match &new_png {
                Some(p) => Some(prof::time("new composite: decode", || pix::pixels_from_bytes(p, &pctx.limits))?.into_dense()),
                None if imported.is_some() => return Err(fwd("an import without a composite")),
                None => None,
            };
            let old_composite = old_composite?;
            if let Some(nc) = new_composite {
                let (old_png, oc) = match old_composite {
                    Some((p, i)) => (Some(p), i),
                    None => (None, pix::blank(nc.width, nc.height)),
                };
                composites = Some((old_png, oc, new_png, nc));
            }
        }

        // Which layers differ (their readable property changes).
        let mut candidates: Vec<String> = new_order.clone();
        candidates.extend(old_order.iter().filter(|s| !new_layers.contains_key(*s)).cloned());
        if !candidates.is_empty() && candidates.iter().all(|s| s == RASTER_LAYER_ID) {
            fr.raster = true;
        }
        let mut changed: Vec<(LayerResult, Option<&LayerView>, Option<&LayerView>, usize)> = Vec::new();
        for (cand_idx, sid) in candidates.iter().enumerate() {
            let before = old_layers.get(sid);
            let after = new_layers.get(sid);
            let layer = match (before, after) {
                (None, Some(a)) => LayerResult { name: a.name.clone(), state: "added", prop_changes: Vec::new(), bbox: None, html: None },
                (Some(b), None) => LayerResult { name: b.name.clone(), state: "deleted", prop_changes: Vec::new(), bbox: None, html: None },
                (Some(b), Some(a)) => {
                    let metadata_changed = match (&b.metadata, &a.metadata) {
                        (Some(bm), Some(am)) => py_eq(&bm.0, &am.0) && !py_eq(&bm.1, &am.1) && py_eq(&b.pixel, &a.pixel),
                        _ => false,
                    };
                    let changes = layer_changes::property_change_parts(&b.props, &a.props, metadata_changed)?;
                    if py_eq(&b.pixel, &a.pixel) && changes.is_empty() {
                        continue;
                    }
                    LayerResult { name: a.name.clone(), state: "changed", prop_changes: changes, bbox: None, html: None }
                }
                (None, None) => unreachable!(),
            };
            changed.push((layer, before, after, cand_idx + 1));
        }

        // The composites and the layers' pixels, side by side.
        let run_ref: &DiffRun = run;
        let raster = fr.raster;
        let path = rel.as_str();
        let html = run.mode == Mode::Html;
        // Python says the composite's preview phase before the layers'
        // (without --html); with --html its thread may still be encoding
        // when the layers are done.
        let gate = Gate::new(run.progress, html || composites.is_none());
        let composite_done = std::sync::atomic::AtomicBool::new(false);
        let on_bbox = |found: bool| gate.open(found.then(|| format!("encoding composite previews of {rel}")));
        let composite_job = || -> Result<CompositeOut> {
            let out = match &composites {
                Some((op, oc, np, nc)) => compare_composites(run_ref, path, oc, nc, op.as_deref(), np.as_deref(), &on_bbox),
                None => Ok(CompositeOut::default()),
            };
            gate.open(None);
            composite_done.store(true, std::sync::atomic::Ordering::SeqCst);
            out
        };
        let total = candidates.len();
        let layers_job = || -> Vec<Result<LayerOut>> {
            use rayon::prelude::*;
            if total > 0 {
                gate.report(format!("comparing layers 1/{total}"));
            }
            let say = |(_, _, _, idx): &(LayerResult, Option<&LayerView>, Option<&LayerView>, usize)| {
                if *idx > 1 {
                    gate.report(format!("comparing layers {idx}/{total}"));
                }
            };
            if summary {
                return changed.iter().map(|_| Ok(LayerOut::default())).collect();
            }
            if html {
                // The report's layer previews write no files: the layers go
                // through without waiting for each other, a few at a time.
                return in_flight(&changed, LAYERS_AT_ONCE, |item| {
                    say(item);
                    let (l, b, a, _) = item;
                    layer_pixels(run_ref, &pctx, path, &l.name, raster, *b, *a)
                });
            }
            let mut outs = Vec::with_capacity(changed.len());
            for chunk in changed.chunks(LAYERS_AT_ONCE) {
                let mut part: Vec<Result<LayerOut>> = chunk
                    .par_iter()
                    .map(|item| {
                        say(item);
                        let (l, b, a, _) = item;
                        layer_pixels(run_ref, &pctx, path, &l.name, raster, *b, *a)
                    })
                    .collect();
                for r in part.iter_mut() {
                    let Ok(out) = r else { break };
                    if let Some((name, png)) = out.file.take() {
                        match run_ref.out.mkdir().and_then(|()| run_ref.out.write(&name, &png)) {
                            Ok(text) => out.output = Some(text),
                            Err(e) => {
                                *r = Err(e);
                                break;
                            }
                        }
                    }
                }
                let failed = part.iter().any(|r| r.is_err());
                outs.extend(part);
                if failed {
                    break;
                }
            }
            outs
        };
        // The composites go on a thread with a pool of their own: joined in
        // the shared pool, a worker waiting inside them took up a layer
        // worker's whole run and the composites ended after the layers.
        let (composite, layer_outs) = std::thread::scope(|s| {
            let composite = s.spawn(|| {
                let pool = rayon::ThreadPoolBuilder::new().num_threads(rayon::current_num_threads()).stack_size(8 << 20).build();
                let job = || prof::time("composite job (wall)", composite_job);
                match pool {
                    Ok(pool) => pool.install(job),
                    Err(_) => job(),
                }
            });
            let layer_outs = prof::time("layers job (wall)", layers_job);
            if html && composites.is_some() && !composite_done.load(std::sync::atomic::Ordering::SeqCst) {
                tmi_import::report(run_ref.progress, || format!("encoding composite previews of {rel}"));
            }
            (composite.join().unwrap_or_else(|p| std::panic::resume_unwind(p)), layer_outs)
        });
        let mut composite = Some(composite);
        // Python: without --html the composite is written first (its error
        // first); with --html its thread is joined after the layers.
        if run.mode != Mode::Html {
            let c = composite.take().unwrap()?;
            fr.bbox = c.bbox;
            fr.outputs.extend(c.output);
        }
        let mut layer_outputs = Vec::new();
        for ((mut layer, _, _, _), out) in changed.into_iter().zip(layer_outs) {
            let out = out?;
            layer.bbox = out.bbox;
            layer.html = out.html;
            layer_outputs.extend(out.output);
            fr.layers.push(layer);
        }
        if let Some(c) = composite {
            let c = c?;
            fr.bbox = c.bbox;
            fr.html = c.html;
        }
        fr.outputs.extend(layer_outputs);
        if summary || fr.bbox.is_some() || !fr.layers.is_empty() || old.is_none() || !fr.notes.is_empty() {
            results.push(fr);
        }
    }
    Ok(results)
}

/// The out directory name of an automatic diff (`%Y%m%d-%H%M%S`, local).
pub fn auto_dir_name(t: &crate::localtime::Local) -> String {
    format!("{:04}{:02}{:02}-{:02}{:02}{:02}", t.year, t.month, t.day, t.hour, t.minute, t.second)
}

pub fn path_of(p: &Path) -> String {
    p.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_rounds_half_to_even() {
        assert_eq!(fit((100, 50), 200), (100, 50));
        assert_eq!(fit((4096, 2048), 2048), (2048, 1024));
        assert_eq!(fit((5, 3), 2), (2, 1)); // 1.2 -> 1, 0.4 -> max(1, 0)
        assert_eq!(fit((2894, 4093), 1024).1, 1024);
    }

    /// A canvas grown between the commits, the change beyond the old width:
    /// the preview region stayed within the old (left) side and came out
    /// with a negative width.
    #[test]
    fn a_layer_changed_outside_a_grown_canvas_previews_a_real_region() {
        let left = Px::Sparse(pix::Sparse::empty((10, 10)));
        let right = Px::Sparse(pix::Sparse::empty((1000, 1000)));
        let bx = html_layer_box((900, 900, 901, 901), &left, &right);
        assert!(bx.0 < bx.2 && bx.1 < bx.3, "{bx:?}");
        let size = fit(((bx.2 - bx.0) as usize, (bx.3 - bx.1) as usize), HTML_LAYER_EDGE);
        assert!(pix::resize_region(&left, bx, size).is_ok());
        assert!(pix::resize_region(&right, bx, size).is_ok());
    }

    #[test]
    fn stems() {
        assert_eq!(safe_rel_stem("art/a.clip"), safe_filename("art/a.clip"));
        assert_ne!(safe_rel_stem("a.clip"), safe_rel_stem("a.png"));
        // ".hidden" has no suffix; safe_filename strips the dot and adds a digest
        assert_eq!(safe_rel_stem(".hidden"), safe_filename(".hidden"));
        assert!(safe_rel_stem(".hidden").starts_with("hidden-"));
    }
}
