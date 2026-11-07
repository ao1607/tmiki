//! repo.py `stage_file` (whole files), `stage_delete`, `materialize_import`,
//! `_write_layer_tree`, `_layer_blob_reuse`, `stage_all_modified` and the
//! read-write `_StatCache` (stage 3b-2). Imports run in tmi-import.
//!
//! What Python does that is not reproduced here (a `path:layer` stage,
//! `.tmiignore` layer patterns, merge conflicts, a file tmi-import
//! declines) is `Forward`. Before the command ends nothing but
//! content-addressed objects and render-cache entries is written, so a
//! command can still be handed to Python whole: the index lives in memory
//! (`Stager::index`) and is written by `Stager::finish`.

use crate::error::{RepoError, Result};
use crate::ignore::{self, IgnorePattern};
use crate::py;
use crate::pyeq::py_eq;
use crate::repo::{same_entry, Entries, Repo};
use crate::wfs;
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use tmi_import::{CompositeBase, ImportedFile, ImportedLayer, LayerBlobs, Limits, Options, PriorDecode, RenderCache, Warnings};
use tmi_store::json::{Int, Map, Value};
use tmi_store::oid::sha256_hex;

fn fwd(why: impl Into<String>) -> RepoError {
    RepoError::forward(why)
}

/// `_is_deleted_index_entry(entry)`.
pub fn is_deleted(entry: &Value) -> bool {
    entry.get("deleted").is_some_and(Value::truthy)
}

/// `_deleted_index_entry(rel)`.
pub fn deleted_entry(rel: &str) -> Value {
    let mut m = Map::new();
    m.insert("path".into(), Value::Str(rel.into()));
    m.insert("deleted".into(), Value::Bool(true));
    Value::Object(m)
}

/// `_committable_entry(entry)`: without the index-only source_meta keys.
pub fn committable(entry: &Value) -> Value {
    let mut cleaned = entry.as_object().cloned().unwrap_or_default();
    let mut meta = match cleaned.get("source_meta") {
        Some(Value::Object(m)) => m.clone(),
        _ => Map::new(),
    };
    for k in ["staged_layer_selector", "staged_layer_name", "staged_layer_stable_source_id"] {
        meta.shift_remove(k);
    }
    cleaned.insert("source_meta".into(), Value::Object(meta));
    Value::Object(cleaned)
}

/// `read_file_digest(path)`.
pub fn read_file_digest(path: &Path) -> Result<(Vec<u8>, String)> {
    let data = std::fs::read(path).map_err(|e| fwd(format!("{}: {e}", path.display())))?;
    let digest = sha256_hex(&data);
    Ok((data, digest))
}

/// `sha256_file(path)` (streamed).
fn sha256_file(path: &Path) -> Result<String> {
    let mut f = std::fs::File::open(path).map_err(|e| fwd(format!("{}: {e}", path.display())))?;
    let mut h = tmi_store::oid::Sha256::default();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf).map_err(|e| fwd(e.to_string()))?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.hexdigest())
}

/// The read-write `_StatCache` of one command.
pub struct StatCache {
    entries: Map,
    written_ns: i128,
    seen: HashSet<String>,
    dirty: bool,
}

fn as_i128(v: Option<&Value>) -> Option<i128> {
    match v {
        Some(Value::Int(Int::Small(i))) => Some(*i as i128),
        _ => None,
    }
}

impl StatCache {
    /// `read_stat_cache()`: a missing or damaged file is an empty cache.
    pub fn load(repo: &Repo) -> Result<StatCache> {
        let mut cache = StatCache { entries: Map::new(), written_ns: 0, seen: HashSet::new(), dirty: false };
        let Ok(bytes) = std::fs::read(repo.repo_dir.join("stat_cache")) else { return Ok(cache) };
        let Ok(text) = String::from_utf8(bytes) else { return Ok(cache) };
        let Ok(data) = tmi_store::json::loads(&text) else { return Ok(cache) };
        let Some(Value::Object(entries)) = data.get("entries") else { return Ok(cache) };
        cache.entries = entries.clone();
        cache.written_ns = match data.get("written_ns") {
            None | Some(Value::Null) => 0,
            Some(v) if !v.truthy() => 0,
            Some(Value::Int(Int::Small(n))) => *n as i128,
            Some(_) => return Err(fwd("stat cache written_ns is not an int")),
        };
        Ok(cache)
    }

