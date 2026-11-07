//! TIFF (the first image) as Pillow 12.3.0 opens and loads it.
//!
//! Pillow reads the tags itself (`ImageFileDirectory_v2.load`, `_setup`,
//! the `OPEN_INFO` table of modes and raw modes), then decodes in one of two
//! ways, both followed here:
//!
//! - uncompressed: its own raw decoder, one tile per strip / tile offset as
//!   `_setup` lists them (planar images band by band), reading on past a
//!   strip's end as ImageFile.load does;
//! - compressed: libtiff 4.7.1 through TiffDecode.c (`_decodeStrip` /
//!   `_decodeTile`, `_pickUnpackers`, the RGBa fix for planar associated
//!   alpha). The codecs are libtiff's: LZW (tif_lzw.c's decoder transcribed:
//!   the early change, a code not yet in the table is an error, the output
//!   ends when the strip is full), deflate / Adobe deflate (zlib, through
//!   flate2's zlib-rs, called as ZIPDecode calls inflate), PackBits; the
//!   horizontal predictor (8 and 16 bits), byte swapping of 16-bit samples
//!   into the native (little-endian) order Pillow's ";16N" raw modes expect,
//!   and FillOrder 2 reversing the bits of the compressed data.
//!
//! Then `load_end`: the EXIF orientation transposes the image. `info["dpi"]`
//! is X/YResolution by ResolutionUnit (none: as inches, and 1 when the tags
//! are missing; centimetres x 2.54), `info["icc_profile"]` the ICCProfile
//! tag.
//!
//! Declined (Pillow may read them; not reproduced): BigTIFF, compressions
//! other than none / LZW / deflate / PackBits (JPEG, CCITT, LZMA, ZSTD,
//! WebP, ...), YCbCr and LAB, the "I", "F" and "PA" modes and 12-bit grey,
//! old-style LZW, predictors other than none / horizontal on 8 or 16 bits,
//! tags of unexpected types or counts, IFDs that run past the end of the
//! file, and for compressed images anything where libtiff's reading of the
//! directory could part from Pillow's (strip / tile tables of the wrong
//! length, empty or truncated strips, non-standard headers).

use std::collections::HashMap;

use tmi_pillow::image::{Mode, Palette};

use crate::unpack::{self, ImBuf, RawError, Unpacker};
use crate::{DecodeError, Format, Info, Limit, Opened};

/// `_accept` (PREFIXES).
pub fn accept(data: &[u8]) -> bool {
    [b"MM\x00\x2a", b"II\x2a\x00", b"MM\x2a\x00", b"II\x00\x2a", b"MM\x00\x2b", b"II\x2b\x00"]
        .iter()
        .any(|p| data.starts_with(*p))
}

fn corrupt(m: &str) -> DecodeError {
    DecodeError::Corrupt(Format::Tiff, m.to_string())
}

type Res<T> = Result<T, DecodeError>;

// ── the IFD as ImageFileDirectory_v2 reads it ──────────────────────────────

struct Entry {
    typ: u16,
    count: u64,
    data: Vec<u8>,
}

struct Ifd {
    be: bool,
    tags: HashMap<u16, Entry>,
    /// The load stopped early (a truncated entry table or out-of-line value:
    /// Pillow keeps the tags read so far).
    cut: bool,
}

fn unit_size(typ: u16) -> Option<u64> {
    Some(match typ {
        1 | 2 | 6 | 7 => 1,
        3 | 8 => 2,
        4 | 9 | 11 | 13 => 4,
        5 | 10 | 12 | 16 => 8,
        _ => return None,
    })
}

impl Ifd {
    fn u16at(&self, b: &[u8]) -> u64 {
        let v = [b[0], b[1]];
        (if self.be { u16::from_be_bytes(v) } else { u16::from_le_bytes(v) }) as u64
    }

    fn u32at(&self, b: &[u8]) -> u64 {
        let v = [b[0], b[1], b[2], b[3]];
        (if self.be { u32::from_be_bytes(v) } else { u32::from_le_bytes(v) }) as u64
    }

    /// `ImageFileDirectory_v2.load(fp)` at `offset`.
    fn load(data: &[u8], offset: usize, be: bool) -> Ifd {
        let mut ifd = Ifd { be, tags: HashMap::new(), cut: false };
        let Some(head) = data.get(offset..offset.saturating_add(2)) else {
            ifd.cut = true;
            return ifd;
        };
        let count = ifd.u16at(head) as usize;
        for i in 0..count {
            let at = offset + 2 + 12 * i;
            let Some(e) = data.get(at..at + 12) else {
                ifd.cut = true;
                return ifd;
            };
            let tag = ifd.u16at(&e[0..2]) as u16;
            let typ = ifd.u16at(&e[2..4]) as u16;
            let count = ifd.u32at(&e[4..8]);
            let Some(unit) = unit_size(typ) else { continue };
            let size = count * unit;
            let value = if size > 4 {
                let off = ifd.u32at(&e[8..12]) as usize;
                match data.get(off..off.saturating_add(size as usize)) {
                    Some(v) => v.to_vec(),
                    None => {
                        ifd.cut = true;
                        return ifd;
                    }
                }
            } else {
                e[8..8 + size as usize].to_vec()
            };
            if value.is_empty() {
                continue;
            }
            ifd.tags.insert(tag, Entry { typ, count, data: value });
        }
        if data.get(offset + 2 + 12 * count..offset + 6 + 12 * count).is_none() {
            ifd.cut = true;
        }
        ifd
    }

    fn has(&self, tag: u16) -> bool {
        self.tags.contains_key(&tag)
    }

