//! The built-in CLIP reader (PLUGINS.md P3): `read_structure` describes
//! every leaf's offscreen as a recipe (the 256 px block grid, each block's
//! "4-byte LE length then zlib" span, groups `[a]` / `[gray]` / `[b,g,r,pad]`
//! per `research/clip-format.md` §3.3) where the tile sizes are ones the host
//! knows how to lay out, but the actual import always goes through
//! `native_pixels`, which calls `tmi_clip::tiles::decode_offscreen` exactly
//! as `tmi_import::clip`'s old `decode_leaf` did (unknown tile sizes become
//! transparent with a warning, duplicate/oversized tiles are composited
//! `over` in file order). Not for speed (the recipe's tight decode is as
//! fast): the recipe cannot say an unknown tile size's warning, which only a
//! decoded (not reused) leaf prints, nor that a tile of 0 bytes must still
//! be a whole zlib stream (PLUGINS.md "recipe の疎なデコード"). The composite
//! stays native forever
//! (`NativeComposite`): CLIP's own renderer (CanvasPreview, partial
//! composite, banded render) is unchanged.
//!
//! Escape hatches used (all on `FormatReader`, all default-off, all read
//! back from the `ClipExtra` this file stashes in `ReadCx`): `legacy_source_key`,
//! `legacy_extra_meta`, `legacy_extra_file_meta`, `legacy_pixel_source_always`,
//! `uses_layered_limit`, `has_native_composite` / `native_composite`,
//! `shows_layer_progress`, `ticks_every_non_folder_layer`, `native_pixels`,
//! `legacy_mid_progress`, `legacy_warnings`. None of this changes what is
//! recorded.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use rayon::prelude::*;

use tmi_clip::attr::{parse_offscreen_attribute, OffscreenAttribute};
use tmi_clip::container::{self, Chunk};
use tmi_clip::tiles;
use tmi_import::clip::{self as c, ClipFile, Node};
use tmi_import::crop::LayerCrop;
use tmi_import::{decline, pixel_source_key, semantic_digest, Options, Rendered, Res, Warnings};
use tmi_store::json::{self, DumpOptions, Int, Map, Value};
use tmi_store::oid::hex;

use crate::reader::{FormatReader, ReadCx, Source};
use crate::types::*;

pub struct ClipReader {
    info: PluginInfo,
}

impl Default for ClipReader {
    fn default() -> Self {
        ClipReader::new()
    }
}

impl ClipReader {
    pub fn new() -> ClipReader {
        ClipReader {
            info: PluginInfo {
                id: "clip".into(),
                version: "0.1.0".into(),
                formats: vec![FormatInfo { name: "CLIP STUDIO PAINT".into(), extensions: vec!["clip".into()], mime: None }],
                probe_bytes: 24,
                can_write: false,
            },
        }
    }
}

fn corrupt(msg: impl Into<String>) -> ReadError {
    ReadError::Corrupt(msg.into())
}

fn unsupported(msg: impl Into<String>) -> ReadError {
    ReadError::Unsupported(msg.into())
}

fn to_read_error(d: tmi_import::Decline) -> ReadError {
    corrupt(d.0)
}

fn json_text(v: Value) -> String {
    json::dumps(&v, &DumpOptions::DEFAULT)
}

fn int(v: i64) -> Value {
    Value::Int(Int::Small(v))
}

/// A `tmi_store::json::Value` (from `_clip_extra_layer_meta`) as a `MetaEntry`
/// (scalars kept as their own `MetaValue`; objects / arrays as `Json`).
fn value_meta(key: &str, v: &Value) -> MetaEntry {
    let mv = match v {
        Value::Null => MetaValue::Null,
        Value::Bool(b) => MetaValue::Boolean(*b),
        Value::Int(_) => MetaValue::Integer(v.as_i64().expect("Value::Int")),
        Value::Float(f) => MetaValue::Number(*f),
        Value::Str(s) => MetaValue::Text(s.clone()),
        Value::Object(_) | Value::Array(_) => MetaValue::Json(json_text(v.clone())),
    };
    MetaEntry { key: key.into(), value: mv }
}

fn opt_str_meta(key: &str, v: &Option<String>) -> MetaEntry {
    MetaEntry { key: key.into(), value: v.as_ref().map_or(MetaValue::Null, |s| MetaValue::Text(s.clone())) }
}

