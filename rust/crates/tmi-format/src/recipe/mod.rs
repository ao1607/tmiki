//! The native recipe decoder (PLUGINS.md 2.2, "describe, don't decode"): a
//! `Bitmap` recipe into the RGBA8 rows of the layer, which the pipeline cuts
//! to a `LayerCrop`. Everything here is the host's: the plugin only
//! described where the bytes are and how to read them.

pub mod layout;
pub mod sparse;
pub mod transform;

use rayon::prelude::*;
use tmi_image::{DecodeError, Format};
use tmi_pillow::image::Image;

use crate::reader::{FormatReader, ImageKey, ReadCx};
use crate::types::*;

/// What a decode needs: the source bytes, the reader (for `Codec::Plugin`)
/// and the read's shared state.
pub struct DecodeEnv<'a> {
    pub source: &'a [u8],
    pub reader: Option<&'a dyn FormatReader>,
    pub cx: &'a ReadCx,
}

/// A decoded bitmap: `width * height` RGBA8 pixels.
pub struct DecodedBitmap {
    pub width: usize,
    pub height: usize,
    pub data: Vec<u8>,
}

fn corrupt(msg: impl Into<String>) -> ReadError {
    ReadError::Corrupt(msg.into())
}

/// The bytes of `width` x `height` RGBA8 pixels, None when that does not fit
/// a `usize`: a multiplication that wraps must never pass a length check.
pub fn raw_len(width: u32, height: u32) -> Option<usize> {
    (width as usize).checked_mul(height as usize)?.checked_mul(4)
}

/// The message of the pixel limit, as Python reports it.
pub fn too_large(w: usize, h: usize) -> ReadError {
    ReadError::TooLarge(format!("image of {w}x{h} pixels is over core.maxImagePixels: Python reports it"))
}

/// `check_raster_pixels`: an empty side counts as 1.
fn check_pixels(env: &DecodeEnv, w: usize, h: usize) -> Result<(), ReadError> {
    let pixels = (w.max(1) as u128) * (h.max(1) as u128);
    if env.cx.max_pixels.is_some_and(|m| m != 0 && pixels > m as u128) {
        return Err(too_large(w, h));
    }
    if pixels > transform::MAX_EXPANDED as u128 {
        return Err(corrupt(format!("a bitmap of {w}x{h} pixels")));
    }
    Ok(())
}

fn expected_format(c: ImageCodec, f: Format) -> bool {
    matches!(
        (c, f),
        (ImageCodec::Png, Format::Png)
            | (ImageCodec::JpegV1, Format::Jpeg)
            | (ImageCodec::WebpV1, Format::WebP)
            | (ImageCodec::Gif, Format::Gif)
            | (ImageCodec::Bmp, Format::Bmp | Format::Dib)
            | (ImageCodec::Tiff, Format::Tiff)
            | (ImageCodec::PsdBuiltin, Format::Psd)
    )
}

/// An `encoded-image` as RGBA8. A decode a built-in reader offered in `cx`
/// (the same single span, no transforms) is taken instead of repeated.
pub fn decode_image(env: &DecodeEnv, img: &EncodedImage) -> Result<Image, ReadError> {
    if img.pre.is_empty() {
        if let Bytes::Span(s) = &img.data {
            let key = ImageKey { offset: s.offset, len: s.len, codec: img.codec, frame: img.frame };
            if let Some(image) = env.cx.take_decoded(&key) {
                return Ok(image);
            }
        }
    }
    if img.frame != 0 {
        return Err(ReadError::Unsupported(format!("frame {}: only frame 0 of an image is read", img.frame)));
    }
    if img.codec == ImageCodec::Qoi {
        return Err(ReadError::Unsupported("the qoi decoder is not built yet".into()));
    }
    let data = transform::resolve(env.source, &img.data)?;
    let data = transform::apply(env, &img.pre, data, None)?;
    let opened = tmi_image::decode_rgba_any(&data, env.cx.max_pixels.filter(|&m| m != 0)).map_err(|e| match e {
        DecodeError::TooLarge(w, h) => too_large(w, h),
        e => corrupt(e.to_string()),
    })?;
    if !expected_format(img.codec, opened.format) {
        return Err(corrupt(format!("the data is {}, not {:?}", opened.format.name(), img.codec)));
    }
    Ok(opened.image)
}

fn decode_raster(env: &DecodeEnv, r: &Raster, w: usize, h: usize) -> Result<Vec<u8>, ReadError> {
    let expect = layout::expected_len(&r.layout, w, h)?;
    let data = transform::resolve(env.source, &r.data)?;
    let data = transform::apply(env, &r.codecs, data, Some(expect))?;
    layout::assemble(&r.layout, &data, w, h)
}

/// One tile's RGBA8 (`width * height * 4` bytes); None for `empty`.
fn decode_tile(env: &DecodeEnv, t: &Tile) -> Result<Option<Vec<u8>>, ReadError> {
    let (w, h) = (t.width as usize, t.height as usize);
    if w == 0 || h == 0 {
        return Ok(None);
    }
    check_pixels(env, w, h)?;
    match &t.content {
        TileContent::Empty => Ok(None),
        TileContent::Fill(c) => Ok(Some([c.r, c.g, c.b, c.a].repeat(w * h))),
        TileContent::Raster(r) => decode_raster(env, r, w, h).map(Some),
        TileContent::Image(img) => {
            let image = decode_image(env, img)?;
            if (image.width, image.height) != (w, h) {
                return Err(corrupt(format!("a tile of {w}x{h} holds an image of {}x{}", image.width, image.height)));
            }
            Ok(Some(image.data))
        }
    }
}