    /// The values of a SHORT / LONG tag (other types: declined).
    fn uints(&self, tag: u16) -> Res<Option<Vec<u64>>> {
        let Some(e) = self.tags.get(&tag) else { return Ok(None) };
        let v = match e.typ {
            3 => e.data.chunks_exact(2).map(|b| self.u16at(b)).collect(),
            4 => e.data.chunks_exact(4).map(|b| self.u32at(b)).collect(),
            _ => return Err(DecodeError::Declined("TIFF: a tag of an unexpected type")),
        };
        Ok(Some(v))
    }

    /// A scalar SHORT / LONG tag (Pillow keeps the first of several values;
    /// declined).
    fn uint(&self, tag: u16) -> Res<Option<u64>> {
        match self.uints(tag)? {
            None => Ok(None),
            Some(v) if v.len() == 1 => Ok(Some(v[0])),
            Some(_) => Err(DecodeError::Declined("TIFF: a tag with several values where one is expected")),
        }
    }

    /// A RATIONAL tag of one value: (numerator, denominator).
    fn rational(&self, tag: u16) -> Res<Option<(u64, u64)>> {
        let Some(e) = self.tags.get(&tag) else { return Ok(None) };
        if e.typ != 5 || e.count != 1 {
            return Err(DecodeError::Declined("TIFF: resolution of an unexpected type"));
        }
        Ok(Some((self.u32at(&e.data[0..4]), self.u32at(&e.data[4..8]))))
    }
}

// ── OPEN_INFO ──────────────────────────────────────────────────────────────

/// `OPEN_INFO[(prefix, photo, sample_format, fillorder, bps, extra)]`:
/// (mode name, raw mode), or None (KeyError).
fn open_info(be: bool, photo: u64, sf: &[u64], fill: u64, bps: &[u64], extra: &[u64]) -> Option<(&'static str, &'static str)> {
    const N: u8 = 0; // both byte orders
    const II: u8 = 1;
    const MM: u8 = 2;
    #[rustfmt::skip]
    const TABLE: &[(u8, u64, &[u64], u64, &[u64], &[u64], &str, &str)] = &[
        (N, 0, &[1], 1, &[1], &[], "1", "1;I"),
        (N, 0, &[1], 2, &[1], &[], "1", "1;IR"),
        (N, 1, &[1], 1, &[1], &[], "1", "1"),
        (N, 1, &[1], 2, &[1], &[], "1", "1;R"),
        (N, 0, &[1], 1, &[2], &[], "L", "L;2I"),
        (N, 0, &[1], 2, &[2], &[], "L", "L;2IR"),
        (N, 1, &[1], 1, &[2], &[], "L", "L;2"),
        (N, 1, &[1], 2, &[2], &[], "L", "L;2R"),
        (N, 0, &[1], 1, &[4], &[], "L", "L;4I"),
        (N, 0, &[1], 2, &[4], &[], "L", "L;4IR"),
        (N, 1, &[1], 1, &[4], &[], "L", "L;4"),
        (N, 1, &[1], 2, &[4], &[], "L", "L;4R"),
        (N, 0, &[1], 1, &[8], &[], "L", "L;I"),
        (N, 0, &[1], 2, &[8], &[], "L", "L;IR"),
        (N, 1, &[1], 1, &[8], &[], "L", "L"),
        (N, 1, &[2], 1, &[8], &[], "L", "L"),
        (N, 1, &[1], 2, &[8], &[], "L", "L;R"),
        (II, 1, &[1], 1, &[12], &[], "I;16", "I;12"),
        (II, 0, &[1], 1, &[16], &[], "I;16", "I;16"),
        (II, 1, &[1], 1, &[16], &[], "I;16", "I;16"),
        (MM, 1, &[1], 1, &[16], &[], "I;16B", "I;16B"),
        (II, 1, &[1], 2, &[16], &[], "I;16", "I;16R"),
        (II, 1, &[2], 1, &[16], &[], "I", "I;16S"),
        (MM, 1, &[2], 1, &[16], &[], "I", "I;16BS"),
        (II, 0, &[3], 1, &[32], &[], "F", "F;32F"),
        (MM, 0, &[3], 1, &[32], &[], "F", "F;32BF"),
        (II, 1, &[1], 1, &[32], &[], "I", "I;32N"),
        (II, 1, &[2], 1, &[32], &[], "I", "I;32S"),
        (MM, 1, &[2], 1, &[32], &[], "I", "I;32BS"),
        (II, 1, &[3], 1, &[32], &[], "F", "F;32F"),
        (MM, 1, &[3], 1, &[32], &[], "F", "F;32BF"),
        (N, 1, &[1], 1, &[8, 8], &[2], "LA", "LA"),
        (N, 2, &[1], 1, &[8, 8, 8], &[], "RGB", "RGB"),
        (N, 2, &[1], 2, &[8, 8, 8], &[], "RGB", "RGB;R"),
        (N, 2, &[1], 1, &[8, 8, 8, 8], &[], "RGBA", "RGBA"),
        (N, 2, &[1], 1, &[8, 8, 8, 8], &[0], "RGB", "RGBX"),
        (N, 2, &[1], 1, &[8, 8, 8, 8, 8], &[0, 0], "RGB", "RGBXX"),
        (N, 2, &[1], 1, &[8, 8, 8, 8, 8, 8], &[0, 0, 0], "RGB", "RGBXXX"),
        (N, 2, &[1], 1, &[8, 8, 8, 8], &[1], "RGBA", "RGBa"),
        (N, 2, &[1], 1, &[8, 8, 8, 8, 8], &[1, 0], "RGBA", "RGBaX"),
        (N, 2, &[1], 1, &[8, 8, 8, 8, 8, 8], &[1, 0, 0], "RGBA", "RGBaXX"),
        (N, 2, &[1], 1, &[8, 8, 8, 8], &[2], "RGBA", "RGBA"),
        (N, 2, &[1], 1, &[8, 8, 8, 8, 8], &[2, 0], "RGBA", "RGBAX"),
        (N, 2, &[1], 1, &[8, 8, 8, 8, 8, 8], &[2, 0, 0], "RGBA", "RGBAXX"),
        (N, 2, &[1], 1, &[8, 8, 8, 8], &[999], "RGBA", "RGBA"),
        (II, 2, &[1], 1, &[16, 16, 16], &[], "RGB", "RGB;16L"),
        (MM, 2, &[1], 1, &[16, 16, 16], &[], "RGB", "RGB;16B"),
        (II, 2, &[1], 1, &[16, 16, 16, 16], &[], "RGBA", "RGBA;16L"),
        (MM, 2, &[1], 1, &[16, 16, 16, 16], &[], "RGBA", "RGBA;16B"),
        (II, 2, &[1], 1, &[16, 16, 16, 16], &[0], "RGB", "RGBX;16L"),
        (MM, 2, &[1], 1, &[16, 16, 16, 16], &[0], "RGB", "RGBX;16B"),
        (II, 2, &[1], 1, &[16, 16, 16, 16], &[1], "RGBA", "RGBa;16L"),
        (MM, 2, &[1], 1, &[16, 16, 16, 16], &[1], "RGBA", "RGBa;16B"),
        (II, 2, &[1], 1, &[16, 16, 16, 16], &[2], "RGBA", "RGBA;16L"),
        (MM, 2, &[1], 1, &[16, 16, 16, 16], &[2], "RGBA", "RGBA;16B"),
        (N, 3, &[1], 1, &[1], &[], "P", "P;1"),
        (N, 3, &[1], 2, &[1], &[], "P", "P;1R"),
        (N, 3, &[1], 1, &[2], &[], "P", "P;2"),
        (N, 3, &[1], 2, &[2], &[], "P", "P;2R"),
        (N, 3, &[1], 1, &[4], &[], "P", "P;4"),
        (N, 3, &[1], 2, &[4], &[], "P", "P;4R"),
        (N, 3, &[1], 1, &[8], &[], "P", "P"),
        (N, 3, &[1], 1, &[8, 8], &[0], "P", "PX"),
        (N, 3, &[1], 1, &[8, 8], &[2], "PA", "PA"),
        (N, 3, &[1], 2, &[8], &[], "P", "P;R"),
        (N, 5, &[1], 1, &[8, 8, 8, 8], &[], "CMYK", "CMYK"),
        (N, 5, &[1], 1, &[8, 8, 8, 8, 8], &[0], "CMYK", "CMYKX"),
        (N, 5, &[1], 1, &[8, 8, 8, 8, 8, 8], &[0, 0], "CMYK", "CMYKXX"),
        (II, 5, &[1], 1, &[16, 16, 16, 16], &[], "CMYK", "CMYK;16L"),
        (MM, 5, &[1], 1, &[16, 16, 16, 16], &[], "CMYK", "CMYK;16B"),
        (N, 6, &[1], 1, &[8], &[], "L", "L"),
        (N, 6, &[1], 1, &[8, 8, 8], &[], "RGB", "RGBX"),
        (N, 8, &[1], 1, &[8, 8, 8], &[], "LAB", "LAB"),
    ];
    let order = if be { MM } else { II };
    TABLE
        .iter()
        .find(|t| (t.0 == N || t.0 == order) && t.1 == photo && t.2 == sf && t.3 == fill && t.4 == bps && t.5 == extra)
        .map(|t| (t.6, t.7))
}

