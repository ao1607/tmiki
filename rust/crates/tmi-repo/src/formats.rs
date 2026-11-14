//! Which reader imports a repository's file (PLUGINS.md 4, P6):
//! `.tmiattributes` first, then the built-in rules (`.clip`, `.mdp`, image
//! signatures, else opaque: `tmi_format::import_file`).
//!
//! A file mapped to `plugin:<id>` is read by the component
//! `.tmiplugins/<id>.wasm` of the working tree (or, for a merge, of the
//! merge result: [`Formats::from_entries`]) through
//! `tmi_format::pipeline::import_plugin`; its record gets the `importer`.
//! When that component is missing, unreadable, refused by `plugin.trust`
//! or built for another interface, the file is never stored another way
//! (PLUGINS.md 4.4): `import` fails with a message that says which plugin
//! and why, and the callers that can do without the import (status, diff,
//! merge) ask [`Formats::unavailable`] first. A plugin-format file never
//! goes to Python, which does not run plugins.
//!
//! Nothing here starts wasmtime until a file mapped to a plugin is
//! imported; reading `.tmiattributes` is one small file read, once per
//! command, and only when a file is imported.

use crate::attributes::{self, Attributes, Format, Mapping, OnError};
use crate::error::{RepoError, Result};
use crate::repo::{Entries, Repo};
use crate::structure_cache::{self, StructureCache};
use crate::trust;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use tmi_format::pipeline::{self, ImportError, Importer};
use tmi_format::types::ReadError;
use tmi_import::{ImportedFile, Options, Warnings};
use tmi_plugin_host::{DevOptions, DevProcessBackend, DynReader, PluginBackend, PluginError, PluginHost, Settings};
use tmi_store::json::Value;

/// A plugin ready to read: a component of the repository, or a development
/// plugin (`TMI_PLUGIN_DEV`, [`dev_plugins`]).
pub struct Loaded {
    pub reader: DynReader,
    pub importer: Importer,
}

impl Loaded {
    pub fn backend(&self) -> &dyn PluginBackend {
        self.reader.backend()
    }

    pub fn component(&self) -> trust::Component<'_> {
        trust::Component { id: &self.importer.id, version: &self.importer.version, sha256: &self.importer.component_sha256 }
    }

    /// Whether the plugin writes layers back (P7: a `writer` export and
    /// `can-write` in its info).
    pub fn can_write(&self) -> bool {
        self.backend().can_write()
    }

    /// A development plugin (`tmiki plugin dev`): its records cannot be
    /// committed.
    pub fn is_dev(&self) -> bool {
        self.importer.is_dev()
    }

    /// `plugin 'ora' 0.1.0`, as messages name it.
    pub fn label(&self) -> String {
        format!("plugin '{}' {}", self.importer.id, self.importer.version)
    }
}

/// Writes `edits` into `current` (the working file `rel`, last component
/// `name`) through `loaded`'s writer (PLUGINS.md P7): the user's consent
/// for this repository and component first (`trust::require_write_consent`,
/// Q1), then `tmi_format::write::write_checked`, which reads the file
/// before, runs the writer, joins its pieces and checks what it wrote
/// (PLUGINS.md 5). The new bytes; nothing is written to disk here.
#[allow(clippy::too_many_arguments)]
pub fn write_layers(
    repo: &Repo,
    loaded: &Loaded,
    rel: &str,
    name: &str,
    current: &[u8],
    edits: Vec<tmi_format::write::Edit>,
    limits: tmi_import::Limits,
    warns: &mut Warnings,
) -> Result<Vec<u8>> {
    if !loaded.can_write() {
        return Err(RepoError::runtime(format!("{rel}: {} cannot write layers back", loaded.label())));
    }
    trust::require_write_consent(repo, &loaded.component(), rel)?;
    tmi_format::write::write_checked(&loaded.reader, &loaded.importer, name, current, edits, limits, warns)
        .map_err(|e| RepoError::runtime(format!("{rel}: {} could not write layers back: {e}", loaded.label())))
}

/// Why a plugin cannot be used (PLUGINS.md 4.4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unavailable {
    Missing,
    Unreadable(String),
    Denied(String),
    /// Built for an interface this tmi does not serve.
    Abi(String),
    /// `TMI_PLUGIN_DEV` names a development plugin that does not start.
    Dev(String),
}

impl Unavailable {
    pub fn why(&self, id: &str) -> String {
        match self {
            // both ways out: the plugin back, or no plugin (issue #50)
            Unavailable::Missing => format!(
                "{}/{id}.wasm is missing; 'tmiki plugin add <file.wasm>' puts it back, 'tmiki plugin remove {id}' stops using it",
                attributes::PLUGIN_DIR
            ),
            Unavailable::Unreadable(m) => format!("{}/{id}.wasm cannot be used: {m}", attributes::PLUGIN_DIR),
            Unavailable::Denied(m) => format!("not trusted: {m}"),
            Unavailable::Abi(m) => format!("{}/{id}.wasm is built for another interface: {m}", attributes::PLUGIN_DIR),
            Unavailable::Dev(m) => format!("the development plugins of {DEV_ENV} cannot be used: {m}"),
        }
    }
}

