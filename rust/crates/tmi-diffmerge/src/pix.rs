//! tmi/raster.py and the image_ops.py helpers diff and merge use: `Rgba`
//! (packed RGBA8) and `Sparse` (a layer blob's painted rectangle in its
//! full-size layer), stored pixels read without Pillow
//! (`pixels_from_bytes`), and Pillow's operations through tmi-pillow,
//! tmi-image and tmi-codec, the kernels tmi_accel gives Python. Each gives
//! the bytes the Python side gives; what Python would hand to Pillow (an
//! image tmi_image does not decode, a header raster.py is unsure of) is
//! `Forward`.

use std::borrow::Cow;
use tmi_import::{Limits, Rgba};
use tmi_pillow::image::{Image, Mode};
use tmi_pillow::resample::{Filter, Window};
use tmi_repo::{RepoError, Result};

pub fn fwd(why: impl Into<String>) -> RepoError {
    RepoError::forward(why)
}

pub fn blank(width: usize, height: usize) -> Rgba {
    Rgba { width, height, data: vec![0u8; width * height * 4] }
}

pub fn filled(width: usize, height: usize, fill: [u8; 4]) -> Rgba {
    Rgba { width, height, data: fill.repeat(width * height) }
}

/// A layer that is transparent black outside `crop` at (`x0`, `y0`).
#[derive(Clone)]
pub struct Sparse {
    pub width: usize,
    pub height: usize,
    pub x0: i64,
    pub y0: i64,
    pub crop: Option<Rgba>,
}

/// `tmi.raster.Rgba` or `tmi.raster.Sparse`.
#[derive(Clone)]
pub enum Px {
    Dense(Rgba),
    Sparse(Sparse),
}

pub type Rect = (i64, i64, i64, i64);

impl Sparse {
    pub fn new(size: (usize, usize), x0: i64, y0: i64, crop: Option<Rgba>) -> Sparse {
        let crop = crop.filter(|c| c.width > 0 && c.height > 0);
        Sparse { width: size.0, height: size.1, x0, y0, crop }
    }

    pub fn empty(size: (usize, usize)) -> Sparse {
        Sparse { width: size.0, height: size.1, x0: 0, y0: 0, crop: None }
    }
}

impl Px {
    pub fn size(&self) -> (usize, usize) {
        match self {
            Px::Dense(r) => (r.width, r.height),
            Px::Sparse(s) => (s.width, s.height),
        }
    }

    pub fn width(&self) -> usize {
        self.size().0
    }

    pub fn height(&self) -> usize {
        self.size().1
    }

    /// `.rect`: the painted rectangle.
    pub fn rect(&self) -> Option<Rect> {
        match self {
            Px::Dense(r) => (r.width > 0 && r.height > 0).then_some((0, 0, r.width as i64, r.height as i64)),
            Px::Sparse(s) => s.crop.as_ref().map(|c| (s.x0, s.y0, s.x0 + c.width as i64, s.y0 + c.height as i64)),
        }
    }