// ── _setup ─────────────────────────────────────────────────────────────────

/// `MAX_SAMPLESPERPIXEL`.
const MAX_SPP: u64 = 6;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Codec {
    Raw,
    Lzw,
    Deflate,
    PackBits,
}

struct Setup {
    be: bool,
    /// The header is one of the invalid-but-accepted ones.
    odd_header: bool,
    mode: Mode,
    rawmode: &'static str,
    width: u64,
    height: u64,
    orientation: Option<u64>,
    codec: Codec,
    planar: u64,
    photo: u64,
    fillorder: u64,
    bps: Vec<u64>,
    extra: Vec<u64>,
    spp: u64,
    bps_count: u64,
    palette: Option<Palette>,
    dpi: Option<(f64, f64)>,
    icc: Option<Vec<u8>>,
}

fn mode_of(name: &str) -> Res<Mode> {
    Ok(match name {
        "1" => Mode::One,
        "L" => Mode::L,
        "LA" => Mode::LA,
        "P" => Mode::P,
        "RGB" => Mode::RGB,
        "RGBA" => Mode::RGBA,
        "CMYK" => Mode::CMYK,
        "I;16" => Mode::I16,
        "I;16B" => Mode::I16B,
        _ => return Err(DecodeError::Declined("TIFF: an I, F, PA or LAB image")),
    })
}