/// `_COMPOSITE_MAP` (`tmi_render::clip::composite_name`'s table, by id, but
/// into the WIT `blend-mode` enum directly instead of round-tripping
/// through its snake_case name): every id maps onto a `BlendMode` variant
/// whose `name()` already is that snake_case string (checked by
/// `wit_tests.rs`), so no `legacy_blend_mode` override is needed. `None`:
/// an unknown id, drawn (and recorded) as `Normal` after a warning.
fn blend_of(id: i64) -> Option<BlendMode> {
    use BlendMode::*;
    Some(match id {
        0 => Normal,
        1 => Darken,
        2 => Multiply,
        3 => ColorBurn,
        4 => LinearBurn,
        5 => Subtract,
        6 => DarkerColor,
        7 => Lighten,
        8 => Screen,
        9 => ColorDodge,
        10 => GlowDodge,
        11 => LinearDodge,
        12 => GlowAdd,
        13 => LighterColor,
        14 => Overlay,
        15 => SoftLight,
        16 => HardLight,
        17 => VividLight,
        18 => LinearLight,
        19 => PinLight,
        20 => HardMix,
        21 => Difference,
        22 => Exclusion,
        23 => Hue,
        24 => Saturation,
        25 => Color,
        26 => Luminosity,
        30 => PassThrough,
        36 => Divide,
        _ => return None,
    })
}

/// What a node's conversion keeps for `legacy_extra_meta`.
struct LegacyInfo {
    extra: Vec<(String, MetaValue)>,
}

/// A leaf, for `native_pixels` and `legacy_source_key`: its MainId and its
/// `pixel_source` key, computed on first use. The key hashes the whole
/// offscreen payload (by far the largest cost of reading the structure), so
/// it is left to the pipeline's parallel per-layer step, where the old
/// importer computed it too, right before deciding whether to decode.
struct Leaf {
    main_id: i64,
    key: OnceLock<String>,
}

/// Stashed in `ReadCx` by `read_structure`: `Arc`s so `native_pixels` /
/// `native_composite` / `legacy_source_key` can clone them out and release
/// `ReadCx`'s lock before decoding, rendering or hashing (decoding and
/// rendering use rayon internally; P2's deadlock lesson, PLUGINS.md 6.1,
/// applies here too).
struct ClipExtra {
    file: Arc<ClipFile>,
    leaves: HashMap<String, Arc<Leaf>>,
    legacy: HashMap<String, LegacyInfo>,
    notes: Arc<Notes>,
}

/// `pixel_source_key("clip", "", [Attribute, *_clip_exta_pieces])`, as the
/// old `decode_leaf` computed it (`None`: not a leaf with an offscreen).
fn source_key(raw: &[u8], file: &ClipFile, main_id: i64) -> Option<String> {
    let info = file.leaf_infos.get(&main_id)?.as_ref()?;
    let exta = file.extas.get(&info.outer_id)?;
    let pieces = c::exta_pieces(raw, exta);
    Some(pixel_source_key("clip", "", &[&[info.attribute.as_slice()], pieces.as_slice()]))
}

/// What the import has to say on stderr, kept apart so `legacy_warnings`
/// can replay it in the order `tmi_import::clip`'s old `import` printed it
/// (whatever order the parallel decode ran in): every decoded leaf's unknown
/// tile sizes in tree order, then the render's report, then the unknown
/// composite ids in `convert`'s post-order.
struct Notes {
    /// The leaves (MainIds), in tree (pre-)order.
    leaf_order: Vec<i64>,
    /// `decode_offscreen`'s unknown tile sizes, by leaf MainId.
    tile_sizes: Mutex<HashMap<i64, Vec<(usize, usize)>>>,
    /// The layered render's report (`native_composite`), if it ran.
    report: Mutex<Option<tmi_render::clip::band::Report>>,
    /// Composite ids `blend_of` does not know, in post-order (children first).
    unknown_composites: Vec<i64>,
}

impl Notes {
    fn replay(&self, w: &mut Warnings) {
        let tile_sizes = self.tile_sizes.lock().unwrap_or_else(|e| e.into_inner());
        for id in &self.leaf_order {
            for &(bpt, pixels) in tile_sizes.get(id).into_iter().flatten() {
                w.tile_size(bpt, pixels);
            }
        }
        if let Some(r) = self.report.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            c::replay(r, w);
        }
        for &id in &self.unknown_composites {
            w.composite(id);
        }
    }
}

