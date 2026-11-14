//! PSD as Pillow 12.3.0 opens and loads it (PsdImagePlugin): the merged
//! composite of the image data section, which is what tmiki read through
//! Pillow before. Layers are not read (a future plugin's).
//!
//! - Version 1 files; the colour modes and depths of Pillow's `MODES`:
//!   bitmap (1 bit, Pillow's "1" unpacking: a set bit is white), grey,
//!   indexed, RGB (RGBA with exactly four channels), CMYK (planes stored
//!   inverted), multichannel and duotone (as grey), all 8 bits. 16-bit
//!   files are not in `MODES`: Pillow refuses them, and so does this.
//! - Raw or PackBits planes, one tile per channel as `_maketile` lays them
//!   out (the PackBits offsets from the row byte counts of the channels
//!   Pillow uses, even where the file has more), decoded by Pillow's raw
//!   decoder or PackDecode.c (runs cut at the row's end), reading on past
//!   a plane's end as ImageFile.load does.
//! - An indexed image takes its palette only from a 768-byte colour mode
//!   section (else the core image's black palette).
//! - `info["icc_profile"]` is image resource 1039 (the last one); the
//!   resources are walked as Pillow walks them. No dpi.
//!
//! Declined: LAB (Pillow has no conversion to RGBA for it).

use tmi_pillow::image::{Mode, Palette};

use crate::unpack::{self, ImBuf, RawError, Unpacker};
use crate::{Cursor, DecodeError, Format, Info, Limit, Opened};

/// `_accept`.
pub fn accept(data: &[u8]) -> bool {
    data.starts_with(b"8BPS")
}

fn corrupt(m: &str) -> DecodeError {
    DecodeError::Corrupt(Format::Psd, m.to_string())
}

fn i16(b: &[u8], at: usize) -> Option<u64> {
    Some(u16::from_be_bytes([*b.get(at)?, *b.get(at + 1)?]) as u64)
}

fn i32(b: &[u8], at: usize) -> Option<u64> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?) as u64)
}

enum Tiles {
    Raw,
    PackBits,
    /// Another compression: no tiles ("cannot load this image").
    None,
}

struct Header {
    mode: Mode,
    channels: usize,
    width: u64,
    height: u64,
    palette: Option<Palette>,
    icc: Option<Vec<u8>>,
    tiles: Tiles,
    /// The channel planes' offsets, in channel order.
    offsets: Vec<u64>,
}

fn header(data: &[u8]) -> Result<Header, DecodeError> {
    let mut r = Cursor::new(data);
    let s = r.read(26);
    if !accept(s) || i16(s, 4) != Some(1) {
        return Err(corrupt("not a PSD file"));
    }
    if s.len() < 26 {
        return Err(corrupt("short header"));
    }
    let (bits, psd_channels, psd_mode) = (i16(s, 22).unwrap(), i16(s, 12).unwrap() as usize, i16(s, 24).unwrap());
    let (mut mode, mut channels) = match (psd_mode, bits) {
        (0, 1) => (Mode::One, 1),
        (0, 8) | (1, 8) | (7, 8) | (8, 8) => (Mode::L, 1),
        (2, 8) => (Mode::P, 1),
        (3, 8) => (Mode::RGB, 3),
        (4, 8) => (Mode::CMYK, 4),
        (9, 8) => return Err(DecodeError::Declined("PSD: LAB")),
        _ => return Err(corrupt("unsupported mode or depth")),
    };
    if channels > psd_channels {
        return Err(corrupt("not enough channels"));
    }
    if mode == Mode::RGB && psd_channels == 4 {
        mode = Mode::RGBA;
        channels = 4;
    }
    let (width, height) = (i32(s, 18).unwrap(), i32(s, 14).unwrap());
    let short = || corrupt("short section");
    // colour mode data
    let size = i32(r.read(4), 0).ok_or_else(short)?;
    let mut palette = None;
    if size != 0 {
        let d = r.read(size as usize);
        if mode == Mode::P && size == 768 {
            // ImagePalette.raw("RGB;L", data), realized by putpalette.
            let n = d.len() / 3;
            let mut entries = vec![[0u8, 0, 0, 255]; 256];
            for (i, e) in entries.iter_mut().enumerate().take(n) {
                *e = [d[i], d[i + n], d[i + 2 * n], 255];
            }
            palette = Some(Palette { entries });
        }
    }
    // image resources
    let size = i32(r.read(4), 0).ok_or_else(short)?;
    let mut icc = None;
    if size != 0 {
        let end = r.tell() as u64 + size;
        while (r.tell() as u64) < end {
            r.read(4);
            let id = i16(r.read(2), 0).ok_or_else(short)?;
            let n = *r.read(1).first().ok_or_else(short)? as usize;
            let name = r.read(n);
            if name.len() & 1 == 0 {
                r.read(1);
            }
            let len = i32(r.read(4), 0).ok_or_else(short)?;
            let d = r.read(len as usize);
            if d.len() & 1 == 1 {
                r.read(1);
            }
            if id == 1039 {
                icc = Some(d.to_vec());
            }
        }
    }
    // layer and mask information
    let size = i32(r.read(4), 0).ok_or_else(short)?;
    if size != 0 {
        let end = r.tell() + size as usize;
        i32(r.read(4), 0).ok_or_else(short)?;
        r.seek(end);
    }
    // image data: _maketile
    let compression = i16(r.read(2), 0).ok_or_else(short)?;
    let (xsize, ysize) = (width, height);
    let mut offset = r.tell() as u64;
    let mut offsets = Vec::new();
    let tiles = match compression {
        0 => {
            for _ in 0..channels {
                offsets.push(offset);
                offset = offset.saturating_add(xsize * ysize);
            }
            Tiles::Raw
        }
        1 => {
            let counts = r.read(channels * ysize as usize * 2);
            offset = r.tell() as u64;
            let mut i = 0;
            for _ in 0..channels {
                offsets.push(offset);
                for _ in 0..ysize {
                    offset += i16(counts, i).ok_or_else(|| corrupt("short byte counts"))?;
                    i += 2;
                }
            }
            Tiles::PackBits
        }
        _ => Tiles::None,
    };
    Ok(Header { mode, channels, width, height, palette, icc, tiles, offsets })
}