fn setup(data: &[u8]) -> Res<(Setup, Ifd)> {
    if data.len() < 8 || !accept(data) {
        return Err(corrupt("not a TIFF file"));
    }
    if data[2] == 43 || data[3] == 43 {
        return Err(DecodeError::Declined("TIFF: BigTIFF"));
    }
    let be = data.starts_with(b"MM");
    let odd_header = !(data.starts_with(b"MM\x00\x2a") || data.starts_with(b"II\x2a\x00"));
    let first = if be { u32::from_be_bytes(data[4..8].try_into().unwrap()) } else { u32::from_le_bytes(data[4..8].try_into().unwrap()) } as usize;
    if first == 0 {
        return Err(corrupt("no more images in TIFF file"));
    }
    let ifd = Ifd::load(data, first, be);
    if ifd.cut {
        return Err(DecodeError::Declined("TIFF: the directory runs past the end of the file"));
    }
    if ifd.has(0xBC01) {
        return Err(corrupt("Windows Media Photo files not yet supported"));
    }
    let compression = ifd.uint(259)?.unwrap_or(1);
    let codec = match compression {
        1 => Codec::Raw,
        5 => Codec::Lzw,
        8 | 32946 => Codec::Deflate,
        32773 => Codec::PackBits,
        2 | 3 | 4 | 6 | 7 | 32771 | 32809 | 34676 | 34677 | 34925 | 50000 | 50001 => {
            return Err(DecodeError::Declined("TIFF: a compression tmi_image does not decode"))
        }
        _ => return Err(corrupt("unknown compression")),
    };
    let planar = ifd.uint(284)?.unwrap_or(1);
    let photo = ifd.uint(262)?.unwrap_or(0);
    let fillorder = ifd.uint(266)?.unwrap_or(1);
    let (Some(width), Some(height)) = (ifd.uint(256)?, ifd.uint(257)?) else {
        return Err(corrupt("Missing dimensions"));
    };
    let orientation = ifd.uint(274)?;
    let mut sf = ifd.uints(339)?.unwrap_or_else(|| vec![1]);
    if sf.len() > 1 && sf.iter().max() == sf.iter().min() {
        sf.truncate(1);
    }
    let mut bps = ifd.uints(258)?.unwrap_or_else(|| vec![1]);
    let mut extra = ifd.uints(338)?.unwrap_or_default();
    let mut spp = ifd.uint(277)?.unwrap_or(1) as i64;
    let mut bps_count: u64 = match photo {
        2 | 6 | 8 => 3,
        5 => 4,
        _ => 1,
    };
    if planar == 2 && !extra.is_empty() && extra.iter().max() == Some(&0) {
        // bps_tuple[:-len(extra)]
        let keep = bps.len().saturating_sub(extra.len());
        bps.truncate(keep);
        spp -= extra.len() as i64;
        extra.clear();
    }
    bps_count += extra.len() as u64;
    if spp > MAX_SPP as i64 {
        return Err(corrupt("Invalid value for samples per pixel"));
    }
    let actual = bps.len() as i64;
    if spp < actual {
        bps.truncate(spp.max(0) as usize);
    } else if spp > actual && actual == 1 {
        bps = vec![bps[0]; spp as usize];
    }
    if bps.len() as i64 != spp {
        return Err(corrupt("unknown data organization"));
    }
    if photo == 6 || photo == 8 {
        return Err(DecodeError::Declined("TIFF: YCbCr or LAB"));
    }
    let Some((mode_name, rawmode)) = open_info(be, photo, &sf, fillorder, &bps, &extra) else {
        return Err(corrupt("unknown pixel mode"));
    };
    let mode = mode_of(mode_name)?;
    if rawmode == "I;12" {
        return Err(DecodeError::Declined("TIFF: 12-bit grey"));
    }

    // info["dpi"]
    let xres = ifd.rational(282)?;
    let yres = ifd.rational(283)?;
    let as_float = |r: Option<(u64, u64)>| -> Res<f64> {
        match r {
            None => Ok(1.0),
            Some((_, 0)) => Err(DecodeError::Declined("TIFF: a resolution with denominator 0")),
            Some((n, d)) => Ok(n as f64 / d as f64),
        }
    };
    let (x, y) = (as_float(xres)?, as_float(yres)?);
    let mut dpi = None;
    if x != 0.0 && y != 0.0 {
        match ifd.uint(296)? {
            Some(2) | None => dpi = Some((x, y)),
            Some(3) => dpi = Some((x * 2.54, y * 2.54)),
            Some(_) => {}
        }
    }
    let icc = match ifd.tags.get(&34675) {
        None => None,
        Some(e) if e.typ == 7 || e.typ == 1 => Some(e.data.clone()),
        Some(_) => return Err(DecodeError::Declined("TIFF: an ICC profile of an unexpected type")),
    };
    let palette = if mode == Mode::P {
        let Some(map) = ifd.uints(320)? else {
            return Err(corrupt("missing colormap"));
        };
        // ImagePalette.raw("RGB;L", bytes(b // 256 ...)) realized by putpalette
        let bytes: Vec<u8> = map.iter().map(|&v| ((v / 256) & 255) as u8).collect();
        let n = bytes.len() / 3;
        if n > 256 {
            return Err(corrupt("invalid palette size"));
        }
        let mut entries = vec![[0u8, 0, 0, 255]; 256];
        for (i, e) in entries.iter_mut().enumerate().take(n) {
            *e = [bytes[i], bytes[i + n], bytes[i + 2 * n], 255];
        }
        Some(Palette { entries })
    } else {
        None
    };
    let s = Setup {
        be,
        odd_header,
        mode,
        rawmode,
        width,
        height,
        orientation,
        codec,
        planar,
        photo,
        fillorder,
        bps,
        extra,
        spp: spp as u64,
        bps_count,
        palette,
        dpi,
        icc,
    };
    Ok((s, ifd))
}

/// The size `Image.open(data).size` reports.
pub fn size(data: &[u8]) -> Result<(u64, u64), DecodeError> {
    let (s, _) = setup(data)?;
    Ok(if matches!(s.orientation, Some(5..=8)) { (s.height, s.width) } else { (s.width, s.height) })
}

pub(crate) fn open(data: &[u8], limit: Limit) -> Result<Opened, DecodeError> {
    let (s, ifd) = setup(data)?;
    if s.width == 0 || s.height == 0 {
        return Err(corrupt("not identified by this driver"));
    }
    let (w, h) = (s.width as usize, s.height as usize);
    limit.check(w, h)?;
    let mut im = ImBuf::new(s.mode, w, h);
    match s.codec {
        Codec::Raw => load_raw(data, &s, &ifd, &mut im)?,
        _ => load_libtiff(data, &s, &ifd, &mut im)?,
    }
    // load_end: ImageOps.exif_transpose
    let im = match s.orientation {
        Some(o) => im.orient(o),
        None => im,
    };
    let info = Info { dpi: s.dpi, icc_profile: s.icc.clone() };
    Ok(Opened { image: im.into_image(s.palette.clone()), format: Format::Tiff, info })
}

