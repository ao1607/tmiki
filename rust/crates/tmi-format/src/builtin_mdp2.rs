//! The built-in MDP2 reader (PLUGINS.md P2): `read_structure` describes
//! every renderable leaf layer's pixels as `pixel-source::tiles` (128 px
//! tiles, zlib, `overlap: over`, cropped by the bitmap), reproducing
//! `tmi_clip::mdp2::plan_layer` / `fill` exactly (`rust/crates/tmi-clip/src/mdp2.rs`).
//! The pipeline decodes those recipes itself (`recipe::sparse`, which never
//! allocates the whole layer), as it will a plugin's: no `native_pixels`.
//! The composite stays native (`NativeComposite`): generic-v1 equals MDP2's
//! own compositor on the example files and on most random ones, but not on
//! all (PLUGINS.md P4): MDP2 never draws a masking layer, whatever its kind,
//! and draws its leaves in file order, not in the tree's.
//!
//! A few things the WIT types cannot say are kept as this file's own escape
//! hatches on the `FormatReader` trait (`legacy_source_key`,
//! `legacy_extra_meta`, `legacy_blend_mode`, `legacy_integer_dpi`,
//! `uses_layered_limit`), all gated on `builtin()`: the raw XML blend-mode
//! string (never the WIT `blend-mode` enum's canonical names), `dpi` as an
//! integer, `semantic_digest` / `write_capability` with MDP2's own
//! computation, the `pixel_source` reuse key, and the layered (not raster)
//! `core.maxImagePixels` threshold. None of this changes what is recorded.

use std::collections::HashMap;
use std::sync::Arc;

use tmi_clip::mdp2::{self as m, Layer as MLayer, Mdp2, TileFormat};
use tmi_import::{decline, pixel_source_key, semantic_digest, Rendered, Res, Rgba};
use tmi_store::json::{self, DumpOptions, Map, Value};

use crate::reader::{FormatReader, ReadCx, Source};
use crate::types::*;

pub struct Mdp2Reader {
    info: PluginInfo,
}

impl Default for Mdp2Reader {
    fn default() -> Self {
        Mdp2Reader::new()
    }
}

impl Mdp2Reader {
    pub fn new() -> Mdp2Reader {
        Mdp2Reader {
            info: PluginInfo {
                id: "mdp2".into(),
                version: "0.1.0".into(),
                formats: vec![FormatInfo { name: "MediBang / FireAlpaca".into(), extensions: vec!["mdp".into()], mime: None }],
                probe_bytes: 8,
                can_write: false,
            },
        }
    }
}

/// What a leaf layer's conversion keeps for the escape-hatch trait methods.
struct LegacyInfo {
    pixel_source: Option<String>,
    extra: Vec<(String, String)>,
    blend_mode: String,
}

/// Stashed in `ReadCx` by `read_structure`, read back by `native_composite`
/// and the escape hatches (no re-parse, no second pass over the XML).
struct Mdp2Extra {
    doc: Arc<Mdp2>,
    legacy: HashMap<String, LegacyInfo>,
}

fn corrupt(msg: impl Into<String>) -> ReadError {
    ReadError::Corrupt(msg.into())
}

fn unsupported(msg: impl Into<String>) -> ReadError {
    ReadError::Unsupported(msg.into())
}

fn json_text(v: Value) -> String {
    json::dumps(&v, &DumpOptions::DEFAULT)
}

fn opt_str_meta(key: &str, v: &Option<String>) -> MetaEntry {
    MetaEntry { key: key.into(), value: v.as_ref().map_or(MetaValue::Null, |s| MetaValue::Text(s.clone())) }
}

fn opt_int_meta(key: &str, v: Option<i64>) -> MetaEntry {
    MetaEntry { key: key.into(), value: v.map_or(MetaValue::Null, MetaValue::Integer) }
}

fn opt_bool_meta(key: &str, v: Option<bool>) -> MetaEntry {
    MetaEntry { key: key.into(), value: v.map_or(MetaValue::Null, MetaValue::Boolean) }
}

