//! Image codecs matched to Pillow 12.3 (RUST_MIGRATION.md stage 2):
//! decoders that give the image `PIL.Image.open` gives (mode, pixels,
//! palette, transparency), so `tmi_pillow::convert` turns it into the bytes
//! `.convert("RGBA")` gives; encoders configured as Pillow's.
//!
//! [`open`] / [`decode_rgba`] read PNG and JPEG, the formats tmi routes here
//! today. [`open_any`] / [`decode_rgba_any`] add the formats of PLUGINS.md
//! P0a–c (GIF, BMP / DIB, TIFF, WebP, PSD), each pixel for pixel as Pillow
//! 12.3.0 opens it; what one of them cannot reproduce is
//! [`DecodeError::Declined`], never different pixels. [`import::open_for_import`]
//! is tmi's rule for which files are images at all (P0e): those these
//! decoders read; anything else is an opaque blob.

pub mod bmp;
pub mod gif;
pub mod header;
pub mod import;
pub mod jpeg;
pub mod png_dec;
pub mod png_enc;
pub mod psd;
pub mod tiff;
mod unpack;
pub mod webp_dec;
pub mod webp_enc;
pub mod writeback;

use tmi_pillow::convert::{convert, ConvertError};
use tmi_pillow::image::{Image, Mode};

#[derive(Debug)]
pub enum DecodeError {
    Png(png_dec::PngError),
    Jpeg(jpeg::JpegError),
    Convert(ConvertError),
    /// What this crate leaves to Pillow: an animated PNG (Pillow's APNG
    /// frame handling and validation are not reproduced), and in the P0
    /// decoders whatever they do not reproduce exactly.
    Declined(&'static str),
    /// Data Pillow 12.3.0 refuses too (`Image.open` or `load` raises).
    Corrupt(Format, String),
    /// More pixels than the caller's limit (Pillow's decompression-bomb
    /// check, which tmi sets to its own limit).
    TooLarge(usize, usize),
    /// None of the formats this entry point reads.
    UnknownFormat,
}

/// A sentence for users (a plugin's layer image that does not decode,
/// issue #51). The built-in readers' messages keep the `Debug` form.
impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::Png(png_dec::PngError::Decode(m)) => write!(f, "a PNG image is damaged: {m}"),
            DecodeError::Png(png_dec::PngError::Unsupported) => write!(f, "a PNG image has a bit depth and colour type tmi does not read"),
            DecodeError::Jpeg(jpeg::JpegError::Libjpeg(m) | jpeg::JpegError::Corrupt(_, m)) => write!(f, "a JPEG image is damaged: {m}"),
            DecodeError::Jpeg(jpeg::JpegError::Unsupported(m)) => write!(f, "a JPEG image tmi does not read: {m}"),
            DecodeError::Jpeg(jpeg::JpegError::Truncated) => write!(f, "a JPEG image is cut short"),
            DecodeError::Jpeg(jpeg::JpegError::TooLarge(w, h)) | DecodeError::TooLarge(w, h) => {
                write!(f, "an image of {w}x{h} pixels is over the pixel limit")
            }
            DecodeError::Convert(_) => write!(f, "an image in a colour mode tmi cannot convert"),
            DecodeError::Declined(m) => write!(f, "an image tmi does not decode itself: {m}"),
            DecodeError::Corrupt(fmt, m) => write!(f, "a {} image is damaged: {m}", fmt.name().to_ascii_uppercase()),
            DecodeError::UnknownFormat => write!(f, "an image in a format tmi does not read"),
        }
    }
}

/// The source format `decode` recognised.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Png,
    Jpeg,
    Gif,
    Bmp,
    /// A BMP without the file header (Pillow's DibImagePlugin).
    Dib,
    Tiff,
    WebP,
    Psd,
}

impl Format {
    /// `Image.format.lower()`.
    pub fn name(self) -> &'static str {
        match self {
            Format::Png => "png",
            Format::Jpeg => "jpeg",
            Format::Gif => "gif",
            Format::Bmp => "bmp",
            Format::Dib => "dib",
            Format::Tiff => "tiff",
            Format::WebP => "webp",
            Format::Psd => "psd",
        }
    }
}

/// The `info` entries tmi reads from an opened image (after `load()` and
/// `convert("RGBA")`, which keeps them).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Info {
    /// `info["dpi"]`.
    pub dpi: Option<(f64, f64)>,
    /// `info["icc_profile"]` (present, even if empty).
    pub icc_profile: Option<Vec<u8>>,
}

impl Info {
    /// What importers.py records as the canvas dpi: `float(dpi[0])`.
    pub fn canvas_dpi(&self) -> Option<f64> {
        self.dpi.map(|d| d.0)
    }
}

/// An image as `Image.open(data)` + `load()` leave it.
#[derive(Debug)]
pub struct Opened {
    pub image: Image,
    pub format: Format,
    pub info: Info,
}

/// `Image.open(data)` for PNG and JPEG (by signature, as Pillow tries them).
pub fn open(data: &[u8]) -> Result<(Image, Format), DecodeError> {
    open_max(data, None)
}

