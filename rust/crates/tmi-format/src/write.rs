//! Writing layers back through a format's writer (PLUGINS.md 2.2 "書き戻し",
//! 5 "書き戻しのあとの照合", stage P7).
//!
//! - [`WriteCx`]: the host's side of the WIT `edit` interface. It holds the
//!   new pixels (`pixels` resources: the edits' first, then what
//!   `composite-after-edit` and `thumbnail` make) and the bytes it encoded
//!   for the writer (`blob` resources, of which the writer only learns the
//!   length and the CRC). A wasm plugin reaches it through
//!   `tmi-plugin-host`; a native writer calls it directly.
//! - [`stitch`]: a `write-plan`'s pieces checked (every span inside the
//!   file, every blob the host's, at most once) and joined into the new
//!   file.
//! - [`write_checked`]: the whole write. The file is read before (its
//!   records), the writer runs, the pieces are joined, and the result is
//!   read again with the same reader and compared with what was there: the
//!   edited layers must hold exactly the new pixels, every other layer the
//!   pixels and properties it had, the canvas must be the same. Anything
//!   else fails the write, and the caller does not replace the working file.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tmi_import::crop::LayerCrop;
use tmi_import::{ImportedFile, ImportedLayer, LayerBlobs, Limits, Options, PriorDecode, Rgba, Warnings};
use tmi_store::json::Value;

use crate::pipeline::{self, ImportError, Importer};
use crate::reader::{FormatWriter, ReadCx, Source};
use crate::recipe::{decode_layer_crop, DecodeEnv};
use crate::types::*;

/// `edit.pixels`: pixels the host holds (RGBA8, not premultiplied).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pixels {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// What `composite-after-edit` draws: the edited document.
pub type CompositeFn = Box<dyn Fn() -> Result<Pixels, String> + Send>;

/// At most this many `pixels` and `blob` handles in one write.
pub const MAX_HANDLES: usize = 1 << 16;
/// The blobs of one write hold at most this many bytes.
pub const MAX_BLOB_BYTES: u64 = 4 << 30;
/// The `bytes` pieces of one plan hold at most this many bytes (the
/// `inline` limit of reading, 3.4).
pub const MAX_PLAN_BYTES: u64 = 256 << 20;
/// A plan has at most this many pieces. A rewrite of a file cuts it at the
/// records it changes (a few pieces per entry, 100 000 layers at most), so a
/// million is far beyond any honest plan.
pub const MAX_PLAN_PIECES: usize = 1 << 20;
/// The pixels the host makes in one write (`composite-after-edit`,
/// `thumbnail`) hold at most this many bytes in all, as the blobs do: a
/// thumbnail of a big image is almost as big as it.
pub const MAX_PIXEL_BYTES: u64 = 4 << 30;
/// One `pixels.read` / `encode-tile` gives at most this many bytes, as no
/// decoded tile may be longer (`transform::MAX_EXPANDED`).
pub const MAX_READ_BYTES: u64 = 1 << 30;

/// The host's `edit` resources of one write.
pub struct WriteCx {
    pixels: Vec<Arc<Pixels>>,
    blobs: Vec<Vec<u8>>,
    blob_bytes: u64,
    pixel_bytes: u64,
    pixel_budget: u64,
    blob_budget: u64,
    composite: Option<CompositeFn>,
    composite_handle: Option<u32>,
}

impl WriteCx {
    /// `edits`: the new pixels, handles 0, 1, ...
    pub fn new(edits: Vec<Pixels>, composite: Option<CompositeFn>) -> WriteCx {
        WriteCx { pixels: edits.into_iter().map(Arc::new).collect(), blobs: Vec::new(), blob_bytes: 0, pixel_bytes: 0, pixel_budget: MAX_PIXEL_BYTES, blob_budget: MAX_BLOB_BYTES, composite, composite_handle: None }
    }

    /// The byte budgets of the pixels the host makes and of the blobs
    /// (tests use small ones).
    pub fn with_budgets(mut self, pixel_bytes: u64, blob_bytes: u64) -> WriteCx {
        self.pixel_budget = pixel_bytes;
        self.blob_budget = blob_bytes;
        self
    }