/// `_mdp_layer_kind` (`layer_kind` in `tmi-import/src/mdp2.rs`).
fn layer_kind(layer: &MLayer) -> LayerKind {
    if layer.kind == "folder" {
        return LayerKind::Folder;
    }
    if layer.masking && layer.masking_type == Some(0) {
        return LayerKind::Mask;
    }
    if layer.masking && layer.masking_type == Some(1) {
        return LayerKind::Stencil;
    }
    match layer.kind.as_str() {
        "text" => LayerKind::Text,
        "vector" => LayerKind::Vector,
        "32bpp" | "8bpp" | "1bpp" | "64bpp" => LayerKind::Raster,
        _ => LayerKind::Unknown,
    }
}

/// The WIT `blend-mode` of an MDP2 layer, for the compositor only (the
/// record keeps the raw XML string, `legacy_blend_mode`): MDP2 never draws
/// a folder as a group of its own (`tmi_render::mdp2`: its children land
/// straight on the canvas), so a layer with children is `pass-through`;
/// a leaf's mode is `tmi_render::mdp2::blend_mode`'s table, unknown modes
/// `other` (drawn as normal, as there).
fn wit_blend(layer: &MLayer) -> BlendMode {
    use BlendMode::*;
    if !layer.children.is_empty() {
        return PassThrough;
    }
    match layer.mode.as_str() {
        "" | "normal" => Normal,
        "through" => PassThrough,
        "mul" => Multiply,
        "add" | "add2" | "ldodge" => LinearDodge,
        "sub" => Subtract,
        "div" => Divide,
        "overlay" => Overlay,
        "screen" => Screen,
        "light" => Lighten,
        "dark" => Darken,
        "diff" => Difference,
        "exclusion" => Exclusion,
        "dodge" => ColorDodge,
        "burn" => ColorBurn,
        "lburn" => LinearBurn,
        "softlight" => SoftLight,
        "hardlight" => HardLight,
        "hue" => Hue,
        "saturation" => Saturation,
        "color" => Color,
        "luminosity" => Luminosity,
        "hardmix" => HardMix,
        _ => Other,
    }
}

/// The meta entries `layer_source_meta` always recorded, plus the
/// `semantic_digest` input and the `write_capability` string (kept apart:
/// both are reserved meta keys the host adds itself, PLUGINS.md 3.4).
fn layer_meta(layer: &MLayer) -> (Vec<MetaEntry>, String, &'static str) {
    let mut attrs_obj = Map::new();
    for (k, v) in &layer.attributes {
        if k != "bin" {
            attrs_obj.insert(k.clone(), Value::Str(v.clone()));
        }
    }
    let text_lines_val = Value::Array(layer.text_lines.iter().map(|s| Value::Str(s.clone())).collect());
    let attrs_val = Value::Object(attrs_obj.clone());
    let meta = vec![
        MetaEntry { key: "type".into(), value: MetaValue::Text(layer.kind.clone()) },
        MetaEntry { key: "width".into(), value: MetaValue::Integer(layer.width) },
        MetaEntry { key: "height".into(), value: MetaValue::Integer(layer.height) },
        opt_str_meta("bin_name", &layer.bin_name),
        opt_str_meta("bin_type", &layer.bin_type),
        MetaEntry { key: "parent_id".into(), value: MetaValue::Integer(layer.parent_id) },
        MetaEntry { key: "protect_alpha".into(), value: MetaValue::Boolean(layer.protect_alpha) },
        MetaEntry { key: "folder_open".into(), value: MetaValue::Boolean(layer.folder_open) },
        MetaEntry { key: "masking".into(), value: MetaValue::Boolean(layer.masking) },
        opt_int_meta("masking_type", layer.masking_type),
        MetaEntry { key: "draft".into(), value: MetaValue::Boolean(layer.draft) },
        opt_bool_meta("lumi", layer.lumi),
        opt_str_meta("color", &layer.color),
        opt_str_meta("halftone_type", &layer.halftone_type),
        opt_int_meta("halftone_line", layer.halftone_line),
        opt_int_meta("transform_offset_x", layer.transform_offset_x),
        opt_int_meta("transform_offset_y", layer.transform_offset_y),
        MetaEntry { key: "text_lines".into(), value: MetaValue::Json(json_text(text_lines_val.clone())) },
        MetaEntry { key: "xml_attrs".into(), value: MetaValue::Json(json_text(attrs_val)) },
    ];
    let mut sem = Map::new();
    sem.insert("attrs".into(), Value::Object(attrs_obj));
    sem.insert("text_lines".into(), text_lines_val);
    let digest = semantic_digest(&Value::Object(sem));
    let writable = matches!(layer.kind.as_str(), "32bpp" | "8bpp" | "1bpp" | "text" | "vector") && !layer.masking;
    (meta, digest, if writable { "raster-pixels" } else { "metadata-only" })
}

