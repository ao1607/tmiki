//! The pipeline and the registry: the raster records as they always were,
//! and a synthetic reader for what raster files do not exercise (a tree, tiles,
//! several composites, metadata).

use tmi_import::crop::LayerCrop;
use tmi_import::{LayerBlobs, Limits, Options, Warnings};
use tmi_pillow::image::{Image, Mode};
use tmi_store::json::Value;

use crate::pipeline::{self, ImportError};
use crate::reader::{FormatReader, ReadCx, Source};
use crate::registry::Registry;
use crate::types::*;

fn opts<'a>(blobs: &'a LayerBlobs, render: bool, limits: Limits) -> Options<'a> {
    Options {
        render_composite: render,
        cache: None,
        reuse_pixels: None,
        reuse_decoded: None,
        layer_blobs: blobs,
        composite_base: None,
        limits,
        verify_partial: false,
        progress: None,
    }
}

fn gradient(w: usize, h: usize) -> Vec<u8> {
    (0..w * h).flat_map(|i| [(i * 7) as u8, (i * 13) as u8, (i * 29) as u8, if i % 5 == 0 { 0 } else { 255 }]).collect()
}

fn png(w: usize, h: usize) -> (Vec<u8>, Vec<u8>) {
    let px = gradient(w, h);
    (tmi_image::png_enc::encode_png(&Image::new(Mode::RGBA, w, h, px.clone()), 0, &[], &[]).unwrap(), px)
}

fn import(name: &str, data: &[u8], render: bool, limits: Limits) -> tmi_import::Res<tmi_import::ImportedFile> {
    let blobs = LayerBlobs::All;
    let digest = tmi_store::oid::sha256_hex(data);
    crate::import_file(name, data, &digest, &opts(&blobs, render, limits), &mut Warnings::default())
}

fn sha(l: &tmi_import::ImportedLayer) -> Option<String> {
    l.pixel_sha256.as_ref().and_then(|v| v.as_str()).map(String::from)
}

fn s(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| x.as_str()).map(String::from)
}

#[test]
fn a_png_is_one_raster_layer_with_the_digest_of_its_crop() {
    let (bytes, px) = png(7, 5);
    let f = import("cat.png", &bytes, true, Limits::NONE).unwrap();
    assert_eq!(f.source_format, "png");
    assert_eq!(f.layers.len(), 1);
    let l = &f.layers[0];
    assert_eq!((l.layer_id.as_str(), l.stable_source_id.as_str(), l.name.as_str(), l.kind.as_str()), ("image:0", "image:0", "cat", "raster"));
    let crop = LayerCrop::from_full(&px, 7, 5);
    assert_eq!(sha(l), Some(crop.digest()));
    assert_eq!(l.blob_bytes.as_deref(), Some(&crop.encode().unwrap()[..]));
    assert_eq!(l.source_meta.len(), 1);
    assert_eq!(s(&Value::Object(l.source_meta.clone()), "source_path_name").as_deref(), Some("cat.png"));
    assert!(!f.composite_png.is_empty() && !f.thumbnail_png.is_empty());
    assert!(f.source_meta.is_empty());
    let c = Value::Object(f.canvas_meta.clone());
    assert_eq!(s(&c, "source_format").as_deref(), Some("png"));
    assert_eq!(c.get("width").and_then(|v| v.as_i64()), Some(7));
    assert!(c.get("dpi").unwrap().is_null() && c.get("color_profile").unwrap().is_null());
    // no composite wanted: none made
    let g = import("cat.png", &bytes, false, Limits::NONE).unwrap();
    assert!(g.composite_png.is_empty() && g.thumbnail_png.is_empty());
    assert_eq!(sha(&g.layers[0]), sha(l));
}

#[test]
fn the_source_format_follows_the_name_then_the_content() {
    let (bytes, _) = png(2, 2);
    for (name, want) in [("a.png", "png"), ("a.PNG", "png"), ("a.jpg", "jpeg"), ("a.jpeg", "jpeg"), ("a.dat", "png"), ("a", "png")] {
        assert_eq!(import(name, &bytes, false, Limits::NONE).unwrap().source_format, want, "{name}");
    }
}