    /// No pixels, no composite (a placeholder while a write is away).
    pub fn empty() -> WriteCx {
        WriteCx::new(Vec::new(), None)
    }

    /// How many pixels handles there are.
    pub fn pixel_count(&self) -> usize {
        self.pixels.len()
    }

    pub fn pixels(&self, id: u32) -> Result<&Pixels, String> {
        self.pixels.get(id as usize).map(|p| p.as_ref()).ok_or_else(|| format!("no pixels {id}"))
    }

    fn add_pixels(&mut self, p: Pixels) -> Result<u32, String> {
        if self.pixels.len() >= MAX_HANDLES {
            return Err(format!("more than {MAX_HANDLES} pixels"));
        }
        if self.pixel_bytes.saturating_add(p.rgba.len() as u64) > self.pixel_budget {
            return Err(format!("the pixels made in one write are over {} MiB", self.pixel_budget >> 20));
        }
        self.pixel_bytes += p.rgba.len() as u64;
        self.pixels.push(Arc::new(p));
        Ok((self.pixels.len() - 1) as u32)
    }

    fn add_blob(&mut self, b: Vec<u8>) -> Result<u32, String> {
        if self.blobs.len() >= MAX_HANDLES {
            return Err(format!("more than {MAX_HANDLES} blobs"));
        }
        if self.blob_bytes.saturating_add(b.len() as u64) > self.blob_budget {
            return Err(format!("the blobs of one write are over {} MiB", self.blob_budget >> 20));
        }
        self.blob_bytes += b.len() as u64;
        self.blobs.push(b);
        Ok((self.blobs.len() - 1) as u32)
    }

    /// `blob.len` and `blob.crc32`.
    pub fn blob(&self, id: u32) -> Result<(u64, u32), String> {
        let b = self.blobs.get(id as usize).ok_or_else(|| format!("no blob {id}"))?;
        let mut c = flate2::Crc::new();
        c.update(b);
        Ok((b.len() as u64, c.sum()))
    }

    pub fn blob_bytes(&self, id: u32) -> Option<&[u8]> {
        self.blobs.get(id as usize).map(Vec::as_slice)
    }

    /// `pixels.bbox`: `(x0, y0, x1, y1)` of the pixels whose alpha is not 0
    /// (tmi's `pixel_bbox`), None when every pixel is transparent.
    pub fn bbox(&self, id: u32) -> Result<Option<(u32, u32, u32, u32)>, String> {
        let p = self.pixels(id)?;
        Ok(tmi_pillow::bbox::alpha_bbox(&p.rgba, p.width as usize).map(|b| (b.0 as u32, b.1 as u32, b.2 as u32, b.3 as u32)))
    }

    /// `pixels.read(x, y, w, h, layout)`: the region in `layout`'s groups,
    /// each group's rows one after another, `row-padding` zero bytes after
    /// each row. Depths 8 and 16 (a 16-bit sample is the 8-bit one times
    /// 257); `gray` is Pillow's L (`(r*19595 + g*38470 + b*7471 + 0x8000) >>
    /// 16`); `premultiplied` multiplies colour by alpha as Pillow's RGBA to
    /// RGBa does. A tint is a reading-side notion: refused.
    pub fn read(&self, id: u32, x: u32, y: u32, w: u32, h: u32, layout: &Layout) -> Result<Vec<u8>, String> {
        let p = self.pixels(id)?;
        if x.checked_add(w).is_none_or(|e| e > p.width) || y.checked_add(h).is_none_or(|e| e > p.height) {
            return Err(format!("the region {x},{y} {w}x{h} is outside the {}x{} pixels", p.width, p.height));
        }
        if layout.tint.is_some() {
            return Err("a layout with a tint cannot be read".into());
        }
        // the length is known before a byte is made (row_padding is a u32 per row)
        let len = read_len(layout, w, h)?;
        let mut out = Vec::with_capacity(len);
        for g in &layout.groups {
            let wide = match g.depth {
                Depth::Eight => false,
                Depth::SixteenBe | Depth::SixteenLe => true,
                other => return Err(format!("pixels are not read at depth {other:?}")),
            };
            for row in y..y + h {
                for col in x..x + w {
                    let at = ((row as usize) * (p.width as usize) + col as usize) * 4;
                    let (mut r, mut gg, mut b, a) = (p.rgba[at], p.rgba[at + 1], p.rgba[at + 2], p.rgba[at + 3]);
                    if layout.premultiplied {
                        let mul = |c: u8| -> u8 {
                            let t = c as u32 * a as u32 + 128;
                            ((t + (t >> 8)) >> 8) as u8
                        };
                        (r, gg, b) = (mul(r), mul(gg), mul(b));
                    }
                    for c in &g.channels {
                        let v = match c {
                            Channel::R => r,
                            Channel::G => gg,
                            Channel::B => b,
                            Channel::A => a,
                            Channel::Gray => ((r as u32 * 19595 + gg as u32 * 38470 + b as u32 * 7471 + 0x8000) >> 16) as u8,
                            Channel::Pad => 0,
                        };
                        if wide {
                            let v16 = v as u16 * 257;
                            out.extend_from_slice(&if g.depth == Depth::SixteenBe { v16.to_be_bytes() } else { v16.to_le_bytes() });
                        } else {
                            out.push(v);
                        }
                    }
                }
                out.extend(std::iter::repeat_n(0u8, layout.row_padding as usize));
            }
        }
        Ok(out)
    }

