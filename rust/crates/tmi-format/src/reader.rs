//! The reading side of the interface: the source file, what a read carries
//! with it, and the traits a built-in format implements (PLUGINS.md 8.1).
//! A wasm plugin implements `FormatReader` in `tmi-plugin-host` (P5).

use std::any::Any;
use std::collections::HashMap;
use std::sync::Mutex;

use tmi_import::crop::LayerCrop;
use tmi_import::{Options, Rendered, Res, Warnings};
use tmi_pillow::image::Image;

use crate::types::*;

/// The source file (`source.source-file`): the bytes of the working file and
/// the last element of its path.
#[derive(Clone, Copy)]
pub struct Source<'a> {
    pub name: &'a str,
    pub data: &'a [u8],
}

impl<'a> Source<'a> {
    pub fn new(name: &'a str, data: &'a [u8]) -> Source<'a> {
        Source { name, data }
    }

    pub fn size(&self) -> u64 {
        self.data.len() as u64
    }

    /// `read(offset, len)`: short only at the end of the file.
    pub fn read(&self, offset: u64, len: u32) -> &'a [u8] {
        let start = (offset.min(self.data.len() as u64)) as usize;
        let end = start.saturating_add(len as usize).min(self.data.len());
        &self.data[start..end]
    }
}

/// Which decode of an image a `ReadCx` holds: a single span of the file,
/// no transforms before it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ImageKey {
    pub offset: u64,
    pub len: u64,
    pub codec: ImageCodec,
    pub frame: u32,
}

/// What a read shares between the reader and the host.
pub struct ReadCx {
    /// The raster pixel limit (`core.maxImagePixels`; None: no limit).
    pub max_pixels: Option<u64>,
    /// Images a built-in reader has already decoded while it decided what the
    /// file is. The recipe decoder takes one out instead of decoding the same
    /// bytes again (so a PNG is decoded once, as before the recipes).
    decoded: Mutex<HashMap<ImageKey, Image>>,
    /// A built-in's own scratch state, made in `read_structure` and read back
    /// by `legacy_source_key` / `legacy_extra_meta` / `native_composite`
    /// (PLUGINS.md 8.1's "escape hatch": wasm plugins never use this).
    extra: Mutex<Option<Box<dyn Any + Send>>>,
}

impl ReadCx {
    pub fn new(max_pixels: Option<u64>) -> ReadCx {
        ReadCx { max_pixels, decoded: Mutex::new(HashMap::new()), extra: Mutex::new(None) }
    }

    /// Offers the RGBA8 `image` as the decode of `key`.
    pub fn offer_decoded(&self, key: ImageKey, image: Image) {
        self.decoded.lock().unwrap_or_else(|e| e.into_inner()).insert(key, image);
    }

    /// The offered decode of `key`, if any (it is handed over once).
    pub fn take_decoded(&self, key: &ImageKey) -> Option<Image> {
        self.decoded.lock().unwrap_or_else(|e| e.into_inner()).remove(key)
    }

    /// Stashes `value` for later calls on the same read.
    pub fn put_extra<T: Any + Send>(&self, value: T) {
        *self.extra.lock().unwrap_or_else(|e| e.into_inner()) = Some(Box::new(value));
    }

    /// Runs `f` on the stashed value of type `T`, if any was stashed.
    pub fn with_extra<T: Any + Send, R>(&self, f: impl FnOnce(&T) -> R) -> Option<R> {
        let guard = self.extra.lock().unwrap_or_else(|e| e.into_inner());
        guard.as_ref().and_then(|b| b.downcast_ref::<T>()).map(f)
    }
}

/// A format reader: `reader.wit`'s `reader` interface, plus the few things
/// only a built-in can say (PLUGINS.md 8.1).
pub trait FormatReader: Send + Sync {
    fn info(&self) -> &PluginInfo;

    fn probe(&self, name: &str, head: &[u8], size: u64) -> Confidence;

    fn read_structure(&self, src: &Source, cx: &ReadCx) -> Result<Structure, ReadError>;

