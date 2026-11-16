//! `pipeline::import` (PLUGINS.md 8.1): one reader, one file, into what tmiki
//! stores. `read_structure`, the host's checks, the recipes decoded into
//! `LayerCrop`s, `digest_and_encode`, the composite and its thumbnail, and
//! the `ImportedFile` / `ImportedLayer` records `tmi-import` always made.
//!
//! `import` runs the built-in readers (their records as they always were);
//! `import_plugin` a format plugin's, whose records carry an `importer` and
//! the host's `pixel_source` / `semantic_digest` / `write_capability`
//! (PLUGINS.md 4.2, P6).

use rayon::prelude::*;
use tmi_import::crop::{bbox_value, digest_and_encode, LayerCrop};
use tmi_import::{composite_pngs, decline, round6, Decline, ImportedFile, ImportedLayer, Options, Rgba, Warnings};
use tmi_store::json::{self, Int, Map, Value};

use crate::compose::render_cache_key;
use crate::reader::{FormatReader, ReadCx, Source};
use crate::recipe::{decode_bitmap, decode_layer_crop, DecodeEnv, DecodedBitmap};
use crate::types::*;
use crate::validate;

/// Why an import did not produce a record.
#[derive(Debug)]
pub enum ImportError {
    /// The reader said so (`NotThisFormat` for a built-in raster: the caller stores an opaque blob).
    Read(ReadError),
    /// Not reproduced here: the command goes to Python.
    Decline(Decline),
}

impl From<ReadError> for ImportError {
    fn from(e: ReadError) -> Self {
        ImportError::Read(e)
    }
}

impl From<Decline> for ImportError {
    fn from(e: Decline) -> Self {
        ImportError::Decline(e)
    }
}

fn corrupt(msg: impl Into<String>) -> ImportError {
    ImportError::Read(ReadError::Corrupt(msg.into()))
}

fn int(v: i64) -> Value {
    Value::Int(Int::Small(v))
}

/// `meta` entries as the JSON object a record holds.
fn meta_map(entries: &[MetaEntry], env: &DecodeEnv) -> Result<Map, ImportError> {
    let mut m = Map::new();
    for e in entries {
        let v = match &e.value {
            MetaValue::Null => Value::Null,
            MetaValue::Boolean(b) => Value::Bool(*b),
            MetaValue::Integer(i) => int(*i),
            MetaValue::Number(n) => Value::Float(*n),
            MetaValue::Text(s) => Value::Str(s.clone()),
            MetaValue::Sha256Of(b) => {
                let bytes = crate::recipe::transform::resolve(env.source, b)?;
                Value::Str(tmi_store::oid::sha256_hex(&bytes))
            }
            MetaValue::Json(s) => json::loads(s).map_err(|je| corrupt(format!("meta {:?}: invalid JSON ({})", e.key, je.0)))?,
        };
        m.insert(e.key.clone(), v);
    }
    Ok(m)
}

/// A `legacy_extra_meta` entry's value (never `Sha256Of`, which needs the
/// source bytes `meta_map` resolves for a reader's own `Layer::meta`).
fn simple_meta_value(v: MetaValue) -> Result<Value, ImportError> {
    Ok(match v {
        MetaValue::Null => Value::Null,
        MetaValue::Boolean(b) => Value::Bool(b),
        MetaValue::Integer(i) => int(i),
        MetaValue::Number(n) => Value::Float(n),
        MetaValue::Text(s) => Value::Str(s),
        MetaValue::Json(s) => json::loads(&s).map_err(|je| corrupt(format!("legacy meta: invalid JSON ({})", je.0)))?,
        MetaValue::Sha256Of(_) => return Err(corrupt("legacy_extra_meta may not return Sha256Of")),
    })
}

/// A layer's decoded pixels handed down from the composite step.
struct Prepared<'a> {
    index: usize,
    width: usize,
    height: usize,
    data: &'a [u8],
}

struct LayerPixelsOut {
    /// A str, or a prior decode's value as recorded.
    sha: Option<Value>,
    bbox: Value,
    blob: Option<Vec<u8>>,
    /// The decoded pixels, kept for the generic compositor.
    crop: Option<LayerCrop>,
    /// A plugin layer's `pixel_source`, which the host makes from the recipe
    /// (`recipe_key`; a deferred layer's recipe is only known here).
    source_key: Option<String>,
}