/// A span of `t.compressed` (a sub-slice of `raw`) inside the source file:
/// the zlib bytes after the tile record's 4-byte little-endian length.
fn span_of(base: &[u8], sub: &[u8]) -> Span {
    let off = sub.as_ptr() as usize - base.as_ptr() as usize;
    Span { offset: off as u64, len: sub.len() as u64 }
}

const WHITE: Rgba8 = Rgba8 { r: 255, g: 255, b: 255, a: 255 };

/// A leaf's `pixel-source::tiles` recipe (research/clip-format.md §3.3): one
/// `Tile` per raw tile record whose byte count matches a known layout
/// (`tile_kind`); a tile of an unrecognized size is left out (both the
/// recipe and the native decode draw it as nothing, so the two still agree).
fn leaf_bitmap(raw: &[u8], exta: &Chunk, attr: &OffscreenAttribute) -> Bitmap {
    let payload = container::payload(raw, exta);
    let mut out = Vec::new();
    if let Some(raw_tiles) = tiles::scan_tiles(payload) {
        for t in &raw_tiles {
            // No CSP tile is over 256 x 256 (`tiles::decode_offscreen` refuses
            // one); the recipe leaves it out rather than describe a bomb.
            if t.tile_w > tiles::TILE_SIZE || t.tile_h > tiles::TILE_SIZE {
                continue;
            }
            let pixels = t.tile_w * t.tile_h;
            let Some(kind) = tiles::tile_kind(t.bytes_per_tile, pixels) else { continue };
            let (groups, tint): (Vec<Group>, Option<Rgba8>) = match kind {
                1 => (vec![Group { channels: vec![Channel::A], depth: Depth::OneBitMsb }], Some(WHITE)),
                2 => (
                    vec![
                        Group { channels: vec![Channel::A], depth: Depth::OneBitMsb },
                        Group { channels: vec![Channel::Gray], depth: Depth::OneBitMsb },
                    ],
                    None,
                ),
                3 => (vec![Group { channels: vec![Channel::A], depth: Depth::Eight }], Some(WHITE)),
                4 => (
                    vec![
                        Group { channels: vec![Channel::A], depth: Depth::Eight },
                        Group { channels: vec![Channel::Gray], depth: Depth::Eight },
                    ],
                    None,
                ),
                _ => (
                    vec![
                        Group { channels: vec![Channel::A], depth: Depth::Eight },
                        Group { channels: vec![Channel::B, Channel::G, Channel::R, Channel::Pad], depth: Depth::Eight },
                    ],
                    None,
                ),
            };
            let (Ok(x), Ok(y), Ok(w), Ok(h)) = (
                i32::try_from((t.block_index % attr.tile_cols.max(1)) * tiles::TILE_SIZE),
                i32::try_from((t.block_index / attr.tile_cols.max(1)) * tiles::TILE_SIZE),
                u32::try_from(t.tile_w),
                u32::try_from(t.tile_h),
            ) else {
                continue;
            };
            out.push(Tile {
                x,
                y,
                width: w,
                height: h,
                content: TileContent::Raster(Raster {
                    data: Bytes::Span(span_of(raw, t.compressed)),
                    codecs: vec![Codec::Zlib],
                    layout: Layout { groups, row_padding: 0, tint, premultiplied: false },
                }),
            });
        }
    }
    let (Ok(w), Ok(h)) = (u32::try_from(attr.pixel_width), u32::try_from(attr.pixel_height)) else {
        return Bitmap { size: Some((0, 0)), source: PixelSource::Tiles(Tiled { tiles: vec![], overlap: Overlap::Over }) };
    };
    Bitmap { size: Some((w, h)), source: PixelSource::Tiles(Tiled { tiles: out, overlap: Overlap::Over }) }
}

/// What a leaf needs decided at structure time (cheap: parses the offscreen
/// attribute and lists its tiles, never decodes or hashes one) before
/// `convert_node` can fill in its `Layer`: `None` when this node has no
/// usable pixel source (a folder, or a missing/unreadable offscreen link that
/// is not itself an error). Exactly the nodes `source_key` gives a key.
struct LeafDesc {
    bitmap: Bitmap,
    meta: [MetaEntry; 6],
    write_capability: &'static str,
}