    /// `.region(box)` / `.crop(box)`: the pixels of `box`, transparent
    /// black outside.
    pub fn region(&self, bx: Rect) -> Cow<'_, Rgba> {
        match self {
            Px::Dense(r) => crop(r, bx),
            Px::Sparse(s) => {
                let (x0, y0, x1, y1) = bx;
                let (w, h) = ((x1 - x0) as usize, (y1 - y0) as usize);
                let Some(c) = &s.crop else { return Cow::Owned(blank(w, h)) };
                let (bx0, by0) = (x0.max(0), y0.max(0));
                let (bx1, by1) = (x1.min(s.width as i64), y1.min(s.height as i64));
                if Some(bx) == self.rect() {
                    return Cow::Borrowed(c);
                }
                if bx0 >= bx1 || by0 >= by1 {
                    return Cow::Owned(blank(w, h));
                }
                if (bx0, by0, bx1, by1) == bx {
                    return Cow::Owned(crop(c, (x0 - s.x0, y0 - s.y0, x1 - s.x0, y1 - s.y0)).into_owned());
                }
                let inner = crop(c, (bx0 - s.x0, by0 - s.y0, bx1 - s.x0, by1 - s.y0));
                Cow::Owned(compose(w, h, &[Op::Paste(&inner, bx0 - x0, by0 - y0)], [0, 0, 0, 0], None))
            }
        }
    }

    /// The pixels `region(bx)` would hold that can be non-zero, as a window
    /// in `bx`'s coordinates (None: `region(bx)` is all 0), without building
    /// the region.
    pub fn window(&self, bx: Rect) -> Option<Window<'_>> {
        let (bw, bh) = ((bx.2 - bx.0).max(0) as usize, (bx.3 - bx.1).max(0) as usize);
        match self {
            Px::Dense(r) => Window::placed(&r.data, r.width, r.height, 4, -bx.0, -bx.1, bw, bh),
            Px::Sparse(s) => {
                let c = s.crop.as_ref()?;
                if Some(bx) == self.rect() {
                    // region() hands the crop back as it is
                    return Window::placed(&c.data, c.width, c.height, 4, 0, 0, bw, bh);
                }
                // the crop within the layer (region() leaves out the rest)
                let (lx0, ly0) = (s.x0.max(0).max(bx.0), s.y0.max(0).max(bx.1));
                let lx1 = (s.x0 + c.width as i64).min(s.width as i64).min(bx.2);
                let ly1 = (s.y0 + c.height as i64).min(s.height as i64).min(bx.3);
                if lx0 >= lx1 || ly0 >= ly1 {
                    return None;
                }
                let stride = c.width * 4;
                let at = (ly0 - s.y0) as usize * stride + (lx0 - s.x0) as usize * 4;
                let rect = ((lx0 - bx.0) as usize, (ly0 - bx.1) as usize, (lx1 - bx.0) as usize, (ly1 - bx.1) as usize);
                Some(Window { data: &c.data[at..], stride, rect })
            }
        }
    }

    /// The painted rectangle within the layer's own size: where `dense()`
    /// can be non-zero (None: nowhere).
    pub fn inner_rect(&self) -> Option<Rect> {
        let r = self.rect()?;
        let (x0, y0) = (r.0.max(0), r.1.max(0));
        let (x1, y1) = (r.2.min(self.width() as i64), r.3.min(self.height() as i64));
        (x0 < x1 && y0 < y1).then_some((x0, y0, x1, y1))
    }

    pub fn dense(&self) -> Cow<'_, Rgba> {
        match self {
            Px::Dense(r) => Cow::Borrowed(r),
            Px::Sparse(s) => self.region((0, 0, s.width as i64, s.height as i64)),
        }
    }

    pub fn into_dense(self) -> Rgba {
        match self {
            Px::Dense(r) => r,
            Px::Sparse(_) => self.dense().into_owned(),
        }
    }
}

/// `Rgba.crop(box)`: the image itself for its own box.
pub fn crop(img: &Rgba, bx: Rect) -> Cow<'_, Rgba> {
    let (x0, y0, x1, y1) = bx;
    if (x0, y0, x1, y1) == (0, 0, img.width as i64, img.height as i64) {
        return Cow::Borrowed(img);
    }
    let data = tmi_pillow::rgba::crop(&img.data, img.width, img.height, x0, y0, x1, y1);
    Cow::Owned(Rgba { width: (x1 - x0) as usize, height: (y1 - y0) as usize, data })
}

pub enum Op<'a> {
    Over(&'a Rgba, i64, i64),
    Paste(&'a Rgba, i64, i64),
}

/// `raster.compose(width, height, ops, fill, base)`.
pub fn compose(width: usize, height: usize, ops: &[Op], fill: [u8; 4], base: Option<&Rgba>) -> Rgba {
    let mut canvas = match base {
        Some(b) => b.data.clone(),
        None => fill.repeat(width * height),
    };
    for op in ops {
        match op {
            Op::Over(src, x, y) => tmi_pillow::rgba::over(&mut canvas, width, height, &src.data, src.width, src.height, *x, *y),
            Op::Paste(src, x, y) => tmi_pillow::rgba::paste(&mut canvas, width, height, &src.data, src.width, src.height, *x, *y),
        }
    }
    Rgba { width, height, data: canvas }
}

/// `raster.pad(image, size)`.
fn pad<'a>(img: Cow<'a, Rgba>, size: (usize, usize)) -> Cow<'a, Rgba> {
    if (img.width, img.height) == size {
        return img;
    }
    Cow::Owned(compose(size.0, size.1, &[Op::Over(&img, 0, 0)], [0, 0, 0, 0], None))
}

/// `raster.same_size(left, right)` of two dense images.
fn same_size_rgba<'a>(left: Cow<'a, Rgba>, right: Cow<'a, Rgba>) -> (Cow<'a, Rgba>, Cow<'a, Rgba>) {
    let size = (left.width.max(right.width), left.height.max(right.height));
    (pad(left, size), pad(right, size))
}

