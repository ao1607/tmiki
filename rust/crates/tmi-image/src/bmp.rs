//! BMP (and the header-less DIB) as Pillow 12.3.0 opens and loads it:
//! BmpImagePlugin `_bitmap`, the raw decoder (RawDecode.c with Unpack.c's
//! unpackers) or the RLE decoder (`BmpRleDecoder`, then `set_as_raw`).
//!
//! - Headers of 12 (OS/2 1.x), 40, 52, 56, 64 (OS/2 2.x), 108 (V4) and 124
//!   (V5) bytes; a negative height (top byte 0xFF) is a top-down image.
//! - 1 / 4 / 8 bits: mode "P", or "1" / "L" when the palette is the black /
//!   white pair or the grey ramp (and then the rows are unpacked as "1" or
//!   "L" whatever the depth, as Pillow does). 16 / 24 / 32 bits: "RGB" (32
//!   bits without bit fields ignores the fourth byte).
//! - BI_BITFIELDS: only Pillow's mask layouts (the others are refused as
//!   Pillow refuses them); 32-bit layouts with an alpha mask are "RGBA".
//! - RLE8 / RLE4, with Pillow's quirks (an odd absolute run in RLE4 drops
//!   its last pixel, word alignment from the start of the file, too little
//!   data is an error).
//! - `info["dpi"]` is the pixels per metre / 39.3701 (headers of 40 bytes
//!   and more); no ICC profile (Pillow ignores the V5 profile).

use tmi_pillow::image::{Mode, Palette};

use crate::unpack::{self, ImBuf, RawError};
use crate::{Cursor, DecodeError, Format, Info, Limit, Opened};

/// `_accept`.
pub fn accept(data: &[u8]) -> bool {
    data.starts_with(b"BM")
}

/// `_dib_accept`.
pub fn accept_dib(data: &[u8]) -> bool {
    data.len() >= 4 && [12, 40, 52, 56, 64, 108, 124].contains(&u32::from_le_bytes([data[0], data[1], data[2], data[3]]))
}

fn i16(b: &[u8], at: usize) -> Option<u64> {
    Some(u16::from_le_bytes([*b.get(at)?, *b.get(at + 1)?]) as u64)
}

fn i32(b: &[u8], at: usize) -> Option<u64> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?) as u64)
}

const RAW: u64 = 0;
const RLE8: u64 = 1;
const RLE4: u64 = 2;
const BITFIELDS: u64 = 3;

enum Decoder {
    Raw { stride: u64 },
    Rle { rle4: bool },
}

struct Bitmap {
    format: Format,
    mode: Mode,
    raw_mode: &'static str,
    width: u64,
    height: u64,
    direction: i32,
    offset: u64,
    decoder: Decoder,
    /// BGR(X) entries of a "P" image.
    palette: Option<(Vec<u8>, usize)>,
    dpi: Option<(f64, f64)>,
}