#[test]
fn what_tmi_does_not_read_is_an_opaque_blob() {
    for (name, data) in [("a.png", &b"not a png"[..]), ("a.tga", b"\0\0\x02"), ("empty.bin", b""), ("a.gif", b"GIF89a\x01\0\x01\0not really")] {
        let f = import(name, data, true, Limits::NONE).unwrap();
        assert_eq!((f.source_format.as_str(), f.layers[0].layer_id.as_str()), ("binary", "blob:0"), "{name}");
        assert!(f.composite_png.is_empty());
    }
}

#[test]
fn an_image_over_the_limit_declines() {
    let (bytes, _) = png(4, 4);
    let d = import("a.png", &bytes, true, Limits { layered: 0, raster: 15 }).err().expect("declined");
    assert_eq!(d.0, "image of 4x4 pixels is over core.maxImagePixels: Python reports it");
    assert!(import("a.png", &bytes, true, Limits { layered: 0, raster: 16 }).is_ok());
}

#[test]
fn the_registry_falls_back_to_the_raster_reader_and_an_empty_one_selects_nothing() {
    let (bytes, _) = png(2, 2);
    let reg = Registry::builtin();
    assert_eq!(reg.select("x.png", &bytes).unwrap().info().id, "raster");
    assert_eq!(reg.select("x.bin", b"anything").unwrap().info().id, "raster");
    assert!(Registry::new().select("x.png", &bytes).is_none());
}

// ---------------------------------------------------------------------------
// a synthetic reader

struct Synthetic {
    info: PluginInfo,
    structure: Structure,
}

impl FormatReader for Synthetic {
    fn info(&self) -> &PluginInfo {
        &self.info
    }
    fn probe(&self, _: &str, _: &[u8], _: u64) -> Confidence {
        Confidence::Yes
    }
    fn builtin(&self) -> bool {
        true
    }
    fn read_structure(&self, _: &Source, _: &ReadCx) -> Result<Structure, ReadError> {
        Ok(self.structure.clone())
    }
    fn read_layer(&self, _: &Source, context: &[u8], token: u32, _: &ReadCx) -> Result<Bitmap, ReadError> {
        assert_eq!(context, b"ctx");
        assert_eq!(token, 7);
        Ok(fill_bitmap(2, 2, Rgba8 { r: 10, g: 20, b: 30, a: 255 }))
    }
    fn source_format(&self, _: &str, _: &Structure) -> String {
        "synthetic".into()
    }
    fn legacy_source_key(&self, _src: &Source, _cx: &ReadCx, layer: &Layer) -> Option<String> {
        (layer.id == "b").then(|| "key-b".to_string())
    }
}

fn fill_bitmap(w: u32, h: u32, c: Rgba8) -> Bitmap {
    Bitmap {
        size: Some((w, h)),
        source: PixelSource::Tiles(Tiled { tiles: vec![Tile { x: 0, y: 0, width: w, height: h, content: TileContent::Fill(c) }], overlap: Overlap::Replace }),
    }
}

fn layer(id: &str, parent: Option<u32>, kind: LayerKind, pixels: LayerPixels) -> Layer {
    Layer {
        id: id.into(),
        stable_id: format!("s-{id}"),
        parent,
        name: format!("layer {id}"),
        kind,
        visible: true,
        opacity: 0.5,
        blend: BlendMode::Multiply,
        clip_to_below: true,
        offset_x: -3,
        offset_y: 4,
        pixels,
        meta: vec![],
        semantic: vec![],
        write: WriteCapability::Pixels,
    }
}