/// `raster.diff_bbox(left, right)`.
pub fn diff_bbox(left: &Px, right: &Px) -> Option<Rect> {
    let width = left.width().max(right.width()) as i64;
    let height = left.height().max(right.height()) as i64;
    let rects: Vec<Rect> = [left.rect(), right.rect()].into_iter().flatten().collect();
    if rects.is_empty() {
        return None;
    }
    let bx = (
        rects.iter().map(|r| r.0).min().unwrap().max(0),
        rects.iter().map(|r| r.1).min().unwrap().max(0),
        rects.iter().map(|r| r.2).max().unwrap().min(width),
        rects.iter().map(|r| r.3).max().unwrap().min(height),
    );
    if bx.0 >= bx.2 || bx.1 >= bx.3 {
        return None;
    }
    // A pixel can differ only inside one of the two painted rectangles (both
    // sides are transparent black elsewhere), and whether it differs depends
    // on its two values alone: when the rectangles are far apart, look at
    // each instead of the box around both (the same box comes out).
    if let [a, b] = rects[..] {
        let inside = |p: &Px, r: Rect| r.0 >= 0 && r.1 >= 0 && r.2 <= p.width() as i64 && r.3 <= p.height() as i64;
        let area = |r: Rect| ((r.2 - r.0).max(0) as u128) * ((r.3 - r.1).max(0) as u128);
        if inside(left, a) && inside(right, b) && area(a) + area(b) < area(bx) {
            let found = [a, b].into_iter().filter_map(|r| difference_in(left, right, r));
            return found.reduce(|p, q| (p.0.min(q.0), p.1.min(q.1), p.2.max(q.2), p.3.max(q.3)));
        }
    }
    difference_in(left, right, bx)
}

/// The visible difference box of `left` and `right` within `bx`.
fn difference_in(left: &Px, right: &Px, bx: Rect) -> Option<Rect> {
    // Against transparent black a pixel differs visibly exactly when its
    // alpha is not 0 (equal alphas of 0 make every colour difference
    // invisible): the box is the other side's alpha box.
    let one_side = match (left.window(bx).is_none(), right.window(bx).is_none()) {
        (true, true) => return None,
        (true, false) => Some(right),
        (false, true) => Some(left),
        (false, false) => None,
    };
    if let Some(side) = one_side {
        let p = side.region(bx);
        let found = tmi_pillow::bbox::alpha_bbox(&p.data, p.width)?;
        return Some((found.0 as i64 + bx.0, found.1 as i64 + bx.1, found.2 as i64 + bx.0, found.3 as i64 + bx.1));
    }
    let (pa, pb) = (left.region(bx), right.region(bx));
    let found = tmi_pillow::rgba::visible_difference_bbox(&pa.data, &pb.data, pa.width)?;
    Some((found.0 as i64 + bx.0, found.1 as i64 + bx.1, found.2 as i64 + bx.0, found.3 as i64 + bx.1))
}

/// image_ops `side_by_side(left, right)`.
pub fn side_by_side(left: &Px, right: &Px) -> Rgba {
    side_by_side_rgba(&left.dense(), &right.dense())
}

pub fn side_by_side_rgba(left: &Rgba, right: &Rgba) -> Rgba {
    let (l, r) = same_size_rgba(Cow::Borrowed(left), Cow::Borrowed(right));
    compose(
        l.width * 2 + 16,
        l.height,
        &[Op::Over(&l, 0, 0), Op::Over(&r, l.width as i64 + 16, 0)],
        [32, 32, 32, 255],
        None,
    )
}

/// image_ops `overlay(left, right)`: `Image.blend(left, right, 0.5)`.
pub fn overlay(left: &Px, right: &Px) -> Rgba {
    overlay_rgba(&left.dense(), &right.dense())
}

pub fn overlay_rgba(left: &Rgba, right: &Rgba) -> Rgba {
    let (l, r) = same_size_rgba(Cow::Borrowed(left), Cow::Borrowed(right));
    let data = tmi_pillow::blend::blend(&l.data, &r.data, 0.5);
    Rgba { width: l.width, height: l.height, data }
}

/// image_ops `diff_preview(left, right, bbox=...)`: `bbox` None computes it.
pub fn diff_preview(left: &Px, right: &Px, bbox: Option<Option<Rect>>) -> Rgba {
    diff_preview_rgba(&left.dense(), &right.dense(), bbox)
}