    /// `lookup(rel, full, st=..., want_bytes=...)`: the digest, and the
    /// bytes it hashed when it read the file (and `want_bytes`).
    pub fn lookup(&mut self, rel: &str, full: &Path, meta: &std::fs::Metadata, want_bytes: bool) -> Result<(String, Option<Vec<u8>>)> {
        self.seen.insert(rel.to_string());
        let st_mtime = py::mtime_ns(meta);
        let size = meta.len() as i128;
        if let Some(ent) = self.entries.get(rel) {
            let Some(obj) = ent.as_object() else { return Err(fwd("stat cache entry is not an object")) };
            if as_i128(obj.get("size")) == Some(size) && as_i128(obj.get("mtime_ns")) == Some(st_mtime) && st_mtime < self.written_ns {
                return match obj.get("digest") {
                    Some(Value::Str(d)) => Ok((d.clone(), None)),
                    _ => Err(fwd("stat cache digest is not a str")),
                };
            }
        }
        let (data, digest) = if want_bytes {
            let (d, g) = read_file_digest(full)?;
            (Some(d), g)
        } else {
            (None, sha256_file(full)?)
        };
        let mut e = Map::new();
        e.insert("size".into(), Value::Int(Int::Small(meta.len() as i64)));
        e.insert("mtime_ns".into(), Value::Int(Int::Small(st_mtime as i64)));
        e.insert("digest".into(), Value::Str(digest.clone()));
        self.entries.insert(rel.to_string(), Value::Object(e));
        self.dirty = true;
        Ok((digest, data))
    }

    /// `changed(rel, full, expected, ...)`: (modified, (bytes, digest) when read).
    pub fn changed(
        &mut self,
        rel: &str,
        full: &Path,
        expected: Option<&Value>,
        meta: &std::fs::Metadata,
        want_bytes: bool,
    ) -> Result<(bool, Option<(Vec<u8>, String)>)> {
        let (digest, data) = self.lookup(rel, full, meta, want_bytes)?;
        if matches!(expected, Some(Value::Str(e)) if *e == digest) {
            return Ok((false, None));
        }
        Ok((true, data.map(|d| (d, digest))))
    }

    /// `flush()`: prune unseen entries; write when anything changed.
    pub fn flush(mut self, repo: &Repo) -> Result<()> {
        let stale: Vec<String> = self.entries.keys().filter(|k| !self.seen.contains(*k)).cloned().collect();
        if !stale.is_empty() {
            for k in &stale {
                self.entries.shift_remove(k);
            }
            self.dirty = true;
        }
        if self.dirty {
            let ns = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos()) as i64;
            let mut m = Map::new();
            m.insert("version".into(), Value::Int(Int::Small(1)));
            m.insert("entries".into(), Value::Object(self.entries));
            m.insert("written_ns".into(), Value::Int(Int::Small(ns)));
            wfs::atomic_write_unmarked(&repo.repo_dir.join("stat_cache"), &wfs::indented_json(&Value::Object(m)))?;
        }
        Ok(())
    }
}

/// A `_layer_blob_reuse` record.
#[derive(Clone, Debug)]
pub struct BlobReuse {
    pub pixel_sha256: Value,
    pub blob_ref: Value,
    pub pixel_bbox: Value,
    pub pixel_source: Value,
}

/// `flatten_layers(tree_ref)` (without path_name): every stored layer.
pub fn flatten_layers(repo: &Repo, tree_ref: &str, out: &mut Vec<Value>, depth: usize) -> Result<()> {
    if depth > 200 {
        return Err(fwd("layer tree too deep"));
    }
    let tree = repo.store.read_json(tree_ref, Some("tree"))?;
    let Some(layers) = tree.get("layers") else { return Ok(()) };
    let layers = layers.as_array().ok_or_else(|| fwd("tree layers is not a list"))?;
    for layer in layers {
        if layer.as_object().is_none() {
            return Err(fwd("tree layer is not an object"));
        }
        out.push(layer.clone());
        match layer.get("subtree_ref") {
            Some(Value::Str(s)) if !s.is_empty() => flatten_layers(repo, s, out, depth + 1)?,
            Some(v) if v.truthy() => return Err(fwd("subtree_ref is not a str")),
            _ => {}
        }
    }
    Ok(())
}

