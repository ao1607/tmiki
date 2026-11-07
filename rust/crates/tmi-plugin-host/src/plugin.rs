//! Loading a component and calling it (PLUGINS.md 3, 5, 6.2).
//!
//! A [`PluginHost`] is one engine, one linker and one set of worker
//! threads. [`PluginHost::load`] checks a component's imports against the
//! allowed list (3.2), compiles it (or maps it from the cache), makes its
//! `InstancePre` and reads `info`. Each call then gets a **fresh instance**
//! in a fresh store (3.5): fuel, the epoch deadline and the memory limit are
//! the store's, so no call can see what another left behind, and the order
//! of calls cannot change an answer.

use std::sync::Arc;
use std::time::{Duration, Instant};

use wasmtime::component::{HasSelf, InstancePre, Linker, Resource};
use wasmtime::{Engine, Store, Trap};

use tmi_format::types as t;
use tmi_format::write::WriteCx;

use crate::bindings::{reader_export, writer_export, FormatPlugin};
use crate::cache;
use crate::convert;
use crate::engine::{self, Settings};
use crate::host::{HostState, SourceFileRes, SourceView};
use crate::workers::Workers;

/// How a load or a call ended when it did not give an answer.
#[derive(Clone, Debug, PartialEq)]
pub enum PluginError {
    /// Not a component wasmtime compiles.
    Compile(String),
    /// Imports outside what a plugin may have (PLUGINS.md 3.2), e.g.
    /// `wasi:sockets/tcp@0.2.6`.
    Imports(Vec<String>),
    /// Does not fit the linker or does not export `tmi:format/reader@0.1`.
    Link(String),
    /// `info` broke a rule (the id, the version, `probe-bytes`).
    Info(String),
    /// The call used up its fuel (the same place on every machine).
    Fuel { limit: u64 },
    /// The call ran past the time limit.
    Timeout { limit: Duration },
    /// The call wanted more linear memory than its limit.
    Memory { limit: usize },
    /// The call overflowed its wasm stack.
    StackOverflow,
    /// Any other trap (a panic, `unreachable`, a bad resource handle).
    Trap(String),
    /// The plugin's own answer: a `read-error`.
    Read(t::ReadError),
    /// The plugin's own answer: a `write-error`.
    Write(t::WriteError),
}

impl std::fmt::Display for PluginError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PluginError::Compile(m) => write!(f, "not a usable component: {m}"),
            PluginError::Imports(l) => write!(f, "the component imports what a plugin may not use: {}", l.join(", ")),
            PluginError::Link(m) => write!(f, "the component does not fit tmi:format 0.1: {m}"),
            PluginError::Info(m) => write!(f, "bad plugin info: {m}"),
            PluginError::Fuel { limit } => write!(f, "ran out of fuel ({limit} per call; plugin.fuel)"),
            PluginError::Timeout { limit } => write!(f, "ran past the time limit ({} s per call; plugin.timeout)", limit.as_secs_f64()),
            PluginError::Memory { limit } => write!(f, "went over its memory limit ({} MiB; plugin.maxMemory)", limit >> 20),
            PluginError::StackOverflow => write!(f, "overflowed its stack"),
            PluginError::Trap(m) => write!(f, "crashed ({m})"),
            PluginError::Read(e) => write!(f, "{e}"),
            PluginError::Write(e) => write!(f, "{e}"),
        }
    }
}

/// The sentence for a component that does not fit the pooling allocator's
/// slots (wasmtime refuses it when it is compiled or mapped): the slots are
/// sized for plugins written in Rust (PLUGINS.md Q14, `engine.rs`), and a
/// component that needs more was made another way (componentize-py's have
/// 38 core instances). None for any other error.
pub fn pooling_limit_message(error: &str) -> Option<String> {
    let has = |what: &str| -> Option<(u64, u64)> {
        // "The component transitively contains 38 core module instances,
        // which exceeds the configured maximum of 16 in the pooling allocator"
        let rest = &error[error.find("The component transitively contains ")? + "The component transitively contains ".len()..];
        let (n, rest) = rest.split_once(' ')?;
        let rest = rest.strip_prefix(what)?;
        let max = rest.split("configured maximum of ").nth(1)?.split(' ').next()?;
        Some((n.parse().ok()?, max.parse().ok()?))
    };
    let (n, max, what) = [("core module instances", "core instances"), ("Wasm linear memories", "memories"), ("tables", "tables")]
        .into_iter()
        .find_map(|(wasmtime_says, name)| has(wasmtime_says).map(|(n, max)| (n, max, name)))?;
    Some(format!(
        "it has {n} {what} and a plugin may have {max}: tmi runs format plugins written in Rust \
         ('tmiki plugin new' writes one on tmi-plugin-sdk); a component made from another language is larger than that"
    ))
}