pub fn diff_preview_rgba(left: &Rgba, right: &Rgba, bbox: Option<Option<Rect>>) -> Rgba {
    let (l, r) = same_size_rgba(Cow::Borrowed(left), Cow::Borrowed(right));
    let bbox = match bbox {
        Some(b) => b,
        None => diff_bbox_rgba(&l, &r),
    };
    let r = r.into_owned();
    let Some(b) = bbox else { return r };
    let width = ((r.width / 300) as i32).max(2);
    outline(r, b, [255, 0, 0, 255], width)
}

/// `raster.diff_bbox(left, right)` of two dense images.
pub fn diff_bbox_rgba(left: &Rgba, right: &Rgba) -> Option<Rect> {
    if (left.width, left.height) != (right.width, right.height) {
        return diff_bbox(&Px::Dense(left.clone()), &Px::Dense(right.clone()));
    }
    if left.width == 0 || left.height == 0 {
        return None;
    }
    let found = tmi_pillow::rgba::visible_difference_bbox(&left.data, &right.data, left.width)?;
    Some((found.0 as i64, found.1 as i64, found.2 as i64, found.3 as i64))
}

/// `raster.outline(image, box, ink, width)`.
pub fn outline(mut img: Rgba, bx: Rect, ink: [u8; 4], width: i32) -> Rgba {
    if width == 0 {
        return img;
    }
    let (w, h) = (img.width, img.height);
    tmi_pillow::draw::rectangle_outline_rgba(&mut img.data, w, h, bx.0 as i32, bx.1 as i32, bx.2 as i32, bx.3 as i32, ink, width);
    img
}

/// `image.resize(size, LANCZOS, box=box)` (the image itself when nothing
/// changes).
pub fn resize(img: &Rgba, size: (usize, usize), bx: Option<[f64; 4]>) -> Result<Rgba> {
    if bx.is_none() && size == (img.width, img.height) {
        return Ok(img.clone());
    }
    let image = Image::new(Mode::RGBA, img.width, img.height, img.data.clone());
    let out = tmi_pillow::resize::resize(&image, size, Filter::Lanczos, bx, None).map_err(|e| fwd(format!("resize: {e:?}")))?;
    Ok(Rgba { width: out.width, height: out.height, data: out.data })
}

/// `resize(&px.region(bx), size, None)` without building the region: only
/// the painted pixels inside `bx` are resampled (`resize_sparse`; the bytes
/// are the same).
pub fn resize_region(px: &Px, bx: Rect, size: (usize, usize)) -> Result<Rgba> {
    let (w, h) = ((bx.2 - bx.0) as usize, (bx.3 - bx.1) as usize);
    let out = tmi_pillow::resize::resize_sparse(Mode::RGBA, w, h, px.window(bx), size, Filter::Lanczos, None)
        .map_err(|e| fwd(format!("resize: {e:?}")))?;
    Ok(Rgba { width: out.width, height: out.height, data: out.data })
}

/// The ops that draw `px`'s painted pixels at (`x`, `y`), or None when the
/// two sides cannot be drawn so (different sizes: `same_size` pads them).
fn painted<'a>(px: &'a Px) -> Option<(Cow<'a, Rgba>, i64, i64)> {
    let r = px.inner_rect()?;
    Some((px.region(r), r.0, r.1))
}

/// `side_by_side(left, right)` drawing only the painted pixels: an
/// `alpha_composite` of a transparent pixel leaves the target as it is.
pub fn side_by_side_sparse(left: &Px, right: &Px) -> Rgba {
    if left.size() != right.size() {
        return side_by_side(left, right);
    }
    let (w, h) = left.size();
    let (l, r) = (painted(left), painted(right));
    let mut ops = Vec::new();
    if let Some((img, x, y)) = &l {
        ops.push(Op::Over(img, *x, *y));
    }
    if let Some((img, x, y)) = &r {
        ops.push(Op::Over(img, *x + w as i64 + 16, *y));
    }
    compose(w * 2 + 16, h, &ops, [32, 32, 32, 255], None)
}