// ── uncompressed: Pillow's tiles and raw decoder ───────────────────────────

fn load_raw(data: &[u8], s: &Setup, ifd: &Ifd, im: &mut ImBuf) -> Res<()> {
    let (xsize, ysize) = (s.width, s.height);
    let (offsets, w, h) = if ifd.has(273) {
        let offsets = ifd.uints(273)?.unwrap();
        (offsets, xsize, ifd.uint(278)?.unwrap_or(ysize))
    } else if ifd.has(324) {
        let offsets = ifd.uints(324)?.unwrap();
        let (Some(tw), Some(th)) = (ifd.uint(322)?, ifd.uint(323)?) else {
            return Err(corrupt("Invalid tile dimensions"));
        };
        (offsets, tw, th)
    } else {
        return Err(corrupt("unknown data organization"));
    };
    let offsets: Vec<u64> = if w == xsize && h == ysize && s.planar != 2 { offsets[offsets.len() - 1..].to_vec() } else { offsets };
    let bits_sum: u64 = s.bps.iter().sum();
    // (extents, offset, rawmode, stride)
    let mut tiles: Vec<((u64, u64, u64, u64), u64, String, u64)> = Vec::new();
    let (mut x, mut y, mut layer) = (0u64, 0u64, 0usize);
    for &offset in &offsets {
        let mut stride = if x + w > xsize { (w * bits_sum) as f64 / 8.0 } else { 0.0 };
        let mut rawmode = s.rawmode.to_string();
        if s.planar == 2 {
            let Some(c) = s.rawmode.get(layer..layer + 1) else {
                return Err(corrupt("string index out of range"));
            };
            rawmode = c.to_string();
            stride /= s.bps_count as f64;
        }
        tiles.push(((x, y, (x + w).min(xsize), (y + h).min(ysize)), offset, rawmode, stride as u64));
        x += w;
        if x >= xsize {
            x = 0;
            y += h;
            if y >= ysize {
                y = 0;
                layer += 1;
            }
        }
    }
    // ImageFile.load: sorted by offset (stable), consecutive duplicates
    // that only differ by their offset keep the last one.
    tiles.sort_by_key(|t| t.1);
    let mut kept: Vec<&((u64, u64, u64, u64), u64, String, u64)> = Vec::new();
    for t in &tiles {
        if let Some(last) = kept.last() {
            if last.0 == t.0 && last.2 == t.2 && last.3 == t.3 {
                kept.pop();
            }
        }
        kept.push(t);
    }
    for (ext, offset, rawmode, stride) in kept {
        let (x0, y0, x1, y1) = *ext;
        if x1 <= x0 || y1 <= y0 {
            return Err(corrupt("tile cannot extend outside image"));
        }
        let Some(un) = unpack::find(s.mode, rawmode) else {
            return Err(DecodeError::Declined("TIFF: a raw mode tmi_image does not unpack"));
        };
        let rest = data.get(*offset as usize..).unwrap_or(&[]);
        if rest.is_empty() {
            return Err(corrupt("image file is truncated"));
        }
        let ext = (x0 as usize, y0 as usize, x1 as usize, y1 as usize);
        unpack::raw_decode(im, ext, un, *stride as usize, 1, rest).map_err(|e| match e {
            RawError::Config => corrupt("bad configuration"),
            RawError::Short => corrupt("image file is truncated"),
        })?;
    }
    Ok(())
}

// ── compressed: TiffDecode.c over libtiff ──────────────────────────────────

/// `TIFFReadEncodedStrip` / `TIFFReadEncodedTile` of one strile into
/// `out` (exactly `out.len()` bytes): fill, bit reversal, decode,
/// predictor, byte swap.
struct Strile<'a> {
    data: &'a [u8],
    s: &'a Setup,
    codec: Codec,
    predictor: u64,
    /// The predictor's stride (samples) and row size (bytes).
    stride: usize,
    rowsize: usize,
    bits: u64,
    swab16: bool,
}

impl Strile<'_> {
    fn read(&self, offset: u64, count: u64, out: &mut [u8]) -> Res<()> {
        let size = self.data.len() as u64;
        if count == 0 || offset == 0 {
            // libtiff may estimate the byte count of a single strip.
            return Err(DecodeError::Declined("TIFF: an empty strip or tile"));
        }
        if count > size || offset > size - count {
            return Err(corrupt("read error on strip"));
        }
        let raw = &self.data[offset as usize..(offset + count) as usize];
        let reversed: Vec<u8>;
        let raw = if self.s.fillorder == 2 {
            reversed = raw.iter().map(|b| b.reverse_bits()).collect();
            &reversed[..]
        } else {
            raw
        };
        match self.codec {
            Codec::Lzw => lzw_decode(raw, out)?,
            Codec::Deflate => zip_decode(raw, out)?,
            Codec::PackBits => packbits_decode(raw, out)?,
            Codec::Raw => unreachable!(),
        }
        let predicts = self.predictor == 2 && matches!(self.codec, Codec::Lzw | Codec::Deflate);
        if self.swab16 && self.bits == 16 {
            for p in out.chunks_exact_mut(2) {
                p.swap(0, 1);
            }
        }
        if predicts {
            if out.len() % self.rowsize != 0 {
                return Err(corrupt("occ0%rowsize != 0"));
            }
            for row in out.chunks_exact_mut(self.rowsize) {
                if self.bits == 8 {
                    for i in self.stride..row.len() {
                        row[i] = row[i].wrapping_add(row[i - self.stride]);
                    }
                } else {
                    let n = row.len() / 2;
                    for i in self.stride..n {
                        let prev = u16::from_le_bytes([row[2 * (i - self.stride)], row[2 * (i - self.stride) + 1]]);
                        let cur = u16::from_le_bytes([row[2 * i], row[2 * i + 1]]);
                        row[2 * i..2 * i + 2].copy_from_slice(&cur.wrapping_add(prev).to_le_bytes());
                    }
                }
            }
        }
        Ok(())
    }
}