/// The size `Image.open(data).size` reports.
pub fn size(data: &[u8]) -> Result<(u64, u64), DecodeError> {
    let h = header(data)?;
    Ok((h.width, h.height))
}

/// The raw mode of each channel (`mode[channel]`, ";I" for CMYK).
fn layer_mode(mode: Mode, channel: usize) -> &'static str {
    match mode {
        Mode::One => "1",
        Mode::L => "L",
        Mode::P => "P",
        Mode::CMYK => ["C;I", "M;I", "Y;I", "K;I"][channel],
        _ => ["R", "G", "B", "A"][channel],
    }
}

pub(crate) fn open(data: &[u8], limit: Limit) -> Result<Opened, DecodeError> {
    let h = header(data)?;
    if h.width == 0 || h.height == 0 {
        return Err(corrupt("not identified by this driver"));
    }
    let (w, ht) = (h.width as usize, h.height as usize);
    limit.check(w, ht)?;
    if matches!(h.tiles, Tiles::None) {
        return Err(corrupt("cannot load this image"));
    }
    let mut im = ImBuf::new(h.mode, w, ht);
    // ImageFile.load: tiles sorted by offset (stable).
    let mut order: Vec<usize> = (0..h.channels).collect();
    order.sort_by_key(|&c| h.offsets[c]);
    for c in order {
        let un = unpack::find(h.mode, layer_mode(h.mode, c)).ok_or_else(|| corrupt("unknown raw mode"))?;
        let rest = data.get(h.offsets[c] as usize..).unwrap_or(&[]);
        if rest.is_empty() {
            return Err(corrupt("image file is truncated"));
        }
        let res = match h.tiles {
            Tiles::Raw => unpack::raw_decode(&mut im, (0, 0, w, ht), un, 0, 1, rest),
            _ => packbits_rows(&mut im, w, ht, un, rest),
        };
        res.map_err(|_| corrupt("image file is truncated"))?;
    }
    let info = Info { dpi: None, icc_profile: h.icc };
    Ok(Opened { image: im.into_image(h.palette), format: Format::Psd, info })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A PSD of 0xFFFFFFFF x 0xFFFFFFFF opened with no pixel limit
    /// (core.maxImagePixels = 0): refused, not a wrapped buffer size.
    #[test]
    fn a_psd_whose_size_wraps_a_buffer_is_refused_even_without_a_limit() {
        let mut data = b"8BPS\x00\x01\x00\x00\x00\x00\x00\x00\x00\x03".to_vec();
        data.extend_from_slice(&u32::MAX.to_be_bytes());
        data.extend_from_slice(&u32::MAX.to_be_bytes());
        data.extend_from_slice(&[0, 8, 0, 3]);
        data.extend_from_slice(&[0; 12]);
        data.extend_from_slice(&[0, 0, 1, 2, 3, 4]);
        assert!(matches!(open(&data, Limit(None)), Err(DecodeError::TooLarge(..))));
    }
}

/// PackDecode.c over the bytes from the tile's offset on: rows of the
/// unpacker's size, a run or literal past the row's end cut there.
fn packbits_rows(im: &mut ImBuf, xsize: usize, ysize: usize, un: Unpacker, data: &[u8]) -> Result<(), RawError> {
    let bytes = (xsize * un.bits).div_ceil(8);
    let mut row = vec![0u8; bytes];
    let (mut x, mut y, mut p) = (0usize, 0usize, 0usize);
    loop {
        let Some(&b) = data.get(p) else { return Err(RawError::Short) };
        if b & 0x80 != 0 {
            if b == 0x80 {
                p += 1;
                continue;
            }
            let Some(&v) = data.get(p + 1) else { return Err(RawError::Short) };
            for _ in 0..(257 - b as usize) {
                if x >= bytes {
                    break;
                }
                row[x] = v;
                x += 1;
            }
            p += 2;
        } else {
            let n = b as usize + 2;
            if data.len() - p < n {
                return Err(RawError::Short);
            }
            for i in 1..n {
                if x >= bytes {
                    break;
                }
                row[x] = data[p + i];
                x += 1;
            }
            p += n;
        }
        if x >= bytes {
            (un.f)(im.row_from(y, 0), &row, xsize);
            x = 0;
            y += 1;
            if y >= ysize {
                return Ok(());
            }
        }
    }
}