    /// A layer `read_structure` left `Deferred(token)`.
    fn read_layer(&self, _src: &Source, _context: &[u8], _token: u32, _cx: &ReadCx) -> Result<Bitmap, ReadError> {
        Err(ReadError::Unsupported("this reader defers no layers".into()))
    }

    /// One chunk of `Codec::Plugin(codec_id)`; `out_len` is the size the
    /// chunk must decode to when the host knows it (the plugin codec is the
    /// last of the tile's transforms), else 0.
    fn decode_chunk(&self, _codec_id: u32, _input: &[u8], _out_len: u32) -> Result<Vec<u8>, ReadError> {
        Err(ReadError::Unsupported("this reader has no codec of its own".into()))
    }

    /// A built-in's record is kept as it always was: no `importer`, and
    /// nothing added to the layers' `source_meta` (PLUGINS.md 4.2, Q3).
    fn builtin(&self) -> bool {
        false
    }

    /// The record's `source_format` (and `canvas_meta.source_format`).
    fn source_format(&self, _name: &str, _structure: &Structure) -> String {
        format!("plugin:{}", self.info().id)
    }

    /// Built-ins only: the `pixel_source` key a layer has always been
    /// recorded with (wasm plugins never have one: PLUGINS.md 3.3). It is
    /// also the reuse key: when `Options::reusable(stable_id, key)` holds a
    /// prior decode, the pipeline records that prior's `pixel_sha256` /
    /// `pixel_bbox` and decodes nothing (the old importers' `decode_leaf`,
    /// importers.py `_reusable_decode`). That prior is not always what a
    /// decode would give: a layer a `.tmiignore` pattern substituted is
    /// recorded with the working file's key but the digest it inherited from
    /// HEAD, and Python keeps recording that digest (no new blob) for as long
    /// as the key stays the same.
    fn legacy_source_key(&self, _src: &Source, _cx: &ReadCx, _layer: &Layer) -> Option<String> {
        None
    }

    /// Built-ins only: whether `pixel_source` is recorded on every layer
    /// (CLIP: `null` on a layer with no pixel source) rather than omitted
    /// when `legacy_source_key` returns `None` (MDP2's own, narrower,
    /// pre-existing behaviour, which this defaults to keep).
    fn legacy_pixel_source_always(&self) -> bool {
        false
    }

    /// Built-ins only: extra `source_meta` entries the host would otherwise
    /// compute from `Layer::semantic` / `Layer::write` (PLUGINS.md 4.2), kept
    /// exactly as a format recorded them before this trait existed. A format
    /// with nothing to add (the raster reader) returns nothing, as before.
    fn legacy_extra_meta(&self, _cx: &ReadCx, _layer: &Layer) -> Vec<(String, MetaValue)> {
        Vec::new()
    }

    /// Built-ins only: the `blend_mode` string as the format itself spelled
    /// it (MDP2's raw XML `mode`, never translated to the WIT `blend-mode`
    /// enum's canonical names). `None`: use `layer.blend.name()`.
    fn legacy_blend_mode(&self, _cx: &ReadCx, _layer: &Layer) -> Option<String> {
        None
    }

    /// Whether `read_structure` already checked the canvas against
    /// `core.maxImagePixels` using the *layered* threshold (MDP2 / CLIP: 100
    /// bytes/pixel) instead of the raster one (20 bytes/pixel).
    fn uses_layered_limit(&self) -> bool {
        false
    }

    /// Built-ins only: `canvas_meta.dpi` was always recorded as an integer
    /// (MDP2's `dpi` XML attribute), not the `canvas.dpi` float the WIT type
    /// holds.
    fn legacy_integer_dpi(&self) -> bool {
        false
    }

    /// Whether this reader draws its own composite instead of the embedded
    /// image / `render(generic-v1)` paths (PLUGINS.md 8.1's `NativeComposite`).
    fn has_native_composite(&self) -> bool {
        false
    }

