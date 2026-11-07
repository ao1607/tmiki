//! Write a tmi format plugin once, run it two ways (PLUGINS.md 7.1, P8).
//!
//! - Built for `wasm32-wasip2`, [`export_plugin!`] makes the crate a
//!   component of the `tmi:format/format-plugin` world (wit-bindgen): what
//!   `tmiki plugin add` puts in `.tmiplugins/<id>.wasm`.
//! - Built natively, [`dev_main`] runs the same code as a development
//!   plugin: a program that speaks the interface on its stdin and stdout
//!   (the CBOR RPC of `tmiki plugin dev`, crate `tmi-plugin-dev`). Try it on
//!   a repository with `tmiki plugin dev target/debug/my-plugin -- add a.ora`
//!   (its records cannot be committed), debug it with a native debugger,
//!   print to stderr.
//!
//! The plugin implements [`Plugin`] with the WIT's own types ([`Structure`],
//! [`Layer`], [`Bytes`], ...: wit-bindgen's, the same on both targets) and
//! calls the host through this crate: [`SourceFile`], [`codecs`], [`sqlite`],
//! [`xml`], [`log`], [`edit`]. A write plan is [`WritePlan`] (its blobs are
//! [`edit::Blob`]s).
//!
//! ```ignore
//! use tmi_plugin_sdk::*;
//!
//! pub struct Mine;
//! impl Plugin for Mine {
//!     fn info() -> PluginInfo { ... }
//!     fn probe(name: &str, head: &[u8], size: u64) -> Confidence { ... }
//!     fn read_structure(src: &SourceFile) -> Result<Structure, ReadError> { ... }
//! }
//! tmi_plugin_sdk::export_plugin!(Mine);          // the component (wasm32 only)
//!
//! // src/bin/mine-dev.rs (native):
//! fn main() { tmi_plugin_sdk::dev_main::<mine::Mine>() }
//! ```
//!
//! Natively, a plugin must not print to stdout (it is the pipe to tmi; use
//! `eprintln!`, which tmi shows as `tmiki: plugin <id> (dev): ...`), and host
//! calls are made from the thread that runs the call.

/// wit-bindgen's bindings of `tmi-format/wit` (world `format-plugin`). The
/// types are used as they are; on a native target their import functions
/// are unreachable (this crate's own functions call tmi instead).
#[allow(clippy::all, unused, unsafe_op_in_unsafe_fn)]
pub mod bindings {
    wit_bindgen::generate!({
        path: "../../crates/tmi-format/wit",
        world: "format-plugin",
        pub_export_macro: true,
        export_macro_name: "export_tmi_format",
        default_bindings_module: "tmi_plugin_sdk::bindings",
    });
}

pub use bindings::exports::tmi::format::writer::{LayerEdit, PropChange, WriteError};
pub use bindings::tmi::format::codecs::ImageHeader;
pub use bindings::tmi::format::edit::{Encoding, ImageEncoding, ZlibKernel};
pub use bindings::tmi::format::log::Level;
pub use bindings::tmi::format::sqlite::SqlValue;
pub use bindings::tmi::format::types::*;
pub use bindings::tmi::format::xml::Element;

#[cfg(target_arch = "wasm32")]
#[path = "wasm_sys.rs"]
mod sys;

#[cfg(not(target_arch = "wasm32"))]
#[path = "native_sys.rs"]
mod sys;

#[cfg(not(target_arch = "wasm32"))]
mod wire;

#[cfg(not(target_arch = "wasm32"))]
mod native;

#[cfg(not(target_arch = "wasm32"))]
pub use native::dev_main;

#[cfg(target_arch = "wasm32")]
#[doc(hidden)]
pub mod __guest;

/// The interface version this SDK speaks.
pub const ABI: &str = "tmi:format@0.1.0";

/// A format plugin: `tmi:format`'s `reader` (and `writer`) exports. The
/// functions are called on a fresh instance each (PLUGINS.md 3.5: a wasm
/// plugin keeps nothing between calls; a development plugin is one process,
/// but must not rely on that either).
pub trait Plugin {
    /// `reader.info`: id (`[a-z0-9-]{1,40}`), semver version, formats,
    /// how much of the file `probe` sees, whether it writes layers back.
    fn info() -> PluginInfo;

    /// `reader.probe`: whether the start of the file looks like the format.
    fn probe(name: &str, head: &[u8], size: u64) -> Confidence;