fn load_libtiff(data: &[u8], s: &Setup, ifd: &Ifd, im: &mut ImBuf) -> Res<()> {
    let decline = |m: &'static str| DecodeError::Declined(m);
    if s.odd_header {
        return Err(decline("TIFF: a compressed image under a non-standard header"));
    }
    // What libtiff reads from the directory, where it must agree with Pillow.
    let bps0 = s.bps[0];
    let spp_tag = ifd.uint(277)?.unwrap_or(1);
    let bps_tag = ifd.uints(258)?.unwrap_or_else(|| vec![1]);
    if s.bps.iter().any(|&b| b != bps0)
        || bps_tag.iter().any(|&b| b != bps0)
        || !(bps_tag.len() == 1 || bps_tag.len() as u64 == spp_tag)
    {
        return Err(decline("TIFF: bits per sample libtiff may read differently"));
    }
    if !(s.planar == 1 || s.planar == 2) || s.planar == 1 && s.spp != spp_tag {
        return Err(decline("TIFF: a sample layout libtiff may read differently"));
    }
    if !(s.fillorder == 1 || s.fillorder == 2) {
        return Err(decline("TIFF: fill order"));
    }
    let predictor = ifd.uint(317)?.unwrap_or(1);
    if matches!(s.codec, Codec::Lzw | Codec::Deflate) && !(predictor == 1 || predictor == 2 && (bps0 == 8 || bps0 == 16)) {
        return Err(decline("TIFF: a predictor tmi_image does not undo"));
    }
    // The raw mode TiffImagePlugin passes (fill order 1, native 16 bits).
    let mut rawmode = s.rawmode;
    let mut mode = s.mode;
    if s.fillorder == 2 {
        let sf = ifd.uints(339)?.unwrap_or_else(|| vec![1]);
        let sf = if sf.len() > 1 && sf.iter().max() == sf.iter().min() { vec![sf[0]] } else { sf };
        let (m, r) = open_info(s.be, s.photo, &sf, 1, &s.bps, &s.extra).ok_or_else(|| decline("TIFF: fill order"))?;
        mode = mode_of(m)?;
        rawmode = r;
    }
    if mode != s.mode {
        return Err(decline("TIFF: fill order"));
    }
    let native: String = if rawmode == "I;16" {
        "I;16N".into()
    } else if rawmode.ends_with(";16B") || rawmode.ends_with(";16L") {
        format!("{}N", &rawmode[..rawmode.len() - 1])
    } else {
        rawmode.to_string()
    };
    let Some(shuffle) = unpack::find(mode, &native) else {
        return Err(decline("TIFF: a raw mode tmi_image does not unpack"));
    };
    let (xsize, ysize) = (s.width, s.height);
    let separate = s.planar == 2;
    let bands = match mode {
        Mode::One | Mode::L | Mode::P | Mode::I16 | Mode::I16B => 1,
        Mode::LA | Mode::La => 2,
        Mode::RGB => 3,
        _ => 4,
    };
    // _pickUnpackers
    let planes = if separate && bands > 1 {
        if !(bps0 == 8 || bps0 == 16) {
            return Err(corrupt("planar bits per sample"));
        }
        bands
    } else {
        1
    };
    let unpackers: Vec<Unpacker> = if planes > 1 {
        (0..planes).map(|b| Unpacker { bits: bps0 as usize, f: unpack::band(b, (bps0 / 8) as usize, false) }).collect()
    } else {
        vec![shuffle]
    };
    let samples_per_row = if separate { 1 } else { spp_tag };
    let strile = |rowsize: usize| Strile {
        data,
        s,
        codec: s.codec,
        predictor,
        stride: samples_per_row as usize,
        rowsize,
        bits: bps0,
        swab16: s.be,
    };
    let state_bits = shuffle.bits as u64;
    if ifd.has(324) || ifd.has(322) {
        // _decodeTile
        let (Some(tw), Some(tl)) = (ifd.uint(322)?, ifd.uint(323)?) else {
            return Err(decline("TIFF: tile dimensions"));
        };
        if tw == 0 || tl == 0 || tw > 1 << 16 || tl > 1 << 16 {
            return Err(decline("TIFF: zero tile size"));
        }
        let offsets = ifd.uints(324)?.ok_or_else(|| decline("TIFF: tile offsets"))?;
        let counts = ifd.uints(325)?.ok_or_else(|| decline("TIFF: tile byte counts"))?;
        let across = xsize.div_ceil(tw);
        let down = ysize.div_ceil(tl);
        let per_plane = across * down;
        let ntiles = per_plane * if separate { spp_tag } else { 1 };
        if offsets.len() as u64 != ntiles || counts.len() as u64 != ntiles || ifd.has(273) {
            return Err(decline("TIFF: a tile table libtiff may read differently"));
        }
        let row_byte_size = (tw * bps0 * samples_per_row).div_ceil(8);
        let tile_bytes = tl * row_byte_size;
        if tile_bytes == 0 || row_byte_size == 0 || row_byte_size > tile_bytes {
            return Err(corrupt("tile size"));
        }
        if tile_bytes > (tl * state_bits / planes as u64).div_ceil(8) * tw {
            return Err(corrupt("tile size"));
        }
        too_large_counts(&counts, tile_bytes)?;
        let st = strile(row_byte_size as usize);
        let mut buf = vec![0u8; tile_bytes as usize];
        let mut y = 0u64;
        while y < ysize {
            for (plane, un) in unpackers.iter().enumerate() {
                let mut x = 0u64;
                while x < xsize {
                    let tile = (y / tl) * across + x / tw + if separate { plane as u64 * per_plane } else { 0 };
                    st.read(offsets[tile as usize], counts[tile as usize], &mut buf)?;
                    let cw = tw.min(xsize - x) as usize;
                    let cl = tl.min(ysize - y) as usize;
                    for ty in 0..cl {
                        let row = &buf[ty * row_byte_size as usize..];
                        (un.f)(im.row_from(y as usize + ty, x as usize), row, cw);
                    }
                    x += tw;
                }
            }
            y += tl;
        }
    } else {
        // _decodeStrip
        let offsets = ifd.uints(273)?.ok_or_else(|| corrupt("missing strip offsets"))?;
        let counts = ifd.uints(279)?.ok_or_else(|| decline("TIFF: strip byte counts missing"))?;
        let rps_tag = ifd.uint(278)?;
        let rps_dir = rps_tag.unwrap_or(u32::MAX as u64);
        if rps_dir == 0 || rps_dir > i32::MAX as u64 && rps_dir != u32::MAX as u64 {
            return Err(decline("TIFF: zero rows per strip"));
        }
        let per_plane = if rps_dir == u32::MAX as u64 { 1 } else { ysize.div_ceil(rps_dir) };
        let nstrips = per_plane * if separate { spp_tag } else { 1 };
        if offsets.len() as u64 != nstrips || counts.len() as u64 != nstrips {
            return Err(decline("TIFF: a strip table libtiff may read differently"));
        }
        let rows_per_strip = if rps_dir == u32::MAX as u64 { ysize } else { rps_dir };
        let scanline = (xsize * bps0 * samples_per_row).div_ceil(8);
        let strip_rows = rows_per_strip.min(ysize);
        let strip_size = strip_rows * scanline;
        let unpacker_row = (xsize * state_bits / planes as u64).div_ceil(8);
        if strip_size > unpacker_row * rows_per_strip {
            return Err(corrupt("strip size"));
        }
        if scanline == 0 || unpacker_row > scanline {
            return Err(corrupt("scanline size"));
        }
        too_large_counts(&counts, strip_size)?;
        let st = strile(scanline as usize);
        let mut buf = vec![0u8; strip_size as usize];
        let mut y = 0u64;
        while y < ysize {
            for (plane, un) in unpackers.iter().enumerate() {
                let strip = y / rps_dir.min(u32::MAX as u64) + if separate { plane as u64 * per_plane } else { 0 };
                // TIFFReadEncodedStrip decodes the rows this strip has.
                let stripinplane = strip % per_plane;
                let rows = (ysize - stripinplane * strip_rows).min(strip_rows);
                let size = (rows * scanline) as usize;
                st.read(offsets[strip as usize], counts[strip as usize], &mut buf[..size])?;
                for r in 0..rows_per_strip.min(ysize - y) as usize {
                    let row = &buf[r * scanline as usize..];
                    (un.f)(im.row_from(y as usize + r, 0), row, xsize as usize);
                }
            }
            y += rows_per_strip;
        }
    }
    // Planar associated alpha: RGBa to RGBA in place.
    // Without ExtraSamples, libtiff defines the channels past the colour ones
    // as unspecified extra samples.
    let first_extra = if ifd.has(338) { s.extra.first().copied() } else if spp_tag > 3 { Some(0) } else { None };
    if planes > 3 && mode == Mode::RGBA && matches!(first_extra, Some(0) | Some(1)) {
        for px in im.data.chunks_exact_mut(4) {
            let mut out = [0u8; 4];
            unpack::unpack_rgba_premul(&mut out, px, 1);
            px.copy_from_slice(&out);
        }
    }
    Ok(())
}