/// The end of the message of a file a plugin could not read (with
/// `on-error=refuse`, the default): how to keep such a file instead. A bulk
/// `add` lists those files and says this once (issue #49).
pub const ON_ERROR_HINT: &str = " (with on-error=binary in .tmiattributes such a file is kept as an opaque blob)";

/// The message of a file whose plugin cannot be used.
pub fn not_available_message(rel: &str, id: &str, u: &Unavailable) -> String {
    format!("{rel}: {} maps it to plugin '{id}', which is not available ({})", attributes::FILE, u.why(id))
}

/// Where the components come from.
enum Components {
    /// `.tmiplugins/<id>.wasm` in the working tree.
    Worktree(Option<PathBuf>),
    /// The blobs of a set of entries (a merge result).
    Stored(HashMap<String, String>),
}

type Slot = Arc<OnceLock<std::result::Result<Arc<Loaded>, Unavailable>>>;

/// One command's view of how its repository's files are read.
pub struct Formats {
    attrs: OnceLock<Attributes>,
    attrs_text: Option<String>,
    worktree: Option<PathBuf>,
    components: Components,
    loaded: Mutex<HashMap<String, Slot>>,
    /// What plugins said about files (PLUGINS.md 6.2; `structure_cache`).
    structures: StructureCache,
}

/// `.tmi/cache/plugin`, where the structure cache keeps its files.
fn structure_dir(repo: &Repo) -> PathBuf {
    repo.repo_dir.join("cache").join("plugin")
}

/// A loaded plugin's reader, shareable with a thread of its own.
struct SharedReader(Arc<Loaded>);

impl tmi_format::reader::FormatReader for SharedReader {
    fn info(&self) -> &tmi_format::types::PluginInfo {
        self.0.reader.info()
    }
    fn probe(&self, name: &str, head: &[u8], size: u64) -> tmi_format::types::Confidence {
        self.0.reader.probe(name, head, size)
    }
    fn read_structure(&self, src: &tmi_format::reader::Source, cx: &tmi_format::reader::ReadCx) -> std::result::Result<tmi_format::types::Structure, ReadError> {
        self.0.reader.read_structure(src, cx)
    }
}

/// The process's plugin host (made on the first plugin load, with the
/// first repository's `plugin.*` settings).
pub fn host(repo: &Repo) -> std::result::Result<PluginHost, String> {
    static HOST: OnceLock<std::result::Result<PluginHost, String>> = OnceLock::new();
    HOST.get_or_init(|| {
        let settings = settings(repo).map_err(|e| e.to_string())?;
        PluginHost::new(settings).map_err(|e| e.to_string())
    })
    .clone()
}

/// `plugin.fuel` (instructions per call), `plugin.timeout` (seconds per
/// call) and `plugin.maxMemory` (bytes, or with a k / m / g suffix): the
/// repository's config, else the user's (PLUGINS.md 3.1).
pub fn settings(repo: &Repo) -> Result<Settings> {
    let repo_cfg = repo.read_config()?;
    let user_cfg = crate::config::read_user_config()?;
    let get = |key: &str| -> Result<Option<Value>> {
        Ok(match crate::config::get_value(&repo_cfg, key)? {
            Some(v) => Some(v.clone()),
            None => crate::config::get_value(&user_cfg, key)?.cloned(),
        })
    };
    let number = |key: &str, v: Value| -> Result<u64> {
        let bad = || RepoError::runtime(format!("{key} is not a positive number"));
        match v {
            Value::Int(tmi_store::json::Int::Small(n)) if n > 0 => Ok(n as u64),
            Value::Float(f) if f > 0.0 && f.is_finite() => Ok(f as u64),
            Value::Str(s) => {
                let t = s.trim().to_ascii_lowercase();
                let (digits, mul) = match t.chars().last() {
                    Some('k') => (&t[..t.len() - 1], 1u64 << 10),
                    Some('m') => (&t[..t.len() - 1], 1 << 20),
                    Some('g') => (&t[..t.len() - 1], 1 << 30),
                    _ => (t.as_str(), 1),
                };
                digits.trim().parse::<u64>().ok().filter(|n| *n > 0).map(|n| n.saturating_mul(mul)).ok_or_else(bad)
            }
            _ => Err(bad()),
        }
    };
    let mut s = Settings::default();
    if let Some(v) = get("plugin.fuel")? {
        s.fuel = Some(number("plugin.fuel", v)?);
    }
    if let Some(v) = get("plugin.timeout")? {
        s.timeout = Some(std::time::Duration::from_secs(number("plugin.timeout", v)?));
    }
    if let Some(v) = get("plugin.maxMemory")? {
        s.max_memory = number("plugin.maxMemory", v)? as usize;
    }
    s.workers = plugin_workers(s.workers, s.max_memory as u64, tmi_import::limit::total_ram_bytes());
    // measurements only (tools/bench_plugins.py, PLUGINS.md 6.4 item 7):
    // `TMI_PLUGIN_WORKERS=K` sets 6.1's K over the memory budget; unset,
    // the default is unchanged
    if let Some(k) = std::env::var("TMI_PLUGIN_WORKERS").ok().and_then(|v| v.trim().parse::<usize>().ok()).filter(|k| *k > 0) {
        s.workers = k;
    }
    Ok(s)
}