/// What a format plugin's records carry (PLUGINS.md 4.2): the `importer`
/// entry key, and the component that keys the render cache and a plugin
/// codec's `pixel_source`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Importer {
    pub id: String,
    pub version: String,
    /// SHA-256 of the component's bytes (not an object id).
    pub component_sha256: String,
    /// The interface the component exports, `tmi:format@0.1.0`.
    pub abi: String,
}

/// The `component_sha256` of a development plugin's records (`tmiki plugin
/// dev`; PLUGINS.md 5: no commit takes them). The same as
/// `tmi_plugin_dev::DEV_COMPONENT`.
pub const DEV_COMPONENT: &str = "dev";

impl Importer {
    /// A development plugin's (`tmiki plugin dev`): no component pins what it
    /// does, so nothing it made is reused (the render cache, prior decodes).
    pub fn is_dev(&self) -> bool {
        self.component_sha256 == DEV_COMPONENT
    }

    /// The `importer` value of an index / tree entry.
    pub fn record(&self) -> Map {
        let mut m = Map::new();
        m.insert("id".into(), Value::Str(self.id.clone()));
        m.insert("version".into(), Value::Str(self.version.clone()));
        m.insert("component_sha256".into(), Value::Str(self.component_sha256.clone()));
        m.insert("abi".into(), Value::Str(self.abi.clone()));
        m
    }
}

/// `semantic_digest_version` of a plugin layer's `semantic_digest` (the
/// SHA-256 of the canonical JSON of its `semantic` entries).
pub const PLUGIN_SEMANTIC_DIGEST_VERSION: i64 = 1;

/// `write_capability` as tmiki has always spelled it (PLUGINS.md 4.2).
pub fn write_capability_name(w: WriteCapability) -> &'static str {
    match w {
        WriteCapability::None => "metadata-only",
        WriteCapability::Pixels | WriteCapability::PixelsAndProps => "raster-pixels",
    }
}

/// Layer `index`'s pixels (which it must have), decoded.
fn decode_crop(
    reader: &dyn FormatReader,
    src: &Source,
    structure: &Structure,
    index: usize,
    env: &DecodeEnv,
    prepared: Option<&Prepared>,
) -> Result<LayerCrop, ImportError> {
    let layer = &structure.layers[index];
    if let Some(native) = reader.native_pixels(src, &layer.stable_id, env.cx) {
        return Ok(native?);
    }
    Ok(match (&layer.pixels, prepared) {
        (LayerPixels::None, _) => unreachable!("a layer without pixels is not decoded"),
        (_, Some(p)) if p.index == index => LayerCrop::from_full(p.data, p.width, p.height),
        (LayerPixels::Ready(b), _) => decode_layer_crop(env, b)?,
        (LayerPixels::Deferred(token), _) => {
            let b = reader.read_layer(src, &structure.context, *token, env.cx)?;
            decode_layer_crop(env, &b)?
        }
    })
}