/// `overlay(left, right)` blending only where a side is painted:
/// `Image.blend` of two 0 bytes is 0.
pub fn overlay_sparse(left: &Px, right: &Px) -> Rgba {
    if left.size() != right.size() {
        return overlay(left, right);
    }
    let (w, h) = left.size();
    let mut out = blank(w, h);
    let mut rects: Vec<Rect> = [left.inner_rect(), right.inner_rect()].into_iter().flatten().collect();
    if let [a, b] = rects[..] {
        let u = (a.0.min(b.0), a.1.min(b.1), a.2.max(b.2), a.3.max(b.3));
        let area = |r: Rect| ((r.2 - r.0) as u128) * ((r.3 - r.1) as u128);
        if area(a) + area(b) >= area(u) {
            rects = vec![u];
        }
    }
    for r in rects {
        let (a, b) = (left.region(r), right.region(r));
        let mixed = tmi_pillow::blend::blend(&a.data, &b.data, 0.5);
        tmi_pillow::rgba::paste(&mut out.data, w, h, &mixed, a.width, a.height, r.0, r.1);
    }
    out
}

/// `diff_preview(left, right, None)` given `bbox = diff_bbox(left, right)`:
/// for sides of one size the box `diff_preview` finds on the full images is
/// that one (outside the painted rectangles both are transparent black).
pub fn diff_preview_sparse(left: &Px, right: &Px, bbox: Option<Rect>) -> Rgba {
    if left.size() != right.size() {
        return diff_preview(left, right, None);
    }
    // diff_preview_rgba's steps for two images of one size
    let r = right.dense().into_owned();
    let Some(b) = bbox else { return r };
    let width = ((r.width / 300) as i32).max(2);
    outline(r, b, [255, 0, 0, 255], width)
}

/// `raster.webp(image, quality, method)`.
pub fn webp(img: &Rgba, quality: f32, method: i32) -> Result<Vec<u8>> {
    let opts = tmi_image::webp_enc::WebpOptions { lossless: false, quality, alpha_quality: 100.0, method, exact: false };
    tmi_image::webp_enc::encode_webp(&img.data, img.width, img.height, 4, &opts).map_err(|e| fwd(format!("webp: {e}")))
}

/// `image_to_png_bytes(image)` / `raster.png(image)`: mtpng (a 0-pixel
/// image goes to Pillow in Python).
pub fn png(img: &Rgba) -> Result<Vec<u8>> {
    if img.width == 0 || img.height == 0 {
        return Err(fwd("an empty image: Pillow encodes it"));
    }
    tmi_codec::png_enc::encode(&img.data, img.width as u32, img.height as u32, None).map_err(|e| fwd(format!("png: {e}")))
}

// ───────────────────────────── decoding ─────────────────────────────

pub const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";
const LAYER_TEXT_KEY: &str = "tmi-layer";

fn le32(d: &[u8], at: usize) -> u64 {
    u32::from_le_bytes(d[at..at + 4].try_into().unwrap()) as u64
}

/// `pixels_from_bytes(data)`: a layer blob (TMIL, or a legacy PNG with the
/// "tmi-layer" header) as a `Sparse`, any other image as an `Rgba`.
pub fn pixels_from_bytes(data: &[u8], limits: &Limits) -> Result<Px> {
    if data.starts_with(b"TMIL") {
        let header = tmi_codec::layer_blob::parse(data).map_err(|e| fwd(format!("TMIL layer blob: {e}")))?;
        let size = (le32(data, 8) as usize, le32(data, 12) as usize);
        let (x0, y0) = (le32(data, 16) as i64, le32(data, 20) as i64);
        if header.w == 0 {
            limits.check_raster(size.0, size.1).map_err(fwd)?;
            return Ok(Px::Sparse(Sparse::empty(size)));
        }
        limits.check_raster(header.w, header.h).map_err(fwd)?;
        let mut rgba = vec![0u8; 4 * header.w * header.h];
        tmi_codec::layer_blob::decode_into(data, &header, &mut rgba).map_err(|e| fwd(format!("TMIL layer blob: {e}")))?;
        limits.check_raster(size.0, size.1).map_err(fwd)?;
        let crop = Rgba { width: header.w, height: header.h, data: rgba };
        return Ok(Px::Sparse(Sparse::new(size, x0, y0, Some(crop))));
    }
    let legacy = png_text_before_idat(data, LAYER_TEXT_KEY)?;
    let decoded = decode_rgba(data, limits)?;
    let Some(text) = legacy else { return Ok(Px::Dense(decoded)) };
    let parts: Vec<&str> = text.split_whitespace().collect();
    if parts.first() != Some(&"v1") || !(parts.len() == 3 || parts.len() == 7) {
        return Err(fwd("unsupported tmi-layer header"));
    }
    let nums: Vec<i64> = parts[1..].iter().map(|p| p.parse::<i64>().map_err(|_| fwd("tmi-layer header int"))).collect::<Result<_>>()?;
    // the header's layer size is a claim of the file's: held to the raster
    // limit before anything is allocated for it (a negative number is huge)
    let size = (usize::try_from(nums[0]).unwrap_or(usize::MAX), usize::try_from(nums[1]).unwrap_or(usize::MAX));
    limits.check_raster(size.0, size.1).map_err(fwd)?;
    if nums.len() != 6 {
        return Ok(Px::Sparse(Sparse::empty(size)));
    }
    Ok(Px::Sparse(Sparse::new(size, nums[2], nums[3], Some(decoded))))
}