fn leaf_desc(raw: &[u8], file: &ClipFile, node: &Node) -> Result<Option<LeafDesc>, ReadError> {
    if node.folder != 0 {
        return Ok(None);
    }
    let Some(Some(info)) = file.leaf_infos.get(&node.main_id) else { return Ok(None) };
    let Some(exta) = file.extas.get(&info.outer_id) else { return Ok(None) };
    let attr = parse_offscreen_attribute(&info.attribute).map_err(|e| unsupported(format!("unreadable CLIP offscreen attribute ({e})")))?;
    let writable = attr.channel_profile == "color-alpha-bgra";
    Ok(Some(LeafDesc {
        bitmap: leaf_bitmap(raw, exta, &attr),
        meta: [
            MetaEntry { key: "pixel_width".into(), value: MetaValue::Integer(attr.pixel_width as i64) },
            MetaEntry { key: "pixel_height".into(), value: MetaValue::Integer(attr.pixel_height as i64) },
            MetaEntry { key: "tile_cols".into(), value: MetaValue::Integer(attr.tile_cols as i64) },
            MetaEntry { key: "tile_rows".into(), value: MetaValue::Integer(attr.tile_rows as i64) },
            MetaEntry { key: "channel_profile".into(), value: MetaValue::Text(attr.channel_profile.clone()) },
            MetaEntry { key: "offscreen_id".into(), value: MetaValue::Integer(info.offscreen_id) },
        ],
        write_capability: if writable { "raster-pixels" } else { "metadata-only" },
    }))
}

/// `convert`'s per-node fields (everything but the recipe pixels, which
/// `leaf_desc` already computed): `Layer` and the matching `LegacyInfo`. An
/// unknown composite id is drawn and recorded as `Normal`; its warning is
/// `Notes::unknown_composites`'.
fn convert_node(raw: &[u8], file: &ClipFile, node: &Node, parent: Option<u32>, leaf: &Option<LeafDesc>) -> (Layer, LegacyInfo) {
    let extra = file.extra.get(&node.main_id).cloned().unwrap_or_default();
    let has_pixels = leaf.is_some();
    let kind = if node.folder != 0 {
        LayerKind::Folder
    } else if extra.get("text").is_some_and(Value::truthy) {
        LayerKind::Text
    } else if extra.get("vector_outer_ids").is_some_and(Value::truthy) {
        LayerKind::Vector
    } else if has_pixels {
        LayerKind::Raster
    } else if node.layer_type == Some(1584) {
        LayerKind::Paper
    } else if node.layer_type == Some(4098) {
        LayerKind::Adjustment
    } else {
        LayerKind::Unknown
    };
    let render_payload = if has_pixels { None } else { c::exta_payload_sha256(file, raw, node.render_outer_id.as_deref()) };
    let mask_payload = c::exta_payload_sha256(file, raw, node.mask_outer_id.as_deref());
    let opt = |s: &Option<String>| s.as_ref().map_or(Value::Null, |v| Value::Str(v.clone()));

    let mut semantic = Map::new();
    semantic.insert("name".into(), opt(&node.name));
    semantic.insert("type".into(), node.layer_type.map_or(Value::Null, int));
    semantic.insert("folder".into(), int(node.folder));
    semantic.insert("visible".into(), Value::Bool(node.visible));
    semantic.insert("opacity".into(), int(node.opacity));
    semantic.insert("composite".into(), int(node.composite));
    semantic.insert("clip".into(), int(node.clip));
    semantic.insert("masking".into(), int(node.masking));
    semantic.insert("offset_x".into(), int(node.offset_x));
    semantic.insert("offset_y".into(), int(node.offset_y));
    semantic.insert("uuid".into(), opt(&node.uuid));
    semantic.insert("render_payload_sha256".into(), opt(&render_payload));
    semantic.insert("mask_payload_sha256".into(), opt(&mask_payload));
    for (k, v) in &extra {
        semantic.insert(k.clone(), v.clone());
    }
    let digest = semantic_digest(&Value::Object(semantic));

    let mut meta = vec![
        MetaEntry { key: "main_id".into(), value: MetaValue::Integer(node.main_id) },
        MetaEntry { key: "type".into(), value: node.layer_type.map_or(MetaValue::Null, MetaValue::Integer) },
        MetaEntry { key: "folder".into(), value: MetaValue::Integer(node.folder) },
        MetaEntry { key: "composite_id".into(), value: MetaValue::Integer(node.composite) },
        opt_str_meta("render_outer_id", &node.render_outer_id),
        opt_str_meta("mask_outer_id", &node.mask_outer_id),
        opt_str_meta("render_payload_sha256", &render_payload),
        opt_str_meta("mask_payload_sha256", &mask_payload),
        MetaEntry { key: "masking".into(), value: MetaValue::Integer(node.masking) },
    ];
    match leaf {
        Some(l) => meta.extend(l.meta.iter().cloned()),
        None => {
            for key in ["pixel_width", "pixel_height", "tile_cols", "tile_rows", "channel_profile", "offscreen_id"] {
                meta.push(MetaEntry { key: key.into(), value: MetaValue::Null });
            }
        }
    }
    for (k, v) in &extra {
        meta.push(value_meta(k, v));
    }

    let blend = blend_of(node.composite).unwrap_or(BlendMode::Normal);
    let name = match &node.name {
        Some(n) if !n.is_empty() => n.clone(),
        _ => format!("layer-{}", node.main_id),
    };
    let write_capability = leaf.as_ref().map_or("metadata-only", |l| l.write_capability);
    let legacy = LegacyInfo {
        extra: vec![
            ("semantic_digest".into(), MetaValue::Text(digest)),
            ("semantic_digest_version".into(), MetaValue::Integer(2)),
            ("write_capability".into(), MetaValue::Text(write_capability.into())),
        ],
    };
    let layer = Layer {
        id: format!("clip:{}", node.main_id),
        stable_id: node.sid(),
        parent,
        name,
        kind,
        visible: node.visible,
        opacity: node.opacity as f64 / 256.0,
        blend,
        clip_to_below: node.clip != 0,
        offset_x: node.offset_x,
        offset_y: node.offset_y,
        pixels: match leaf { Some(l) => LayerPixels::Ready(l.bitmap.clone()), None => LayerPixels::None },
        meta,
        semantic: vec![],
        write: WriteCapability::None,
    };
    (layer, legacy)
}