/// `_layer_blob_reuse(entry)`.
pub fn layer_blob_reuse(repo: &Repo, entry: Option<&Value>) -> Result<HashMap<String, BlobReuse>> {
    let mut out = HashMap::new();
    let Some(entry) = entry else { return Ok(out) };
    if is_deleted(entry) {
        return Ok(out);
    }
    let tree_ref = match entry.get("tree_ref") {
        Some(Value::Str(s)) if !s.is_empty() => s.clone(),
        Some(v) if v.truthy() => return Err(fwd("tree_ref is not a str")),
        _ => return Ok(out),
    };
    let mut layers = Vec::new();
    flatten_layers(repo, &tree_ref, &mut layers, 0)?;
    for layer in layers {
        let sid = match layer.get("stable_source_id") {
            Some(Value::Str(s)) if !s.is_empty() => s.clone(),
            Some(v) if v.truthy() => return Err(fwd("stable_source_id is not a str")),
            _ => continue,
        };
        let pixel_sha = layer.get("pixel_sha256").cloned().unwrap_or(Value::Null);
        let blob_ref = layer.get("blob_ref").cloned().unwrap_or(Value::Null);
        if !pixel_sha.is_null() && !blob_ref.is_null() {
            let pixel_source = layer.get("source_meta").and_then(|m| m.get("pixel_source")).cloned().unwrap_or(Value::Null);
            if layer.get("source_meta").is_some_and(|m| m.as_object().is_none() && m.truthy()) {
                return Err(fwd("source_meta is not an object"));
            }
            out.insert(
                sid,
                BlobReuse {
                    pixel_sha256: pixel_sha,
                    blob_ref,
                    pixel_bbox: layer.get("pixel_bbox").cloned().unwrap_or(Value::Null),
                    pixel_source,
                },
            );
        }
    }
    Ok(out)
}

/// `{sid: d["pixel_sha256"]}` (only str digests can equal a computed one).
pub fn reuse_pixels(reuse: &HashMap<String, BlobReuse>) -> HashMap<String, String> {
    reuse
        .iter()
        .filter_map(|(k, v)| match &v.pixel_sha256 {
            Value::Str(s) => Some((k.clone(), s.clone())),
            _ => None,
        })
        .collect()
}

/// `_reuse_decoded(blob_reuse)`.
pub fn reuse_decoded(reuse: &HashMap<String, BlobReuse>) -> Option<HashMap<String, PriorDecode>> {
    let off = std::env::var("TMI_NO_DECODE_REUSE").unwrap_or_default().trim().to_lowercase();
    if ["1", "true", "yes", "on"].contains(&off.as_str()) {
        return None;
    }
    let out: HashMap<String, PriorDecode> = reuse
        .iter()
        .filter(|(_, d)| d.pixel_source.truthy())
        .map(|(k, d)| {
            (
                k.clone(),
                PriorDecode { pixel_source: Some(d.pixel_source.clone()), pixel_sha256: d.pixel_sha256.clone(), pixel_bbox: d.pixel_bbox.clone() },
            )
        })
        .collect();
    (!out.is_empty()).then_some(out)
}

/// `_reuses_blob(reuse, layer)`.
fn reuses_blob(reuse: Option<&BlobReuse>, layer: &ImportedLayer) -> bool {
    match (reuse, &layer.pixel_sha256) {
        (Some(r), Some(p)) => !r.pixel_sha256.is_null() && !p.is_null() && py_eq(&r.pixel_sha256, p),
        _ => false,
    }
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

/// `_composite_base(entry)`: (source_ref, composite_ref) of a CLIP entry.
fn composite_base_refs(entry: Option<&Value>) -> Option<(String, String)> {
    let entry = entry?;
    if is_deleted(entry) || entry.get("source_format").and_then(Value::as_str) != Some("clip") {
        return None;
    }
    let s = entry.get("source_blob_ref").filter(|v| v.truthy())?.as_str()?.to_string();
    let c = entry.get("composite_ref").filter(|v| v.truthy())?.as_str()?.to_string();
    Some((s, c))
}

fn walk_layers<'a>(layers: &'a [ImportedLayer], out: &mut Vec<&'a ImportedLayer>) {
    for l in layers {
        out.push(l);
        walk_layers(&l.children, out);
    }
}

/// `import_file` on a working file, with what the repository lends it.
pub struct ImportContext {
    pub limits: Limits,
    pub render_cache: RenderCache,
    pub verify_partial: bool,
    /// `.tmiattributes` and the format plugins (PLUGINS.md 4, P6).
    pub formats: crate::formats::Formats,
}