    /// `reader.read-structure`.
    fn read_structure(src: &SourceFile) -> Result<Structure, ReadError>;

    /// `reader.read-layer`: a layer `read_structure` left `deferred(token)`.
    fn read_layer(_src: &SourceFile, _context: &[u8], _token: u32) -> Result<Bitmap, ReadError> {
        Err(ReadError::Unsupported("this plugin defers no layers".into()))
    }

    /// `reader.decode-chunk`: one chunk of the plugin's own codec.
    fn decode_chunk(_codec_id: u32, _input: &[u8], _out_len: u32) -> Result<Vec<u8>, ReadError> {
        Err(ReadError::Unsupported("this plugin has no codec of its own".into()))
    }

    /// `writer.write-layers` (tmi calls it only when `info` says
    /// `can-write`). `edits` name their new pixels by index in `pixels`.
    fn write_layers(_src: &SourceFile, _edits: Vec<LayerEdit>, _pixels: Vec<edit::Pixels>) -> Result<WritePlan, WriteError> {
        Err(WriteError::Unsupported("this plugin does not write layers back".into()))
    }
}

/// `source.source-file`: the file being read (or written). Read only what
/// is needed: a 60 MB file is not copied whole.
pub struct SourceFile<'a>(pub(crate) sys::Source<'a>);

impl SourceFile<'_> {
    pub fn size(&self) -> u64 {
        self.0.size()
    }

    /// `len` bytes at `offset`; short only at the end of the file.
    pub fn read(&self, offset: u64, len: u32) -> Vec<u8> {
        self.0.read(offset, len)
    }

    /// The last element of the file's path (for messages and names only).
    pub fn name(&self) -> String {
        self.0.name()
    }
}

/// `writer.piece`.
pub enum Piece {
    /// Bytes of the file being written, copied by the host.
    Source(Span),
    /// The plugin's own bytes.
    Bytes(Vec<u8>),
    /// What `edit` encoded (each blob at most once).
    Blob(edit::Blob),
}

/// `writer.write-plan`: the pieces the host joins into the new file.
pub struct WritePlan {
    pub pieces: Vec<Piece>,
    pub warnings: Vec<Warning>,
}

/// `codecs`: the host's byte transforms and image headers (PLUGINS.md 3.3).
pub mod codecs {
    use super::*;

    /// The transforms of `codecs` over `data` (small metadata, such as an
    /// OpenRaster `stack.xml`); every stage at most `max_out` bytes.
    pub fn transform(data: &Bytes, codecs: &[Codec], max_out: u32) -> Result<Vec<u8>, String> {
        sys::transform(data, codecs, max_out)
    }

    pub fn crc32(data: &Bytes) -> u32 {
        sys::crc32(data)
    }

    pub fn sha256(data: &Bytes) -> Vec<u8> {
        sys::sha256(data)
    }

    /// The header of a PNG or `jpeg-v1` image (no pixels decoded).
    pub fn image_info(img: &EncodedImage) -> Result<ImageHeader, String> {
        sys::image_info(img)
    }
}

/// `sqlite`: the host's SQLite 3.50.4.
pub mod sqlite {
    use super::*;

    pub struct Database(pub(crate) sys::Database);

    impl Database {
        /// Read only (the image deserialized).
        pub fn open(image: &Bytes) -> Result<Database, String> {
            sys::Database::open(image, false).map(Database)
        }

        /// For write-back.
        pub fn open_writable(image: &Bytes) -> Result<Database, String> {
            sys::Database::open(image, true).map(Database)
        }

        pub fn query(&self, sql: &str, params: &[SqlValue]) -> Result<Vec<Vec<SqlValue>>, String> {
            self.0.query(sql, params)
        }

        pub fn execute(&self, sql: &str, params: &[SqlValue]) -> Result<u64, String> {
            self.0.execute(sql, params)
        }

        pub fn serialize(&self) -> Result<Vec<u8>, String> {
            self.0.serialize()
        }
    }
}

/// `xml`: roxmltree with tmi-clip's MDP2 rules (a plugin may parse XML
/// itself instead: PLUGINS.md 3.3 recommends it).
pub mod xml {
    use super::*;

    pub fn parse(text: &[u8]) -> Result<Vec<Element>, String> {
        sys::xml_parse(text)
    }
}

/// `log`: shown by `tmiki plugin test` and `TMI_PLUGIN_LOG=1` only.
pub mod log {
    use super::*;