/// The rectangle of a `w` x `h` source at (`x`, `y`) inside a `dw` x `dh`
/// destination: (source x, source y, dest x, dest y, width, height).
fn clip(dw: usize, dh: usize, w: usize, h: usize, x: i64, y: i64) -> Option<(usize, usize, usize, usize, usize, usize)> {
    let (x0, y0) = (x.max(0), y.max(0));
    let x1 = (x + w as i64).min(dw as i64);
    let y1 = (y + h as i64).min(dh as i64);
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    Some(((x0 - x) as usize, (y0 - y) as usize, x0 as usize, y0 as usize, (x1 - x0) as usize, (y1 - y0) as usize))
}

/// `bitmap`'s pixels. The size is `bitmap.size`, or for a single image the
/// image's own size.
pub fn decode_bitmap(env: &DecodeEnv, bitmap: &Bitmap) -> Result<DecodedBitmap, ReadError> {
    match &bitmap.source {
        PixelSource::Empty => {
            let (w, h) = bitmap.size.map_or((0, 0), |(w, h)| (w as usize, h as usize));
            check_pixels(env, w, h)?;
            Ok(DecodedBitmap { width: w, height: h, data: vec![0u8; w * h * 4] })
        }
        PixelSource::Image(p) => {
            let image = decode_image(env, &p.image)?;
            let (iw, ih) = (image.width, image.height);
            let (w, h) = bitmap.size.map_or((iw, ih), |(w, h)| (w as usize, h as usize));
            check_pixels(env, w, h)?;
            if (w, h) == (iw, ih) && p.x == 0 && p.y == 0 {
                return Ok(DecodedBitmap { width: w, height: h, data: image.data });
            }
            let mut data = vec![0u8; w * h * 4];
            tmi_pillow::rgba::paste(&mut data, w, h, &image.data, iw, ih, p.x as i64, p.y as i64);
            Ok(DecodedBitmap { width: w, height: h, data })
        }
        PixelSource::Raw(r) => {
            let (rw, rh) = (r.width as usize, r.height as usize);
            if raw_len(r.width, r.height) != Some(r.rgba.len()) {
                return Err(corrupt(format!("raw pixels: {} bytes for {rw}x{rh}", r.rgba.len())));
            }
            let (w, h) = bitmap.size.map_or((rw, rh), |(w, h)| (w as usize, h as usize));
            check_pixels(env, w, h)?;
            let mut data = vec![0u8; w * h * 4];
            tmi_pillow::rgba::paste(&mut data, w, h, &r.rgba, rw, rh, r.x as i64, r.y as i64);
            Ok(DecodedBitmap { width: w, height: h, data })
        }
        PixelSource::Tiles(t) => {
            let Some((w, h)) = bitmap.size else {
                return Err(corrupt("a bitmap of tiles needs a size"));
            };
            let (w, h) = (w as usize, h as usize);
            check_pixels(env, w, h)?;
            decode_tiles(env, t, w, h)
        }
    }
}

/// `bitmap`'s pixels as the `LayerCrop` tmi records: `decode_bitmap` cut by
/// `LayerCrop::from_full`, byte for byte, but tiles go through
/// `sparse::decode`, which never allocates the whole bitmap.
pub fn decode_layer_crop(env: &DecodeEnv, bitmap: &Bitmap) -> Result<tmi_import::crop::LayerCrop, ReadError> {
    if let PixelSource::Tiles(t) = &bitmap.source {
        let Some((w, h)) = bitmap.size else {
            return Err(corrupt("a bitmap of tiles needs a size"));
        };
        let (w, h) = (w as usize, h as usize);
        check_pixels(env, w, h)?;
        return sparse::decode(env, t, w, h);
    }
    let d = decode_bitmap(env, bitmap)?;
    Ok(tmi_import::crop::LayerCrop::from_full(&d.data, d.width, d.height))
}

/// Tiles are decoded in parallel, a batch at a time, and laid down in order.
const BATCH: usize = 64;

fn decode_tiles(env: &DecodeEnv, t: &Tiled, w: usize, h: usize) -> Result<DecodedBitmap, ReadError> {
    let mut data = vec![0u8; w * h * 4];
    let mut covered = (t.overlap == Overlap::Reject).then(|| vec![false; w * h]);
    for batch in t.tiles.chunks(BATCH) {
        let decoded: Vec<Result<Option<Vec<u8>>, ReadError>> = batch.par_iter().map(|tile| decode_tile(env, tile)).collect();
        for (tile, px) in batch.iter().zip(decoded) {
            let Some(px) = px? else { continue };
            let (tw, th) = (tile.width as usize, tile.height as usize);
            let (x, y) = (tile.x as i64, tile.y as i64);
            if let Some(mask) = covered.as_mut() {
                if let Some((_, _, dx, dy, cw, ch)) = clip(w, h, tw, th, x, y) {
                    for row in dy..dy + ch {
                        for m in &mut mask[row * w + dx..row * w + dx + cw] {
                            if *m {
                                return Err(corrupt(format!("tiles overlap at ({x}, {y}) and overlap is reject")));
                            }
                            *m = true;
                        }
                    }
                }
            }
            match t.overlap {
                Overlap::Over => tmi_pillow::rgba::over(&mut data, w, h, &px, tw, th, x, y),
                Overlap::Replace | Overlap::Reject => tmi_pillow::rgba::paste(&mut data, w, h, &px, tw, th, x, y),
            }
        }
    }
    Ok(DecodedBitmap { width: w, height: h, data })
}

#[cfg(test)]
mod tests;
