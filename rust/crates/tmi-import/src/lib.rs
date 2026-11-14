//! tmi/importers.py in Rust (RUST_MIGRATION.md 3b-2): a working file
//! (CLIP, MDP2, PNG / JPEG) into what tmiki stores for it: the layer records
//! (`ImportedLayer`), their pixel digests and TMIL blobs, the composite PNG
//! (tagged `tmi-composite` when it is a layered render a later version can
//! build on), its thumbnail, `canvas_meta` and `source_meta`, and the
//! warnings Python prints, in its words and order.
//!
//! The front door, `import_file`, is `tmi_format::import_file` (PLUGINS.md
//! P1): CLIP and MDP2 are imported here, every other file through the
//! reader registry of `tmi-format`, whose built-in raster reader is what
//! `raster.rs` was.
//!
//! Everything here gives Python's bytes. Where Python would do something
//! this crate does not reproduce (a layered file Python tracks as an opaque
//! blob after a parse error, an image over `core.maxImagePixels`, a value
//! of an unexpected type in a CLIP database), the import returns `Decline`
//! and the caller hands the whole command to Python. Whether a file is an
//! image at all is tmiki's own rule (`tmi_image::import`, PLUGINS.md P0e): no
//! Pillow, and no `Decline` for a file tmiki cannot read, which is stored as
//! an opaque blob.

pub mod cache;
pub mod clip;
pub mod crop;
pub use tmi_image::header;
pub mod limit;
pub mod partial;
pub mod warn;

use std::collections::{HashMap, HashSet};

use tmi_store::json::{self, Map, Value};
use tmi_store::oid::Sha256;

pub use cache::RenderCache;
pub use limit::Limits;
pub use warn::Warnings;

/// Not reproduced here: run the command in Python.
#[derive(Debug, Clone)]
pub struct Decline(pub String);

impl std::fmt::Display for Decline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

pub type Res<T> = Result<T, Decline>;

pub fn decline(why: impl Into<String>) -> Decline {
    Decline(why.into())
}

/// A packed RGBA8 image.
#[derive(Clone)]
pub struct Rgba {
    pub width: usize,
    pub height: usize,
    pub data: Vec<u8>,
}

/// `import_file(layer_blobs=...)`: every layer's blob, or those of the
/// stable ids listed.
pub enum LayerBlobs {
    All,
    Only(HashSet<String>),
}

impl LayerBlobs {
    pub fn wants(&self, sid: &str) -> bool {
        match self {
            LayerBlobs::All => true,
            LayerBlobs::Only(set) => set.contains(sid),
        }
    }
}

/// A prior decode of a layer (`reuse_decoded`): `{pixel_source,
/// pixel_sha256, pixel_bbox}`.
#[derive(Clone, Debug)]
pub struct PriorDecode {
    pub pixel_source: Option<Value>,
    pub pixel_sha256: Value,
    pub pixel_bbox: Value,
}

/// clip_partial.py `CompositeBase`: an earlier version of the same CLIP
/// file, loaded only when needed.
pub trait CompositeBase: Sync {
    fn load_composite_png(&self) -> Result<Vec<u8>, String>;
    fn load_source(&self) -> Result<Vec<u8>, String>;
}

/// `on_progress(msg)`: the CLI's progress line (called from worker threads too).
pub type Progress<'a> = &'a (dyn Fn(&str) + Sync);

/// `if on_progress: on_progress(msg)`.
pub fn report(progress: Option<Progress>, msg: impl FnOnce() -> String) {
    if let Some(p) = progress {
        p(&msg());
    }
}

/// importers.py `_layer_ticker`: "decoding layers k/total", k counted
/// across the decode pool's threads, one report at a time.
pub struct LayerTicker<'a> {
    progress: Option<Progress<'a>>,
    total: usize,
    done: std::sync::Mutex<usize>,
}

impl<'a> LayerTicker<'a> {
    /// Says "decoding layers 0/total" (nothing for no layers: Python makes
    /// no ticker then).
    pub fn new(progress: Option<Progress<'a>>, total: usize) -> LayerTicker<'a> {
        let progress = progress.filter(|_| total > 0);
        report(progress, || format!("decoding layers 0/{total}"));
        LayerTicker { progress, total, done: std::sync::Mutex::new(0) }
    }

    pub fn tick(&self) {
        let Some(p) = self.progress else { return };
        let mut done = self.done.lock().unwrap_or_else(|e| e.into_inner());
        *done += 1;
        p(&format!("decoding layers {}/{}", *done, self.total));
    }
}

pub struct Options<'a> {
    pub render_composite: bool,
    pub cache: Option<&'a RenderCache>,
    pub reuse_pixels: Option<&'a HashMap<String, String>>,
    pub reuse_decoded: Option<&'a HashMap<String, PriorDecode>>,
    pub layer_blobs: &'a LayerBlobs,
    pub composite_base: Option<&'a dyn CompositeBase>,
    pub limits: Limits,
    /// `TMI_VERIFY_PARTIAL`: check an incremental composite against the
    /// whole-canvas render.
    pub verify_partial: bool,
    /// `on_progress`: the import's phases (decoding layers, rendering the
    /// composite, ...), as importers.py reports them.
    pub progress: Option<Progress<'a>>,
}