/// A span of `t.data` (a sub-slice of `raw`, which `base` must be) inside the
/// source file.
fn span_of(base: &[u8], sub: &[u8]) -> Span {
    let off = sub.as_ptr() as usize - base.as_ptr() as usize;
    Span { offset: off as u64, len: sub.len() as u64 }
}

/// A renderable leaf's `pixel-source::tiles` bitmap (`plan_layer` without
/// decoding a single tile: it only describes where they are).
fn leaf_bitmap(layer: &MLayer, doc: &Mdp2, raw: &[u8]) -> Result<Bitmap, ReadError> {
    let bin = layer.bin_name.as_deref().ok_or_else(|| unsupported("renderable MDP2 layer without a bin"))?;
    let blob = doc.blob_by_name.get(bin).map(|&i| &doc.blobs[i]).ok_or_else(|| unsupported("MDP2 bin blob missing"))?;
    let (lw, lh) = m::image_size(layer).map_err(corrupt)?;
    let (lw, lh) = (
        u32::try_from(lw).map_err(|_| corrupt("MDP2 layer width out of range"))?,
        u32::try_from(lh).map_err(|_| corrupt("MDP2 layer height out of range"))?,
    );
    let tiles = m::scan_tiles(blob.payload(raw), blob.payload_size).map_err(corrupt)?;
    if tiles.is_empty() {
        return Ok(Bitmap { size: Some((lw, lh)), source: PixelSource::Tiles(Tiled { tiles: vec![], overlap: Overlap::Over }) });
    }
    let limit = i32::MAX as u64;
    if tiles.iter().any(|t| t.x as u64 * m::TILE as u64 + m::TILE as u64 > limit || t.y as u64 * m::TILE as u64 + m::TILE as u64 > limit) {
        return Err(corrupt("signed integer is greater than maximum"));
    }
    let format = TileFormat::for_layer(layer).map_err(unsupported)?;
    let (group, tint) = match format {
        TileFormat::Bgra => (Group { channels: vec![Channel::B, Channel::G, Channel::R, Channel::A], depth: Depth::Eight }, None),
        TileFormat::Gray(c) => (Group { channels: vec![Channel::A], depth: Depth::Eight }, Some(Rgba8 { r: c[0], g: c[1], b: c[2], a: c[3] })),
        TileFormat::Mono(c) => (Group { channels: vec![Channel::A], depth: Depth::OneBitMsb }, Some(Rgba8 { r: c[0], g: c[1], b: c[2], a: c[3] })),
    };
    let out_tiles = tiles
        .iter()
        .map(|t| Tile {
            x: (t.x as i64 * m::TILE as i64) as i32,
            y: (t.y as i64 * m::TILE as i64) as i32,
            width: m::TILE as u32,
            height: m::TILE as u32,
            content: TileContent::Raster(Raster {
                data: Bytes::Span(span_of(raw, t.data)),
                codecs: vec![Codec::Zlib],
                layout: Layout { groups: vec![group.clone()], row_padding: 0, tint, premultiplied: false },
            }),
        })
        .collect();
    Ok(Bitmap { size: Some((lw, lh)), source: PixelSource::Tiles(Tiled { tiles: out_tiles, overlap: Overlap::Over }) })
}