/// PLUGINS.md 6.1's K: at most `cores` (min(cores, 8), the host's
/// default) instances at once, and no more than the memory budget holds
/// at `plugin.maxMemory` each. The budget is half the machine's memory,
/// as for `core.maxImagePixels` (pixel_limit.py); unknown memory leaves K
/// as it is. At least 1. (The deferred layers of a file bound it too: a
/// call per layer, never more at once than there are layers.)
pub fn plugin_workers(cores: usize, max_memory: u64, ram: Option<u64>) -> usize {
    let budget = match ram {
        Some(ram) if ram > 0 && max_memory > 0 => (ram / 2 / max_memory).max(1) as usize,
        _ => usize::MAX,
    };
    cores.min(budget).max(1)
}

/// The environment variable that names development plugins: a program, a
/// JSON array of a program and its arguments, or a JSON array of such
/// arrays (`tmi_plugin_host::dev::parse_env`). `tmiki plugin dev <program>
/// -- <command>` sets it for the command it runs.
pub const DEV_ENV: &str = "TMI_PLUGIN_DEV";

/// The development plugins `TMI_PLUGIN_DEV` names, started on first use
/// (once per process, with `plugin.timeout` as their time limit per call),
/// by id. Empty when the variable is not set (nothing is started).
pub fn dev_plugins(repo: &Repo) -> &'static std::result::Result<Vec<Arc<Loaded>>, String> {
    static DEV: OnceLock<std::result::Result<Vec<Arc<Loaded>>, String>> = OnceLock::new();
    DEV.get_or_init(|| {
        let Some(value) = std::env::var_os(DEV_ENV).filter(|v| !v.is_empty()) else { return Ok(Vec::new()) };
        let value = value.into_string().map_err(|_| format!("{DEV_ENV} is not valid Unicode"))?;
        let commands = tmi_plugin_host::dev::parse_env(&value)?;
        let timeout = settings(repo).map_err(|e| e.to_string())?.timeout;
        let mut out: Vec<Arc<Loaded>> = Vec::new();
        for cmd in commands {
            let shown = cmd.display();
            let backend = DevProcessBackend::start(cmd, DevOptions { timeout, ..DevOptions::default() }).map_err(|e| format!("{shown}: {e}"))?;
            let importer = backend.importer();
            if out.iter().any(|l| l.importer.id == importer.id) {
                return Err(format!("{shown}: a second development plugin '{}'", importer.id));
            }
            let backend: Arc<dyn PluginBackend> = Arc::new(backend);
            out.push(Arc::new(Loaded { reader: DynReader::from_backend(backend), importer }));
        }
        Ok(out)
    })
}

/// The development plugin `id`, if `TMI_PLUGIN_DEV` names one.
fn dev_plugin(repo: &Repo, id: &str) -> std::result::Result<Option<Arc<Loaded>>, Unavailable> {
    if std::env::var_os(DEV_ENV).is_none_or(|v| v.is_empty()) {
        return Ok(None);
    }
    match dev_plugins(repo) {
        Ok(list) => Ok(list.iter().find(|l| l.importer.id == id).cloned()),
        Err(e) => Err(Unavailable::Dev(e.clone())),
    }
}

fn classify(e: PluginError) -> Unavailable {
    match e {
        PluginError::Link(m) => Unavailable::Abi(m),
        other => Unavailable::Unreadable(other.to_string()),
    }
}

impl Formats {
    /// The working tree's `.tmiattributes` and `.tmiplugins/`.
    pub fn for_repo(repo: &Repo) -> Formats {
        Formats {
            attrs: OnceLock::new(),
            attrs_text: None,
            worktree: repo.worktree.clone(),
            components: Components::Worktree(repo.worktree.clone()),
            loaded: Mutex::new(HashMap::new()),
            structures: StructureCache::new(Some(structure_dir(repo))),
        }
    }

    /// The `.tmiattributes` and `.tmiplugins/` that `entries` record (the
    /// result of a merge, PLUGINS.md 4.5).
    pub fn from_entries(repo: &Repo, entries: &Entries) -> Result<Formats> {
        let blob = |e: &Value| e.get("source_blob_ref").and_then(Value::as_str).map(str::to_string);
        let attrs_text = match entries.get(attributes::FILE).and_then(blob) {
            Some(b) => Some(String::from_utf8_lossy(&repo.store.read_blob(&b)?).into_owned()),
            None => Some(String::new()),
        };
        let prefix = format!("{}/", attributes::PLUGIN_DIR);
        let mut stored = HashMap::new();
        for (path, e) in entries {
            if let Some(id) = path.strip_prefix(&prefix).and_then(|n| n.strip_suffix(".wasm")) {
                if let Some(b) = blob(e) {
                    stored.insert(id.to_string(), b);
                }
            }
        }
        Ok(Formats {
            attrs: OnceLock::new(),
            attrs_text,
            worktree: repo.worktree.clone(),
            components: Components::Stored(stored),
            loaded: Mutex::new(HashMap::new()),
            structures: StructureCache::new(Some(structure_dir(repo))),
        })
    }