    /// `edit.encode-tile`: `read`, then each step in turn.
    #[allow(clippy::too_many_arguments)]
    pub fn encode_tile(&mut self, id: u32, x: u32, y: u32, w: u32, h: u32, layout: &Layout, steps: &[Encoding]) -> Result<u32, String> {
        let mut data = self.read(id, x, y, w, h, layout)?;
        for s in steps {
            data = match *s {
                Encoding::Raw => data,
                Encoding::Zlib(ZlibKernel::ZlibV131, level) => zlib(&data, level)?,
                Encoding::Deflate(ZlibKernel::ZlibV131, level) => {
                    let z = zlib(&data, level)?;
                    // a zlib stream is a 2-byte header, the raw deflate and the adler32
                    z[2..z.len() - 4].to_vec()
                }
            };
        }
        self.add_blob(data)
    }

    /// `edit.encode-image`: `png-v1` is tmi's own PNG encoder (8-bit RGBA,
    /// the encoder of every composite and layer PNG tmi writes); the others
    /// are not offered yet.
    pub fn encode_image(&mut self, id: u32, enc: &ImageEncoding) -> Result<u32, String> {
        let p = self.pixels(id)?;
        let png = match enc {
            ImageEncoding::PngV1 => {
                if p.width == 0 || p.height == 0 {
                    return Err(format!("a {}x{} image has no PNG", p.width, p.height));
                }
                tmi_codec::png_enc::encode(&p.rgba, p.width, p.height, None).map_err(|e| format!("png: {e}"))?
            }
            ImageEncoding::JpegKeepV1(_) => return Err("jpeg-keep-v1 is not offered yet".into()),
            ImageEncoding::WebpV1(_) => return Err("webp-v1 is not offered yet".into()),
        };
        self.add_blob(png)
    }

    /// `edit.composite-after-edit` (made once per write).
    pub fn composite_after_edit(&mut self) -> Result<u32, String> {
        if let Some(h) = self.composite_handle {
            return Ok(h);
        }
        let f = self.composite.as_ref().ok_or("no composite of the edited document here")?;
        let p = f()?;
        let h = self.add_pixels(p)?;
        self.composite_handle = Some(h);
        Ok(h)
    }

    /// `edit.thumbnail(px, max-side)`: fitted inside `max-side` x
    /// `max-side`, the aspect kept (Pillow's `thumbnail` with LANCZOS and
    /// `reducing_gap=2.0`, tmi's own thumbnail kernel); pixels already that
    /// small are returned as they are.
    pub fn thumbnail(&mut self, id: u32, max_side: u32) -> Result<u32, String> {
        use tmi_pillow::image::{Image, Mode};
        let p = self.pixels(id)?;
        let img = Image::new(Mode::RGBA, p.width as usize, p.height as usize, p.rgba.clone());
        let side = max_side.max(1) as f64;
        let t = tmi_pillow::resize::thumbnail(&img, (side, side), tmi_pillow::resample::Filter::Lanczos, Some(2.0))
            .map_err(|e| format!("thumbnail: {e:?}"))?;
        self.add_pixels(Pixels { width: t.width as u32, height: t.height as u32, rgba: t.data })
    }
}