impl FormatReader for ClipReader {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn probe(&self, _name: &str, head: &[u8], _size: u64) -> Confidence {
        if head.starts_with(b"CSFCHUNK") {
            Confidence::Yes
        } else {
            Confidence::No
        }
    }

    fn builtin(&self) -> bool {
        true
    }

    fn uses_layered_limit(&self) -> bool {
        true
    }

    fn has_native_composite(&self) -> bool {
        true
    }

    fn source_format(&self, _name: &str, _structure: &Structure) -> String {
        "clip".into()
    }

    fn read_structure(&self, src: &Source, cx: &ReadCx) -> Result<Structure, ReadError> {
        if std::env::var_os("TMI_RENDER_TRACE").is_some_and(|v| !v.is_empty()) {
            return Err(unsupported("TMI_RENDER_TRACE"));
        }
        let file = c::read(src.data).map_err(to_read_error)?;
        let cw = u32::try_from(file.width).map_err(|_| unsupported("CLIP canvas width out of range"))?;
        let ch = u32::try_from(file.height).map_err(|_| unsupported("CLIP canvas height out of range"))?;

        // The tree in pre-order (the order of `Structure.layers`), each node
        // with its parent's index; post-order for the composite warnings.
        fn walk<'a>(nodes: &'a [Node], parent: Option<u32>, pre: &mut Vec<(&'a Node, Option<u32>)>, post: &mut Vec<i64>) {
            for n in nodes {
                let idx = pre.len() as u32;
                pre.push((n, parent));
                walk(&n.children, Some(idx), pre, post);
                if blend_of(n.composite).is_none() {
                    post.push(n.composite);
                }
            }
        }
        let mut pre = Vec::new();
        let mut unknown_composites = Vec::new();
        walk(&file.root_children, None, &mut pre, &mut unknown_composites);

        // Every node's fields in parallel (the render / mask payload hashes
        // and the tile lists add up), then assembled in order (the first
        // failing node in tree order decides the error, as a serial walk
        // would). The leaves' `pixel_source` keys wait for `legacy_source_key`.
        let raw = src.data;
        let converted: Vec<Result<(Layer, LegacyInfo, bool), ReadError>> = pre
            .par_iter()
            .map(|&(node, parent)| {
                let leaf = leaf_desc(raw, &file, node)?;
                let (layer, info) = convert_node(raw, &file, node, parent, &leaf);
                Ok((layer, info, leaf.is_some()))
            })
            .collect();
        let mut layers = Vec::with_capacity(pre.len());
        let mut legacy = HashMap::with_capacity(pre.len());
        let mut leaves = HashMap::new();
        let mut leaf_order = Vec::new();
        for (r, &(node, _)) in converted.into_iter().zip(&pre) {
            let (layer, info, has_pixels) = r?;
            if node.folder == 0 {
                leaf_order.push(node.main_id);
            }
            if has_pixels {
                leaves.insert(layer.stable_id.clone(), Arc::new(Leaf { main_id: node.main_id, key: OnceLock::new() }));
            }
            legacy.insert(layer.stable_id.clone(), info);
            layers.push(layer);
        }
        drop(pre);

        let dpi = file.resolution;
        let chunk_count = file.chunk_count as i64;
        let exta_count = file.extas.len() as i64;
        let head_uuid = hex(&file.head_uuid);
        let notes = Notes { leaf_order, tile_sizes: Mutex::new(HashMap::new()), report: Mutex::new(None), unknown_composites };
        cx.put_extra(ClipExtra { file: Arc::new(file), leaves, legacy, notes: Arc::new(notes) });

        Ok(Structure {
            format: "clip".into(),
            canvas: Canvas { width: cw, height: ch, dpi: Some(dpi), profile: ColorProfile::None },
            layers,
            composite: Composite::None,
            file_meta: vec![
                MetaEntry { key: "chunks".into(), value: MetaValue::Integer(chunk_count) },
                MetaEntry { key: "exta_count".into(), value: MetaValue::Integer(exta_count) },
                MetaEntry { key: "head_uuid".into(), value: MetaValue::Text(head_uuid) },
            ],
            context: Vec::new(),
            warnings: Vec::new(),
        })
    }