    pub fn attributes(&self) -> &Attributes {
        self.attrs.get_or_init(|| match &self.attrs_text {
            Some(text) => Attributes::parse(text),
            None => Attributes::load(self.worktree.as_deref()).unwrap_or_default(),
        })
    }

    /// What `rel` maps to.
    pub fn mapping(&self, rel: &str) -> Mapping {
        self.attributes().lookup(rel)
    }

    /// The bytes of component `id`.
    fn component_bytes(&self, repo: &Repo, id: &str) -> std::result::Result<Vec<u8>, Unavailable> {
        match &self.components {
            Components::Worktree(None) => Err(Unavailable::Missing),
            Components::Worktree(Some(root)) => {
                let path = root.join(attributes::PLUGIN_DIR).join(format!("{id}.wasm"));
                match std::fs::read(&path) {
                    Ok(b) => Ok(b),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(Unavailable::Missing),
                    Err(e) => Err(Unavailable::Unreadable(e.to_string())),
                }
            }
            Components::Stored(map) => match map.get(id) {
                None => Err(Unavailable::Missing),
                Some(b) => repo.store.read_blob(b).map_err(|e| Unavailable::Unreadable(e.to_string())),
            },
        }
    }

    fn load(&self, repo: &Repo, id: &str) -> std::result::Result<Arc<Loaded>, Unavailable> {
        // a development plugin of the same id stands in for the component
        // (`tmiki plugin dev`; the user named it, so no trust question)
        if let Some(dev) = dev_plugin(repo, id)? {
            return Ok(dev);
        }
        let bytes = self.component_bytes(repo, id)?;
        // decided before anything of it is compiled or run (deny leaves no .cwasm)
        trust::check_before_load(repo, id, &tmi_store::oid::sha256_hex(&bytes)).map_err(Unavailable::Denied)?;
        let host = host(repo).map_err(Unavailable::Unreadable)?;
        let plugin = host.load(&bytes).map_err(classify)?;
        if plugin.info.id != id {
            return Err(Unavailable::Unreadable(format!("the component says it is plugin '{}'", plugin.info.id)));
        }
        let importer = plugin.importer();
        if !importer.abi.starts_with("tmi:format@0.1.") {
            return Err(Unavailable::Abi(format!("it exports {}, this tmi reads tmi:format 0.1", importer.abi)));
        }
        let backend: Arc<dyn PluginBackend> = Arc::new(plugin);
        let loaded = Loaded { reader: DynReader::from_backend(backend), importer };
        trust::check_read(repo, &loaded.component()).map_err(Unavailable::Denied)?;
        Ok(Arc::new(loaded))
    }

    /// Plugin `id`, loaded once per command (checked, compiled or mapped
    /// from the cache, trusted). No lock is held while it loads.
    pub fn plugin(&self, repo: &Repo, id: &str) -> std::result::Result<Arc<Loaded>, Unavailable> {
        let slot: Slot = {
            let mut map = self.loaded.lock().unwrap_or_else(|e| e.into_inner());
            map.entry(id.to_string()).or_default().clone()
        };
        slot.get_or_init(|| self.load(repo, id)).clone()
    }

    /// Plugin `id` if this command has already loaded it (and it can be
    /// used); never loads it (no compile, no trust notice).
    fn already_loaded(&self, id: &str) -> Option<Arc<Loaded>> {
        let slot = self.loaded.lock().unwrap_or_else(|e| e.into_inner()).get(id).cloned()?;
        slot.get()?.as_ref().ok().cloned()
    }

    /// PLUGINS.md 6.1: the next file of a bulk `add` (`rel`, holding `bytes`
    /// with sha256 `digest`) has its structure read on a thread of its own
    /// while the current file decodes, when it maps to a plugin this command
    /// has already loaded. Whether `rel` maps to a plugin that is loaded.
    pub fn prefetch(&self, rel: &str, name: &str, bytes: &Arc<Vec<u8>>, digest: &str) -> bool {
        let Some(id) = self.mapping(rel).plugin_id().map(str::to_string) else { return false };
        let Some(loaded) = self.already_loaded(&id) else { return false };
        if !structure_cache::usable(&loaded.importer) {
            return false;
        }
        let importer = loaded.importer.clone();
        self.structures.prefetch(Arc::new(SharedReader(loaded)), &importer, name.to_string(), Arc::clone(bytes), digest);
        true
    }

    /// Whether `rel` maps to a plugin this command has already loaded.
    pub fn maps_to_loaded_plugin(&self, rel: &str) -> bool {
        self.mapping(rel).plugin_id().is_some_and(|id| self.already_loaded(id).is_some())
    }