impl ImportContext {
    pub fn for_repo(repo: &Repo) -> Result<ImportContext> {
        let repo_cfg = repo.read_config()?;
        let user_cfg = crate::config::read_user_config()?;
        let key = "core.maxImagePixels";
        let limits = tmi_import::limit::resolve(
            crate::config::get_value(&repo_cfg, key)?,
            crate::config::get_value(&user_cfg, key)?,
        )
        .map_err(fwd)?;
        let verify = std::env::var("TMI_VERIFY_PARTIAL").unwrap_or_default().trim().to_lowercase();
        Ok(ImportContext {
            limits,
            render_cache: RenderCache::new(repo.repo_dir.join("cache").join("render")),
            verify_partial: ["1", "true", "yes", "on"].contains(&verify.as_str()),
            formats: crate::formats::Formats::for_repo(repo),
        })
    }
}

/// Python's file name for `full` (`Path.name`).
pub fn file_name(full: &Path) -> String {
    full.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

pub struct Stager<'a> {
    pub repo: &'a Repo,
    pub head_entries: Entries,
    /// The index as written so far (`read_index()` after our writes).
    pub index: Value,
    pub index_dirty: bool,
    pub warns: Warnings,
    pub ctx: ImportContext,
    patterns: Option<Vec<IgnorePattern>>,
    /// `on_progress`: the staging's phases (cli.py's `Progress`).
    pub progress: Option<tmi_import::Progress<'a>>,
    /// Paths whose conflict records `clear_conflict_files` moves aside,
    /// in order (done by `finish`, after the index; stage 4a).
    clears: Vec<String>,
    /// Whether the last `stage_file` failed because a format plugin could
    /// not be used or could not read the file (issue #49), or because its
    /// path cannot be recorded (`recpath`): a bulk `add` lists such a file and
    /// goes on with the rest.
    pub plugin_refused: bool,
}

/// What `split_pathspec(spec)` gives: Err(Forward) for a `path:layer`.
fn split_pathspec(spec: &str) -> Result<String> {
    if Path::new(spec).exists() {
        return Ok(spec.to_string());
    }
    if spec.contains('\n') {
        return Err(fwd("pathspec with a newline"));
    }
    let lower = spec.to_lowercase();
    if lower.len() == spec.len() {
        for (i, _) in spec.match_indices(':') {
            let head = &lower[..i];
            let tail = &spec[i + 1..];
            if tail.is_empty() {
                continue;
            }
            for ext in [".png", ".jpg", ".jpeg", ".mdp", ".clip"] {
                if head.len() > ext.len() && head.ends_with(ext) {
                    return Err(fwd("a layer stage (path:layer)"));
                }
            }
        }
    } else if spec.contains(':') {
        return Err(fwd("pathspec case folding"));
    }
    Ok(spec.to_string())
}