    fn native_composite(&self, src: &Source, _structure: &Structure, cx: &ReadCx, opts: &Options) -> Option<Res<Rendered>> {
        // Clones the `Arc`s and releases `ReadCx`'s lock before `composite`,
        // which uses rayon internally (same reasoning as `builtin_mdp2`'s
        // `native_composite`: holding the lock into it risks starving the
        // pool when several layers' `native_pixels` wait on it in parallel).
        let (file, notes) = cx.with_extra::<ClipExtra, _>(|e| (e.file.clone(), e.notes.clone()))?;
        Some((|| -> Res<Rendered> {
            let (rendered, report) = c::composite(&file, src.data, opts)?;
            *notes.report.lock().unwrap_or_else(|e| e.into_inner()) = report;
            Ok(rendered)
        })())
    }

    /// Computed once per leaf, outside `ReadCx`'s lock (see `Leaf`).
    fn legacy_source_key(&self, src: &Source, cx: &ReadCx, layer: &Layer) -> Option<String> {
        let (file, leaf) = cx.with_extra::<ClipExtra, _>(|e| Some((e.file.clone(), e.leaves.get(&layer.stable_id)?.clone()))).flatten()?;
        let key = leaf.key.get_or_init(|| source_key(src.data, &file, leaf.main_id).expect("a leaf has a pixel source"));
        Some(key.clone())
    }

    fn legacy_pixel_source_always(&self) -> bool {
        true
    }

    fn legacy_extra_meta(&self, cx: &ReadCx, layer: &Layer) -> Vec<(String, MetaValue)> {
        cx.with_extra::<ClipExtra, _>(|e| e.legacy.get(&layer.stable_id).map(|l| l.extra.clone())).flatten().unwrap_or_default()
    }

    fn legacy_extra_file_meta(&self, _cx: &ReadCx) -> Vec<(String, String)> {
        vec![("write_capability".into(), "raster-pixels".into())]
    }

    fn shows_layer_progress(&self) -> bool {
        true
    }

    fn ticks_every_non_folder_layer(&self) -> bool {
        true
    }