impl FormatReader for Mdp2Reader {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn probe(&self, _name: &str, head: &[u8], _size: u64) -> Confidence {
        if head.starts_with(m::MAGIC) {
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
        "mdp2".into()
    }

    fn read_structure(&self, src: &Source, cx: &ReadCx) -> Result<Structure, ReadError> {
        let doc = m::parse(src.data).map_err(|e| corrupt(format!("MDP2 does not parse ({e})")))?;
        if !doc.big.is_empty() || doc.layers.iter().any(|l| !l.big.is_empty()) {
            return Err(unsupported("MDP2 integers beyond i64"));
        }
        let cw = u32::try_from(doc.canvas_width).map_err(|_| unsupported("MDP2 canvas width out of range"))?;
        let ch = u32::try_from(doc.canvas_height).map_err(|_| unsupported("MDP2 canvas height out of range"))?;

        // `parent_of[i]`: the index (in `doc.layers`) of `i`'s parent, built
        // the same way `convert`'s recursion walked (`children`, in document
        // order). A DFS pre-order from the roots gives every layer an index
        // its parent already has (PLUGINS.md 3.4: "parent is before it"),
        // while reproducing `convert`'s exact tree shape and sibling order.
        let mut parent_of: Vec<Option<usize>> = vec![None; doc.layers.len()];
        for p in 0..doc.layers.len() {
            for &c in &doc.layers[p].children {
                parent_of[c] = Some(p);
            }
        }
        fn visit(doc: &Mdp2, i: usize, order: &mut Vec<usize>, depth: usize) -> Result<(), ReadError> {
            if depth > 500 {
                return Err(unsupported("MDP2 layer tree too deep (Python: RecursionError)"));
            }
            order.push(i);
            for &c in &doc.layers[i].children {
                visit(doc, c, order, depth + 1)?;
            }
            Ok(())
        }
        let mut order: Vec<usize> = Vec::with_capacity(doc.layers.len());
        for i in 0..doc.layers.len() {
            if parent_of[i].is_none() {
                visit(&doc, i, &mut order, 0)?;
            }
        }
        // Python decodes every renderable leaf of the file and files the
        // pixels by `layer_id` (`_render_mdp2_leaves_parallel`): one outside
        // the tree (a parent cycle) still fails the import when it does not
        // decode, and a shared id records the last such leaf's pixels on
        // every layer of that id. So a leaf outside the tree must decode
        // (checked here), and a layer of the tree whose id the last such
        // leaf of the file has must be that leaf, else Python imports it.
        let leaf = |l: &MLayer| l.children.is_empty() && l.kind != "folder" && l.renderable;
        let mut in_tree = vec![false; doc.layers.len()];
        for &i in &order {
            in_tree[i] = true;
        }
        let mut last_leaf: HashMap<i64, usize> = HashMap::new();
        for (i, l) in doc.layers.iter().enumerate() {
            if leaf(l) {
                last_leaf.insert(l.layer_id, i);
            }
        }
        for (i, l) in doc.layers.iter().enumerate() {
            if in_tree[i] && last_leaf.get(&l.layer_id).is_some_and(|&k| k != i) {
                return Err(unsupported("MDP2 layer sharing its id with a later renderable layer"));
            }
            if leaf(l) && !in_tree[i] {
                m::image_size(l).map_err(corrupt)?;
                m::plan_layer(&doc, src.data, i, m::Strictness::Image).map_err(corrupt)?;
            }
        }
        let mut orig_to_out: HashMap<usize, u32> = HashMap::with_capacity(order.len());
        for (k, &orig) in order.iter().enumerate() {
            orig_to_out.insert(orig, k as u32);
        }

        let mut legacy: HashMap<String, LegacyInfo> = HashMap::with_capacity(order.len());
        let mut layers: Vec<Layer> = Vec::with_capacity(order.len());
        for &orig in &order {
            let layer = &doc.layers[orig];
            let sid = format!("mdp2:{}", layer.layer_id);
            let is_leaf = layer.children.is_empty() && layer.kind != "folder" && layer.renderable;
            let pixels = if is_leaf { LayerPixels::Ready(leaf_bitmap(layer, &doc, src.data)?) } else { LayerPixels::None };
            let pixel_source = if is_leaf {
                let bin = layer.bin_name.as_deref().expect("renderable implies a bin");
                let blob = &doc.blobs[doc.blob_by_name[bin]];
                let color = layer.color.as_deref().unwrap_or("None");
                let params = format!("{} {}x{} {}", layer.kind, layer.width, layer.height, color);
                Some(pixel_source_key("mdp2", &params, &[&[blob.payload(src.data)]]))
            } else {
                None
            };
            let (meta, digest, write_cap) = layer_meta(layer);
            let blend_mode = if layer.mode.is_empty() { "normal".to_string() } else { layer.mode.clone() };
            legacy.insert(
                sid.clone(),
                LegacyInfo {
                    pixel_source,
                    extra: vec![("semantic_digest".into(), digest), ("write_capability".into(), write_cap.into())],
                    blend_mode,
                },
            );
            layers.push(Layer {
                id: sid.clone(),
                stable_id: sid,
                parent: parent_of[orig].map(|p| orig_to_out[&p]),
                name: if layer.name.is_empty() { format!("layer-{}", layer.layer_id) } else { layer.name.clone() },
                kind: layer_kind(layer),
                visible: layer.visible,
                opacity: layer.alpha as f64 / 255.0,
                blend: wit_blend(layer),
                clip_to_below: layer.clipping,
                offset_x: layer.offset_x,
                offset_y: layer.offset_y,
                pixels,
                meta,
                semantic: vec![],
                write: WriteCapability::None,
            });
        }

        let mut file_meta = Vec::new();
        file_meta.push(MetaEntry { key: "xml_size".into(), value: MetaValue::Integer(doc.xml_size as i64) });
        file_meta.push(MetaEntry { key: "pack_size".into(), value: MetaValue::Integer(doc.pack_size as i64) });

        let dpi = doc.dpi as f64;
        cx.put_extra(Mdp2Extra { doc: Arc::new(doc), legacy });

        Ok(Structure {
            format: "mdp2".into(),
            canvas: Canvas { width: cw, height: ch, dpi: Some(dpi), profile: ColorProfile::None },
            layers,
            composite: Composite::None,
            file_meta,
            context: Vec::new(),
            warnings: Vec::new(),
        })
    }