/// `raster.png_text_before_idat(data, key)`; Forward where raster.py is
/// unsure (Pillow reads it).
pub fn png_text_before_idat(data: &[u8], key: &str) -> Result<Option<String>> {
    if !data.starts_with(PNG_SIGNATURE) {
        return Ok(None);
    }
    tmi_import::header::png_header(data, false).map_err(fwd)?;
    let mut found = None;
    for (kind, body, before) in png_chunks(data)? {
        if !before {
            break;
        }
        let k = body.split(|&b| b == 0).next().unwrap_or(&[]);
        if k.is_empty() {
            continue;
        }
        let k_text: String = k.iter().map(|&b| b as char).collect();
        if k_text != key {
            continue;
        }
        match kind {
            b"tEXt" => {
                let v = &body[(k.len() + 1).min(body.len())..];
                found = Some(v.iter().map(|&b| b as char).collect());
            }
            b"zTXt" | b"iTXt" => return Err(fwd("compressed text chunk")),
            _ => {}
        }
    }
    Ok(found)
}

/// (type, body, before the first IDAT) of each chunk up to IEND, as
/// `raster.png_chunks` walks them (the structure checked by png_header).
pub fn png_chunks(data: &[u8]) -> Result<Vec<(&[u8], &[u8], bool)>> {
    let mut out = Vec::new();
    let mut pos = 8usize;
    let mut seen_idat = false;
    let n = data.len();
    loop {
        if pos + 8 > n {
            if !seen_idat {
                return Err(fwd("truncated PNG"));
            }
            return Ok(out);
        }
        let length = u32::from_be_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
        let kind = &data[pos + 4..pos + 8];
        if !kind.iter().all(|&b| b.is_ascii_alphanumeric() || b == b'_') {
            if !seen_idat {
                return Err(fwd("broken PNG chunk"));
            }
            return Ok(out);
        }
        if kind == b"IEND" {
            return Ok(out);
        }
        if kind == b"IDAT" || kind == b"fdAT" {
            seen_idat = true;
        }
        let end = pos + 8 + length;
        if end > n {
            return Err(fwd("truncated PNG chunk"));
        }
        let body: &[u8] = if kind == b"IDAT" || kind == b"fdAT" { &[] } else { &data[pos + 8..end] };
        out.push((kind, body, !seen_idat));
        pos = end + 4;
    }
}

/// `raster.decode_rgba(data)` (`decode(data)`, meta=False) for what
/// tmi_image decodes itself.
pub fn decode_rgba(data: &[u8], limits: &Limits) -> Result<Rgba> {
    let (format, size) = if data.starts_with(PNG_SIGNATURE) {
        let h = tmi_import::header::png_header(data, true).map_err(fwd)?;
        ("png", h.size)
    } else if data.len() >= 3 && data[..3] == [0xFF, 0xD8, 0xFF] {
        let h = tmi_import::header::jpeg_header(data).map_err(fwd)?;
        ("jpeg", h.size)
    } else {
        return Err(fwd("not a PNG or JPEG: Pillow decodes it"));
    };
    limits.check_raster(size.0, size.1).map_err(fwd)?;
    let (img, fmt) = tmi_image::decode_rgba(data).map_err(|e| fwd(format!("tmi_image declines: {e:?}")))?;
    let rust_fmt = fmt.name();
    if rust_fmt != format || (img.width, img.height) != size {
        return Err(fwd("decoded format or size differs from the header"));
    }
    Ok(Rgba { width: img.width, height: img.height, data: img.data })
}

/// `layer_blob_to_png(data)`: a plain full-size PNG for a layer blob
/// (other PNG / JPEG bytes pass through).
pub fn layer_blob_to_png(data: &[u8], limits: &Limits) -> Result<Vec<u8>> {
    if data.starts_with(b"TMIL") {
        return png(&pixels_from_bytes(data, limits)?.into_dense());
    }
    match png_text_before_idat(data, LAYER_TEXT_KEY)? {
        None => Ok(data.to_vec()),
        Some(_) => png(&pixels_from_bytes(data, limits)?.into_dense()),
    }
}