    /// `tmi_clip::tiles::decode_offscreen` directly, as `tmi_import::clip`'s
    /// old `decode_leaf` did (its `pixel_source`-keyed reuse is the
    /// pipeline's, through `legacy_source_key`).
    fn native_pixels(&self, src: &Source, stable_id: &str, cx: &ReadCx) -> Option<Res<LayerCrop>> {
        let (file, main_id, notes) = cx
            .with_extra::<ClipExtra, _>(|e| {
                let main_id = e.leaves.get(stable_id)?.main_id;
                Some((e.file.clone(), main_id, e.notes.clone()))
            })
            .flatten()?;
        Some((|| -> Res<LayerCrop> {
            let info = file.leaf_infos.get(&main_id).and_then(|o| o.as_ref()).ok_or_else(|| decline("CLIP leaf info missing"))?;
            let exta = file.extas.get(&info.outer_id).ok_or_else(|| decline("CLIP exta chunk missing"))?;
            let attr = parse_offscreen_attribute(&info.attribute).map_err(|e| decline(format!("unreadable CLIP offscreen attribute ({e})")))?;
            // The offscreen's declared size sizes the decode's output: hold it
            // to the pixel limit (and to its canvas) before anything is allocated.
            let pixels = (attr.pixel_width as u128) * (attr.pixel_height as u128);
            if cx.max_pixels.is_some_and(|m| m != 0 && pixels > m as u128) {
                return Err(decline(format!("CLIP offscreen of {pixels} pixels is over core.maxImagePixels")));
            }
            if let Some(why) = tiles::offscreen_oversize(attr.pixel_width, attr.pixel_height, file.width.max(0) as usize, file.height.max(0) as usize) {
                return Err(decline(why));
            }
            let d = tiles::decode_offscreen(src.data, exta.payload_off, exta.payload_size, attr.tile_cols, attr.tile_rows, attr.pixel_width, attr.pixel_height)
                .map_err(|e| decline(format!("unreadable CLIP offscreen ({e})")))?;
            if !d.unknown_sizes.is_empty() {
                notes.tile_sizes.lock().unwrap_or_else(|e| e.into_inner()).insert(main_id, d.unknown_sizes);
            }
            Ok(LayerCrop::from_tight((attr.pixel_width, attr.pixel_height), d.x, d.y, d.w, d.h, d.rgba))
        })())
    }

    fn legacy_mid_progress(&self) -> Option<&'static str> {
        Some("reading layer metadata")
    }

    /// In the old importer's order (see `Notes`), into the caller's
    /// `Warnings` (de-duplicated across the files of one command, as before).
    fn legacy_warnings(&self, cx: &ReadCx, w: &mut Warnings) {
        if let Some(notes) = cx.with_extra::<ClipExtra, _>(|e| e.notes.clone()) {
            notes.replay(w);
        }
    }
}

