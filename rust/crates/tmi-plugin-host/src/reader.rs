//! A plugin behind `tmi_format::FormatReader` (and `FormatWriter`), so
//! `tmi_format::pipeline` decodes its recipes as it decodes the built-ins'
//! (PLUGINS.md 8.1), whatever runs it ([`PluginBackend`]: a wasm component
//! or a development executable).
//!
//! `read_layer` and `decode_chunk` are called by the pipeline from rayon's
//! threads, in parallel; for a wasm plugin each blocks only on a plugin
//! worker (`workers.rs`), with nothing locked, and gets an instance of its
//! own; a development plugin answers one call at a time.

use std::sync::Arc;

use tmi_format::reader::{FormatReader, FormatWriter, ReadCx, Source};
use tmi_format::write::WriteCx;
use tmi_format::types::*;

use crate::backend::PluginBackend;
use crate::plugin::{Plugin, PluginError};

/// A [`PluginBackend`] as a reader and writer. `BackendReader<WasmBackend>`
/// (= [`WasmReader`]) calls the component without a vtable;
/// `BackendReader<dyn PluginBackend>` (= [`DynReader`]) holds either kind.
pub struct BackendReader<B: PluginBackend + ?Sized> {
    backend: Arc<B>,
}

/// A wasm plugin's reader (P5's name).
pub type WasmReader = BackendReader<crate::plugin::WasmBackend>;

/// A reader of either backend (what a repository's `Formats` holds).
pub type DynReader = BackendReader<dyn PluginBackend>;

impl<B: PluginBackend + ?Sized> BackendReader<B> {
    pub fn from_backend(backend: Arc<B>) -> BackendReader<B> {
        BackendReader { backend }
    }

    pub fn backend(&self) -> &B {
        &self.backend
    }

    pub fn backend_arc(&self) -> &Arc<B> {
        &self.backend
    }

    fn label(&self) -> String {
        let i = self.backend.info();
        format!("plugin {} {}", i.id, i.version)
    }

    fn error(&self, e: PluginError) -> ReadError {
        match e {
            PluginError::Read(e) => e,
            other => ReadError::Plugin(format!("{}: {other}", self.label())),
        }
    }
}

impl WasmReader {
    pub fn new(plugin: Arc<Plugin>) -> WasmReader {
        BackendReader { backend: plugin }
    }

    pub fn plugin(&self) -> &Plugin {
        &self.backend
    }
}

impl<B: PluginBackend + ?Sized> FormatReader for BackendReader<B> {
    fn info(&self) -> &PluginInfo {
        self.backend.info()
    }

    /// A probe that fails (a trap, fuel) answers `No`: the reader is not
    /// picked. P5 never probes through the registry; P6 decides what a
    /// failing probe means for a file `.tmiattributes` maps to the plugin.
    fn probe(&self, name: &str, head: &[u8], size: u64) -> Confidence {
        let head = &head[..head.len().min(self.backend.info().probe_bytes as usize)];
        self.backend.probe(name, head, size).unwrap_or(Confidence::No)
    }

    fn read_structure(&self, src: &Source, _cx: &ReadCx) -> Result<Structure, ReadError> {
        self.backend.read_structure(src.name, src.data).map_err(|e| self.error(e))
    }

    fn read_layer(&self, src: &Source, context: &[u8], token: u32, _cx: &ReadCx) -> Result<Bitmap, ReadError> {
        self.backend.read_layer(src.name, src.data, context, token).map_err(|e| self.error(e))
    }

    fn decode_chunk(&self, codec_id: u32, input: &[u8], out_len: u32) -> Result<Vec<u8>, ReadError> {
        self.backend.decode_chunk(codec_id, input, out_len).map_err(|e| self.error(e))
    }
}

/// The writer (P7): `edit`'s resources go into the call and come back with
/// the blobs the plugin made. Whether the plugin writes at all is
/// `PluginBackend::can_write`; callers ask that first.
impl<B: PluginBackend + ?Sized> FormatWriter for BackendReader<B> {
    fn write_layers(&self, src: &Source, edits: &[LayerEdit], cx: &mut WriteCx) -> Result<WritePlan, WriteError> {
        let taken = std::mem::replace(cx, WriteCx::empty());
        let (r, back) = self.backend.write_layers(src.name, src.data, edits, taken);
        *cx = back;
        r.map_err(|e| match e {
            PluginError::Write(e) => e,
            other => WriteError::Plugin(format!("{}: {other}", self.label())),
        })
    }
}