fn zlib(data: &[u8], level: i8) -> Result<Vec<u8>, String> {
    if !(-1..=9).contains(&level) {
        return Err(format!("zlib level {level} is not -1..9"));
    }
    if data.len() > u32::MAX as usize {
        return Err("over 4 GiB for one deflate".into());
    }
    Ok(tmi_clip::zlib::compress(data, level as i32))
}

/// The bytes `pixels.read` of a `w` x `h` region gives in `layout`, worked
/// out in 128 bits and held to [`MAX_READ_BYTES`] before anything is made.
fn read_len(layout: &Layout, w: u32, h: u32) -> Result<usize, String> {
    let mut total: u128 = 0;
    for g in &layout.groups {
        let bits: u128 = match g.depth {
            Depth::Eight => 8,
            Depth::SixteenBe | Depth::SixteenLe => 16,
            other => return Err(format!("pixels are not read at depth {other:?}")),
        };
        let row = (w as u128 * g.channels.len() as u128 * bits).div_ceil(8);
        // (the row of a depth-8 or 16 group is whole bytes already)
        total += (row + layout.row_padding as u128) * h as u128;
        if total > MAX_READ_BYTES as u128 {
            return Err(format!("a read of {w}x{h} pixels with row-padding {} is over {} MiB", layout.row_padding, MAX_READ_BYTES >> 20));
        }
    }
    Ok(total as usize)
}

/// A plan's pieces checked and joined: every `source` span inside the file,
/// every blob the host's and used at most once, the plugin's own `bytes` at
/// most [`MAX_PLAN_BYTES`], at most [`MAX_PLAN_PIECES`] pieces, and the whole
/// no longer than the file plus the blobs plus the plugin's own bytes' limit
/// (a rewrite holds what was there, what the host made and a little of the
/// plugin's: a plan that repeats spans to grow past that is refused before
/// the output is allocated).
pub fn stitch(source: &[u8], plan: &WritePlan, cx: &WriteCx) -> Result<Vec<u8>, String> {
    let size = source.len() as u64;
    if plan.pieces.len() > MAX_PLAN_PIECES {
        return Err(format!("the plan has {} pieces (at most {MAX_PLAN_PIECES})", plan.pieces.len()));
    }
    let max_total = size.saturating_add(cx.blob_bytes).saturating_add(MAX_PLAN_BYTES);
    let mut total: u64 = 0;
    let mut own: u64 = 0;
    let mut used = HashSet::new();
    for p in &plan.pieces {
        let add = match p {
            Piece::Source(s) => {
                if s.offset.checked_add(s.len).is_none_or(|e| e > size) {
                    return Err(format!("the piece {}+{} is outside the {size}-byte file", s.offset, s.len));
                }
                s.len
            }
            Piece::Bytes(b) => {
                own += b.len() as u64;
                if own > MAX_PLAN_BYTES {
                    return Err(format!("the plugin's own bytes are over {} MiB", MAX_PLAN_BYTES >> 20));
                }
                b.len() as u64
            }
            Piece::Blob(id) => {
                let b = cx.blob_bytes(*id).ok_or_else(|| format!("blob {id} is not the host's"))?;
                if !used.insert(*id) {
                    return Err(format!("blob {id} is placed twice"));
                }
                b.len() as u64
            }
        };
        total = total.saturating_add(add);
        if total > max_total {
            return Err(format!("the plan is longer than the {size}-byte file, the host's blobs and {} MiB of its own", MAX_PLAN_BYTES >> 20));
        }
    }
    let mut out = Vec::with_capacity(total as usize);
    for p in &plan.pieces {
        match p {
            Piece::Source(s) => out.extend_from_slice(&source[s.offset as usize..(s.offset + s.len) as usize]),
            Piece::Bytes(b) => out.extend_from_slice(b),
            Piece::Blob(id) => out.extend_from_slice(cx.blob_bytes(*id).expect("checked")),
        }
    }
    Ok(out)
}

