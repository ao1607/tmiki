//! What runs a plugin's exports (PLUGINS.md 7.1): a wasm component
//! ([`crate::WasmBackend`], the only kind a repository records) or a
//! development executable over the RPC of `tmiki plugin dev`
//! ([`crate::dev::DevProcessBackend`]). Both answer the same calls with
//! `tmi_format::types` and fail with the same [`PluginError`]s, so
//! [`crate::BackendReader`] puts either behind `tmi_format`'s
//! `FormatReader` / `FormatWriter` and the pipeline, the write-back check
//! and every command treat them alike.

use tmi_format::pipeline::Importer;
use tmi_format::types as t;
use tmi_format::write::WriteCx;

use crate::plugin::PluginError;

pub trait PluginBackend: Send + Sync {
    /// `reader.info`, read (and checked) when the backend was made.
    fn info(&self) -> &t::PluginInfo;

    /// The `importer` of the records it makes (`component_sha256` is
    /// `"dev"` for a development plugin).
    fn importer(&self) -> Importer;

    /// Whether it writes layers back: a `writer` export and `can-write`.
    fn can_write(&self) -> bool;

    fn probe(&self, name: &str, head: &[u8], size: u64) -> Result<t::Confidence, PluginError>;

    /// `reader.read-structure` (not yet checked: `tmi_format::validate`).
    fn read_structure(&self, name: &str, data: &[u8]) -> Result<t::Structure, PluginError>;

    fn read_layer(&self, name: &str, data: &[u8], context: &[u8], token: u32) -> Result<t::Bitmap, PluginError>;

    fn decode_chunk(&self, codec_id: u32, input: &[u8], out_len: u32) -> Result<Vec<u8>, PluginError>;

    /// `writer.write-layers` (not yet checked: `tmi_format::write`). `cx`
    /// holds the edits' new pixels (handles 0..) and comes back with the
    /// blobs the plugin had made.
    fn write_layers(&self, name: &str, data: &[u8], edits: &[t::LayerEdit], cx: WriteCx) -> (Result<t::WritePlan, PluginError>, WriteCx);
}