    /// For a file mapped to a plugin that cannot be used: the plugin's id
    /// and why. None for every other file.
    pub fn unavailable(&self, repo: &Repo, rel: &str) -> Option<(String, Unavailable)> {
        let id = self.mapping(rel).plugin_id()?.to_string();
        match self.plugin(repo, &id) {
            Ok(_) => None,
            Err(u) => Some((id, u)),
        }
    }

    /// Imports the working file `rel` (`name`: its last path component)
    /// holding `bytes` with sha256 `digest`, as `.tmiattributes` says.
    #[allow(clippy::too_many_arguments)]
    pub fn import(&self, repo: &Repo, rel: &str, name: &str, bytes: &[u8], digest: &str, opts: &Options, warns: &mut Warnings) -> Result<ImportedFile> {
        let mapping = self.mapping(rel);
        match &mapping.format {
            Format::Builtin => tmi_format::import_file(name, bytes, digest, opts, warns).map_err(|d| builtin_decline(repo, rel, d.0, opts)),
            Format::Binary => Ok(tmi_import::opaque(name, bytes, digest)),
            Format::Invalid(msg) => Err(RepoError::runtime(format!("{rel}: {msg}"))),
            Format::Plugin(id) => {
                let loaded = self.plugin(repo, id).map_err(|u| RepoError::runtime(not_available_message(rel, id, &u)))?;
                self.import_with(repo, &loaded, mapping.on_error, rel, name, bytes, digest, opts, warns)
            }
        }
    }

    /// `import` of a plugin-format file with `loaded`.
    #[allow(clippy::too_many_arguments)]
    pub fn import_with(
        &self,
        repo: &Repo,
        loaded: &Loaded,
        on_error: OnError,
        rel: &str,
        name: &str,
        bytes: &[u8],
        digest: &str,
        opts: &Options,
        warns: &mut Warnings,
    ) -> Result<ImportedFile> {
        let i = &loaded.importer;
        let plugin = format!("plugin '{}' {}", i.id, i.version);
        // (the reason for on-error=binary's warning, the refusal)
        // what the plugin said about these bytes before, if kept (6.2)
        let result = self.structures.import_plugin(&loaded.reader, i, name, bytes, digest, opts, warns);
        let (why, refusal) = match result {
            Ok(f) => return Ok(f),
            // the host's verdict on the plugin (a crash, its limits): "plugin
            // 'bad' 0.1.0 crashed (panic: ...)" (issue #51)
            Err(ImportError::Read(ReadError::Plugin(m))) => {
                let m = m.strip_prefix(&format!("plugin {} {}: ", i.id, i.version)).unwrap_or(&m).to_string();
                let refusal = format!("{rel}: {plugin} {m}{ON_ERROR_HINT}");
                (m, refusal)
            }
            // the pixel limit, said as for the built-in formats (issue #17)
            Err(ImportError::Read(ReadError::TooLarge(m))) if too_large_pixels(&m).is_some() => {
                let pixels = too_large_pixels(&m).unwrap_or(0);
                let why = format!("it has {pixels} pixels, over core.maxImagePixels ({})", opts.limits.raster);
                (why, pixel_limit_message(repo, rel, pixels, opts.limits.raster))
            }
            Err(ImportError::Read(e)) => {
                let why = read_error_text(&e);
                let refusal = format!("{rel}: {plugin} could not read it: {why}{ON_ERROR_HINT}");
                (why, refusal)
            }
            Err(ImportError::Decline(d)) => {
                let refusal = format!("{rel}: {plugin} could not read it: {}{ON_ERROR_HINT}", d.0);
                (d.0, refusal)
            }
        };
        match on_error {
            OnError::Binary => {
                warns.lines.push(format!("tmiki: warning: could not read {rel} with {plugin} ({why}); tracking as opaque binary (on-error=binary)"));
                Ok(tmi_import::opaque(name, bytes, digest))
            }
            OnError::Refuse => Err(RepoError::runtime(refusal)),
        }
    }
}

/// The pixel count of the host's pixel-limit messages ("image of WxH
/// pixels ...", "canvas of N pixels ..."; `tmi_format::recipe::too_large`,
/// `pipeline::check_canvas`). None for a plugin's own `too-large` text.
fn too_large_pixels(m: &str) -> Option<u128> {
    let rest = m.strip_prefix("image of ").or_else(|| m.strip_prefix("canvas of "))?;
    let count = rest.split_once(" pixels")?.0;
    match count.split_once('x') {
        Some((w, h)) => Some(w.parse::<u128>().ok()?.max(1) * h.parse::<u128>().ok()?.max(1)),
        None => count.parse().ok(),
    }
}