/// A layer to write: its stable id and its new pixels (the whole bitmap of
/// the layer, as a layer blob holds it).
pub struct Edit {
    pub stable_id: String,
    pub pixels: Rgba,
}

/// Why a write did not happen.
#[derive(Debug)]
pub enum WriteFailure {
    /// Reading the file before writing failed.
    Read(ImportError),
    /// A layer that is not there or cannot be written.
    Layer(String),
    /// The writer's own answer (or the host's verdict on its call).
    Writer(WriteError),
    /// The plan breaks the host's rules.
    Plan(String),
    /// What was written does not pass the check after writing.
    Check(String),
}

impl std::fmt::Display for WriteFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WriteFailure::Read(ImportError::Read(e)) => write!(f, "reading it first failed ({e})"),
            WriteFailure::Read(ImportError::Decline(d)) => write!(f, "reading it first failed ({})", d.0),
            WriteFailure::Layer(m) => write!(f, "{m}"),
            WriteFailure::Writer(e) => write!(f, "the writer failed ({e})"),
            WriteFailure::Plan(m) => write!(f, "the writer's plan is invalid ({m})"),
            WriteFailure::Check(m) => write!(f, "what it wrote does not pass the check after writing ({m})"),
        }
    }
}

fn walk<'a>(layers: &'a [ImportedLayer], parent: Option<usize>, out: &mut Vec<(Option<usize>, &'a ImportedLayer)>) {
    for l in layers {
        out.push((parent, l));
        let me = out.len() - 1;
        walk(&l.children, Some(me), out);
    }
}