/// `_bitmap(header, offset)` reading from `r`.
fn bitmap(r: &mut Cursor, format: Format, mut offset: u64) -> Result<Bitmap, DecodeError> {
    let corrupt = |m: &str| DecodeError::Corrupt(format, m.to_string());
    let header_size = i32(r.read(4), 0).ok_or_else(|| corrupt("short header"))?;
    let need = header_size.saturating_sub(4) as usize;
    let header = r.read(need);
    if header.len() < need {
        return Err(corrupt("Truncated File Read"));
    }
    let h = header;
    let (width, height, bits, compression, padding, colors, dpi, direction);
    let mut masks: Option<(u64, u64, u64, u64)> = None;
    if header_size == 12 {
        width = i16(h, 0).ok_or_else(|| corrupt("short header"))?;
        height = i16(h, 2).ok_or_else(|| corrupt("short header"))?;
        bits = i16(h, 6).ok_or_else(|| corrupt("short header"))?;
        compression = RAW;
        padding = 3u64;
        colors = 0u64;
        dpi = None;
        direction = -1;
    } else if [40, 52, 56, 64, 108, 124].contains(&header_size) {
        let y_flip = h[7] == 0xFF;
        direction = if y_flip { 1 } else { -1 };
        width = i32(h, 0).unwrap();
        height = if y_flip { (1u64 << 32) - i32(h, 4).unwrap() } else { i32(h, 4).unwrap() };
        bits = i16(h, 10).unwrap();
        compression = i32(h, 12).unwrap();
        let ppm = (i32(h, 20).unwrap(), i32(h, 24).unwrap());
        colors = i32(h, 28).unwrap();
        padding = 4;
        dpi = Some((ppm.0 as f64 / 39.3701, ppm.1 as f64 / 39.3701));
        if compression == BITFIELDS {
            if h.len() >= 48 {
                let a = if h.len() >= 52 { i32(h, 48).unwrap() } else { 0 };
                masks = Some((i32(h, 36).unwrap(), i32(h, 40).unwrap(), i32(h, 44).unwrap(), a));
            } else {
                let mut m = [0u64; 3];
                for v in m.iter_mut() {
                    *v = i32(r.read(4), 0).ok_or_else(|| corrupt("short masks"))?;
                }
                masks = Some((m[0], m[1], m[2], 0));
            }
        }
    } else {
        return Err(corrupt("Unsupported BMP header type"));
    }
    let colors = if colors == 0 { if bits >= 64 { u64::MAX } else { 1u64 << bits } } else { colors };
    if offset == 14 + header_size && bits <= 8 {
        offset += padding * colors;
    }
    let (mut mode, mut raw_mode) = match bits {
        1 => (Mode::P, "P;1"),
        4 => (Mode::P, "P;4"),
        8 => (Mode::P, "P"),
        16 => (Mode::RGB, "BGR;15"),
        24 => (Mode::RGB, "BGR"),
        32 => (Mode::RGB, "BGRX"),
        _ => return Err(corrupt("Unsupported BMP pixel depth")),
    };
    let mut decoder = Decoder::Raw { stride: ((width * bits + 31) >> 3) & !3 };
    match compression {
        BITFIELDS => {
            let (rm, gm, bm, am) = masks.unwrap();
            let layout = match (bits, rm, gm, bm, am) {
                (32, 0xFF0000, 0xFF00, 0xFF, 0x0) => "BGRX",
                (32, 0xFF000000, 0xFF0000, 0xFF00, 0x0) => "XBGR",
                (32, 0xFF000000, 0xFF00, 0xFF, 0x0) => "BGXR",
                (32, 0xFF000000, 0xFF0000, 0xFF00, 0xFF) => "ABGR",
                (32, 0xFF, 0xFF00, 0xFF0000, 0xFF000000) => "RGBA",
                (32, 0xFF0000, 0xFF00, 0xFF, 0xFF000000) => "BGRA",
                (32, 0xFF000000, 0xFF00, 0xFF, 0xFF0000) => "BGAR",
                (32, 0x0, 0x0, 0x0, 0x0) => "BGRA",
                (24, 0xFF0000, 0xFF00, 0xFF, _) => "BGR",
                (16, 0xF800, 0x7E0, 0x1F, _) => "BGR;16",
                (16, 0x7C00, 0x3E0, 0x1F, _) => "BGR;15",
                _ => return Err(corrupt("Unsupported BMP bitfields layout")),
            };
            raw_mode = layout;
            if bits == 32 && layout.contains('A') {
                mode = Mode::RGBA;
            }
        }
        RAW => {}
        RLE8 | RLE4 => decoder = Decoder::Rle { rle4: compression == RLE4 },
        _ => return Err(corrupt("Unsupported BMP compression")),
    }
    let mut palette = None;
    if mode == Mode::P {
        if !(0 < colors && colors <= 65536) {
            return Err(corrupt("Unsupported BMP Palette size"));
        }
        let pal = r.read((padding * colors) as usize);
        let indices: Vec<u64> = if colors == 2 { vec![0, 255] } else { (0..colors).collect() };
        let grayscale = indices.iter().enumerate().all(|(ind, &val)| {
            let at = ind * padding as usize;
            let rgb = &pal[at.min(pal.len())..(at + 3).min(pal.len())];
            let v = (val & 255) as u8;
            rgb == [v, v, v]
        });
        if grayscale {
            mode = if colors == 2 { Mode::One } else { Mode::L };
            raw_mode = if colors == 2 { "1" } else { "L" };
        } else {
            palette = Some((pal.to_vec(), padding as usize));
        }
    }
    let offset = if offset != 0 { offset } else { r.tell() as u64 };
    Ok(Bitmap { format, mode, raw_mode, width, height, direction, offset, decoder, palette, dpi })
}