    fn native_composite(&self, src: &Source, _structure: &Structure, cx: &ReadCx, _opts: &tmi_import::Options) -> Option<Res<Rendered>> {
        // Clones the `Arc` and releases `ReadCx`'s lock immediately: `render`
        // uses rayon itself, and holding the lock into it can starve the
        // pool (another worker blocked on this same lock in `native_pixels`,
        // with no free worker left to finish `render`'s own parallel work).
        let doc = cx.with_extra::<Mdp2Extra, _>(|e| e.doc.clone())?;
        Some((|| -> Res<Rendered> {
            let subset: Vec<usize> = (0..doc.layers.len()).collect();
            let (w, h) = tmi_render::mdp2::canvas_size(&doc).map_err(decline)?;
            let data = tmi_render::mdp2::render(&doc, src.data, [255, 255, 255, 255], Some(&subset)).map_err(decline)?;
            Ok(Rendered { image: Rgba { width: w, height: h, data }, png: None, text: None, encoded_in_render: false })
        })())
    }

    fn legacy_source_key(&self, _src: &Source, cx: &ReadCx, layer: &Layer) -> Option<String> {
        cx.with_extra::<Mdp2Extra, _>(|e| e.legacy.get(&layer.stable_id).and_then(|l| l.pixel_source.clone())).flatten()
    }