/// Layer `index`'s record values; `keep`: also hand back its decoded pixels
/// (decoding them even when a prior decode is reused for the record).
#[allow(clippy::too_many_arguments)]
fn layer_pixels(
    reader: &dyn FormatReader,
    src: &Source,
    structure: &Structure,
    index: usize,
    env: &DecodeEnv,
    opts: &Options,
    prepared: Option<&Prepared>,
    tick: &tmi_import::LayerTicker,
    keep: bool,
    plugin: Option<&Importer>,
) -> Result<LayerPixelsOut, ImportError> {
    let layer = &structure.layers[index];
    if matches!(layer.pixels, LayerPixels::None) {
        if reader.ticks_every_non_folder_layer() && layer.kind != LayerKind::Folder {
            tick.tick();
        }
        return Ok(LayerPixelsOut { sha: None, bbox: Value::Null, blob: None, crop: None, source_key: None });
    }
    if let Some(importer) = plugin {
        // The recipe first (a deferred layer's comes from `read-layer`, once),
        // then the host's key of it, then (unless a prior decode of the same
        // key is reused) its pixels.
        let deferred;
        let bitmap = match &layer.pixels {
            LayerPixels::Ready(b) => b,
            LayerPixels::Deferred(token) => {
                deferred = reader.read_layer(src, &structure.context, *token, env.cx)?;
                &deferred
            }
            LayerPixels::None => unreachable!(),
        };
        let key = crate::recipe_key::pixel_source(src.data, bitmap, &importer.component_sha256);
        let decode = || -> Result<LayerCrop, ImportError> {
            Ok(match prepared {
                Some(p) if p.index == index => LayerCrop::from_full(p.data, p.width, p.height),
                _ => decode_layer_crop(env, bitmap)?,
            })
        };
        if let Some(prior) = opts.reusable(&layer.stable_id, &key) {
            let crop = if keep { Some(decode()?) } else { None };
            tick.tick();
            return Ok(LayerPixelsOut { sha: Some(prior.pixel_sha256.clone()), bbox: prior.pixel_bbox.clone(), blob: None, crop, source_key: Some(key) });
        }
        let crop = decode()?;
        tick.tick();
        let (digest, blob) = digest_and_encode(&crop, &layer.stable_id, opts)?;
        return Ok(LayerPixelsOut {
            sha: Some(Value::Str(digest)),
            bbox: bbox_value(crop.alpha_bbox()),
            blob,
            crop: keep.then_some(crop),
            source_key: Some(key),
        });
    }
    // A prior decode of the same pixel source (`legacy_source_key`'s doc):
    // nothing to decode, digest or encode for the record.
    if let Some(prior) = reader.legacy_source_key(src, env.cx, layer).and_then(|key| opts.reusable(&layer.stable_id, &key)) {
        let crop = if keep { Some(decode_crop(reader, src, structure, index, env, prepared)?) } else { None };
        tick.tick();
        return Ok(LayerPixelsOut { sha: Some(prior.pixel_sha256.clone()), bbox: prior.pixel_bbox.clone(), blob: None, crop, source_key: None });
    }
    let crop = decode_crop(reader, src, structure, index, env, prepared)?;
    tick.tick();
    let (digest, blob) = digest_and_encode(&crop, &layer.stable_id, opts)?;
    Ok(LayerPixelsOut { sha: Some(Value::Str(digest)), bbox: bbox_value(crop.alpha_bbox()), blob, crop: keep.then_some(crop), source_key: None })
}

fn into_rgba(d: DecodedBitmap) -> Rgba {
    Rgba { width: d.width, height: d.height, data: d.data }
}

/// Imports `source` (a file named `name` with sha256 `digest`) with `reader`.
/// `warns` collects `reader.legacy_warnings` at the end (CLIP's stderr
/// notes; MDP2 and the raster readers leave it empty).
pub fn import(
    reader: &dyn FormatReader,
    name: &str,
    source: &[u8],
    digest: &str,
    opts: &Options,
    warns: &mut Warnings,
) -> Result<ImportedFile, ImportError> {
    if !reader.builtin() {
        return Err(decline(format!("plugin {}: a plugin's records need its importer (pipeline::import_plugin)", reader.info().id)).into());
    }
    import_any(reader, None, name, source, digest, opts, warns)
}

/// A format plugin's import (PLUGINS.md 4.2, P6): `import` with the
/// plugin's record. The plugin's `probe` must say the file may be its
/// format (else `NotThisFormat`: `.tmiattributes` mapped a file the plugin
/// does not read); the entry gets `importer`, each layer's `source_meta`
/// the host-made `pixel_source` (from the recipe), `semantic_digest` /
/// `semantic_digest_version` (from `semantic`) and `write_capability`; the
/// render cache is keyed by the component too (6.2); the structure's
/// warnings are printed.
pub fn import_plugin(
    reader: &dyn FormatReader,
    importer: &Importer,
    name: &str,
    source: &[u8],
    digest: &str,
    opts: &Options,
    warns: &mut Warnings,
) -> Result<ImportedFile, ImportError> {
    if reader.builtin() {
        return Err(decline("a built-in reader has no importer record").into());
    }
    let head = &source[..source.len().min(reader.info().probe_bytes as usize)];
    if reader.probe(name, head, source.len() as u64) == Confidence::No {
        let why = if source.is_empty() { "the file is empty" } else { "the plugin looked at the start of the file and said no" };
        return Err(ImportError::Read(ReadError::NotThisFormat(why.to_string())));
    }
    if importer.is_dev() {
        // the same "dev" for every build of the plugin: a cached composite or
        // a prior decode keyed by it could be another build's
        let fresh = Options { cache: None, reuse_decoded: None, reuse_pixels: None, ..*opts };
        return import_any(reader, Some(importer), name, source, digest, &fresh, warns);
    }
    import_any(reader, Some(importer), name, source, digest, opts, warns)
}