fn header(data: &[u8], dib: bool) -> Result<Bitmap, DecodeError> {
    let mut r = Cursor::new(data);
    if dib {
        return bitmap(&mut r, Format::Dib, 0);
    }
    let head = r.read(14);
    if !accept(head) {
        return Err(DecodeError::Corrupt(Format::Bmp, "Not a BMP file".into()));
    }
    let offset = i32(head, 10).ok_or_else(|| DecodeError::Corrupt(Format::Bmp, "short header".into()))?;
    bitmap(&mut r, Format::Bmp, offset)
}

/// The size `Image.open(data).size` reports.
pub fn size(data: &[u8]) -> Result<(u64, u64), DecodeError> {
    let b = header(data, !accept(data))?;
    Ok((b.width, b.height))
}

pub(crate) fn open(data: &[u8], limit: Limit) -> Result<Opened, DecodeError> {
    load(data, header(data, false)?, limit)
}

pub(crate) fn open_dib(data: &[u8], limit: Limit) -> Result<Opened, DecodeError> {
    load(data, header(data, true)?, limit)
}

fn load(data: &[u8], b: Bitmap, limit: Limit) -> Result<Opened, DecodeError> {
    let corrupt = |m: &str| DecodeError::Corrupt(b.format, m.to_string());
    if b.width == 0 || b.height == 0 {
        return Err(corrupt("not identified by this driver"));
    }
    if b.width > u32::MAX as u64 || b.height > u32::MAX as u64 {
        return Err(DecodeError::TooLarge(b.width as usize, b.height as usize));
    }
    let (w, h) = (b.width as usize, b.height as usize);
    limit.check(w, h)?;
    // Image.load: the palette ("invalid palette size" past 256 entries).
    let palette = match &b.palette {
        Some((pal, padding)) => {
            let n = pal.len() / padding;
            if n > 256 {
                return Err(corrupt("invalid palette size"));
            }
            let mut entries = vec![[0u8, 0, 0, 255]; 256];
            for (e, c) in entries.iter_mut().zip(pal.chunks_exact(*padding)) {
                *e = [c[2], c[1], c[0], 255];
            }
            Some(Palette { entries })
        }
        None => None,
    };
    let rest = data.get(b.offset as usize..).unwrap_or(&[]);
    let mut im = ImBuf::new(b.mode, w, h);
    match b.decoder {
        Decoder::Raw { stride } => {
            let un = unpack::find(b.mode, b.raw_mode).ok_or_else(|| corrupt("unknown raw mode"))?;
            if rest.is_empty() {
                return Err(corrupt("image file is truncated"));
            }
            unpack::raw_decode(&mut im, (0, 0, w, h), un, stride as usize, b.direction, rest).map_err(|e| match e {
                RawError::Config => corrupt("bad configuration"),
                RawError::Short => corrupt("image file is truncated"),
            })?;
        }
        Decoder::Rle { rle4 } => {
            let px = rle_decode(data, b.offset as usize, w, h, rle4);
            let raw = if b.mode == Mode::L { "L" } else { "P" };
            let un = unpack::find(b.mode, raw).ok_or_else(|| corrupt("unknown raw mode"))?;
            unpack::raw_decode(&mut im, (0, 0, w, h), un, 0, b.direction, &px).map_err(|_| corrupt("not enough image data"))?;
        }
    }
    let info = Info { dpi: b.dpi, icc_profile: None };
    Ok(Opened { image: im.into_image(palette), format: b.format, info })
}