/// What a built-in reader's decline becomes. The pixel limit (a canvas, an
/// image or a single layer over `core.maxImagePixels`) is an error said here,
/// never forwarded: Python has no per-layer limit and would decode a layer of
/// any size. Every other decline is Python's to handle.
fn builtin_decline(repo: &Repo, rel: &str, why: String, opts: &Options) -> RepoError {
    // `import_file` prefixes a built-in reader's error: "mdp2: too large: ..."
    let m = why.strip_prefix("mdp2: ").or_else(|| why.strip_prefix("clip: ")).unwrap_or(&why);
    let m = m.strip_prefix("too large: ").unwrap_or(m);
    let Some(pixels) = too_large_pixels(m) else { return RepoError::forward(why) };
    let lower = rel.to_ascii_lowercase();
    let limit = if lower.ends_with(".mdp") || lower.ends_with(".clip") { opts.limits.layered } else { opts.limits.raster };
    RepoError::runtime(pixel_limit_message(repo, rel, pixels, limit))
}

/// pixel_limit.py `_too_large_message` for an image a plugin describes
/// (the raster limit): where the limit comes from and how to raise it.
fn pixel_limit_message(repo: &Repo, rel: &str, pixels: u128, limit: u64) -> String {
    const KEY: &str = "core.maxImagePixels";
    let suggestion = pixels.div_ceil(1_000_000) * 1_000_000;
    let what = format!("{rel}: canvas too large: {pixels} pixels");
    if !std::env::var("TMI_MAX_IMAGE_PIXELS").unwrap_or_default().trim().is_empty() {
        return format!("{what} (limit {limit}, set by TMI_MAX_IMAGE_PIXELS). Raise it with TMI_MAX_IMAGE_PIXELS={suggestion}  (0 = no limit)");
    }
    let set_in = |cfg: Result<Value>| cfg.ok().and_then(|c| crate::config::get_value(&c, KEY).ok().flatten().filter(|v| !v.is_null()).cloned());
    if set_in(repo.read_config()).is_some() {
        return format!("{what} (limit {limit}, set by {KEY} in the repo config). Raise it with: tmiki config {KEY} {suggestion}  (0 = no limit)");
    }
    if set_in(crate::config::read_user_config()).is_some() {
        return format!("{what} (limit {limit}, set by {KEY} in the user config). Raise it with: tmiki config --global {KEY} {suggestion}  (0 = no limit)");
    }
    let basis = match tmi_import::limit::total_ram_bytes() {
        Some(ram) => format!("based on {:.1} GB of memory", ram as f64 / (1u64 << 30) as f64),
        None => "this machine's memory could not be detected".to_string(),
    };
    format!(
        "{what} (limit {limit} for images, {basis}); this machine may not have enough memory to process it. If it does, raise it with: tmiki config {KEY} {suggestion}  (0 = no limit)"
    )
}

/// Whether `entry` was recorded by a development plugin (`importer.
/// component_sha256` is `"dev"`).
pub fn is_dev_record(entry: &Value) -> bool {
    entry.get("importer").and_then(|i| i.get("component_sha256")).and_then(Value::as_str) == Some(pipeline::DEV_COMPONENT)
}

/// PLUGINS.md 5: a commit (and a merge commit, and a stash) never holds a
/// record a development plugin made: nothing pins what such a plugin did,
/// so nobody could read the file the same way again. Python's
/// `refuse_dev_records` (repo.py) says the same.
pub fn refuse_dev_records<'a>(entries: impl IntoIterator<Item = (&'a String, &'a Value)>) -> Result<()> {
    let mut dev: Vec<&str> = entries.into_iter().filter(|(_, e)| is_dev_record(e)).map(|(p, _)| p.as_str()).collect();
    if dev.is_empty() {
        return Ok(());
    }
    dev.sort_unstable();
    dev.dedup();
    let mut list = dev.iter().take(5).copied().collect::<Vec<_>>().join(", ");
    if dev.len() > 5 {
        list += &format!(", +{} more", dev.len() - 5);
    }
    let them = if dev.len() == 1 { "it" } else { "them" };
    Err(RepoError::runtime(format!(
        "{list}: recorded with a development plugin (tmiki plugin dev); a commit cannot hold such records. Add {them} again without {DEV_ENV} first"
    )))
}

/// Tracked paths (HEAD and index, not deleted) that `attrs` maps to plugin
/// `id`, sorted.
pub fn tracked_mapped(repo: &Repo, attrs: &Attributes, id: &str) -> Result<Vec<String>> {
    let head = repo.entries_for_commit(repo.current_commit()?.as_deref())?;
    let index = repo.index_entries()?;
    let mut rels: Vec<&String> = head.keys().chain(index.keys()).collect();
    rels.sort();
    rels.dedup();
    Ok(rels
        .into_iter()
        .filter(|rel| !index.get(*rel).or_else(|| head.get(*rel)).is_some_and(crate::stage::is_deleted))
        .filter(|rel| attrs.lookup(rel).plugin_id() == Some(id))
        .cloned()
        .collect())
}

