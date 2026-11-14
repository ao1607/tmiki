//! tmiki's host for wasm format plugins (PLUGINS.md 3, 5, 6; stage P5).
//!
//! A plugin is a WebAssembly component of the `tmi:format/format-reader`
//! world (`tmi-format/wit/tmi-format.wit`). This crate runs it:
//!
//! - [`engine`]: the wasmtime configuration of PLUGINS.md 3.1, in one
//!   function, and the epoch ticker. Nothing here starts until a plugin is
//!   loaded: `tmiki` commands that read no plugin never touch wasmtime.
//! - [`wasi`]: the deterministic WASI of 3.2 (empty filesystem, environment
//!   and stdin; clocks stopped at 0; ChaCha20 with a fixed seed; no sockets)
//!   and the list of imports a component may have (anything else, such as
//!   `wasi:sockets`, is refused when the component is loaded).
//! - [`host`]: the host helpers of the WIT (`source`, `codecs`, `sqlite`,
//!   `xml`, `log`) and the per-call store state with its limits.
//! - [`cache`]: compiled components (`.cwasm`) on disk, keyed by the
//!   component's SHA-256, the wasmtime version and the engine configuration.
//! - [`workers`]: the threads every wasm call runs on (big stacks, never a
//!   rayon thread; at most this many instances at once).
//! - [`plugin`]: a loaded component (`InstancePre`, its `info`), one fresh
//!   instance per call with fuel, the epoch deadline and the memory limit,
//!   and the errors a call ends in.
//! - [`reader`]: [`BackendReader`] ([`WasmReader`], [`DynReader`]), a
//!   `tmi_format::FormatReader` and `FormatWriter` over a plugin, for
//!   `tmi_format::pipeline` and `tmi_format::write`.
//! - [`backend`]: [`PluginBackend`], what runs a plugin's exports: the wasm
//!   component ([`WasmBackend`], also named [`Plugin`]) or a development
//!   executable ([`dev::DevProcessBackend`], `tmiki plugin dev`: PLUGINS.md
//!   7.1, P8; the CBOR RPC of crate `tmi-plugin-dev`, converted in
//!   [`devwire`]).
//!
//! A repository reaches its plugins through `tmi_repo::formats` (P6:
//! `.tmiattributes`, `.tmiplugins/<id>.wasm`, trust), which imports with
//! `tmi_format::pipeline::import_plugin` and [`Plugin::importer`]'s record.

pub mod backend;
pub mod bindings;
pub mod cache;
pub mod convert;
pub mod dev;
pub mod devwire;
pub mod dump;
pub mod engine;
pub mod host;
pub mod plugin;
pub mod reader;
pub mod wasi;
pub mod workers;

#[cfg(any(test, feature = "native-ora"))]
pub mod native_ora;
#[cfg(any(test, feature = "native-ora"))]
pub mod ora_files;
#[cfg(any(test, feature = "native-ora"))]
pub mod test_plugins;

pub use backend::PluginBackend;
pub use dev::{DevCommand, DevOptions, DevProcessBackend};
pub use engine::Settings;
pub use plugin::{is_plugin_id, Plugin, PluginError, PluginHost, WasmBackend};
pub use reader::{BackendReader, DynReader, WasmReader};

#[cfg(test)]
mod tests;

#[cfg(test)]
mod dev_tests;

#[cfg(test)]
mod format_limit_tests;

#[cfg(test)]
mod host_limits_tests;

#[cfg(test)]
mod hardening_tests;
