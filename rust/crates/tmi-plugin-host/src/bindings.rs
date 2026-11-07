//! The host side of `tmi:format/format-plugin` (wasmtime's bindgen): the
//! imports of both worlds (`format-reader`'s and `edit`), so a reader-only
//! component and one with a writer link against the same linker; the
//! exports are reached one interface at a time (`reader` always, `writer`
//! when the component has it: `plugin.rs`). Every import may trap (a bad
//! resource handle is the plugin's fault, not the host's), so they all
//! return `wasmtime::Result`.

wasmtime::component::bindgen!({
    path: "../tmi-format/wit",
    world: "format-plugin",
    imports: { default: trappable },
    with: {
        "tmi:format/source.source-file": crate::host::SourceFileRes,
        "tmi:format/sqlite.database": crate::host::DatabaseRes,
        "tmi:format/edit.pixels": crate::host::PixelsRes,
        "tmi:format/edit.blob": crate::host::BlobRes,
    },
});

pub use exports::tmi::format::reader as reader_export;
pub use exports::tmi::format::writer as writer_export;
pub use tmi::format::types as wit;