    /// The native composite, when `has_native_composite` is true. `opts` is
    /// threaded through only for what the WIT `composite` variant cannot
    /// say: CLIP's incremental render against an earlier version
    /// (`Options::composite_base` / `verify_partial`; everything else
    /// (`render_composite`, caching, progress) the pipeline already applied
    /// before calling this).
    fn native_composite(&self, _src: &Source, _structure: &Structure, _cx: &ReadCx, _opts: &Options) -> Option<Res<Rendered>> {
        None
    }

    /// Whether the pipeline reports "decoding layers k/N" while this reader's
    /// layers decode (MDP2 / CLIP always did, through their own
    /// `LayerTicker`; P1 kept the raster reader silent, as the pre-recipe
    /// `raster.rs` was, so this defaults to false).
    fn shows_layer_progress(&self) -> bool {
        false
    }

    /// Built-ins only: whether the progress total (and one tick per layer)
    /// counts every non-folder layer, pixels or not (CLIP's old leaf-level
    /// `tick.tick()`, run unconditionally for every `folder == 0` node, even
    /// a text/vector/paper/adjustment layer with no offscreen at all) rather
    /// than only layers with a pixel source (MDP2's narrower, pre-existing
    /// count, which this default keeps: `false`).
    fn ticks_every_non_folder_layer(&self) -> bool {
        false
    }

    /// Built-ins only (PLUGINS.md 8.1's `NativePixels`): a faster decode of
    /// a `Ready` / `Deferred` layer straight to its `LayerCrop`, bypassing
    /// the generic recipe decoder, when `Some`. The recipe this reader wrote
    /// into the layer's `pixels` must still describe the same pixels (tested
    /// against it). Speed is no reason for it any more (the recipe's tiles
    /// decode straight into the crop, `recipe::sparse`; MDP2 dropped its
    /// own): only CLIP uses it, for what its recipe does not say
    /// (PLUGINS.md "recipe の疎なデコード").
    fn native_pixels(&self, _src: &Source, _stable_id: &str, _cx: &ReadCx) -> Option<Res<LayerCrop>> {
        None
    }

    /// Built-ins only: a progress line reported once, right after every
    /// layer's pixels (and the composite) have decoded, before the pipeline
    /// builds the records (CLIP's old "reading layer metadata", printed at
    /// that exact point by `tmi_import::clip::import` because that is when
    /// it read `_clip_extra_layer_meta` from the database; `builtin_clip`
    /// now reads that eagerly in `read_structure`, before decoding, but
    /// still reports the same line at the same point for byte-identical
    /// progress output, `tools/conformance.py`'s `progress` scenario).
    fn legacy_mid_progress(&self) -> Option<&'static str> {
        None
    }

    /// Built-ins only: extra top-level `source_meta` entries (PLUGINS.md
    /// 4.2's file-level metadata), for a reserved key (CLIP's own
    /// `write_capability`, recorded at the file level too, not only per
    /// layer) that `Structure::file_meta` itself may not carry
    /// (`validate::reserved_key` forbids it there exactly as for a layer).
    fn legacy_extra_file_meta(&self, _cx: &ReadCx) -> Vec<(String, String)> {
        Vec::new()
    }

    /// Built-ins only: the stderr lines the import collected along the way
    /// (CLIP's unknown tile size / composite id / filter id / unreadable
    /// offscreen notes), written into the command's `Warnings` (which
    /// de-duplicates them across files, as it always did). Not part of the
    /// record; called once, after the whole import.
    fn legacy_warnings(&self, _cx: &ReadCx, _warns: &mut Warnings) {}
}

/// A reader that can also write layers back (`writer`; P7). `edits` name
/// their new pixels by number in `cx` (`write::WriteCx`, the host's `edit`
/// resources); `write::write_checked` runs it and checks what it wrote.
pub trait FormatWriter: FormatReader {
    fn write_layers(&self, src: &Source, edits: &[LayerEdit], cx: &mut crate::write::WriteCx) -> Result<WritePlan, WriteError>;
}

pub use crate::write::Pixels;