fn synthetic(layers: Vec<Layer>, composite: Composite, w: u32, h: u32) -> Synthetic {
    Synthetic {
        info: PluginInfo { id: "synthetic".into(), version: "0".into(), formats: vec![], probe_bytes: 16, can_write: false },
        structure: Structure {
            format: "Synthetic".into(),
            canvas: Canvas { width: w, height: h, dpi: Some(72.0), profile: ColorProfile::Named("sRGB".into()) },
            layers,
            composite,
            file_meta: vec![
                MetaEntry { key: "n".into(), value: MetaValue::Integer(3) },
                MetaEntry { key: "j".into(), value: MetaValue::Json(r#"{"b":[1,2],"a":null}"#.into()) },
            ],
            context: b"ctx".to_vec(),
            warnings: vec![],
        },
    }
}

fn run(r: &Synthetic, data: &[u8], render: bool) -> Result<tmi_import::ImportedFile, ImportError> {
    let blobs = LayerBlobs::All;
    let mut warns = tmi_import::Warnings::default();
    pipeline::import(r, "doc.syn", data, "digest", &opts(&blobs, render, Limits::NONE), &mut warns)
}

#[test]
fn a_tree_of_layers_becomes_nested_records() {
    // a (folder) > [b (tiles), c (deferred)], d (no pixels, a root), e (empty tiles) under d
    let red = Rgba8 { r: 255, g: 0, b: 0, a: 255 };
    let mut b = layer("b", Some(0), LayerKind::Raster, LayerPixels::Ready(fill_bitmap(3, 2, red)));
    b.meta = vec![
        MetaEntry { key: "k".into(), value: MetaValue::Text("v".into()) },
        MetaEntry { key: "sha".into(), value: MetaValue::Sha256Of(Bytes::Inline(b"abc".to_vec())) },
    ];
    let layers = vec![
        layer("a", None, LayerKind::Folder, LayerPixels::None),
        b,
        layer("c", Some(0), LayerKind::Raster, LayerPixels::Deferred(7)),
        layer("d", None, LayerKind::Raster, LayerPixels::None),
        layer("e", Some(3), LayerKind::Raster, LayerPixels::Ready(Bitmap { size: Some((4, 4)), source: PixelSource::Empty })),
    ];
    let r = synthetic(layers, Composite::None, 5, 5);
    let f = run(&r, b"", true).unwrap();
    assert_eq!(f.source_format, "synthetic");
    assert!(f.composite_png.is_empty());
    assert_eq!(f.layers.len(), 2, "two roots");
    let a = &f.layers[0];
    assert_eq!((a.layer_id.as_str(), a.kind.as_str(), a.children.len()), ("a", "folder", 2));
    assert!(a.pixel_sha256.is_none() && a.blob_bytes.is_none() && a.pixel_bbox.is_null());
    let (b, c) = (&a.children[0], &a.children[1]);
    assert_eq!((b.layer_id.as_str(), c.layer_id.as_str()), ("b", "c"));
    assert_eq!((b.stable_source_id.as_str(), b.name.as_str(), b.blend_mode.as_str()), ("s-b", "layer b", "multiply"));
    assert!(b.visible && b.clip_to_below && (b.opacity, b.offset_x, b.offset_y) == (0.5, -3, 4));
    assert_eq!(sha(b), Some(LayerCrop::from_full(&[255, 0, 0, 255].repeat(6), 3, 2).digest()));
    assert_eq!(b.pixel_bbox.as_array().unwrap().len(), 4);
    let m = Value::Object(b.source_meta.clone());
    assert_eq!(s(&m, "k").as_deref(), Some("v"));
    assert_eq!(s(&m, "sha").as_deref(), Some(tmi_store::oid::sha256_hex(b"abc").as_str()));
    assert_eq!(s(&m, "pixel_source").as_deref(), Some("key-b"), "the built-in's own key");
    assert!(c.pixel_sha256.is_some(), "a deferred layer was read through read-layer");
    let d = &f.layers[1];
    assert_eq!((d.layer_id.as_str(), d.children.len(), d.children[0].layer_id.as_str()), ("d", 1, "e"));
    let e = &d.children[0];
    assert!(e.pixel_sha256.is_some() && e.pixel_bbox.is_null(), "empty pixels: a digest, no box");
    // canvas and file metadata
    let cm = Value::Object(f.canvas_meta.clone());
    assert_eq!((s(&cm, "color_profile").as_deref(), s(&cm, "source_format").as_deref()), (Some("sRGB"), Some("synthetic")));
    assert_eq!(cm.get("dpi").map(|v| matches!(v, Value::Float(d) if *d == 72.0)), Some(true));
    let sm = Value::Object(f.source_meta.clone());
    assert_eq!(sm.get("n").and_then(|v| v.as_i64()), Some(3));
    assert!(matches!(sm.get("j"), Some(Value::Object(o)) if o.len() == 2));
}

#[test]
fn an_embedded_composite_is_used_at_the_canvas_size_and_shared_with_its_layer() {
    let c = Rgba8 { r: 1, g: 2, b: 3, a: 255 };
    let bm = fill_bitmap(4, 3, c);
    // the composite is the layer's own recipe
    let r = synthetic(vec![layer("a", None, LayerKind::Raster, LayerPixels::Ready(bm.clone()))], Composite::Image(bm.clone()), 4, 3);
    let f = run(&r, b"", true).unwrap();
    assert!(!f.composite_png.is_empty() && !f.thumbnail_png.is_empty());
    // a different image as the composite
    let other = fill_bitmap(4, 3, Rgba8 { r: 9, g: 9, b: 9, a: 255 });
    let r2 = synthetic(vec![layer("a", None, LayerKind::Raster, LayerPixels::Ready(bm.clone()))], Composite::Image(other), 4, 3);
    let g = run(&r2, b"", true).unwrap();
    assert_ne!(g.composite_png, f.composite_png);
    assert_eq!(sha(&g.layers[0]), sha(&f.layers[0]));
    // not wanted: not made
    assert!(run(&r2, b"", false).unwrap().composite_png.is_empty());
    // the wrong size or the generic compositor: generic-v1 draws it
    let wrong = synthetic(vec![], Composite::Image(fill_bitmap(2, 2, c)), 4, 3);
    let render = synthetic(vec![], Composite::Render(Compositor::GenericV1), 4, 3);
    assert_eq!(run(&wrong, b"", true).unwrap().composite_png, run(&render, b"", true).unwrap().composite_png);
    assert!(run(&render, b"", false).unwrap().composite_png.is_empty(), "no composite wanted, none made");
}

fn plain(id: &str, parent: Option<u32>, kind: LayerKind, pixels: LayerPixels) -> Layer {
    let mut l = layer(id, parent, kind, pixels);
    (l.opacity, l.blend, l.clip_to_below, l.offset_x, l.offset_y) = (1.0, BlendMode::Normal, false, 0, 0);
    l
}

fn decoded(png: &[u8]) -> Vec<u8> {
    tmi_image::decode_rgba(png).unwrap().0.data
}

/// A red square under a half-transparent multiply folder holding a blue
/// one, on 4 x 3.
fn generic_doc(composite: Composite) -> Synthetic {
    let red = Rgba8 { r: 255, g: 0, b: 0, a: 255 };
    let blue = Rgba8 { r: 0, g: 0, b: 255, a: 255 };
    let mut folder = plain("f", None, LayerKind::Folder, LayerPixels::None);
    (folder.opacity, folder.blend) = (0.5, BlendMode::Multiply);
    let mut b = plain("b", Some(1), LayerKind::Raster, LayerPixels::Ready(fill_bitmap(2, 2, blue)));
    b.offset_x = 1;
    let layers = vec![plain("a", None, LayerKind::Raster, LayerPixels::Ready(fill_bitmap(2, 2, red))), folder, b];
    synthetic(layers, composite, 4, 3)
}

#[test]
fn render_generic_v1_draws_the_layers_with_its_tag() {
    let f = run(&generic_doc(Composite::Render(Compositor::GenericV1)), b"", true).unwrap();
    assert_eq!(
        tmi_import::partial::png_text(&f.composite_png, "tmi-composite").as_deref(),
        Some(r#"{"renderer":"generic/v1"}"#)
    );
    let px = decoded(&f.composite_png);
    let at = |x: usize, y: usize| px[(y * 4 + x) * 4..(y * 4 + x) * 4 + 4].to_vec();
    assert_eq!(at(0, 0), [255, 0, 0, 255], "red alone");
    // multiply of red and blue is black: at half opacity, a dark red over
    // red; over white, the isolated folder's blue
    assert!((126..=127).contains(&at(1, 1)[0]) && at(1, 1)[1..] == [0, 0, 255]);
    assert!(at(2, 1)[2] == 255 && at(2, 1)[0] < 255);
    assert_eq!(at(3, 2), [255, 255, 255, 255], "white");
    assert!(!f.thumbnail_png.is_empty());
    // the same as the compositor called directly
    let s = generic_doc(Composite::None).structure;
    let crops: Vec<LayerCrop> = [[255, 0, 0, 255], [0, 0, 0, 0], [0, 0, 255, 255]]
        .iter()
        .map(|c| LayerCrop::from_full(&c.repeat(4), 2, 2))
        .collect();
    let refs: Vec<Option<&LayerCrop>> = vec![Some(&crops[0]), None, Some(&crops[2])];
    assert_eq!(px, crate::compose::render(&s, &refs, [255; 4]).unwrap());
}

#[test]
fn an_embedded_composite_of_the_wrong_size_is_drawn_by_generic_v1_with_a_warning() {
    let r = generic_doc(Composite::Image(fill_bitmap(2, 2, Rgba8 { r: 1, g: 2, b: 3, a: 255 })));
    let blobs = LayerBlobs::All;
    let mut warns = Warnings::default();
    let f = pipeline::import(&r, "doc.syn", b"", "digest", &opts(&blobs, true, Limits::NONE), &mut warns).unwrap();
    let g = run(&generic_doc(Composite::Render(Compositor::GenericV1)), b"", true).unwrap();
    assert_eq!(f.composite_png, g.composite_png);
    assert_eq!(
        warns.lines,
        ["tmiki: warning: doc.syn: the embedded composite is 2x2, not the canvas' 4x3; drawing it with generic-v1"]
    );
}

#[test]
fn generic_v1_uses_the_render_cache_and_reads_reused_layers() {
    let dir = std::env::temp_dir().join(format!("tmi-format-generic-cache-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let cache = tmi_import::RenderCache::new(&dir);
    let r = generic_doc(Composite::Render(Compositor::GenericV1));
    let blobs = LayerBlobs::All;
    let mut o = opts(&blobs, true, Limits::NONE);
    o.cache = Some(&cache);
    let first = pipeline::import(&r, "doc.syn", b"", "digest", &o, &mut Warnings::default()).unwrap();
    // a built-in's key is the source digest, as it always was
    assert_eq!(cache.get("digest"), Some((first.composite_png.clone(), first.thumbnail_png.clone())));
    assert_eq!(crate::compose::render_cache_key("digest", None, Some(Compositor::GenericV1)), "digest");
    assert_ne!(crate::compose::render_cache_key("digest", Some("c0ffee"), Some(Compositor::GenericV1)), "digest");
    cache.put("digest", b"composite", b"thumb");
    let hit = pipeline::import(&r, "doc.syn", b"", "digest", &o, &mut Warnings::default()).unwrap();
    assert_eq!((hit.composite_png.as_slice(), hit.thumbnail_png.as_slice()), (&b"composite"[..], &b"thumb"[..]));
    let _ = std::fs::remove_dir_all(&dir);
    // a prior decode is recorded as given, and its pixels are still drawn
    let mut prior = std::collections::HashMap::new();
    prior.insert(
        "s-b".to_string(),
        tmi_import::PriorDecode { pixel_source: Some(Value::Str("key-b".into())), pixel_sha256: Value::Str("0".repeat(64)), pixel_bbox: Value::Null },
    );
    let mut o2 = opts(&blobs, true, Limits::NONE);
    o2.reuse_decoded = Some(&prior);
    let reused = pipeline::import(&r, "doc.syn", b"", "digest", &o2, &mut Warnings::default()).unwrap();
    assert_eq!(reused.composite_png, first.composite_png);
    assert_eq!(sha(&reused.layers[1].children[0]).as_deref(), Some("0".repeat(64).as_str()));
}

#[test]
fn a_structure_that_breaks_the_rules_fails() {
    let a = || layer("a", None, LayerKind::Raster, LayerPixels::None);
    let dup = synthetic(vec![a(), a()], Composite::None, 1, 1);
    assert!(matches!(run(&dup, b"", true), Err(ImportError::Read(ReadError::Corrupt(_)))));
    let mut out_of_file = a();
    out_of_file.pixels = LayerPixels::Ready(Bitmap {
        size: None,
        source: PixelSource::Image(PlacedImage {
            x: 0,
            y: 0,
            image: EncodedImage { data: Bytes::Span(Span { offset: 0, len: 99 }), pre: vec![], codec: ImageCodec::Png, frame: 0 },
        }),
    });
    let r = synthetic(vec![out_of_file], Composite::None, 1, 1);
    assert!(matches!(run(&r, b"short", true), Err(ImportError::Read(ReadError::Corrupt(_)))));
    let mut reserved = a();
    reserved.meta = vec![MetaEntry { key: "semantic_digest".into(), value: MetaValue::Null }];
    assert!(run(&synthetic(vec![reserved], Composite::None, 1, 1), b"", true).is_err());
}

#[test]
fn a_plugin_reader_goes_through_import_plugin_with_its_record() {
    struct Plugin(Synthetic, Confidence);
    impl FormatReader for Plugin {
        fn info(&self) -> &PluginInfo {
            self.0.info()
        }
        fn probe(&self, _: &str, _: &[u8], _: u64) -> Confidence {
            self.1
        }
        fn read_structure(&self, s: &Source, c: &ReadCx) -> Result<Structure, ReadError> {
            self.0.read_structure(s, c)
        }
    }
    let mut folder = layer("f", None, LayerKind::Folder, LayerPixels::None);
    folder.semantic = vec![MetaEntry { key: "isolation".into(), value: MetaValue::Text("isolate".into()) }];
    folder.write = WriteCapability::None;
    let mut s = synthetic(vec![folder], Composite::None, 1, 1);
    s.structure.warnings = vec![Warning { code: "odd-op".into(), message: "an odd composite-op".into() }];
    let p = Plugin(s, Confidence::Maybe);
    let blobs = LayerBlobs::All;
    let mut warns = tmi_import::Warnings::default();
    let r = pipeline::import(&p, "x", b"", "d", &opts(&blobs, false, Limits::NONE), &mut warns);
    assert!(matches!(r, Err(ImportError::Decline(_))), "the built-ins' entry refuses a plugin");
    let importer = pipeline::Importer { id: "synthetic".into(), version: "0.1.0".into(), component_sha256: "c0ffee".into(), abi: "tmi:format@0.1.0".into() };
    let f = pipeline::import_plugin(&p, &importer, "x", b"", "d", &opts(&blobs, false, Limits::NONE), &mut warns).unwrap();
    assert_eq!(f.source_format, "plugin:synthetic");
    let text = |v: &Value| tmi_store::json::dumps(v, &tmi_store::json::DumpOptions::CANONICAL);
    assert_eq!(text(&Value::Object(f.importer.clone().unwrap())), text(&Value::Object(importer.record())));
    let meta = &f.layers[0].source_meta;
    let digest = tmi_import::semantic_digest(&tmi_store::json::loads(r#"{"isolation":"isolate"}"#).unwrap());
    assert_eq!(meta.get("semantic_digest").and_then(Value::as_str), Some(digest.as_str()));
    assert_eq!(meta.get("semantic_digest_version").map(text).as_deref(), Some("1"));
    assert_eq!(meta.get("write_capability").and_then(Value::as_str), Some("metadata-only"));
    assert!(!meta.contains_key("pixel_source"), "a folder has no pixels");
    assert_eq!(warns.lines, vec!["tmiki: warning: x: an odd composite-op (odd-op)".to_string()]);
    // a plugin that says the file is not its format
    let no = Plugin(synthetic(vec![], Composite::None, 1, 1), Confidence::No);
    let r = pipeline::import_plugin(&no, &importer, "x", b"", "d", &opts(&blobs, false, Limits::NONE), &mut warns);
    assert!(matches!(r, Err(ImportError::Read(ReadError::NotThisFormat(_)))));
}

fn walk_layers<'a>(layers: &'a [tmi_import::ImportedLayer], out: &mut Vec<&'a tmi_import::ImportedLayer>) {
    for l in layers {
        out.push(l);
        walk_layers(&l.children, out);
    }
}

/// A prior decode whose `pixel_source` matches is recorded as it is, without
/// decoding, digesting or encoding the layer (the old CLIP and MDP2
/// importers' `decode_leaf`, importers.py `_reusable_decode`), even when the
/// prior's digest is not what a decode would give. That happens for real: a
/// layer a `.tmiignore` pattern substituted keeps HEAD's digest beside the
/// working file's key, and the next `add` must not write a blob for its
/// working pixels (conformance's `layerignore` scenario saw the extra blob
/// and layer tree when the pipeline decoded such a layer anyway).
#[test]
fn a_matching_prior_decode_is_recorded_as_given() {
    let example = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../example");
    for name in ["tama.clip", "kakenhi-kaguya.mdp"] {
        let data = std::fs::read(example.join(name)).unwrap_or_else(|e| panic!("example/{name}: {e}"));
        let digest = tmi_store::oid::sha256_hex(&data);
        let blobs = LayerBlobs::All;
        let mut warns = Warnings::default();
        let first = crate::import_file(name, &data, &digest, &opts(&blobs, false, Limits::NONE), &mut warns).unwrap();
        let mut layers = Vec::new();
        walk_layers(&first.layers, &mut layers);
        let fake_sha = Value::Str("0".repeat(64));
        let fake_bbox = Value::Array((1..=4).map(|v| Value::Int(tmi_store::json::Int::Small(v))).collect());
        let mut prior = std::collections::HashMap::new();
        for l in &layers {
            if let Some(src @ Value::Str(_)) = l.source_meta.get("pixel_source") {
                prior.insert(
                    l.stable_source_id.clone(),
                    tmi_import::PriorDecode { pixel_source: Some(src.clone()), pixel_sha256: fake_sha.clone(), pixel_bbox: fake_bbox.clone() },
                );
            }
        }
        assert!(prior.len() > 1, "{name}: layers with a pixel source");
        let o = Options { reuse_decoded: Some(&prior), ..opts(&blobs, false, Limits::NONE) };
        let second = crate::import_file(name, &data, &digest, &o, &mut warns).unwrap();
        let mut again = Vec::new();
        walk_layers(&second.layers, &mut again);
        let mut reused = 0;
        for l in again {
            if !prior.contains_key(&l.stable_source_id) {
                continue;
            }
            let text = |v: &Value| tmi_store::json::dumps(v, &tmi_store::json::DumpOptions::CANONICAL);
            assert_eq!(l.pixel_sha256.as_ref().map(text), Some(text(&fake_sha)), "{name}: {} pixel_sha256", l.stable_source_id);
            assert_eq!(text(&l.pixel_bbox), text(&fake_bbox), "{name}: {} pixel_bbox", l.stable_source_id);
            assert!(l.blob_bytes.is_none(), "{name}: {} blob", l.stable_source_id);
            reused += 1;
        }
        assert_eq!(reused, prior.len(), "{name}");
    }
}

/// PLUGINS.md 6.2: a plugin's composite is cached under
/// `sha256(source_digest ‖ component_sha256 ‖ compositor)`, so another
/// build of the plugin (a new version: another component) reading the same
/// bytes draws its own composite instead of taking the old one, and each
/// build still finds its own.
#[test]
fn a_new_build_of_a_plugin_does_not_take_the_old_builds_composite() {
    struct Plugin(Synthetic);
    impl FormatReader for Plugin {
        fn info(&self) -> &PluginInfo {
            self.0.info()
        }
        fn probe(&self, _: &str, _: &[u8], _: u64) -> Confidence {
            Confidence::Yes
        }
        fn read_structure(&self, s: &Source, c: &ReadCx) -> Result<Structure, ReadError> {
            self.0.read_structure(s, c)
        }
    }
    let dir = std::env::temp_dir().join(format!("tmi-format-plugin-cache-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let cache = tmi_import::RenderCache::new(&dir);
    let blobs = LayerBlobs::All;
    let mut cached = opts(&blobs, true, Limits::NONE);
    cached.cache = Some(&cache);
    let uncached = opts(&blobs, true, Limits::NONE);
    let importer = |c: &str| pipeline::Importer { id: "synthetic".into(), version: c.into(), component_sha256: c.into(), abi: "tmi:format@0.1.0".into() };
    // the old build draws the folder at half opacity, the new one at full
    let old = Plugin(generic_doc(Composite::Render(Compositor::GenericV1)));
    let mut new = Plugin(generic_doc(Composite::Render(Compositor::GenericV1)));
    new.0.structure.layers[1].opacity = 1.0;
    let run = |r: &Plugin, c: &str, o: &Options| pipeline::import_plugin(r, &importer(c), "doc.syn", b"", "digest", o, &mut Warnings::default()).unwrap().composite_png;
    let (old_png, new_png) = (run(&old, "aa01", &uncached), run(&new, "bb02", &uncached));
    assert_ne!(old_png, new_png);
    assert_eq!(run(&old, "aa01", &cached), old_png);
    assert_eq!(run(&new, "bb02", &cached), new_png, "the new build draws its own");
    assert_eq!(run(&old, "aa01", &cached), old_png, "the old build's is still there");
    for c in ["aa01", "bb02"] {
        let key = crate::compose::render_cache_key("digest", Some(c), Some(Compositor::GenericV1));
        assert!(cache.get(&key).is_some(), "{c}");
    }
    assert!(cache.get("digest").is_none(), "never under the built-ins' key");
    let _ = std::fs::remove_dir_all(&dir);
}