struct Inner {
    engine: Engine,
    linker: Linker<HostState>,
    workers: Workers,
    settings: Settings,
}

/// The engine, the linker and the workers; cheap to clone.
#[derive(Clone)]
pub struct PluginHost {
    inner: Arc<Inner>,
}

impl PluginHost {
    /// A host for `settings`. Nothing of wasmtime starts before this.
    pub fn new(settings: Settings) -> Result<PluginHost, PluginError> {
        let engine = engine::engine(&settings).map_err(|e| PluginError::Compile(format!("the engine: {e:#}")))?;
        if settings.timeout.is_some() {
            engine::start_epoch_ticker(&engine);
        }
        let mut linker = Linker::<HostState>::new(&engine);
        crate::wasi::add_to_linker(&mut linker).map_err(|e| PluginError::Link(format!("{e:#}")))?;
        FormatPlugin::add_to_linker::<HostState, HasSelf<HostState>>(&mut linker, |s| s).map_err(|e| PluginError::Link(format!("{e:#}")))?;
        let workers = Workers::new(settings.workers, engine::MAX_WASM_STACK + engine::HOST_STACK);
        Ok(PluginHost { inner: Arc::new(Inner { engine, linker, workers, settings }) })
    }

    pub fn settings(&self) -> &Settings {
        &self.inner.settings
    }

    pub fn engine(&self) -> &Engine {
        &self.inner.engine
    }

    /// One empty job to a worker and back (what every call pays besides
    /// wasm; PLUGINS.md 6.4 item 2 subtracts it).
    pub fn worker_roundtrip(&self) {
        self.inner.workers.run(|| ());
    }

    /// Whether the compiled code of the component with SHA-256 `sha256` is
    /// in the cache (a load maps it instead of compiling).
    pub fn is_cached(&self, sha256: &str) -> bool {
        let s = &self.inner.settings;
        s.cache_dir.as_deref().is_some_and(|dir| {
            dir.join(cache::key(&self.inner.engine, &engine::fingerprint(s))).join(format!("{sha256}.cwasm")).is_file()
        })
    }

    /// Checks, compiles (or maps from the cache) and prepares the component
    /// `bytes`, then reads its `info`.
    pub fn load(&self, bytes: &[u8]) -> Result<Plugin, PluginError> {
        let sha256 = tmi_store::oid::sha256_hex(bytes);
        let s = &self.inner.settings;
        let loaded = cache::load_or_compile(&self.inner.engine, &engine::fingerprint(s), bytes, &sha256, s.cache_dir.as_deref()).map_err(|e| {
            let text = format!("{e:#}");
            PluginError::Compile(pooling_limit_message(&text).unwrap_or(text))
        })?;
        let component = &loaded.component;
        let ty = component.component_type();
        let imports: Vec<String> = ty.imports(&self.inner.engine).map(|(name, _)| name.to_string()).collect();
        let exports: Vec<String> = ty.exports(&self.inner.engine).map(|(name, _)| name.to_string()).collect();
        let refused: Vec<String> = imports.iter().filter(|n| !crate::wasi::allowed_import(n)).cloned().collect();
        if !refused.is_empty() {
            return Err(PluginError::Imports(refused));
        }
        let t = Instant::now();
        let pre = self.inner.linker.instantiate_pre(component).map_err(|e| PluginError::Link(format!("{e:#}")))?;
        let reader = reader_export::GuestIndices::new(&pre).map_err(|e| PluginError::Link(format!("{e:#}")))?;
        // a component without the writer reads only (a writer that does not
        // fit the interface is a link error, as a bad reader is)
        let writer = match exports.iter().any(|e| e.starts_with("tmi:format/writer@")) {
            true => Some(writer_export::GuestIndices::new(&pre).map_err(|e| PluginError::Link(format!("{e:#}")))?),
            false => None,
        };
        let prepare = t.elapsed();
        let mut plugin = WasmBackend {
            host: self.clone(),
            sha256,
            pre,
            reader,
            writer,
            info: t::PluginInfo { id: String::new(), version: String::new(), formats: vec![], probe_bytes: 0, can_write: false },
            cache: loaded.outcome,
            compile: loaded.took,
            prepare,
            imports,
            exports,
        };
        let info = plugin.call(None, |store, ex, _| ex.reader.call_info(store))?;
        let info = convert::plugin_info(info);
        check_info(&info)?;
        plugin.info = info;
        Ok(plugin)
    }
}