impl<'a> Stager<'a> {
    pub fn new(repo: &'a Repo) -> Result<Stager<'a>> {
        let head_entries = repo.entries_for_commit(repo.current_commit()?.as_deref())?;
        let index = repo.read_index()?;
        if index.as_object().is_none() {
            return Err(fwd("index is not an object"));
        }
        Ok(Stager {
            repo,
            head_entries,
            index,
            index_dirty: false,
            warns: Warnings::default(),
            ctx: ImportContext::for_repo(repo)?,
            patterns: None,
            clears: Vec::new(),
            progress: None,
            plugin_refused: false,
        })
    }

    /// `clear_conflict_files(rel)` (deferred to `finish`).
    fn clear_conflict(&mut self, rel: &str) {
        self.clears.push(rel.to_string());
    }

    /// `rel in pending_conflict_paths()`, the clears this run will make
    /// already made.
    fn pending_conflict(&self, rel: &str) -> Result<bool> {
        if self.clears.iter().any(|c| c == rel) {
            return Ok(false);
        }
        Ok(self.repo.pending_conflict_paths()?.iter().any(|p| p == rel))
    }

    pub fn patterns(&mut self) -> Result<&[IgnorePattern]> {
        if self.patterns.is_none() {
            self.patterns = Some(ignore::repo_patterns(self.repo.worktree.as_deref())?);
        }
        Ok(self.patterns.as_deref().unwrap())
    }

    /// `index.get("entries", {})`.
    pub fn index_entries(&self) -> Result<Map> {
        match self.index.get("entries") {
            None => Ok(Map::new()),
            Some(Value::Object(m)) => Ok(m.clone()),
            Some(_) => Err(fwd("index entries is not an object")),
        }
    }

    fn entries_mut(&mut self) -> &mut Map {
        let Value::Object(idx) = &mut self.index else { unreachable!() };
        if !idx.contains_key("entries") {
            idx.insert("entries".into(), Value::Object(Map::new()));
        }
        match idx.get_mut("entries") {
            Some(Value::Object(m)) => m,
            _ => unreachable!(),
        }
    }

    /// `write_index` (deferred to `finish`).
    fn write_index(&mut self) {
        self.index_dirty = true;
    }

    /// Write what the staging changed (the index).
    pub fn finish(&mut self) -> Result<()> {
        if self.index_dirty {
            wfs::write_index(self.repo, &self.index)?;
            self.index_dirty = false;
        }
        for rel in std::mem::take(&mut self.clears) {
            self.repo.clear_conflict_files(&rel)?;
        }
        Ok(())
    }

    /// `stage_delete(rel)`.
    pub fn stage_delete(&mut self, rel: &str) {
        self.entries_mut().insert(rel.to_string(), deleted_entry(rel));
        self.write_index();
        self.clear_conflict(rel);
    }

    /// `stage_file(spec, force=..., source=...)` for a whole file: (rel,
    /// changed).
    ///
    /// When the disk is full, the objects and cache files this file's import
    /// wrote are removed again (nothing refers to them yet) and the error says
    /// which file and how much space is missing (issue #42).
    pub fn stage_file(&mut self, spec: &str, force: bool, source: Option<(Vec<u8>, String)>) -> Result<(String, bool)> {
        tmi_store::store::track_begin();
        self.plugin_refused = false;
        let result = self.stage_file_inner(spec, force, source);
        let created = tmi_store::store::track_end();
        match result {
            Err(e) if !matches!(e, RepoError::Runtime(_)) && tmi_store::store::no_space() => {
                for path in created {
                    let _ = std::fs::remove_file(path);
                }
                let (rel, needed) = match pathspec_full_path(self.repo, &split_pathspec(spec).unwrap_or_else(|_| spec.to_string())) {
                    Ok(full) => (
                        relpath(self.repo, &full).unwrap_or_else(|_| spec.to_string()),
                        std::fs::metadata(&full).map_or(0, |m| m.len()),
                    ),
                    Err(_) => (spec.to_string(), 0),
                };
                let free = crate::diskspace::free_bytes(&self.repo.repo_dir);
                Err(RepoError::runtime(crate::diskspace::disk_full_message(&rel, needed, free)))
            }
            other => other,
        }
    }

    fn stage_file_inner(&mut self, spec: &str, force: bool, source: Option<(Vec<u8>, String)>) -> Result<(String, bool)> {
        let file_part = split_pathspec(spec)?;
        let full = pathspec_full_path(self.repo, &file_part)?;
        if !full.exists() {
            let known: HashSet<String> = self.head_entries.keys().cloned().collect();
            let rel = pathspec_relpath(self.repo, &file_part, &known)?;
            if self.head_entries.contains_key(&rel) {
                let delete = deleted_entry(&rel);
                let entries = self.index_entries()?;
                let changed = !same_entry(entries.get(&rel), Some(&delete));
                if changed {
                    self.entries_mut().insert(rel.clone(), delete);
                    self.write_index();
                }
                self.clear_conflict(&rel);
                return Ok((rel, changed));
            }
            return Err(RepoError::NotFound(format!("no such file to add: {}", full.display())));
        }
        let rel = relpath(self.repo, &full)?;
        // a record is made by the rule every tree is read by (recpath.rs)
        if let Err(e) = crate::recpath::check_new_record_path(&rel) {
            self.plugin_refused = true;
            return Err(e);
        }
        if !force && !self.head_entries.contains_key(&rel) {
            let entries = self.index_entries()?;
            let tracked = entries.get(&rel).is_some_and(|e| !is_deleted(e));
            if !tracked && ignore::is_file_ignored(&rel, self.patterns()?)? {
                return Err(RepoError::runtime(format!(
                    "path is ignored by .tmiignore: {rel} (use 'tmiki add -f' to add anyway)"
                )));
            }
        }
        tmi_import::report(self.progress, || format!("parsing {file_part}"));
        let index_entries = self.index_entries()?;
        let head_entry = self.head_entries.get(&rel).cloned();
        let mut blob_reuse = layer_blob_reuse(self.repo, head_entry.as_ref())?;
        blob_reuse.extend(layer_blob_reuse(self.repo, index_entries.get(&rel))?);
        let (bytes, digest) = match source {
            Some(s) => s,
            None => read_file_digest(&full)?,
        };
        // A file whose bytes tmi has not stored needs at least that much room:
        // say so before writing anything (issue #42).
        let known = |e: Option<&Value>| e.and_then(|e| e.get("source_digest")).and_then(Value::as_str) == Some(digest.as_str());
        if !known(index_entries.get(&rel)) && !known(head_entry.as_ref()) {
            let free = crate::diskspace::free_bytes(&self.repo.repo_dir);
            if free < bytes.len() as u64 {
                return Err(RepoError::runtime(crate::diskspace::not_enough_message(&rel, bytes.len() as u64, free)));
            }
        }
        let base = composite_base_refs(index_entries.get(&rel)).or_else(|| composite_base_refs(head_entry.as_ref()));
        let pixels = reuse_pixels(&blob_reuse);
        let decoded = reuse_decoded(&blob_reuse);
        let stored_base = base.map(|(s, c)| StoredBase { repo: self.repo, source_ref: s, composite_ref: c });
        let all = LayerBlobs::All;
        let opts = Options {
            render_composite: true,
            cache: Some(&self.ctx.render_cache),
            reuse_pixels: (!pixels.is_empty()).then_some(&pixels),
            reuse_decoded: decoded.as_ref(),
            layer_blobs: &all,
            composite_base: stored_base.as_ref().map(|b| b as &dyn CompositeBase),
            limits: self.ctx.limits,
            verify_partial: self.ctx.verify_partial,
            progress: self.progress,
        };
        let name = file_name(&full);
        let store = &self.repo.store;
        let formats = &self.ctx.formats;
        // A plugin is loaded (and trusted) here, before the parallel part.
        if let Some((id, u)) = formats.unavailable(self.repo, &rel) {
            self.plugin_refused = true;
            return Err(RepoError::runtime(crate::formats::not_available_message(&rel, &id, &u)));
        }
        let by_plugin = formats.mapping(&rel).plugin_id().is_some();
        // A built-in reader's only error here is the pixel limit
        // (`formats::builtin_decline`): one file a bulk add lists and goes on.
        let builtin = matches!(formats.mapping(&rel).format, crate::attributes::Format::Builtin);
        let mut warns = std::mem::take(&mut self.warns);
        let (source_ref, imported) = rayon::join(
            || store.write_large_blob(&bytes),
            || formats.import(self.repo, &rel, &name, &bytes, &digest, &opts, &mut warns),
        );
        self.warns = warns;
        let imported = match imported {
            Err(e) if (by_plugin || builtin) && e.is_runtime() && !tmi_store::store::no_space() => {
                self.plugin_refused = true;
                return Err(e);
            }
            other => other?,
        };
        let source_ref = source_ref?;
        let mut entry = self.materialize_import(&rel, &imported, &bytes, Some(&blob_reuse), Some(source_ref))?;
        // _apply_layer_ignore (stage 4b)
        let patterns = self.patterns()?.to_vec();
        crate::layer_ignore::apply(self.repo, &self.head_entries, &patterns, &rel, &mut entry, &mut self.warns, self.progress)?;
        let changed = self.put_staged(&rel, entry)?;
        Ok((rel, changed))
    }

    /// The end of `_stage_file`: `entry` staged for `rel` (or the index entry
    /// dropped when it is HEAD's), the conflict records moved aside; whether
    /// anything changed.
    pub fn put_staged(&mut self, rel: &str, entry: Value) -> Result<bool> {
        let index_entries = self.index_entries()?;
        let head_entry = self.head_entries.get(rel).cloned();
        if same_entry(Some(&committable(&entry)), head_entry.as_ref()) {
            let changed = index_entries.contains_key(rel);
            if changed {
                self.entries_mut().shift_remove(rel);
                self.write_index();
            }
            let conflict_changed = self.pending_conflict(rel)?;
            self.clear_conflict(rel);
            return Ok(changed || conflict_changed);
        }
        let changed = !same_entry(index_entries.get(rel), Some(&entry));
        if changed {
            self.entries_mut().insert(rel.to_string(), entry);
            self.write_index();
        }
        self.clear_conflict(rel);
        Ok(changed)
    }

    /// `materialize_import(rel, imported, blob_reuse=..., source_ref=...)`.
    pub fn materialize_import(
        &self,
        rel: &str,
        imported: &ImportedFile,
        source: &[u8],
        blob_reuse: Option<&HashMap<String, BlobReuse>>,
        source_ref: Option<String>,
    ) -> Result<Value> {
        tmi_import::report(self.progress, || format!("writing source blob for {rel}"));
        let mut walked = Vec::new();
        walk_layers(&imported.layers, &mut walked);
        let needing: Vec<&ImportedLayer> = walked
            .into_iter()
            .filter(|l| l.blob_bytes.is_some() && !reuses_blob(blob_reuse.and_then(|r| r.get(&l.stable_source_id)), l))
            .collect();
        let mut payloads: Vec<&[u8]> = Vec::new();
        if source_ref.is_none() {
            payloads.push(source);
        }
        payloads.extend(needing.iter().map(|l| l.blob_bytes.as_deref().unwrap()));
        if !imported.composite_png.is_empty() {
            payloads.push(&imported.composite_png);
        }
        let mut refs = if payloads.is_empty() { Vec::new() } else { self.repo.store.write_large_blobs(&payloads)? };
        let source_ref = match source_ref {
            Some(r) => r,
            None => refs.remove(0),
        };
        let composite_ref = if imported.composite_png.is_empty() { None } else { refs.pop() };
        let mut layer_refs = refs.into_iter();
        let tree_ref = self.write_layer_tree(&imported.layers, rel, blob_reuse, &mut layer_refs)?;
        tmi_import::report(self.progress, || format!("writing thumbnail for {rel}"));
        let thumbnail_ref =
            if imported.thumbnail_png.is_empty() { None } else { Some(self.repo.store.write("blob", &imported.thumbnail_png)?) };
        let opt = |s: Option<String>| s.map_or(Value::Null, Value::Str);
        let mut e = Map::new();
        e.insert("path".into(), Value::Str(rel.to_string()));
        e.insert("source_format".into(), Value::Str(imported.source_format.clone()));
        if let Some(importer) = &imported.importer {
            // a format plugin's record (PLUGINS.md 4.2; never a built-in's)
            e.insert("importer".into(), Value::Object(importer.clone()));
        }
        e.insert("source_blob_ref".into(), Value::Str(source_ref));
        e.insert("source_digest".into(), Value::Str(imported.source_digest.clone()));
        e.insert("tree_ref".into(), Value::Str(tree_ref));
        e.insert("thumbnail_ref".into(), opt(thumbnail_ref));
        e.insert("composite_ref".into(), opt(composite_ref));
        e.insert("canvas_meta".into(), Value::Object(imported.canvas_meta.clone()));
        e.insert("source_meta".into(), Value::Object(imported.source_meta.clone()));
        Ok(Value::Object(e))
    }

    /// `_write_layer_tree(layers, blob_reuse=..., layer_refs=...)`: the
    /// refs of the layers needing blobs come in walk order.
    fn write_layer_tree(
        &self,
        layers: &[ImportedLayer],
        rel: &str,
        blob_reuse: Option<&HashMap<String, BlobReuse>>,
        layer_refs: &mut std::vec::IntoIter<String>,
    ) -> Result<String> {
        let mut entries = Vec::new();
        for layer in layers {
            tmi_import::report(self.progress, || format!("writing layer {rel}:{}", layer.name));
            let reuse = blob_reuse.and_then(|r| r.get(&layer.stable_source_id));
            let blob_ref = if reuses_blob(reuse, layer) {
                reuse.unwrap().blob_ref.clone()
            } else if layer.blob_bytes.is_some() {
                Value::Str(layer_refs.next().ok_or_else(|| fwd("layer refs out of step"))?)
            } else if layer.pixel_sha256.is_some() {
                return Err(fwd("a layer's encode was skipped without a reusable blob (Python raises)"));
            } else {
                Value::Null
            };
            let subtree_ref = if layer.children.is_empty() {
                Value::Null
            } else {
                Value::Str(self.write_layer_tree(&layer.children, rel, blob_reuse, layer_refs)?)
            };
            let mut m = Map::new();
            for key in ["layer_id", "stable_source_id", "name", "kind", "visible", "opacity", "blend_mode", "clip_to_below", "offset_x", "offset_y", "mask_ref"] {
                m.insert(key.into(), layer.field(key));
            }
            m.insert("blob_ref".into(), blob_ref);
            m.insert("subtree_ref".into(), subtree_ref);
            m.insert("pixel_bbox".into(), layer.field("pixel_bbox"));
            m.insert("pixel_sha256".into(), layer.field("pixel_sha256"));
            m.insert("source_meta".into(), layer.field("source_meta"));
            entries.push(Value::Object(m));
        }
        let mut tree = Map::new();
        tree.insert("version".into(), Value::Int(Int::Small(1)));
        tree.insert("kind".into(), Value::Str("layer-tree".into()));
        tree.insert("layers".into(), Value::Array(entries));
        Ok(self.repo.store.write_json("tree", &Value::Object(tree))?)
    }

    /// `stage_all_modified()`: the paths staged. Merge conflicts are
    /// handled by the caller (forwarded).
    pub fn stage_all_modified(&mut self) -> Result<Vec<String>> {
        let conflicts = self.repo.pending_conflict_paths()?;
        if !conflicts.is_empty() {
            let mut summary = conflicts.iter().take(5).cloned().collect::<Vec<_>>().join(", ");
            if conflicts.len() > 5 {
                summary += &format!(", +{} more", conflicts.len() - 5);
            }
            return Err(RepoError::runtime(format!("unresolved merge conflicts: {summary}")));
        }
        let root = self.repo.require_worktree()?.to_path_buf();
        let index_entries = self.index_entries()?;
        let mut cache = StatCache::load(self.repo)?;
        let mut staged = Vec::new();
        let mut rels: Vec<String> = self.head_entries.keys().chain(index_entries.keys()).cloned().collect();
        rels.sort();
        rels.dedup();
        for rel in rels {
            let full = root.join(&rel);
            let index_entry = index_entries.get(&rel);
            let index_deleted = index_entry.is_some_and(is_deleted);
            let Ok(meta) = std::fs::metadata(&full) else {
                if !index_deleted {
                    self.stage_delete(&rel);
                    staged.push(rel);
                }
                continue;
            };
            let target = if index_deleted {
                None
            } else {
                index_entry.filter(|e| e.truthy()).or_else(|| self.head_entries.get(&rel))
            };
            let mut source = None;
            if let Some(t) = target {
                let (modified, src) = cache.changed(&rel, &full, t.get("source_digest"), &meta, true)?;
                if !modified {
                    continue;
                }
                source = src;
            }
            let spec = full.to_string_lossy().into_owned();
            let (_rel, changed) = self.stage_file(&spec, true, source)?;
            if changed {
                staged.push(rel);
            }
        }
        cache.flush(self.repo)?;
        Ok(staged)
    }
}

/// `pathspec_full_path(pathspec)`.
pub fn pathspec_full_path(repo: &Repo, spec: &str) -> Result<PathBuf> {
    let path = Path::new(spec);
    if path.is_absolute() {
        return Ok(py::realpath(path));
    }
    let cwd = std::env::current_dir().map_err(|e| fwd(e.to_string()))?;
    let cwd_path = py::realpath(&cwd.join(path));
    if cwd_path.exists() {
        return Ok(cwd_path);
    }
    let root_path = py::realpath(&repo.require_worktree()?.join(path));
    if root_path.exists() {
        return Ok(root_path);
    }
    Ok(cwd_path)
}

/// `relpath(path)`.
pub fn relpath(repo: &Repo, full: &Path) -> Result<String> {
    let root = repo.require_worktree()?;
    let full = py::realpath(full);
    py::relative_posix(&full, root)
        .ok_or_else(|| RepoError::runtime(format!("path is outside working tree: {}", full.display())))
}

/// `pathspec_relpath(pathspec, known_paths)`.
pub fn pathspec_relpath(repo: &Repo, spec: &str, known: &HashSet<String>) -> Result<String> {
    let path = Path::new(spec);
    if path.is_absolute() {
        return relpath(repo, path);
    }
    let root = repo.require_worktree()?;
    let cwd = std::env::current_dir().map_err(|e| fwd(e.to_string()))?;
    let mut candidates: Vec<String> = Vec::new();
    for candidate in [py::realpath(&cwd.join(path)), py::realpath(&root.join(path))] {
        if let Some(rel) = py::relative_posix(&candidate, root) {
            if !candidates.contains(&rel) {
                candidates.push(rel);
            }
        }
    }
    if let Some(r) = candidates.iter().find(|r| known.contains(*r)) {
        return Ok(r.clone());
    }
    if let Some(r) = candidates.first() {
        return Ok(r.clone());
    }
    Err(fwd("pathspec outside the working tree"))
}