/// Issue #50: for each of `paths` (just removed, or staged by `add` /
/// `commit -a`) that is a component `.tmiplugins/<id>.wasm` no longer
/// tracked while the working tree's `.tmiattributes` still maps files to
/// plugin `<id>`, the warning that those files cannot be added without it
/// (and how to stop using the plugin). Python's `attributes.removed_component_warnings`
/// says the same.
pub fn removed_component_warnings(repo: &Repo, paths: &[String]) -> Result<Vec<String>> {
    let mut out = Vec::new();
    if !paths.iter().any(|p| attributes::component_id(p).is_some()) {
        return Ok(out);
    }
    let head = repo.entries_for_commit(repo.current_commit()?.as_deref())?;
    let index = repo.index_entries()?;
    let attrs = Attributes::load(repo.worktree.as_deref())?;
    for rel in paths {
        let Some(id) = attributes::component_id(rel) else { continue };
        let tracked = match index.get(rel) {
            Some(e) => !crate::stage::is_deleted(e),
            None => head.contains_key(rel),
        };
        if tracked || attrs.patterns_for(id).is_empty() {
            continue;
        }
        let n = tracked_mapped(repo, &attrs, id)?.len();
        out.push(format!(
            "warning: {} still maps files to plugin '{id}' ({n} tracked {}); without {rel} they cannot be added or committed. To stop using the plugin, run 'tmiki plugin remove {id}'",
            attributes::FILE,
            if n == 1 { "file" } else { "files" }
        ));
    }
    Ok(out)
}

/// The ids of the components in `.tmiplugins/` and of the plugins
/// `.tmiattributes` names, sorted.
pub fn plugin_ids(repo: &Repo, formats: &Formats) -> Vec<String> {
    let mut ids = formats.attributes().plugin_ids();
    if let Some(root) = &repo.worktree {
        if let Ok(dir) = std::fs::read_dir(root.join(attributes::PLUGIN_DIR)) {
            for e in dir.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if let Some(id) = name.strip_suffix(".wasm") {
                    if attributes::valid_id(id) && !ids.iter().any(|i| i == id) {
                        ids.push(id.to_string());
                    }
                }
            }
        }
    }
    ids.sort();
    ids
}

/// One plugin made ready by [`prepare`].
pub struct Prepared {
    pub id: String,
    /// The plugin, or why it cannot be used.
    pub result: std::result::Result<Arc<Loaded>, Unavailable>,
}

/// `tmiki plugin prepare` and the end of `clone` (PLUGINS.md 4.3): every
/// plugin of the working tree checked, compiled into the cache when it is
/// not there (`on_compile(id)` first: the "compiling plugin ..." line) and
/// trusted (the notice of `plugin.trust=notice`).
pub fn prepare(repo: &Repo, formats: &Formats, on_compile: &mut dyn FnMut(&str)) -> Vec<Prepared> {
    let mut out = Vec::new();
    for id in plugin_ids(repo, formats) {
        if let (Ok(bytes), Ok(host)) = (formats.component_bytes(repo, &id), host(repo)) {
            if !host.is_cached(&tmi_store::oid::sha256_hex(&bytes)) {
                on_compile(&id);
            }
        }
        let result = formats.plugin(repo, &id);
        out.push(Prepared { id, result });
    }
    out
}