/// TIFFFillStrip / TIFFFillTile cut byte counts over 1 MiB that are more
/// than 10 times the strile size (with a margin): declined.
fn too_large_counts(counts: &[u64], strile_size: u64) -> Res<()> {
    if counts.iter().any(|&c| c > 1024 * 1024 && (c - 4096) / 10 > strile_size) {
        return Err(DecodeError::Declined("TIFF: a byte count libtiff would cut"));
    }
    Ok(())
}

// ── codecs ─────────────────────────────────────────────────────────────────

/// tif_lzw.c `LZWDecode` for one strile (after `LZWPreDecode`): exactly
/// `out.len()` bytes.
fn lzw_decode(input: &[u8], out: &mut [u8]) -> Res<()> {
    if input.len() >= 2 && input[0] == 0 && input[1] & 1 != 0 {
        return Err(DecodeError::Declined("TIFF: old-style LZW"));
    }
    const CLEAR: usize = 256;
    const EOI: usize = 257;
    const FIRST: i64 = 258;
    const CSIZE: i64 = 4095 + 1024;
    #[derive(Clone, Copy, Default)]
    struct Ent {
        next: i32,
        len: u16,
        first: u8,
        value: u8,
    }
    let mut tab = vec![Ent::default(); CSIZE as usize];
    for (c, e) in tab.iter_mut().enumerate().take(256) {
        *e = Ent { next: -1, len: 1, first: c as u8, value: c as u8 };
    }
    let bad = || corrupt("LZWDecode: using code not yet in table");
    // MSB-first codes; a code that needs bytes past the end is an error.
    struct Bits {
        acc: u64,
        n: u64,
        bp: usize,
    }
    let mut pos = Bits { acc: 0, n: 0, bp: 0 };
    let mut nbits: u64 = 9;
    let read = |b: &mut Bits, nbits: u64| -> Res<usize> {
        while b.n < nbits {
            let Some(&byte) = input.get(b.bp) else {
                return Err(corrupt("LZWDecode: strip not terminated with EOI code"));
            };
            b.acc = (b.acc << 8) | byte as u64;
            b.bp += 1;
            b.n += 8;
        }
        b.n -= nbits;
        let v = (b.acc >> b.n) & ((1 << nbits) - 1);
        b.acc &= (1 << b.n) - 1;
        Ok(v as usize)
    };
    let mut free: i64 = -1;
    let mut maxcode: i64 = 511 - 1;
    let mut old: usize = 0;
    let mut op = 0usize;
    let occ0 = out.len();
    let mut string: Vec<u8> = Vec::new();
    // Add the entry (next = old, value) at `free`; grow the code size.
    let add = |tab: &mut [Ent], free: &mut i64, nbits: &mut u64, maxcode: &mut i64, old: usize, value: u8| {
        let o = tab[old];
        tab[*free as usize] = Ent { next: old as i32, len: o.len + 1, first: o.first, value };
        *free += 1;
        if *free > *maxcode {
            *nbits = (*nbits + 1).min(12);
            *maxcode = (1i64 << *nbits) - 1 - 1;
            if *free >= CSIZE {
                *free = -1;
            }
        }
    };
    while op < occ0 {
        let code = read(&mut pos, nbits)?;
        if code >= FIRST as usize {
            let c = code as i64;
            let value = if c >= free {
                if c != free {
                    return Err(bad());
                }
                tab[old].first
            } else {
                tab[code].first
            };
            add(&mut tab, &mut free, &mut nbits, &mut maxcode, old, value);
            old = code;
            // The string of `code`, cut to what still fits.
            string.clear();
            let mut e = code as i32;
            while e >= 0 {
                string.push(tab[e as usize].value);
                e = tab[e as usize].next;
            }
            string.reverse();
            let n = string.len().min(occ0 - op);
            out[op..op + n].copy_from_slice(&string[..n]);
            op += n;
        } else if code < CLEAR {
            if code as i64 > free {
                return Err(bad());
            }
            add(&mut tab, &mut free, &mut nbits, &mut maxcode, old, code as u8);
            old = code;
            out[op] = code as u8;
            op += 1;
        } else if code == EOI {
            break;
        } else {
            // CLEAR
            free = FIRST;
            nbits = 9;
            maxcode = 511 - 1;
            let mut code = read(&mut pos, nbits)?;
            while code == CLEAR {
                code = read(&mut pos, nbits)?;
            }
            if code == EOI {
                break;
            }
            if code > EOI {
                return Err(bad());
            }
            out[op] = code as u8;
            op += 1;
            old = code;
        }
    }
    if op < occ0 {
        return Err(corrupt("LZWDecode: not enough data"));
    }
    Ok(())
}