impl Options<'_> {
    /// `_reusable_decode(reuse_decoded, sid, source_key)`.
    pub fn reusable(&self, sid: &str, source_key: &str) -> Option<&PriorDecode> {
        let prior = self.reuse_decoded?.get(sid)?;
        let same = matches!(&prior.pixel_source, Some(Value::Str(s)) if s == source_key);
        (same && prior.pixel_sha256.truthy()).then_some(prior)
    }
}

#[derive(Clone, Debug)]
pub struct ImportedLayer {
    pub layer_id: String,
    pub stable_source_id: String,
    pub name: String,
    pub kind: String,
    pub visible: bool,
    pub opacity: f64,
    pub blend_mode: String,
    pub clip_to_below: bool,
    pub offset_x: i64,
    pub offset_y: i64,
    pub blob_bytes: Option<Vec<u8>>,
    /// A list of four ints, or null (a reused decode keeps the prior value).
    pub pixel_bbox: Value,
    /// The digest (a str; a reused decode keeps the prior value).
    pub pixel_sha256: Option<Value>,
    pub source_meta: Map,
    pub children: Vec<ImportedLayer>,
}

impl ImportedLayer {
    /// The plain fields as JSON values (`getattr(layer, key)`).
    pub fn field(&self, key: &str) -> Value {
        use tmi_store::json::Int;
        match key {
            "layer_id" => Value::Str(self.layer_id.clone()),
            "stable_source_id" => Value::Str(self.stable_source_id.clone()),
            "name" => Value::Str(self.name.clone()),
            "kind" => Value::Str(self.kind.clone()),
            "visible" => Value::Bool(self.visible),
            "opacity" => Value::Float(self.opacity),
            "blend_mode" => Value::Str(self.blend_mode.clone()),
            "clip_to_below" => Value::Bool(self.clip_to_below),
            "offset_x" => Value::Int(Int::Small(self.offset_x)),
            "offset_y" => Value::Int(Int::Small(self.offset_y)),
            "mask_ref" => Value::Null,
            "pixel_bbox" => self.pixel_bbox.clone(),
            "pixel_sha256" => self.pixel_sha256.clone().unwrap_or(Value::Null),
            "source_meta" => Value::Object(self.source_meta.clone()),
            _ => Value::Null,
        }
    }
}

pub struct ImportedFile {
    pub source_format: String,
    pub source_digest: String,
    pub canvas_meta: Map,
    pub layers: Vec<ImportedLayer>,
    pub composite_png: Vec<u8>,
    pub thumbnail_png: Vec<u8>,
    pub source_meta: Map,
    /// A format plugin's record (PLUGINS.md 4.2: id, version,
    /// component_sha256, abi), a top-level key of the index and tree entry.
    /// None for the built-in formats, whose records never have one (Q3).
    pub importer: Option<Map>,
}

/// `Path(name).suffix` (lowercased by the caller).
pub fn path_suffix(name: &str) -> &str {
    match name.rfind('.') {
        Some(i) if i > 0 && i < name.len() - 1 => &name[i..],
        _ => "",
    }
}

/// `Path(name).stem`.
pub fn path_stem(name: &str) -> &str {
    match name.rfind('.') {
        Some(i) if i > 0 && i < name.len() - 1 => &name[..i],
        _ => name,
    }
}

/// `import_opaque_binary(path)`: one `blob:0` layer without pixels, no
/// previews.
pub fn opaque(name: &str, source: &[u8], digest: &str) -> ImportedFile {
    use tmi_store::json::Int;
    let size = Value::Int(Int::Small(source.len() as i64));
    let mut layer_meta = Map::new();
    layer_meta.insert("source_path_name".into(), Value::Str(name.to_string()));
    layer_meta.insert("byte_size".into(), size.clone());
    let mut canvas = Map::new();
    for key in ["width", "height", "dpi", "color_profile"] {
        canvas.insert(key.into(), Value::Null);
    }
    canvas.insert("source_format".into(), Value::Str("binary".into()));
    let mut source_meta = Map::new();
    source_meta.insert("byte_size".into(), size);
    source_meta.insert("write_capability".into(), Value::Str("opaque-blob".into()));
    ImportedFile {
        source_format: "binary".into(),
        source_digest: digest.to_string(),
        canvas_meta: canvas,
        layers: vec![ImportedLayer {
            layer_id: "blob:0".into(),
            stable_source_id: "blob:0".into(),
            name: name.to_string(),
            kind: "binary".into(),
            visible: true,
            opacity: 1.0,
            blend_mode: "normal".into(),
            clip_to_below: false,
            offset_x: 0,
            offset_y: 0,
            blob_bytes: None,
            pixel_bbox: Value::Null,
            pixel_sha256: None,
            source_meta: layer_meta,
            children: Vec::new(),
        }],
        composite_png: Vec::new(),
        thumbnail_png: Vec::new(),
        source_meta,
        importer: None,
    }
}