    fn legacy_extra_meta(&self, cx: &ReadCx, layer: &Layer) -> Vec<(String, MetaValue)> {
        cx.with_extra::<Mdp2Extra, _>(|e| e.legacy.get(&layer.stable_id).map(|l| l.extra.clone()))
            .flatten()
            .unwrap_or_default()
            .into_iter()
            .map(|(k, v)| (k, MetaValue::Text(v)))
            .collect()
    }

    fn legacy_blend_mode(&self, cx: &ReadCx, layer: &Layer) -> Option<String> {
        cx.with_extra::<Mdp2Extra, _>(|e| e.legacy.get(&layer.stable_id).map(|l| l.blend_mode.clone())).flatten()
    }

    fn legacy_integer_dpi(&self) -> bool {
        true
    }

    fn shows_layer_progress(&self) -> bool {
        true
    }
}

/// The recipe decode of every renderable leaf layer against
/// `tmi_clip::mdp2::plan_layer` / `fill`, the "straightforward reference"
/// PLUGINS.md 10 (P1) asks for, on the example files (the random-file corpus
/// of `tests/test_mdp2_parity.py` / `tools/import_parity.py` covers the same
/// comparison end to end, Rust against Python, for the generated files).
#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};

    use tmi_clip::mdp2::{self as m, Layer as MLayer, Strictness};
    use tmi_import::crop::LayerCrop;

    use super::*;
    use crate::reader::FormatReader;
    use crate::recipe::{decode_bitmap, decode_layer_crop, DecodeEnv};