fn import_any(
    reader: &dyn FormatReader,
    plugin: Option<&Importer>,
    name: &str,
    source: &[u8],
    digest: &str,
    opts: &Options,
    warns: &mut Warnings,
) -> Result<ImportedFile, ImportError> {
    let max_pixels = Some(if reader.uses_layered_limit() { opts.limits.layered } else { opts.limits.raster }).filter(|&m| m != 0);
    let cx = ReadCx::new(max_pixels);
    let src = Source::new(name, source);
    let structure = reader.read_structure(&src, &cx)?;
    validate::check(&structure, source.len() as u64)?;
    if plugin.is_some() {
        for w in &structure.warnings {
            warns.lines.push(format!("tmiki: warning: {name}: {} ({})", w.message, w.code));
        }
    }
    let env = DecodeEnv { source, reader: Some(reader), cx: &cx };
    check_canvas(&env, &structure.canvas, reader.uses_layered_limit())?;

    let native_composite = reader.has_native_composite() && opts.render_composite;
    // The composite. An embedded one is used only at the canvas' size, else
    // the generic compositor draws it (PLUGINS.md 2.2); a built-in may draw
    // its own (PLUGINS.md 8.1's `NativeComposite`, e.g. MDP2).
    let mut generic = match (&structure.composite, opts.render_composite, native_composite) {
        (Composite::Render(c), true, false) => Some(*c),
        _ => None,
    };
    let comp_bitmap = match (&structure.composite, opts.render_composite, native_composite) {
        (Composite::Image(b), true, false) => Some(b),
        _ => None,
    };
    // A composite that is a layer's own recipe is decoded once; another
    // embedded one is decoded first, to see its size.
    let shared = comp_bitmap.and_then(|cb| structure.layers.iter().position(|l| matches!(&l.pixels, LayerPixels::Ready(b) if b == cb)));
    let shared_image = match shared {
        Some(i) => {
            let LayerPixels::Ready(b) = &structure.layers[i].pixels else { unreachable!() };
            Some((i, into_rgba(decode_bitmap(&env, b)?)))
        }
        None => None,
    };
    let canvas = &structure.canvas;
    let at_canvas_size = |image: &Rgba| (image.width, image.height) == (canvas.width as usize, canvas.height as usize);
    let mut embedded = match (&shared_image, comp_bitmap) {
        (Some(_), _) => None,
        (None, Some(b)) => Some(into_rgba(decode_bitmap(&env, b)?)),
        (None, None) => None,
    };
    let wrong_size = shared_image.as_ref().map(|(_, i)| i).or(embedded.as_ref()).filter(|i| !at_canvas_size(i)).map(|i| (i.width, i.height));
    if let Some((w, h)) = wrong_size {
        warns.lines.push(format!(
            "tmiki: warning: {name}: the embedded composite is {w}x{h}, not the canvas' {}x{}; drawing it with generic-v1",
            canvas.width, canvas.height
        ));
        embedded = None;
        generic = Some(Compositor::GenericV1);
    }
    let shared_composite = shared_image.as_ref().filter(|(_, i)| at_canvas_size(i)).map(|(_, i)| i);
    // generic-v1: the render cache first (PLUGINS.md 6.2's key); on a miss
    // the layers' decoded pixels are kept for the compositor.
    let cache_key = render_cache_key(digest, plugin.map(|p| p.component_sha256.as_str()), generic);
    let cached = match generic {
        Some(_) => opts.cache.and_then(|c| c.get(&cache_key)),
        None => None,
    };
    let keep = match (generic, &cached) {
        (Some(_), None) => crate::compose::reads_pixels(&structure),
        _ => vec![false; structure.layers.len()],
    };
    let prepared = shared_image.as_ref().map(|(i, image)| Prepared { index: *i, width: image.width, height: image.height, data: &image.data });
    // Announces "decoding layers 0/N" before the parallel decode starts (N:
    // the layers that actually decode, as the old per-format importers'
    // `LayerTicker` counted only their renderable leaves).
    let decodable = if reader.ticks_every_non_folder_layer() {
        structure.layers.iter().filter(|l| l.kind != LayerKind::Folder).count()
    } else {
        structure.layers.iter().filter(|l| !matches!(l.pixels, LayerPixels::None)).count()
    };
    let tick = tmi_import::LayerTicker::new(opts.progress, if reader.shows_layer_progress() { decodable } else { 0 });
    let decode_layers = || -> Result<Vec<LayerPixelsOut>, ImportError> {
        (0..structure.layers.len())
            .into_par_iter()
            .map(|i| layer_pixels(reader, &src, &structure, i, &env, opts, prepared.as_ref(), &tick, keep[i], plugin))
            .collect()
    };
    let (composite_png, thumbnail_png, mut layers) = match generic {
        // The compositor needs the layers: they decode first.
        Some(c) => {
            let layers = decode_layers()?;
            let (png, thumb) = match cached {
                Some(hit) => hit,
                None => tmi_import::composite_cached(opts.cache, &cache_key, opts.progress, || {
                    let crops: Vec<Option<&LayerCrop>> = layers.iter().map(|l| l.crop.as_ref()).collect();
                    let data = crate::compose::render(&structure, &crops, [255, 255, 255, 255]).map_err(decline)?;
                    let image = Rgba { width: canvas.width as usize, height: canvas.height as usize, data };
                    Ok(tmi_import::Rendered { image, png: None, text: Some(crate::compose::composite_text(c)), encoded_in_render: false })
                })?,
            };
            (png, thumb, layers)
        }
        None => {
            let (comp, layers) = rayon::join(
                || -> Result<(Vec<u8>, Vec<u8>), ImportError> {
                    if native_composite {
                        return Ok(tmi_import::composite_cached(opts.cache, digest, opts.progress, || {
                            reader.native_composite(&src, &structure, &cx, opts).expect("has_native_composite said yes")
                        })?);
                    }
                    match (shared_composite, &embedded) {
                        (Some(image), _) | (None, Some(image)) => Ok(composite_pngs(image, None)?),
                        (None, None) => Ok((Vec::new(), Vec::new())),
                    }
                },
                decode_layers,
            );
            let (png, thumb) = comp?;
            (png, thumb, layers?)
        }
    };
    for l in &mut layers {
        l.crop = None;
    }
    if let Some(msg) = reader.legacy_mid_progress() {
        tmi_import::report(opts.progress, || msg.into());
    }

    // the records
    let mut flat: Vec<Option<ImportedLayer>> = Vec::with_capacity(layers.len());
    for (l, px) in structure.layers.iter().zip(layers) {
        let mut source_meta = meta_map(&l.meta, &env)?;
        if plugin.is_some() {
            // the host's own keys (validate::reserved_key keeps them out of `meta`)
            if let Some(key) = &px.source_key {
                source_meta.insert("pixel_source".into(), Value::Str(key.clone()));
            }
            let semantic = meta_map(&l.semantic, &env)?;
            source_meta.insert("semantic_digest".into(), Value::Str(tmi_import::semantic_digest(&Value::Object(semantic))));
            source_meta.insert("semantic_digest_version".into(), int(PLUGIN_SEMANTIC_DIGEST_VERSION));
            source_meta.insert("write_capability".into(), Value::Str(write_capability_name(l.write).into()));
        }
        let key = reader.legacy_source_key(&src, &cx, l);
        if reader.legacy_pixel_source_always() {
            source_meta.insert("pixel_source".into(), key.map_or(Value::Null, Value::Str));
        } else if let Some(key) = key {
            source_meta.insert("pixel_source".into(), Value::Str(key));
        }
        for (k, v) in reader.legacy_extra_meta(&cx, l) {
            source_meta.insert(k, simple_meta_value(v)?);
        }
        flat.push(Some(ImportedLayer {
            layer_id: l.id.clone(),
            stable_source_id: l.stable_id.clone(),
            name: l.name.clone(),
            kind: l.kind.name().into(),
            visible: l.visible,
            opacity: round6(l.opacity),
            blend_mode: reader.legacy_blend_mode(&cx, l).unwrap_or_else(|| l.blend.name().into()),
            clip_to_below: l.clip_to_below,
            offset_x: l.offset_x,
            offset_y: l.offset_y,
            blob_bytes: px.blob,
            pixel_bbox: px.bbox,
            pixel_sha256: px.sha,
            source_meta,
            children: Vec::new(),
        }));
    }
    let mut kids: Vec<Vec<usize>> = vec![Vec::new(); flat.len()];
    let mut roots = Vec::new();
    for (i, l) in structure.layers.iter().enumerate() {
        match l.parent {
            Some(p) => kids[p as usize].push(i),
            None => roots.push(i),
        }
    }
    fn build(i: usize, flat: &mut [Option<ImportedLayer>], kids: &[Vec<usize>]) -> ImportedLayer {
        let mut l = flat[i].take().expect("a layer is built once");
        l.children = kids[i].iter().map(|&k| build(k, flat, kids)).collect();
        l
    }
    let layers: Vec<ImportedLayer> = roots.into_iter().map(|i| build(i, &mut flat, &kids)).collect();

    let source_format = reader.source_format(name, &structure);
    let mut canvas_meta = Map::new();
    canvas_meta.insert("width".into(), int(canvas.width as i64));
    canvas_meta.insert("height".into(), int(canvas.height as i64));
    canvas_meta.insert(
        "dpi".into(),
        match canvas.dpi {
            None => Value::Null,
            Some(d) if reader.legacy_integer_dpi() => int(d as i64),
            Some(d) => Value::Float(d),
        },
    );
    canvas_meta.insert(
        "color_profile".into(),
        match &canvas.profile {
            ColorProfile::None => Value::Null,
            ColorProfile::Embedded(_) => Value::Str("embedded".into()),
            ColorProfile::Named(n) => Value::Str(n.clone()),
        },
    );
    canvas_meta.insert("source_format".into(), Value::Str(source_format.clone()));
    reader.legacy_warnings(&cx, warns);
    let mut source_meta = meta_map(&structure.file_meta, &env)?;
    for (k, v) in reader.legacy_extra_file_meta(&cx) {
        source_meta.insert(k, Value::Str(v));
    }
    Ok(ImportedFile {
        source_format,
        source_digest: digest.to_string(),
        canvas_meta,
        layers,
        composite_png,
        thumbnail_png,
        source_meta,
        importer: plugin.map(Importer::record),
    })
}

/// The canvas is held to the pixel limit like any bitmap (`layered`: the
/// layered-canvas threshold and message, `check_layered`'s `width == 0 ||
/// height == 0` exemption included; else the raster one, as P1 had it).
fn check_canvas(env: &DecodeEnv, c: &Canvas, layered: bool) -> Result<(), ReadError> {
    if layered {
        if c.width == 0 || c.height == 0 {
            return Ok(());
        }
        let pixels = (c.width as u128) * (c.height as u128);
        if env.cx.max_pixels.is_some_and(|m| m != 0 && pixels > m as u128) {
            return Err(ReadError::TooLarge(format!("canvas of {pixels} pixels is over core.maxImagePixels: Python reports it")));
        }
        return Ok(());
    }
    let pixels = (c.width.max(1) as u128) * (c.height.max(1) as u128);
    if env.cx.max_pixels.is_some_and(|m| m != 0 && pixels > m as u128) {
        return Err(crate::recipe::too_large(c.width as usize, c.height as usize));
    }
    Ok(())
}