/// tif_zip.c `ZIPDecode` (zlib's inflate, called until the strile is full).
fn zip_decode(input: &[u8], out: &mut [u8]) -> Res<()> {
    let mut d = flate2::Decompress::new(true);
    let input = &input[..input.len().min(i32::MAX as usize)];
    let mut occ;
    loop {
        let (in0, out0) = (d.total_in() as usize, d.total_out() as usize);
        let status = d
            .decompress(&input[in0..], &mut out[out0..], flate2::FlushDecompress::None)
            .map_err(|_| corrupt("ZIPDecode: decoding error"))?;
        occ = out.len() - d.total_out() as usize;
        match status {
            flate2::Status::StreamEnd => break,
            flate2::Status::Ok => {}
            flate2::Status::BufError => return Err(corrupt("ZIPDecode: zlib error")),
        }
        if occ == 0 {
            break;
        }
    }
    if occ != 0 {
        return Err(corrupt("ZIPDecode: not enough data"));
    }
    Ok(())
}

/// tif_packbits.c `PackBitsDecode` for one strile.
fn packbits_decode(input: &[u8], out: &mut [u8]) -> Res<()> {
    let (mut bp, mut op) = (0usize, 0usize);
    let mut cc = input.len();
    let mut occ = out.len();
    while cc > 0 && occ > 0 {
        let n = input[bp] as i8 as i64;
        bp += 1;
        cc -= 1;
        if n < 0 {
            if n == -128 {
                continue;
            }
            let mut n = (-n + 1) as usize;
            if occ < n {
                n = occ;
            }
            if cc == 0 {
                break;
            }
            occ -= n;
            let b = input[bp];
            bp += 1;
            cc -= 1;
            out[op..op + n].fill(b);
            op += n;
        } else {
            let mut n = n as usize;
            if occ < n + 1 {
                n = occ - 1;
            }
            if cc < n + 1 {
                break;
            }
            n += 1;
            out[op..op + n].copy_from_slice(&input[bp..bp + n]);
            op += n;
            occ -= n;
            bp += n;
            cc -= n;
        }
    }
    if occ > 0 {
        return Err(corrupt("PackBitsDecode: not enough data"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal TIFF LZW encoder (clear code before every byte: the table
    /// never grows).
    fn lzw_clear_each(bytes: &[u8]) -> Vec<u8> {
        let mut codes: Vec<u16> = Vec::new();
        for &b in bytes {
            codes.push(256);
            codes.push(b as u16);
        }
        codes.push(257);
        let mut out = Vec::new();
        let (mut acc, mut n) = (0u32, 0u32);
        for c in codes {
            acc = (acc << 9) | c as u32;
            n += 9;
            while n >= 8 {
                out.push((acc >> (n - 8)) as u8);
                n -= 8;
            }
        }
        if n > 0 {
            out.push((acc << (8 - n)) as u8);
        }
        out
    }

    #[test]
    fn lzw_and_packbits() {
        let data = [1u8, 2, 3, 250, 0, 7];
        let mut out = [0u8; 6];
        lzw_decode(&lzw_clear_each(&data), &mut out).unwrap();
        assert_eq!(out, data);
        let mut out = [0u8; 7];
        packbits_decode(&[2, 9, 8, 7, 0xFD], &mut out).unwrap_err();
        let mut out = [0u8; 7];
        packbits_decode(&[2, 9, 8, 7, 0xFD, 5], &mut out).unwrap();
        assert_eq!(out, [9, 8, 7, 5, 5, 5, 5]);
    }
}