    pub fn emit(level: Level, message: &str) {
        sys::log_emit(level, message)
    }
}

/// `edit`: the host's resources of a write-back (PLUGINS.md 2.1, P7).
pub mod edit {
    use super::*;

    /// New pixels the host holds.
    pub struct Pixels(pub(crate) sys::Pixels);

    impl Pixels {
        pub fn width(&self) -> u32 {
            self.0.width()
        }

        pub fn height(&self) -> u32 {
            self.0.height()
        }

        /// `(x0, y0, x1, y1)` of the pixels that are not clear.
        pub fn bbox(&self) -> Option<(u32, u32, u32, u32)> {
            self.0.bbox()
        }

        pub fn read(&self, x: u32, y: u32, w: u32, h: u32, layout: &Layout) -> Vec<u8> {
            self.0.read(x, y, w, h, layout)
        }
    }

    /// Bytes the host made; the plugin knows their length and CRC only.
    pub struct Blob(pub(crate) sys::Blob);

    impl Blob {
        pub fn len(&self) -> u64 {
            self.0.len()
        }

        pub fn is_empty(&self) -> bool {
            self.len() == 0
        }

        pub fn crc32(&self) -> u32 {
            self.0.crc32()
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn encode_tile(px: &Pixels, x: u32, y: u32, w: u32, h: u32, layout: &Layout, steps: &[Encoding]) -> Result<Blob, String> {
        sys::encode_tile(&px.0, x, y, w, h, layout, steps).map(Blob)
    }

    pub fn encode_image(px: &Pixels, enc: &ImageEncoding) -> Result<Blob, String> {
        sys::encode_image(&px.0, enc).map(Blob)
    }

    /// The edited document drawn by `generic-v1` on a clear background.
    pub fn composite_after_edit() -> Result<Pixels, String> {
        sys::composite_after_edit().map(Pixels)
    }

    /// At most `max_side` pixels on the longer side, the aspect kept.
    pub fn thumbnail(px: &Pixels, max_side: u32) -> Pixels {
        Pixels(sys::thumbnail(&px.0, max_side))
    }
}

/// Makes the crate the component of `plugin` (a type implementing
/// [`Plugin`]): its `reader` and `writer` exports. Only on `wasm32`; on a
/// native target it expands to nothing (run [`dev_main`] instead).
#[macro_export]
macro_rules! export_plugin {
    ($plugin:ty) => {
        #[cfg(target_arch = "wasm32")]
        const _: () = {
            struct TmiPluginGuest;

            impl $crate::__guest::ReaderGuest for TmiPluginGuest {
                fn info() -> $crate::PluginInfo {
                    <$plugin as $crate::Plugin>::info()
                }

                fn probe(name: ::std::string::String, head: ::std::vec::Vec<u8>, size: u64) -> $crate::Confidence {
                    <$plugin as $crate::Plugin>::probe(&name, &head, size)
                }

                fn read_structure(src: &$crate::__guest::GuestSource) -> ::std::result::Result<$crate::Structure, $crate::ReadError> {
                    <$plugin as $crate::Plugin>::read_structure(&$crate::__guest::source(src))
                }

                fn read_layer(
                    src: &$crate::__guest::GuestSource,
                    context: ::std::vec::Vec<u8>,
                    token: u32,
                ) -> ::std::result::Result<$crate::Bitmap, $crate::ReadError> {
                    <$plugin as $crate::Plugin>::read_layer(&$crate::__guest::source(src), &context, token)
                }

                fn decode_chunk(codec_id: u32, input: ::std::vec::Vec<u8>, out_len: u32) -> ::std::result::Result<::std::vec::Vec<u8>, $crate::ReadError> {
                    <$plugin as $crate::Plugin>::decode_chunk(codec_id, &input, out_len)
                }
            }

            impl $crate::__guest::WriterGuest for TmiPluginGuest {
                fn write_layers(
                    src: &$crate::__guest::GuestSource,
                    edits: ::std::vec::Vec<$crate::LayerEdit>,
                    pixels: ::std::vec::Vec<$crate::__guest::GuestPixels>,
                ) -> ::std::result::Result<$crate::__guest::GuestWritePlan, $crate::WriteError> {
                    $crate::__guest::write_layers::<$plugin>(src, edits, pixels)
                }
            }

            $crate::bindings::export_tmi_format!(TmiPluginGuest with_types_in $crate::bindings);
        };
    };
}