/// The recipe decode of every leaf that has one, against `native_pixels`
/// (`tmi_clip::tiles::decode_offscreen`), on the example CLIP files: the
/// "straightforward reference" comparison PLUGINS.md P3 asks for. A leaf
/// whose recipe ended up with no tiles at all (every raw tile had an
/// unrecognized size, or the offscreen's tile table did not parse) has
/// nothing to compare and is skipped, like `builtin_mdp2`'s test skips a
/// layer without a bin.
#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::recipe::{decode_bitmap, decode_layer_crop, DecodeEnv};

    fn example_files() -> Vec<PathBuf> {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../example");
        let Ok(entries) = std::fs::read_dir(&dir) else { return Vec::new() };
        entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("clip")))
            .collect()
    }

    fn check_file(raw: &[u8], label: &str) -> usize {
        let reader = ClipReader::new();
        let src = Source::new("x.clip", raw);
        let cx = ReadCx::new(None);
        let structure = reader.read_structure(&src, &cx).unwrap_or_else(|e| panic!("{label}: read_structure: {e}"));
        let env = DecodeEnv { source: raw, reader: Some(&reader as &dyn FormatReader), cx: &cx };
        let mut compared = 0;
        for layer in &structure.layers {
            let LayerPixels::Ready(bitmap) = &layer.pixels else { continue };
            let PixelSource::Tiles(t) = &bitmap.source else { continue };
            if t.tiles.is_empty() {
                continue; // nothing recognizable to compare (see the module doc)
            }
            let Ok(decoded) = decode_bitmap(&env, bitmap) else { continue };
            let got = LayerCrop::from_full(&decoded.data, decoded.width, decoded.height);
            let want = reader.native_pixels(&src, &layer.stable_id, &cx).expect("a leaf with pixels").unwrap_or_else(|e| panic!("{label}: {}: native_pixels: {e}", layer.stable_id));
            assert_eq!(got.header(), want.header(), "{label}: {} header", layer.stable_id);
            assert_eq!(got.rgba, want.rgba, "{label}: {} pixels", layer.stable_id);
            assert_eq!(got.digest(), want.digest(), "{label}: {} digest", layer.stable_id);
            // the tight decode a plugin's recipe goes through
            let tight = decode_layer_crop(&env, bitmap).unwrap_or_else(|e| panic!("{label}: {}: tight decode: {e}", layer.stable_id));
            assert_eq!(tight.header(), want.header(), "{label}: {} tight header", layer.stable_id);
            assert_eq!(tight.rgba, want.rgba, "{label}: {} tight pixels", layer.stable_id);
            compared += 1;
        }
        compared
    }

    #[test]
    fn recipe_matches_native_pixels_on_examples() {
        let files = example_files();
        assert!(!files.is_empty(), "example/*.clip not found (is the worktree junction set up?)");
        let mut total = 0;
        for path in &files {
            let raw = std::fs::read(path).unwrap();
            total += check_file(&raw, &path.display().to_string());
        }
        assert!(total > 0, "no example CLIP had a leaf where the recipe applies");
    }

    /// The notes come out in the old importer's order whatever order the
    /// parallel decode filed them in, and an earlier file's notes (the same
    /// `Warnings`) still de-duplicate them.
    #[test]
    fn notes_replay_in_tree_order_and_stay_deduplicated() {
        let notes = Notes {
            leaf_order: vec![3, 1, 2],
            tile_sizes: Mutex::new(HashMap::from([(2, vec![(7, 65536)]), (1, vec![(5, 65536), (9, 65536)]), (3, vec![(9, 65536)])])),
            report: Mutex::new(None),
            unknown_composites: vec![41, 40, 41],
        };
        let mut w = Warnings::default();
        w.tile_size(7, 65536); // an earlier file's
        let before = w.lines.len();
        notes.replay(&mut w);
        let size = |b: usize| format!("tmiki: unknown CLIP tile size {b} for 65536-pixel block; rendering as transparent");
        let comp = |id: i64| format!("tmiki: unknown CLIP composite id {id}; rendering as normal");
        assert_eq!(w.lines[before..], [size(9), size(5), comp(41), comp(40)]);
    }

    /// What `native_pixels` says for each layer of `raw` that has an offscreen.
    fn pixel_errors(raw: &[u8], max_pixels: Option<u64>) -> Vec<String> {
        let reader = ClipReader::new();
        let src = Source::new("x.clip", raw);
        let cx = ReadCx::new(max_pixels);
        let structure = reader.read_structure(&src, &cx).expect("structure");
        structure.layers.iter().filter_map(|l| reader.native_pixels(&src, &l.stable_id, &cx)).map(|r| r.err().map(|e| e.to_string()).unwrap_or_default()).collect()
    }

    /// An offscreen declaring 100,000 x 100,000 pixels from a small file: held
    /// to the pixel limit, and (with no limit) to its canvas, before the
    /// decode sizes anything by it.
    #[test]
    fn an_offscreen_declaring_more_pixels_than_the_limit_is_refused() {
        let base = std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../example/tama.clip")).unwrap();
        let hostile = tmi_clip::testing::declared_offscreen(&base, 100_000, 100_000).unwrap();
        let limited = pixel_errors(&hostile, Some(200_000_000));
        assert!(limited.iter().any(|e| e.contains("core.maxImagePixels")), "{limited:?}");
        let unlimited = pixel_errors(&hostile, None);
        assert!(unlimited.iter().any(|e| e.contains("far larger than")), "{unlimited:?}");
        // The same file with an honest size decodes (no tiles: nothing painted).
        let honest = tmi_clip::testing::declared_offscreen(&base, 2894, 4093).unwrap();
        assert!(!pixel_errors(&honest, Some(200_000_000)).iter().any(|e| e.contains("core.maxImagePixels") || e.contains("far larger than")));
    }}