/// The longest `id`, version and extension an info may name.
const MAX_VERSION: usize = 64;
const MAX_EXTENSION: usize = 16;
const MAX_EXTENSIONS: usize = 64;

/// Whether `v` is a semver (`1.2.3`, `1.2.3-rc.1+build.5`): three numbers,
/// then a pre-release and build of `[0-9A-Za-z-]` and `.`, no empty part.
pub fn is_semver(v: &str) -> bool {
    if v.is_empty() || v.len() > MAX_VERSION {
        return false;
    }
    let (rest, build) = match v.split_once('+') {
        Some((r, b)) => (r, Some(b)),
        None => (v, None),
    };
    let (core, pre) = match rest.split_once('-') {
        Some((c, p)) => (c, Some(p)),
        None => (rest, None),
    };
    let parts: Vec<&str> = core.split('.').collect();
    if parts.len() != 3 || parts.iter().any(|p| p.is_empty() || p.len() > 9 || !p.bytes().all(|b| b.is_ascii_digit())) {
        return false;
    }
    let ident = |s: &str| !s.is_empty() && s.split('.').all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'));
    pre.is_none_or(ident) && build.is_none_or(ident)
}

/// Whether `ext` (`ora`, `.ora`) is a file extension a plugin may claim:
/// `[a-z0-9]{1,16}` after one optional leading dot. These become
/// `.tmiattributes` patterns, so nothing a pattern or a line treats as
/// special may pass.
pub fn is_extension(ext: &str) -> bool {
    let e = ext.strip_prefix('.').unwrap_or(ext);
    (1..=MAX_EXTENSION).contains(&e.len()) && e.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

pub(crate) fn check_info(i: &t::PluginInfo) -> Result<(), PluginError> {
    let id_ok = (1..=40).contains(&i.id.len()) && i.id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if !id_ok {
        return Err(PluginError::Info(format!("the id {} is not [a-z0-9-]{{1,40}}", debug_line(&i.id))));
    }
    if !is_semver(&i.version) {
        return Err(PluginError::Info(format!("the version {} is not semver (1.2.3, 1.2.3-rc.1+build.5; at most {MAX_VERSION} characters)", debug_line(&i.version))));
    }
    if i.probe_bytes > 64 << 10 {
        return Err(PluginError::Info(format!("probe-bytes {} is over 64 KiB", i.probe_bytes)));
    }
    let count: usize = i.formats.iter().map(|f| f.extensions.len()).sum();
    if count > MAX_EXTENSIONS {
        return Err(PluginError::Info(format!("{count} file extensions are over the limit of {MAX_EXTENSIONS}")));
    }
    for f in &i.formats {
        for (what, text) in [("format name", Some(f.name.as_str())), ("mime type", f.mime.as_deref())] {
            if let Some(text) = text {
                if text.chars().count() > 80 || text.chars().any(|c| c.is_control()) {
                    return Err(PluginError::Info(format!("the {what} {} is over 80 characters or has a control character", debug_line(text))));
                }
            }
        }
        for ext in &f.extensions {
            if !is_extension(ext) {
                return Err(PluginError::Info(format!("the extension {} is not [a-z0-9]{{1,{MAX_EXTENSION}}} (one leading dot allowed)", debug_line(ext))));
            }
        }
    }
    Ok(())
}

/// `{:?}` of text a plugin made, its controls escaped, cut to 80 chars.
fn debug_line(s: &str) -> String {
    let cut: String = s.chars().take(80).collect();
    format!("\"{}\"{}", tmi_format::safe_text::line(&cut), if s.chars().count() > 80 { "..." } else { "" })
}
/// A loaded component: the wasm [`crate::PluginBackend`] (also named
/// [`Plugin`], its name before `tmiki plugin dev`).
pub struct WasmBackend {
    host: PluginHost,
    /// SHA-256 of the component's bytes (`importer.component_sha256`).
    pub sha256: String,
    pre: InstancePre<HostState>,
    reader: reader_export::GuestIndices,
    /// The `writer` export, when the component has one.
    writer: Option<writer_export::GuestIndices>,
    pub info: t::PluginInfo,
    /// How the compiled code was obtained, and how long that took.
    pub cache: cache::Outcome,
    pub compile: Duration,
    /// `instantiate_pre` and the export check.
    pub prepare: Duration,
    /// The component's imports (all allowed: [`crate::wasi::allowed_import`]).
    pub imports: Vec<String>,
    /// The component's exports (`tmi:format/reader@0.1.0`, ...).
    pub exports: Vec<String>,
}

impl WasmBackend {
    /// The interface version the component's reader is built against, as
    /// records name it: `tmi:format@0.1.0` (PLUGINS.md 4.2's `abi`).
    pub fn abi(&self) -> String {
        let version = self
            .exports
            .iter()
            .find_map(|e| e.strip_prefix("tmi:format/reader@"))
            .unwrap_or("0.1.0");
        format!("tmi:format@{version}")
    }

    /// The `importer` of the records this component makes.
    pub fn importer(&self) -> tmi_format::pipeline::Importer {
        tmi_format::pipeline::Importer {
            id: self.info.id.clone(),
            version: self.info.version.clone(),
            component_sha256: self.sha256.clone(),
            abi: self.abi(),
        }
    }
}

/// The wasm backend's old name (P5-P7).
pub type Plugin = WasmBackend;

impl crate::backend::PluginBackend for WasmBackend {
    fn info(&self) -> &t::PluginInfo {
        &self.info
    }

    fn importer(&self) -> tmi_format::pipeline::Importer {
        WasmBackend::importer(self)
    }

    fn can_write(&self) -> bool {
        WasmBackend::can_write(self)
    }

    fn probe(&self, name: &str, head: &[u8], size: u64) -> Result<t::Confidence, PluginError> {
        WasmBackend::probe(self, name, head, size)
    }

    fn read_structure(&self, name: &str, data: &[u8]) -> Result<t::Structure, PluginError> {
        WasmBackend::read_structure(self, name, data)
    }

    fn read_layer(&self, name: &str, data: &[u8], context: &[u8], token: u32) -> Result<t::Bitmap, PluginError> {
        WasmBackend::read_layer(self, name, data, context, token)
    }

    fn decode_chunk(&self, codec_id: u32, input: &[u8], out_len: u32) -> Result<Vec<u8>, PluginError> {
        WasmBackend::decode_chunk(self, codec_id, input, out_len)
    }

    fn write_layers(&self, name: &str, data: &[u8], edits: &[t::LayerEdit], cx: WriteCx) -> (Result<t::WritePlan, PluginError>, WriteCx) {
        WasmBackend::write_layers(self, name, data, edits, cx)
    }
}

/// What a call left besides its answer.
#[derive(Clone, Debug, Default)]
pub struct CallLog {
    pub lines: Vec<String>,
}

/// The exports of one instance.
pub struct Exports {
    pub reader: reader_export::Guest,
    pub writer: Option<writer_export::Guest>,
}

impl WasmBackend {
    pub fn host(&self) -> &PluginHost {
        &self.host
    }

    /// Whether the component writes layers back: it exports the `writer`
    /// and its `info` says `can-write` (P7; either alone is a reader).
    pub fn can_write(&self) -> bool {
        self.writer.is_some() && self.info.can_write
    }

    /// Runs `f` against a fresh instance on a worker thread. `source`: the
    /// call's file, given to `f` as a `source-file` handle.
    pub fn call<R: Send>(
        &self,
        source: Option<(&str, &[u8])>,
        f: impl FnOnce(&mut Store<HostState>, &Exports, Option<Resource<SourceFileRes>>) -> wasmtime::Result<R> + Send,
    ) -> Result<R, PluginError> {
        self.call_with(source, None, f).0
    }

    /// `call` with the `edit` resources of a write (`write`, handed back
    /// with the answer).
    fn call_with<R: Send>(
        &self,
        source: Option<(&str, &[u8])>,
        write: Option<WriteCx>,
        f: impl FnOnce(&mut Store<HostState>, &Exports, Option<Resource<SourceFileRes>>) -> wasmtime::Result<R> + Send,
    ) -> (Result<R, PluginError>, Option<WriteCx>) {
        let s = &self.host.inner.settings;
        let (fuel, timeout, max_memory, reverse) = (s.fuel, s.timeout, s.max_memory, s.sqlite_reverse_unordered_selects);
        let engine = &self.host.inner.engine;
        let (pre, reader, writer) = (&self.pre, &self.reader, &self.writer);
        let id = &self.info.id;
        self.host.inner.workers.run(move || {
            // SAFETY: `source` outlives the store, which is dropped at the
            // end of this closure, before `run` returns.
            let view = source.map(|(name, data)| unsafe { SourceView::new(name, data) });
            let mut state = HostState::new(view, max_memory);
            state.write = write;
            state.reverse_unordered_selects = reverse;
            let mut store = Store::new(engine, state);
            store.limiter(|s| &mut s.limiter);
            if let Some(f) = fuel {
                store.set_fuel(f).expect("fuel is configured");
            }
            if let Some(t) = timeout {
                store.set_epoch_deadline(engine::deadline_ticks(t));
                store.epoch_deadline_trap();
            }
            let result = (|| {
                let inst = pre.instantiate(&mut store)?;
                let exports = Exports {
                    reader: reader.load(&mut store, &inst)?,
                    writer: match writer {
                        Some(w) => Some(w.load(&mut store, &inst)?),
                        None => None,
                    },
                };
                let res = match store.data().source {
                    Some(_) => Some(store.data_mut().table.push(SourceFileRes)?),
                    None => None,
                };
                f(&mut store, &exports, res)
            })();
            show_log(id, store.data());
            let result = result.map_err(|e| classify(id, e, store.data(), fuel, timeout, max_memory));
            let write = store.data_mut().write.take();
            (result, write)
        })
    }

    /// `writer.write-layers` (not yet checked: `tmi_format::write`). `cx`
    /// holds the edits' new pixels (handles 0..) and is handed back with
    /// the blobs the plugin had made.
    pub fn write_layers(&self, name: &str, data: &[u8], edits: &[t::LayerEdit], cx: WriteCx) -> (Result<t::WritePlan, PluginError>, WriteCx) {
        if self.writer.is_none() {
            return (Err(PluginError::Write(t::WriteError::Unsupported(format!("plugin {} has no writer", self.info.id)))), cx);
        }
        let given = cx.pixel_count() as u32;
        let (r, cx) = self.call_with(Some((name, data)), Some(cx), move |store, ex, src| {
            let w = ex.writer.as_ref().expect("checked above");
            let list: Vec<writer_export::LayerEdit> = edits
                .iter()
                .map(|e| writer_export::LayerEdit { stable_id: e.stable_id.clone(), new_pixels: e.new_pixels, props: e.props.iter().map(convert::prop_change).collect() })
                .collect();
            let pixels: Vec<Resource<crate::host::PixelsRes>> = (0..given).map(Resource::new_own).collect();
            w.call_write_layers(store, src.expect("a source"), &list, &pixels)
        });
        let cx = cx.unwrap_or_else(WriteCx::empty);
        let r = match r {
            Ok(Ok(plan)) => Ok(convert::write_plan(plan)),
            Ok(Err(e)) => Err(PluginError::Write(convert::write_error(e))),
            Err(e) => Err(e),
        };
        (r, cx)
    }

    /// A fresh store and instance, and nothing called (PLUGINS.md 6.4 item
    /// 2 measures this).
    pub fn instantiate_only(&self) -> Result<(), PluginError> {
        self.call(None, |_, _, _| Ok(()))
    }

    /// `reader.probe`.
    pub fn probe(&self, name: &str, head: &[u8], size: u64) -> Result<t::Confidence, PluginError> {
        let c = self.call(None, |store, ex, _| ex.reader.call_probe(store, name, head, size))?;
        Ok(convert::confidence(c))
    }

    /// `reader.read-structure` (not yet checked: `tmi_format::validate`).
    pub fn read_structure(&self, name: &str, data: &[u8]) -> Result<t::Structure, PluginError> {
        let r = self.call(Some((name, data)), |store, ex, src| ex.reader.call_read_structure(store, src.expect("a source")))?;
        r.map(convert::structure).map_err(|e| PluginError::Read(convert::read_error(e)))
    }

    /// `reader.read-layer`.
    pub fn read_layer(&self, name: &str, data: &[u8], context: &[u8], token: u32) -> Result<t::Bitmap, PluginError> {
        let r = self.call(Some((name, data)), |store, ex, src| ex.reader.call_read_layer(store, src.expect("a source"), context, token))?;
        r.map(convert::bitmap).map_err(|e| PluginError::Read(convert::read_error(e)))
    }

    /// `reader.decode-chunk`.
    pub fn decode_chunk(&self, codec_id: u32, input: &[u8], out_len: u32) -> Result<Vec<u8>, PluginError> {
        let r = self.call(None, |store, ex, _| ex.reader.call_decode_chunk(store, codec_id, input, out_len))?;
        r.map_err(|e| PluginError::Read(convert::read_error(e)))
    }
}

/// `TMI_PLUGIN_LOG=1`: a call's `log.emit` lines, stdout and stderr, on
/// tmi's stderr (PLUGINS.md 2.1: never part of a record).
fn show_log(id: &str, s: &HostState) {
    if std::env::var_os("TMI_PLUGIN_LOG").is_none_or(|v| v.is_empty() || v == "0") {
        return;
    }
    let mut out = String::new();
    for (level, line) in &s.log {
        out.push_str(&format!("tmiki: plugin {id}: {}: {line}\n", if matches!(level, crate::bindings::tmi::format::log::Level::Debug) { "debug" } else { "info" }));
    }
    if s.log_truncated {
        out.push_str(&format!("tmiki: plugin {id}: (log over {} bytes; the rest is not shown)\n", crate::host::LOG_CAPACITY));
    }
    for (what, pipe) in [("stdout", &s.stdout), ("stderr", &s.stderr)] {
        let text = pipe.contents();
        for line in String::from_utf8_lossy(&text).lines() {
            out.push_str(&format!("tmiki: plugin {id}: {what}: {line}\n"));
        }
    }
    eprint!("{}", tmi_format::safe_text::message(&out));
}

fn classify(id: &str, e: wasmtime::Error, s: &HostState, fuel: Option<u64>, timeout: Option<Duration>, max_memory: usize) -> PluginError {
    match e.downcast_ref::<Trap>() {
        Some(Trap::OutOfFuel) => PluginError::Fuel { limit: fuel.unwrap_or(0) },
        Some(Trap::Interrupt) => PluginError::Timeout { limit: timeout.unwrap_or_default() },
        Some(Trap::StackOverflow) => PluginError::StackOverflow,
        _ if s.limiter.denied => PluginError::Memory { limit: max_memory },
        trap => PluginError::Trap(trap_reason(id, &e, trap, &String::from_utf8_lossy(&s.stderr.contents()))),
    }
}

/// What a user is told about a trap (issue #51): the panic message the
/// plugin printed on its stderr (a Rust plugin's `panicked at ...:` line and
/// the message after it), else the trap itself, in one line. The wasm
/// backtrace is for plugin authors: with `TMI_DEBUG_PLUGIN=1` it goes to
/// stderr, with the plugin's stderr, as `tmiki: plugin <id>: ...` lines.
fn trap_reason(id: &str, e: &wasmtime::Error, trap: Option<&Trap>, stderr: &str) -> String {
    if std::env::var_os("TMI_DEBUG_PLUGIN").is_some_and(|v| !v.is_empty() && v != "0") {
        let mut text = String::new();
        for line in format!("trap: {e:#}").lines() {
            text.push_str(&format!("tmiki: plugin {id}: {line}\n"));
        }
        for line in stderr.lines() {
            text.push_str(&format!("tmiki: plugin {id}: stderr: {line}\n"));
        }
        eprint!("{}", tmi_format::safe_text::message(&text));
    }
    match panic_message(stderr) {
        Some(m) => format!("panic: {m}"),
        None => match trap {
            Some(t) => t.to_string(),
            None => e.root_cause().to_string(),
        },
    }
}

/// The message of a Rust panic in `stderr`: the lines after `panicked at
/// <place>:` up to the `note:` line (or, from older toolchains, the text
/// in `panicked at '<message>', <place>`).
pub(crate) fn panic_message(stderr: &str) -> Option<String> {
    panic_message_raw(stderr).map(crate::convert::shown)
}

fn panic_message_raw(stderr: &str) -> Option<String> {
    let mut lines = stderr.lines();
    while let Some(line) = lines.next() {
        let Some(at) = line.find("panicked at ") else { continue };
        let rest = &line[at + "panicked at ".len()..];
        if let Some(quoted) = rest.strip_prefix('\'') {
            if let Some(end) = quoted.rfind("', ") {
                return Some(quoted[..end].to_string());
            }
        }
        let message: Vec<&str> = lines.by_ref().take_while(|l| !l.starts_with("note:")).map(str::trim).filter(|l| !l.is_empty()).collect();
        if !message.is_empty() {
            return Some(message.join(" "));
        }
    }
    None
}