/// Why a plugin could not read a file, as a clause after "could not read
/// it: " (issue #51: words, not the WIT's variant names).
fn read_error_text(e: &ReadError) -> String {
    match e {
        ReadError::NotThisFormat(m) => format!("it does not look like a file this plugin reads ({m})"),
        ReadError::Corrupt(m) => format!("the file is damaged ({m})"),
        ReadError::Unsupported(m) => format!("it uses something the plugin does not support ({m})"),
        ReadError::TooLarge(m) => format!("it is too large ({m})"),
        ReadError::Plugin(m) => m.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hosts_pixel_limit_messages_give_their_pixels() {
        assert_eq!(too_large_pixels("image of 200000x200000 pixels is over core.maxImagePixels: Python reports it"), Some(40_000_000_000));
        assert_eq!(too_large_pixels("image of 0x5 pixels is over core.maxImagePixels: Python reports it"), Some(5));
        assert_eq!(too_large_pixels("canvas of 1234 pixels is over core.maxImagePixels: Python reports it"), Some(1234));
        assert_eq!(too_large_pixels("stack.xml over 16 MiB"), None);
    }

    /// An `.mdp` of a 16x16 canvas with one layer of `lw` x `lh` and no tiles.
    fn mdp_with_layer(lw: i64, lh: i64) -> Vec<u8> {
        let xml = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Mdipack width=\"16\" height=\"16\" dpi=\"72\"><Layers><Layer name=\"L1\" id=\"1\" parentId=\"-1\" width=\"{lw}\" height=\"{lh}\" type=\"32bpp\" bin=\"b1\" alpha=\"255\" visible=\"true\"/></Layers></Mdipack>"
        );
        // one tile (its zlib data is never reached: the size is refused first)
        let mut payload = Vec::new();
        for v in [1u32, 128, 0, 0, 0, 4] {
            payload.extend_from_slice(&v.to_le_bytes());
        }
        payload.extend_from_slice(&[0x78, 0x9c, 0, 0]);
        let mut pac = vec![0u8; 132];
        pac[0..4].copy_from_slice(b"PAC ");
        pac[4..8].copy_from_slice(&((132 + payload.len()) as u32).to_le_bytes());
        pac[12..16].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        pac[68..70].copy_from_slice(b"b1");
        pac.extend_from_slice(&payload);
        let mut data = b"mdipack\0".to_vec();
        data.extend_from_slice(&0u32.to_le_bytes());
        data.extend_from_slice(&(xml.len() as u32).to_le_bytes());
        data.extend_from_slice(&(pac.len() as u32).to_le_bytes());
        data.extend_from_slice(xml.as_bytes());
        data.extend_from_slice(&pac);
        data
    }

    #[test]
    fn a_layer_over_the_pixel_limit_is_refused_here_not_forwarded() {
        let dir = std::env::temp_dir().join(format!("tmi-repo-limit-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".tmi")).unwrap();
        let repo = Repo::open_at(&dir).unwrap();
        let formats = Formats::for_repo(&repo);
        let blobs = tmi_import::LayerBlobs::All;
        let opts = Options {
            render_composite: false,
            cache: None,
            reuse_pixels: None,
            reuse_decoded: None,
            layer_blobs: &blobs,
            composite_base: None,
            limits: tmi_import::Limits { layered: 100_000, raster: 100_000 },
            verify_partial: false,
            progress: None,
        };
        let run = |lw: i64, lh: i64| {
            let data = mdp_with_layer(lw, lh);
            let mut warns = Warnings::default();
            formats.import(&repo, "big.mdp", "big.mdp", &data, "d", &opts, &mut warns)
        };
        // the audit's file: a 574-byte MDP2 whose layer is 2^31-1 x 1
        let e = run(i32::MAX as i64, 1).err().expect("a layer of 2^31 pixels must be refused");
        assert!(e.is_runtime(), "{e}");
        let msg = e.message().unwrap();
        assert!(msg.starts_with("big.mdp: canvas too large: 2147483647 pixels (limit 100000"), "{msg}");
        // (a layer under the limit gets past the size check: its tile is junk)
        let e = run(100, 100).err().expect("junk tile data");
        assert!(e.is_forward(), "{e}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn deny_refuses_a_cloned_component_before_any_of_it_is_compiled_or_run() {
        let dir = std::env::temp_dir().join(format!("tmi-repo-deny-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".tmi")).unwrap();
        std::fs::create_dir_all(dir.join(attributes::PLUGIN_DIR)).unwrap();
        std::fs::write(dir.join(".tmi").join("config"), br#"{"version":1,"plugin":{"trust":"deny"}}"#).unwrap();
        // not even a component: compiling it would fail, so a refusal that
        // comes after the compile would say "unreadable", not "denied"
        std::fs::write(dir.join(attributes::PLUGIN_DIR).join("x.wasm"), b"\0asm hostile").unwrap();
        let repo = Repo::open_at(&dir).unwrap();
        let formats = Formats::for_repo(&repo);
        match formats.plugin(&repo, "x") {
            Err(Unavailable::Denied(why)) => assert!(why.contains("deny"), "{why}"),
            Err(other) => panic!("not refused by plugin.trust first: {}", other.why("x")),
            Ok(_) => panic!("a denied plugin loaded"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_commit_refuses_development_records() {
        let parse = |s: &str| tmi_store::json::loads(s).unwrap();
        let dev = parse(r#"{"importer":{"component_sha256":"dev","id":"ora"}}"#);
        let real = parse(r#"{"importer":{"component_sha256":"9f2c","id":"ora"}}"#);
        let builtin = parse(r#"{"source_format":"png"}"#);
        let (a, b, c) = ("b.ora".to_string(), "a.ora".to_string(), "c.png".to_string());
        assert!(refuse_dev_records([(&a, &real), (&c, &builtin)]).is_ok());
        let e = refuse_dev_records([(&a, &dev), (&b, &dev), (&a, &dev), (&c, &builtin)]).unwrap_err();
        assert_eq!(
            e.message().unwrap_or_default(),
            "a.ora, b.ora: recorded with a development plugin (tmiki plugin dev); a commit cannot hold such records. Add them again without TMI_PLUGIN_DEV first"
        );
    }

    #[test]
    fn k_is_held_to_the_memory_budget() {
        const G: u64 = 1 << 30;
        assert_eq!(plugin_workers(8, G, Some(56 * G)), 8, "plenty of memory: the cores");
        assert_eq!(plugin_workers(8, G, Some(8 * G)), 4, "half of 8 GiB holds 4 instances of 1 GiB");
        assert_eq!(plugin_workers(8, 4 * G, Some(8 * G)), 1);
        assert_eq!(plugin_workers(8, 16 * G, Some(8 * G)), 1, "never below 1");
        assert_eq!(plugin_workers(6, G, None), 6, "memory unknown: the cores");
    }

    #[test]
    fn read_errors_are_said_in_words() {
        assert_eq!(read_error_text(&ReadError::Corrupt("x".into())), "the file is damaged (x)");
        assert_eq!(read_error_text(&ReadError::NotThisFormat("y".into())), "it does not look like a file this plugin reads (y)");
    }
}