    fn example_files() -> Vec<PathBuf> {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../example");
        let Ok(entries) = std::fs::read_dir(&dir) else { return Vec::new() };
        entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("mdp")))
            .collect()
    }

    fn check_file(raw: &[u8], label: &str) {
        let doc = m::parse(raw).unwrap_or_else(|e| panic!("{label}: parse: {e}"));
        let reader = Mdp2Reader::new();
        let src = Source::new("x.mdp", raw);
        let cx = ReadCx::new(None);
        let structure = reader.read_structure(&src, &cx).unwrap_or_else(|e| panic!("{label}: read_structure: {e}"));
        let env = DecodeEnv { source: raw, reader: Some(&reader as &dyn FormatReader), cx: &cx };
        let mut leaves = 0;
        for (i, layer) in doc.layers.iter().enumerate() {
            if !(layer.children.is_empty() && layer.kind != "folder" && layer.renderable) {
                continue;
            }
            leaves += 1;
            let sid = format!("mdp2:{}", layer.layer_id);
            let out_layer = structure.layers.iter().find(|l| l.stable_id == sid).unwrap_or_else(|| panic!("{label}: {sid} missing"));
            let LayerPixels::Ready(bitmap) = &out_layer.pixels else { panic!("{label}: {sid} has no recipe") };
            let decoded = decode_bitmap(&env, bitmap).unwrap_or_else(|e| panic!("{label}: {sid}: decode: {e}"));
            let got = LayerCrop::from_full(&decoded.data, decoded.width, decoded.height);

            let plan = m::plan_layer(&doc, raw, i, Strictness::Image).unwrap_or_else(|e| panic!("{label}: {sid}: plan_layer: {e}"));
            let want = match plan.rect {
                None => LayerCrop::from_tight((decoded.width, decoded.height), 0, 0, 0, 0, Vec::new()),
                Some(r) => {
                    let mut rgba = vec![0u8; (r.2 - r.0) * (r.3 - r.1) * 4];
                    plan.fill(&mut rgba);
                    LayerCrop::from_tight((decoded.width, decoded.height), r.0, r.1, r.2 - r.0, r.3 - r.1, rgba)
                }
            };
            assert_eq!(got.header(), want.header(), "{label}: {sid} header");
            assert_eq!(got.rgba, want.rgba, "{label}: {sid} pixels");
            assert_eq!(got.digest(), want.digest(), "{label}: {sid} digest");
            // the tight decode the pipeline runs
            let tight = decode_layer_crop(&env, bitmap).unwrap_or_else(|e| panic!("{label}: {sid}: tight decode: {e}"));
            assert_eq!(tight.header(), want.header(), "{label}: {sid} tight header");
            assert_eq!(tight.rgba, want.rgba, "{label}: {sid} tight pixels");
        }
        assert!(leaves > 0, "{label}: no renderable leaf layers to compare");
    }

    /// What one file gives `generic-v1` against the native MDP2 composite.
    #[derive(Debug, PartialEq, Eq)]
    enum Parity {
        /// The pipeline would not import it (parse, structure, check or layer decode fails).
        NotImported,
        Same,
        /// The native composite declines, generic-v1 draws.
        NativeFails(String),
        Differs(usize),
    }

    /// `as_masks`: draw MDP2's masking layers of no mask kind as masks (to
    /// diagnose that misfit; the record would change).
    fn generic_parity(raw: &[u8], as_masks: bool) -> Parity {
        let reader = Mdp2Reader::new();
        let src = Source::new("x.mdp", raw);
        let cx = ReadCx::new(None);
        let Ok(mut structure) = reader.read_structure(&src, &cx) else { return Parity::NotImported };
        if as_masks {
            let doc = m::parse(raw).unwrap();
            let masking: std::collections::HashSet<String> =
                doc.layers.iter().filter(|l| l.masking).map(|l| format!("mdp2:{}", l.layer_id)).collect();
            for l in &mut structure.layers {
                if masking.contains(&l.stable_id) {
                    l.kind = LayerKind::Mask;
                }
            }
        }
        if crate::validate::check(&structure, raw.len() as u64).is_err() {
            return Parity::NotImported;
        }
        let reads = crate::compose::reads_pixels(&structure);
        let env = DecodeEnv { source: raw, reader: Some(&reader as &dyn FormatReader), cx: &cx };
        let mut crops = Vec::with_capacity(structure.layers.len());
        for (l, &r) in structure.layers.iter().zip(&reads) {
            let LayerPixels::Ready(bitmap) = &l.pixels else {
                crops.push(None);
                continue;
            };
            match decode_layer_crop(&env, bitmap) {
                Ok(c) => crops.push(r.then_some(c)),
                Err(_) => return Parity::NotImported,
            }
        }
        let refs: Vec<Option<&LayerCrop>> = crops.iter().map(|c| c.as_ref()).collect();
        let got = crate::compose::render(&structure, &refs, [255, 255, 255, 255]).expect("generic-v1 draws");
        let doc = m::parse(raw).unwrap();
        match tmi_render::mdp2::render(&doc, raw, [255, 255, 255, 255], None) {
            Err(e) => Parity::NativeFails(e),
            Ok(want) if want == got => Parity::Same,
            Ok(want) => Parity::Differs(want.chunks_exact(4).zip(got.chunks_exact(4)).filter(|(a, b)| a != b).count()),
        }
    }

    /// Why an MDP2 file's own compositor may not fit generic-v1's definition
    /// (PLUGINS.md P4): the features present, for the report.
    fn misfits(raw: &[u8]) -> Vec<&'static str> {
        let doc = m::parse(raw).unwrap();
        let reader = Mdp2Reader::new();
        let cx = ReadCx::new(None);
        let s = reader.read_structure(&Source::new("x.mdp", raw), &cx).unwrap();
        let mut out = Vec::new();
        let leaf = |l: &MLayer| l.children.is_empty() && l.kind != "folder" && l.renderable && !l.masking;
        let file_order: Vec<String> = doc.layers.iter().filter(|l| leaf(l)).map(|l| format!("mdp2:{}", l.layer_id)).collect();
        let by_sid: HashMap<String, &MLayer> = doc.layers.iter().map(|l| (format!("mdp2:{}", l.layer_id), l)).collect();
        let tree_order: Vec<String> =
            s.layers.iter().filter(|l| by_sid.get(&l.stable_id).is_some_and(|ml| leaf(ml))).map(|l| l.stable_id.clone()).collect();
        if file_order != tree_order {
            out.push("draw order: file order is not the tree's");
        }
        if doc.layers.iter().any(|l| l.masking && !matches!(l.masking_type, Some(0) | Some(1)) && l.renderable && l.children.is_empty()) {
            out.push("masking layer of no mask kind");
        }
        let ids: std::collections::HashSet<&m::IntKey> = doc.layers.iter().map(|l| &l.id_key).collect();
        let root_keys: std::collections::HashSet<&m::IntKey> =
            doc.layers.iter().filter(|l| !ids.contains(&l.parent_key)).map(|l| &l.parent_key).collect();
        if root_keys.len() > 1 && doc.layers.iter().any(|l| l.clipping && !ids.contains(&l.parent_key)) {
            out.push("roots under different parent ids, with clipping");
        }
        out
    }

    #[test]
    fn generic_matches_native_composite_on_examples() {
        let files = example_files();
        assert!(!files.is_empty(), "example/*.mdp not found (is the worktree junction set up?)");
        for path in &files {
            let raw = std::fs::read(path).unwrap();
            assert_eq!(generic_parity(&raw, false), Parity::Same, "{}", path.display());
        }
    }

    /// `TMI_MDP2_CORPUS=<dir> cargo test -p tmi-format --release -- --ignored generic_against`:
    /// every `.mdp` in the directory (e.g. `tests/test_mdp2_parity.py`'s random
    /// files written out), generic-v1 against the native composite, with the
    /// misfit features of each file that differs.
    #[test]
    #[ignore]
    fn generic_against_native_on_corpus() {
        let dir = std::env::var("TMI_MDP2_CORPUS").expect("TMI_MDP2_CORPUS");
        let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().path()).collect();
        paths.sort();
        let mut counts: std::collections::BTreeMap<String, usize> = Default::default();
        let mut causes: std::collections::BTreeMap<String, usize> = Default::default();
        for p in &paths {
            let raw = std::fs::read(p).unwrap();
            let r = generic_parity(&raw, false);
            let key = match &r {
                Parity::NotImported => "not imported".to_string(),
                Parity::Same => "same".to_string(),
                Parity::NativeFails(_) => "native fails".to_string(),
                Parity::Differs(_) => "differs".to_string(),
            };
            *counts.entry(key).or_default() += 1;
            if let Parity::Differs(n) = r {
                let why = misfits(&raw);
                let as_masks = generic_parity(&raw, true);
                println!(
                    "{}: {n} pixels differ; {}; with masking layers as masks: {as_masks:?}",
                    p.display(),
                    if why.is_empty() { "no known misfit".to_string() } else { why.join(", ") }
                );
                *causes.entry(if why.is_empty() { "none known".into() } else { why.join(" + ") }).or_default() += 1;
            } else if let Parity::NativeFails(e) = r {
                println!("{}: native fails: {e}", p.display());
            } else if r == Parity::Same {
                let why = misfits(&raw);
                if !why.is_empty() {
                    *causes.entry(format!("same despite: {}", why.join(" + "))).or_default() += 1;
                }
            }
        }
        println!("{} files: {counts:?}", paths.len());
        println!("causes: {causes:?}");
    }

    #[test]
    fn recipe_matches_plan_layer_on_examples() {
        let files = example_files();
        assert!(!files.is_empty(), "example/*.mdp not found (is the worktree junction set up?)");
        for path in &files {
            let raw = std::fs::read(path).unwrap();
            check_file(&raw, &path.display().to_string());
        }
    }
}