/// `repr(s)` of a str: ASCII printable, kana and kanji kept, the quote
/// Python picks; Forward for characters whose printability (and so
/// escaping) is not decided here.
pub fn py_repr(s: &str) -> tmi_repo::Result<String> {
    let quote = if s.contains('\'') && !s.contains('"') { '"' } else { '\'' };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for c in s.chars() {
        if c == '\\' || c == quote {
            out.push('\\');
        }
        out.push(c);
    }
    out.push(quote);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 1x1 PNG with a "tmi-layer" tEXt chunk (`text`) before its IDAT.
    fn legacy_layer_png(text: &str) -> Vec<u8> {
        let plain = png(&Rgba { width: 1, height: 1, data: vec![1, 2, 3, 255] }).unwrap();
        let mut body = b"tmi-layer\0".to_vec();
        body.extend_from_slice(text.as_bytes());
        let mut chunk = (body.len() as u32).to_be_bytes().to_vec();
        chunk.extend_from_slice(b"tEXt");
        chunk.extend_from_slice(&body);
        let mut crc = flate2::Crc::new();
        crc.update(b"tEXt");
        crc.update(&body);
        chunk.extend_from_slice(&crc.sum().to_be_bytes());
        // after the signature (8) and IHDR (4 + 4 + 13 + 4)
        let mut out = plain[..33].to_vec();
        out.extend_from_slice(&chunk);
        out.extend_from_slice(&plain[33..]);
        out
    }

    #[test]
    fn a_legacy_layer_header_is_held_to_the_raster_limit() {
        let limits = Limits { layered: 1_000_000, raster: 1_000_000 };
        for text in ["v1 100000 100000", "v1 100000 100000 0 0 1 1 ", "v1 -1 -1"] {
            let data = legacy_layer_png(text);
            let err = pixels_from_bytes(&data, &limits).err().unwrap_or_else(|| panic!("{text}: must be refused"));
            assert!(format!("{err}").contains("core.maxImagePixels"), "{text}: {err}");
            assert!(layer_blob_to_png(&data, &limits).is_err());
        }
        // within the limit it is still a Sparse of the stated size
        let ok = legacy_layer_png("v1 10 10");
        match pixels_from_bytes(&ok, &limits).unwrap() {
            Px::Sparse(s) => assert_eq!((s.width, s.height), (10, 10)),
            _ => panic!("a legacy layer PNG is sparse"),
        }
    }

    #[test]
    fn repr_as_python() {
        assert_eq!(py_repr("レイヤー 1").unwrap(), "'レイヤー 1'");
        assert_eq!(py_repr("it's").unwrap(), "\"it's\"");
        assert_eq!(py_repr("a'b\"c\\").unwrap(), "'a\\'b\"c\\\\'");
        assert!(py_repr("tab\there").is_err());
        assert!(py_repr("\u{3000}").is_err()); // Zs: Python escapes it
    }

    struct Rng(u64);
    impl Rng {
        fn below(&mut self, n: usize) -> usize {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (self.0 >> 33) as usize % n.max(1)
        }
    }

    /// A random layer of `size`: a crop touching an edge, 1 px, all of it,
    /// anywhere (sometimes reaching past the layer), none, or dense.
    fn layer(rng: &mut Rng, size: (usize, usize)) -> Px {
        let (w, h) = size;
        let span = |rng: &mut Rng, n: usize| -> (i64, i64) {
            let n = n as i64;
            match rng.below(7) {
                0 => (0, 1 + rng.below(n as usize) as i64),
                1 => (rng.below(n as usize) as i64, n),
                2 => (0, n),
                3 => {
                    let a = rng.below(n as usize) as i64;
                    (a, a + 1)
                }
                4 => (-(rng.below(3) as i64), n + rng.below(3) as i64),
                _ => {
                    let a = rng.below(n as usize) as i64;
                    (a, a + 1 + rng.below((n - a) as usize) as i64)
                }
            }
        };
        let px = |rng: &mut Rng, n: usize| -> Vec<u8> {
            (0..n)
                .flat_map(|_| {
                    let a = [0u8, 255, 255, 90, 1][rng.below(5)];
                    [rng.below(256) as u8, rng.below(256) as u8, rng.below(256) as u8, a]
                })
                .collect()
        };
        match rng.below(10) {
            0 => Px::Sparse(Sparse::empty(size)),
            1 => Px::Dense(Rgba { width: w, height: h, data: px(rng, w * h) }),
            _ => {
                let (x0, x1) = span(rng, w);
                let (y0, y1) = span(rng, h);
                let (cw, ch) = ((x1 - x0) as usize, (y1 - y0) as usize);
                let crop = Rgba { width: cw, height: ch, data: px(rng, cw * ch) };
                Px::Sparse(Sparse::new(size, x0, y0, Some(crop)))
            }
        }
    }

    /// The dense twin of a layer, as `dense()` builds it (a crop reaching
    /// past the layer is cut at its edges).
    fn dense_px(p: &Px) -> Px {
        Px::Dense(p.dense().into_owned())
    }

    /// `diff_bbox` as 2b wrote it: one region around both rectangles.
    fn union_bbox(left: &Px, right: &Px) -> Option<Rect> {
        let width = left.width().max(right.width()) as i64;
        let height = left.height().max(right.height()) as i64;
        let rects: Vec<Rect> = [left.rect(), right.rect()].into_iter().flatten().collect();
        if rects.is_empty() {
            return None;
        }
        let bx = (
            rects.iter().map(|r| r.0).min().unwrap().max(0),
            rects.iter().map(|r| r.1).min().unwrap().max(0),
            rects.iter().map(|r| r.2).max().unwrap().min(width),
            rects.iter().map(|r| r.3).max().unwrap().min(height),
        );
        if bx.0 >= bx.2 || bx.1 >= bx.3 {
            return None;
        }
        difference_in(left, right, bx)
    }

    #[test]
    fn sparse_previews_equal_dense_ones() {
        let mut rng = Rng(5);
        for i in 0..600 {
            let size = (1 + rng.below(70), 1 + rng.below(70));
            let left = layer(&mut rng, size);
            let other = if i % 9 == 0 { (1 + rng.below(70), 1 + rng.below(70)) } else { size };
            let right = layer(&mut rng, other);
            let (dl, dr) = (dense_px(&left), dense_px(&right));
            let bbox = diff_bbox(&left, &right);
            assert_eq!(bbox, union_bbox(&left, &right), "bbox {i}");
            if size == other {
                assert_eq!(bbox, diff_bbox_rgba(&dl.dense(), &dr.dense()), "dense bbox {i}");
            }
            assert_eq!(side_by_side_sparse(&left, &right).data, side_by_side(&left, &right).data, "side by side {i}");
            assert_eq!(overlay_sparse(&left, &right).data, overlay(&left, &right).data, "overlay {i}");
            if bbox.is_some() {
                assert_eq!(diff_preview_sparse(&left, &right, bbox).data, diff_preview(&left, &right, None).data, "diff preview {i}");
            }
            // the HTML layer preview: region of the padded box, shrunk
            let Some(b) = bbox else { continue };
            let bx = ((b.0 - 16).max(0), (b.1 - 16).max(0), (left.width() as i64).min(b.2 + 16), (left.height() as i64).min(b.3 + 16));
            if bx.0 >= bx.2 || bx.1 >= bx.3 {
                continue;
            }
            for edge in [1024usize, 37, 9, 3, 1] {
                let scale = edge as f64 / ((bx.2 - bx.0).max(bx.3 - bx.1)) as f64;
                let fit = |v: i64| if scale >= 1.0 { v as usize } else { ((v as f64 * scale).round_ties_even() as usize).max(1) };
                let size = (fit(bx.2 - bx.0), fit(bx.3 - bx.1));
                for p in [&left, &right] {
                    let want = resize(&p.region(bx), size, None).unwrap();
                    assert_eq!(resize_region(p, bx, size).unwrap().data, want.data, "resize {i} {edge}");
                }
            }
        }
    }

    #[test]
    fn sparse_regions() {
        let crop = Rgba { width: 2, height: 1, data: vec![1, 2, 3, 4, 5, 6, 7, 8] };
        let s = Px::Sparse(Sparse::new((4, 3), 1, 1, Some(crop)));
        assert_eq!(s.rect(), Some((1, 1, 3, 2)));
        let r = s.region((0, 0, 4, 3));
        assert_eq!(&r.data[(4 + 1) * 4..(4 + 3) * 4], &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(r.data.iter().filter(|&&b| b != 0).count(), 8);
        let blank_px = Px::Sparse(Sparse::empty((4, 3)));
        assert_eq!(diff_bbox(&s, &blank_px), Some((1, 1, 3, 2)));
        assert_eq!(diff_bbox(&blank_px, &blank_px), None);
    }
}