/// `image_to_png_bytes(image)` and `_thumbnail_png(image)`, side by side:
/// the composite PNG (with the tEXt chunk `text`) and the 384px thumbnail.
pub fn composite_pngs(image: &Rgba, text: Option<(String, String)>) -> Res<(Vec<u8>, Vec<u8>)> {
    composite_pngs_reporting(image, text, None)
}

/// `composite_pngs`, saying "rendering thumbnail" when the composite PNG is
/// done (Python waits for the thumbnail's thread then).
fn composite_pngs_reporting(image: &Rgba, text: Option<(String, String)>, progress: Option<Progress>) -> Res<(Vec<u8>, Vec<u8>)> {
    let (png, thumb) = rayon::join(
        || {
            let png = png(image, text.as_ref());
            report(progress, || "rendering thumbnail".into());
            png
        },
        || thumbnail_png(image),
    );
    Ok((png?, thumb?))
}

/// tmiki's PNG of RGBA pixels (mtpng).
pub fn png(image: &Rgba, text: Option<&(String, String)>) -> Res<Vec<u8>> {
    tmi_codec::png_enc::check_args(&image.data, image.width as u32, image.height as u32, text).map_err(decline)?;
    tmi_codec::png_enc::encode(&image.data, image.width as u32, image.height as u32, text)
        .map_err(|e| decline(format!("png encode failed: {e}")))
}

/// `make_thumbnail(image)`: `thumbnail((384, 384), LANCZOS)` centred on a
/// transparent 384 x 384 canvas.
pub fn thumbnail(image: &Rgba, size: usize) -> Res<Rgba> {
    use tmi_pillow::image::{Image, Mode};
    use tmi_pillow::resample::Filter;
    let img = Image::new(Mode::RGBA, image.width, image.height, image.data.clone());
    let t = tmi_pillow::resize::thumbnail(&img, (size as f64, size as f64), Filter::Lanczos, Some(2.0))
        .map_err(|e| decline(format!("thumbnail: {e:?}")))?;
    let mut canvas = vec![0u8; size * size * 4];
    let x = (size as i64 - t.width as i64).div_euclid(2);
    let y = (size as i64 - t.height as i64).div_euclid(2);
    tmi_pillow::rgba::over(&mut canvas, size, size, &t.data, t.width, t.height, x, y);
    Ok(Rgba { width: size, height: size, data: canvas })
}

pub fn thumbnail_png(image: &Rgba) -> Res<Vec<u8>> {
    png(&thumbnail(image, 384)?, None)
}

/// importers.py `PIXEL_SOURCE_VERSION`.
pub const PIXEL_SOURCE_VERSION: u32 = 1;

/// `_pixel_source_key(kind, params, *payloads)`: each payload is a list of
/// pieces standing for their concatenation.
pub fn pixel_source_key(kind: &str, params: &str, payloads: &[&[&[u8]]]) -> String {
    let mut h = Sha256::default();
    h.update(format!("tmi-pixel-source v{PIXEL_SOURCE_VERSION} {kind} {params}\0").as_bytes());
    for pieces in payloads {
        let total: u64 = pieces.iter().map(|p| p.len() as u64).sum();
        h.update(&total.to_be_bytes());
        for p in *pieces {
            h.update(p);
        }
    }
    h.hexdigest()
}

/// `_semantic_digest(value)`: sha256 of the canonical JSON.
pub fn semantic_digest(value: &Value) -> String {
    tmi_store::oid::sha256_hex(&json::canonical_bytes(value))
}

/// `round(x, 6)`.
pub fn round6(x: f64) -> f64 {
    json::py_round(x, 6)
}

/// What a composite render gives `composite_cached`.
pub struct Rendered {
    pub image: Rgba,
    /// A PNG of exactly those pixels to store as is.
    pub png: Option<Vec<u8>>,
    /// The tEXt chunk of the PNG to encode.
    pub text: Option<(String, String)>,
    /// Python's render function encodes this PNG itself (a layered CLIP
    /// render): no "encoding composite PNG" phase.
    pub encoded_in_render: bool,
}

/// `_render_composite_cached`: the composite PNG and its thumbnail, from
/// the render cache when it holds `digest`, else from `render`, filling the
/// cache, with Python's phase messages.
pub fn composite_cached(
    cache: Option<&RenderCache>,
    digest: &str,
    progress: Option<Progress>,
    render: impl FnOnce() -> Res<Rendered>,
) -> Res<(Vec<u8>, Vec<u8>)> {
    if let Some(c) = cache {
        if let Some(hit) = c.get(digest) {
            return Ok(hit);
        }
    }
    report(progress, || "rendering composite".into());
    let Rendered { image, png: png_bytes, text, encoded_in_render } = render()?;
    let (composite, thumb) = match png_bytes {
        Some(p) => {
            report(progress, || "rendering thumbnail".into());
            (p, thumbnail_png(&image)?)
        }
        None => {
            if !encoded_in_render {
                report(progress, || "encoding composite PNG".into());
            }
            composite_pngs_reporting(&image, text, progress)?
        }
    };
    if let Some(c) = cache {
        c.put(digest, &composite, &thumb);
    }
    Ok((composite, thumb))
}