/// `BmpRleDecoder.decode`: the pixel bytes (one per pixel), reading the
/// file from `offset`.
fn rle_decode(file: &[u8], offset: usize, xsize: usize, ysize: usize, rle4: bool) -> Vec<u8> {
    let mut fd = Cursor::new(file);
    fd.seek(offset);
    let mut data: Vec<u8> = Vec::new();
    let mut x = 0usize;
    let dest_length = xsize * ysize;
    while data.len() < dest_length {
        let pixels = fd.read(1);
        let byte = fd.read(1);
        if pixels.is_empty() || byte.is_empty() {
            break;
        }
        let (mut num_pixels, byte) = (pixels[0] as usize, byte[0]);
        if num_pixels != 0 {
            // encoded mode
            if x + num_pixels > xsize {
                num_pixels = xsize.saturating_sub(x);
            }
            if rle4 {
                let (first, second) = (byte >> 4, byte & 0x0F);
                for index in 0..num_pixels {
                    data.push(if index % 2 == 0 { first } else { second });
                }
            } else {
                data.extend(std::iter::repeat_n(byte, num_pixels));
            }
            x += num_pixels;
        } else if byte == 0 {
            // end of line
            while data.len() % xsize != 0 {
                data.push(0);
            }
            x = 0;
        } else if byte == 1 {
            // end of bitmap
            break;
        } else if byte == 2 {
            // delta
            let d = fd.read(2);
            if d.len() < 2 {
                break;
            }
            let (right, up) = (d[0] as usize, d[1] as usize);
            data.resize(data.len() + right + up * xsize, 0);
            x = data.len() % xsize;
        } else {
            // absolute mode
            let byte_count = if rle4 { byte as usize / 2 } else { byte as usize };
            let read = fd.read(byte_count);
            if rle4 {
                for &b in read {
                    data.push(b >> 4);
                    data.push(b & 0x0F);
                }
            } else {
                data.extend_from_slice(read);
            }
            if read.len() < byte_count {
                break;
            }
            x += byte as usize;
            // align to 16-bit word boundary
            if fd.tell() % 2 != 0 {
                fd.seek(fd.tell() + 1);
            }
        }
    }
    data
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bmp(bits: u16, w: i32, h: i32, compression: u32, extra: &[u8], pixels: &[u8]) -> Vec<u8> {
        let off = 14 + 40 + extra.len() as u32;
        let mut v = b"BM".to_vec();
        v.extend_from_slice(&(off + pixels.len() as u32).to_le_bytes());
        v.extend_from_slice(&[0; 4]);
        v.extend_from_slice(&off.to_le_bytes());
        v.extend_from_slice(&40u32.to_le_bytes());
        v.extend_from_slice(&w.to_le_bytes());
        v.extend_from_slice(&h.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&bits.to_le_bytes());
        v.extend_from_slice(&compression.to_le_bytes());
        v.extend_from_slice(&(pixels.len() as u32).to_le_bytes());
        v.extend_from_slice(&2835u32.to_le_bytes());
        v.extend_from_slice(&2835u32.to_le_bytes());
        v.extend_from_slice(&[0; 8]);
        v.extend_from_slice(extra);
        v.extend_from_slice(pixels);
        v
    }

    #[test]
    fn bottom_up_24_bit() {
        // 2x2: rows bottom-up, padded to 4 bytes.
        let px = [1, 2, 3, 4, 5, 6, 0, 0, 7, 8, 9, 10, 11, 12, 0, 0];
        let o = open(&bmp(24, 2, 2, 0, &[], &px), Limit(None)).unwrap();
        assert_eq!(o.image.mode, Mode::RGB);
        assert_eq!(o.image.data, vec![9, 8, 7, 12, 11, 10, 3, 2, 1, 6, 5, 4]);
        assert_eq!(o.info.dpi, Some((2835.0 / 39.3701, 2835.0 / 39.3701)));
    }

    #[test]
    fn rle8_with_eol() {
        let pal: Vec<u8> = (0..256u32).flat_map(|i| [i as u8, 0, 0, 0]).collect();
        let px = [2, 7, 0, 0, 1, 9, 0, 0, 0, 1];
        let o = open(&bmp(8, 2, 2, 1, &pal, &px), Limit(None)).unwrap();
        assert_eq!(o.image.mode, Mode::P);
        assert_eq!(o.image.data, vec![9, 0, 7, 7]);
    }
}