fn check_opts(blobs: &LayerBlobs, limits: Limits) -> Options<'_> {
    Options {
        render_composite: false,
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

/// The pixel digest a layer holding `p` is recorded with.
pub fn pixels_digest(p: &Rgba) -> String {
    LayerCrop::from_full(&p.data, p.width, p.height).digest()
}

const PROPS: [&str; 10] = ["layer_id", "name", "kind", "visible", "opacity", "blend_mode", "clip_to_below", "offset_x", "offset_y", "mask_ref"];

fn canon(v: &Value) -> String {
    tmi_store::json::dumps(v, &tmi_store::json::DumpOptions::CANONICAL)
}

fn without_pixel_source(meta: &tmi_store::json::Map) -> String {
    canon(&Value::Object(meta.iter().filter(|(k, _)| k.as_str() != "pixel_source").map(|(k, v)| (k.clone(), v.clone())).collect()))
}

fn sha(l: &ImportedLayer) -> Option<String> {
    l.pixel_sha256.as_ref().map(canon)
}

/// The check after writing (PLUGINS.md 5): `after` holds `before`'s canvas
/// and layers, each with its pixels and properties, but for the edited
/// layers' pixels, which are `want` (stable id -> digest).
pub fn compare(before: &ImportedFile, after: &ImportedFile, want: &HashMap<String, String>) -> Result<(), String> {
    if canon(&Value::Object(before.canvas_meta.clone())) != canon(&Value::Object(after.canvas_meta.clone())) {
        return Err("the canvas changed".into());
    }
    let (mut a, mut b) = (Vec::new(), Vec::new());
    walk(&before.layers, None, &mut a);
    walk(&after.layers, None, &mut b);
    if a.len() != b.len() {
        return Err(format!("{} layers became {}", a.len(), b.len()));
    }
    for ((pa, la), (pb, lb)) in a.iter().zip(&b) {
        let name = &la.name;
        if la.stable_source_id != lb.stable_source_id || pa != pb {
            return Err(format!("the layer tree changed at layer {name} ({})", la.stable_source_id));
        }
        let props_differ = PROPS.iter().any(|k| canon(&la.field(k)) != canon(&lb.field(k))) || without_pixel_source(&la.source_meta) != without_pixel_source(&lb.source_meta);
        if props_differ {
            return Err(format!("the properties of layer {name} ({}) changed", la.stable_source_id));
        }
        match want.get(&la.stable_source_id) {
            Some(digest) => {
                if lb.pixel_sha256.as_ref().and_then(Value::as_str) != Some(digest.as_str()) {
                    return Err(format!("layer {name} ({}) does not hold the new pixels", la.stable_source_id));
                }
            }
            None => {
                if sha(la) != sha(lb) {
                    return Err(format!("layer {name} ({}) changed although it was not edited", la.stable_source_id));
                }
            }
        }
    }
    Ok(())
}

/// Writes `edits` into `source` (the file `name`) with `writer`, whose
/// records carry `importer`, and checks the result (the module's doc). The
/// new bytes, or why there are none. The writer's warnings go to `warns`.
pub fn write_checked(
    writer: &dyn FormatWriter,
    importer: &Importer,
    name: &str,
    source: &[u8],
    edits: Vec<Edit>,
    limits: Limits,
    warns: &mut Warnings,
) -> Result<Vec<u8>, WriteFailure> {
    let reader: &dyn crate::reader::FormatReader = writer;
    // what is there
    let none = LayerBlobs::Only(HashSet::new());
    let digest = tmi_store::oid::sha256_hex(source);
    let mut scratch = Warnings::default();
    let before = pipeline::import_plugin(reader, importer, name, source, &digest, &check_opts(&none, limits), &mut scratch)
        .map_err(WriteFailure::Read)?;
    let mut flat = Vec::new();
    walk(&before.layers, None, &mut flat);
    let mut want = HashMap::new();
    for e in &edits {
        let Some((_, l)) = flat.iter().find(|(_, l)| l.stable_source_id == e.stable_id) else {
            return Err(WriteFailure::Layer(format!("{name} has no layer {}", e.stable_id)));
        };
        let writable = l.source_meta.get("write_capability").and_then(Value::as_str) == Some(pipeline::write_capability_name(WriteCapability::Pixels));
        if !writable || l.pixel_sha256.is_none() {
            return Err(WriteFailure::Layer(format!("plugin '{}' cannot write the pixels of layer {} in {name}", importer.id, l.name)));
        }
        if e.pixels.width > u32::MAX as usize || e.pixels.height > u32::MAX as usize {
            return Err(WriteFailure::Layer(format!("the new pixels of layer {} are too large", l.name)));
        }
        want.insert(e.stable_id.clone(), pixels_digest(&e.pixels));
    }

    // composite-after-edit: the structure (every deferred layer's recipe
    // fetched now: the composite is drawn inside the writer's call, where
    // the plugin cannot be called again) and the new pixels
    let cx_read = ReadCx::new(None);
    let src = Source::new(name, source);
    let mut structure = reader.read_structure(&src, &cx_read).map_err(|e| WriteFailure::Read(ImportError::Read(e)))?;
    crate::validate::check(&structure, source.len() as u64).map_err(|e| WriteFailure::Read(e.into()))?;
    for i in 0..structure.layers.len() {
        if let LayerPixels::Deferred(token) = structure.layers[i].pixels {
            let b = reader.read_layer(&src, &structure.context, token, &cx_read).map_err(|e| WriteFailure::Read(ImportError::Read(e)))?;
            structure.layers[i].pixels = LayerPixels::Ready(b);
        }
    }
    let replaced: HashMap<String, usize> = edits.iter().enumerate().map(|(i, e)| (e.stable_id.clone(), i)).collect();
    let new_pixels: Arc<Vec<Rgba>> = Arc::new(edits.iter().map(|e| e.pixels.clone()).collect());
    let owned = Arc::new(source.to_vec());
    let structure = Arc::new(structure);
    let composite: CompositeFn = {
        let (structure, owned, new_pixels) = (structure.clone(), owned.clone(), new_pixels.clone());
        let max = limits.layered;
        Box::new(move || composite_after_edit(&structure, &owned, &replaced, &new_pixels, max))
    };
    let pixels: Vec<Pixels> = edits.iter().map(|e| Pixels { width: e.pixels.width as u32, height: e.pixels.height as u32, rgba: e.pixels.data.clone() }).collect();
    let mut cx = WriteCx::new(pixels, Some(composite));
    let layer_edits: Vec<LayerEdit> =
        edits.iter().enumerate().map(|(i, e)| LayerEdit { stable_id: e.stable_id.clone(), new_pixels: Some(i as u32), props: Vec::new() }).collect();
    let plan = writer.write_layers(&src, &layer_edits, &mut cx).map_err(WriteFailure::Writer)?;
    let written = stitch(source, &plan, &cx).map_err(WriteFailure::Plan)?;
    drop(cx);

    // read it again (a layer whose recipe and bytes are unchanged is not
    // decoded twice: the before-read's decode of the same pixel source)
    let mut prior = HashMap::new();
    for (_, l) in &flat {
        if let (Some(sha), Some(key)) = (&l.pixel_sha256, l.source_meta.get("pixel_source")) {
            prior.insert(l.stable_source_id.clone(), PriorDecode { pixel_source: Some(key.clone()), pixel_sha256: sha.clone(), pixel_bbox: l.pixel_bbox.clone() });
        }
    }
    let opts = Options { reuse_decoded: Some(&prior), ..check_opts(&none, limits) };
    let after = pipeline::import_plugin(reader, importer, name, &written, &tmi_store::oid::sha256_hex(&written), &opts, &mut scratch)
        .map_err(|e| WriteFailure::Check(match e {
            ImportError::Read(e) => format!("it does not read back: {e}"),
            ImportError::Decline(d) => format!("it does not read back: {}", d.0),
        }))?;
    compare(&before, &after, &want).map_err(WriteFailure::Check)?;
    for w in &plan.warnings {
        warns.lines.push(format!("tmiki: warning: {name}: {} ({})", w.message, w.code));
    }
    Ok(written)
}

/// The edited document drawn by generic-v1 over a transparent background
/// (what `composite-after-edit` gives: a rebuilt embedded composite, such
/// as OpenRaster's `mergedimage.png`, keeps its transparency). `replaced`:
/// stable id -> index into `new_pixels`. Every recipe is already `Ready`; a
/// plugin codec cannot be decoded here (the plugin is busy writing).
fn composite_after_edit(s: &Structure, source: &[u8], replaced: &HashMap<String, usize>, new_pixels: &[Rgba], max_pixels: u64) -> Result<Pixels, String> {
    let cx = ReadCx::new(Some(max_pixels).filter(|&m| m != 0));
    let env = DecodeEnv { source, reader: None, cx: &cx };
    let reads = crate::compose::reads_pixels(s);
    let mut crops: Vec<Option<LayerCrop>> = Vec::with_capacity(s.layers.len());
    for (i, l) in s.layers.iter().enumerate() {
        if !reads[i] {
            crops.push(None);
            continue;
        }
        crops.push(Some(match (replaced.get(&l.stable_id), &l.pixels) {
            (Some(&k), _) => LayerCrop::from_full(&new_pixels[k].data, new_pixels[k].width, new_pixels[k].height),
            (None, LayerPixels::Ready(b)) => decode_layer_crop(&env, b).map_err(|e| format!("layer {}: {e}", l.name))?,
            (None, _) => return Err(format!("layer {} has no recipe", l.name)),
        }));
    }
    let refs: Vec<Option<&LayerCrop>> = crops.iter().map(Option::as_ref).collect();
    let rgba = crate::compose::render(s, &refs, [0, 0, 0, 0])?;
    Ok(Pixels { width: s.canvas.width, height: s.canvas.height, rgba })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn px() -> WriteCx {
        // 2x2: red opaque, green half, blue clear, white opaque
        let rgba = vec![255, 0, 0, 255, 0, 255, 0, 128, 0, 0, 255, 0, 255, 255, 255, 255];
        WriteCx::new(vec![Pixels { width: 2, height: 2, rgba }], None)
    }

    fn layout(groups: Vec<(Vec<Channel>, Depth)>, row_padding: u32, premultiplied: bool) -> Layout {
        Layout { groups: groups.into_iter().map(|(channels, depth)| Group { channels, depth }).collect(), row_padding, tint: None, premultiplied }
    }

    #[test]
    fn pixels_are_read_in_the_layout_asked_for() {
        let cx = px();
        assert_eq!(cx.bbox(0).unwrap(), Some((0, 0, 2, 2)));
        // CLIP's planes: [a] then [b, g, r, pad], one row padded by one byte
        let l = layout(vec![(vec![Channel::A], Depth::Eight), (vec![Channel::B, Channel::G, Channel::R, Channel::Pad], Depth::Eight)], 1, false);
        assert_eq!(cx.read(0, 0, 0, 2, 1, &l).unwrap(), vec![255, 128, 0, 0, 0, 255, 0, 0, 255, 0, 0, 0]);
        // premultiplied, 16-bit big-endian gray of the bottom-right pixel
        let l = layout(vec![(vec![Channel::R, Channel::A], Depth::Eight)], 0, true);
        assert_eq!(cx.read(0, 1, 0, 1, 1, &l).unwrap(), vec![0, 128]);
        let l = layout(vec![(vec![Channel::Gray], Depth::SixteenBe)], 0, false);
        assert_eq!(cx.read(0, 1, 1, 1, 1, &l).unwrap(), vec![0xff, 0xff]);
        assert!(cx.read(0, 1, 1, 2, 1, &l).is_err(), "outside the pixels");
        assert!(cx.read(0, 0, 0, 1, 1, &layout(vec![(vec![Channel::A], Depth::OneBitMsb)], 0, false)).is_err());
    }

    #[test]
    fn tiles_and_images_are_encoded_and_known_by_length_and_crc() {
        let mut cx = px();
        let l = layout(vec![(vec![Channel::R, Channel::G, Channel::B, Channel::A], Depth::Eight)], 0, false);
        let raw = cx.encode_tile(0, 0, 0, 2, 2, &l, &[Encoding::Raw]).unwrap();
        let z = cx.encode_tile(0, 0, 0, 2, 2, &l, &[Encoding::Zlib(ZlibKernel::ZlibV131, 6)]).unwrap();
        let d = cx.encode_tile(0, 0, 0, 2, 2, &l, &[Encoding::Deflate(ZlibKernel::ZlibV131, 6)]).unwrap();
        let raw_bytes = cx.blob_bytes(raw).unwrap().to_vec();
        let inflate = |data: &[u8], zlib: bool| {
            use std::io::Read;
            let mut out = Vec::new();
            if zlib {
                flate2::read::ZlibDecoder::new(data).read_to_end(&mut out).unwrap();
            } else {
                flate2::read::DeflateDecoder::new(data).read_to_end(&mut out).unwrap();
            }
            out
        };
        assert_eq!(inflate(cx.blob_bytes(z).unwrap(), true), raw_bytes);
        assert_eq!(inflate(cx.blob_bytes(d).unwrap(), false), raw_bytes);
        let png = cx.encode_image(0, &ImageEncoding::PngV1).unwrap();
        let (len, crc) = cx.blob(png).unwrap();
        let mut c = flate2::Crc::new();
        c.update(cx.blob_bytes(png).unwrap());
        assert_eq!((len, crc), (cx.blob_bytes(png).unwrap().len() as u64, c.sum()));
        assert!(cx.encode_image(0, &ImageEncoding::WebpV1(80)).is_err());
        assert!(cx.composite_after_edit().is_err(), "no composite was given");
        let t = cx.thumbnail(0, 1).unwrap();
        assert_eq!((cx.pixels(t).unwrap().width, cx.pixels(t).unwrap().height), (1, 1));
    }

    #[test]
    fn a_plan_is_checked_before_its_pieces_are_joined() {
        let mut cx = px();
        let png = cx.encode_image(0, &ImageEncoding::PngV1).unwrap();
        let plan = |pieces: Vec<Piece>| WritePlan { pieces, warnings: vec![] };
        let src = b"0123456789";
        let ok = stitch(src, &plan(vec![Piece::Source(Span { offset: 2, len: 3 }), Piece::Bytes(b"-".to_vec()), Piece::Blob(png)]), &cx).unwrap();
        assert_eq!(&ok[..4], b"234-");
        assert_eq!(&ok[4..], cx.blob_bytes(png).unwrap());
        assert!(stitch(src, &plan(vec![Piece::Source(Span { offset: 8, len: 3 })]), &cx).unwrap_err().contains("outside the 10-byte file"));
        assert!(stitch(src, &plan(vec![Piece::Blob(7)]), &cx).unwrap_err().contains("not the host's"));
        assert!(stitch(src, &plan(vec![Piece::Blob(png), Piece::Blob(png)]), &cx).unwrap_err().contains("placed twice"));
    }
}