/// [`open`] held to `max_pixels` (`TooLarge` above it), checked on the
/// size the header declares before the pixels are allocated.
pub fn open_max(data: &[u8], max_pixels: Option<u64>) -> Result<(Image, Format), DecodeError> {
    let lim = Limit(max_pixels);
    if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        if lim.0.is_some() {
            // (a file `scan` refuses, `decode_png` refuses the same way)
            let size = png_dec::scan(data).map_err(DecodeError::Png)?.info.size;
            lim.check(size.0 as usize, size.1 as usize)?;
        }
        // An APNG decodes to its frame 0, as Image.open gives it.
        let d = png_dec::decode_png(data).map_err(DecodeError::Png)?;
        return Ok((d.image, Format::Png));
    }
    if data.len() >= 3 && data[0] == 0xFF && data[1] == 0xD8 && data[2] == 0xFF {
        let image = jpeg::decode_jpeg_max(data, max_pixels).map_err(|e| match e {
            jpeg::JpegError::TooLarge(w, h) => DecodeError::TooLarge(w, h),
            e => DecodeError::Jpeg(e),
        })?;
        return Ok((image, Format::Jpeg));
    }
    Err(DecodeError::UnknownFormat)
}

/// `Image.open(data).convert("RGBA")` (importers.py's `as_rgba(opened)`).
pub fn decode_rgba(data: &[u8]) -> Result<(Image, Format), DecodeError> {
    decode_rgba_max(data, None)
}

/// [`decode_rgba`] held to `max_pixels` (see [`open_max`]).
pub fn decode_rgba_max(data: &[u8], max_pixels: Option<u64>) -> Result<(Image, Format), DecodeError> {
    let (img, fmt) = open_max(data, max_pixels)?;
    if img.mode == Mode::RGBA {
        return Ok((img, fmt));
    }
    Ok((convert(&img, Mode::RGBA).map_err(DecodeError::Convert)?, fmt))
}

/// The format of the P0 decoder that would read `data`, by the magic bytes
/// Pillow's plugins accept (`_accept`), or None.
pub fn sniff_any(data: &[u8]) -> Option<Format> {
    if gif::accept(data) {
        Some(Format::Gif)
    } else if bmp::accept(data) {
        Some(Format::Bmp)
    } else if bmp::accept_dib(data) {
        Some(Format::Dib)
    } else if tiff::accept(data) {
        Some(Format::Tiff)
    } else if webp_dec::accept(data) {
        Some(Format::WebP)
    } else if psd::accept(data) {
        Some(Format::Psd)
    } else {
        None
    }
}

/// `Image.open(data)` + `load()` for the formats of PLUGINS.md P0a–c (GIF,
/// BMP / DIB, TIFF, WebP, PSD) by their magic bytes; PNG and JPEG go to
/// [`open`] (with no `info`). `max_pixels` is the decompression-bomb limit
/// (`TooLarge` above it, checked before the pixels are allocated).
pub fn open_any(data: &[u8], max_pixels: Option<u64>) -> Result<Opened, DecodeError> {
    let lim = Limit(max_pixels);
    match sniff_any(data) {
        Some(Format::Gif) => gif::open(data, lim),
        Some(Format::Bmp) => bmp::open(data, lim),
        Some(Format::Dib) => bmp::open_dib(data, lim),
        Some(Format::Tiff) => tiff::open(data, lim),
        Some(Format::WebP) => webp_dec::open(data, lim),
        Some(Format::Psd) => psd::open(data, lim),
        _ => {
            let (image, format) = open_max(data, max_pixels)?;
            Ok(Opened { image, format, info: Info::default() })
        }
    }
}

/// `Image.open(data).convert("RGBA")` with its `info`, for every format
/// [`open_any`] reads.
pub fn decode_rgba_any(data: &[u8], max_pixels: Option<u64>) -> Result<Opened, DecodeError> {
    let mut o = open_any(data, max_pixels)?;
    if o.image.mode != Mode::RGBA {
        o.image = convert(&o.image, Mode::RGBA).map_err(DecodeError::Convert)?;
    }
    Ok(o)
}

/// The caller's pixel limit.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Limit(pub Option<u64>);

impl Limit {
    pub fn check(self, w: usize, h: usize) -> Result<(), DecodeError> {
        // With no limit (core.maxImagePixels = 0) still nothing a buffer of
        // four bytes a pixel cannot hold: `ImBuf::new`'s product must not wrap.
        if (w as u128) * (h as u128) * 4 > isize::MAX as u128 {
            return Err(DecodeError::TooLarge(w, h));
        }
        match self.0 {
            Some(max) if (w as u64).saturating_mul(h as u64) > max => Err(DecodeError::TooLarge(w, h)),
            _ => Ok(()),
        }
    }
}

/// A `fp.read()`-style cursor: reads past the end come back short, as a
/// Python file's do.
pub(crate) struct Cursor<'a> {
    pub data: &'a [u8],
    pub pos: usize,
}

impl<'a> Cursor<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Cursor { data, pos: 0 }
    }

    pub fn read(&mut self, n: usize) -> &'a [u8] {
        let start = self.pos.min(self.data.len());
        let end = start.saturating_add(n).min(self.data.len());
        self.pos = self.pos.max(start).saturating_add(end - start);
        &self.data[start..end]
    }

    pub fn seek(&mut self, pos: usize) {
        self.pos = pos;
    }

    pub fn tell(&self) -> usize {
        self.pos
    }
}
