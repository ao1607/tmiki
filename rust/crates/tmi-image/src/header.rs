//! The format, size, dpi and ICC presence Pillow 12.3's `info` shows for a
//! PNG or JPEG after `Image.open(data)` and `load()` (raster.py `_header`
//! and the Pillow decode it falls back to, in one), read by Pillow's own
//! rules: PngImagePlugin's chunk handlers (`tmi_image::png_dec::scan` /
//! `load_facts`, APNG frame 0 included), JpegImagePlugin's marker handlers
//! (Photoshop resources, MPF, JFIF, Adobe, ICC), `_read_dpi_from_exif` over
//! the EXIF directory as TiffImagePlugin reads it (leniently: a short
//! directory keeps the tags read before the cut), and `jpeg_factory`'s MPO
//! test. `Err` exactly where Pillow's `open` / `load` raises (or warns, for a
//! malformed MPO); [`crate::import::open_for_import`] then takes the file as
//! not readable by tmi, so it is stored as an opaque blob.

use crate::png_dec;

pub const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

#[derive(Debug, Clone, PartialEq)]
pub struct Header {
    /// "png" or "jpeg".
    pub format: &'static str,
    pub size: (usize, usize),
    /// `float(info["dpi"][0])`, or None.
    pub dpi: Option<f64>,
    /// `"icc_profile" in info`.
    pub icc: bool,
    /// The mode Pillow opens the image in (PNG; None for a JPEG).
    pub mode: Option<&'static str>,
}

type R<T> = Result<T, String>;

fn refused<T>(what: impl std::fmt::Display) -> R<T> {
    Err(format!("Pillow refuses this file ({what})"))
}

fn be16(d: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([d[at], d[at + 1]])
}

fn be32(d: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(d[at..at + 4].try_into().unwrap())
}

/// `Image.open(data).load()`'s header facts: Ok(None) for neither PNG nor
/// JPEG.
pub fn header(data: &[u8]) -> R<Option<Header>> {
    if data.starts_with(PNG_SIGNATURE) {
        return png_header(data, true).map(Some);
    }
    if data.len() >= 3 && data[..3] == [0xFF, 0xD8, 0xFF] {
        return jpeg_header(data).map(Some);
    }
    Ok(None)
}

/// A PNG's facts after `Image.open` (`after_idat` false) or after `load()`
/// too (the chunks after the image data, up to an animation's next frame).
pub fn png_header(data: &[u8], after_idat: bool) -> R<Header> {
    let scan = png_dec::scan(data).or_else(|e| refused(format!("{e:?}")))?;
    let info = if after_idat { png_dec::load_facts(data, &scan).or_else(|e| refused(format!("{e:?}")))? } else { scan.info.clone() };
    Ok(Header {
        format: "png",
        size: (scan.info.size.0 as usize, scan.info.size.1 as usize),
        dpi: info.dpi.map(|d| d.0),
        icc: info.icc_profile.is_some(),
        mode: scan.info.mode,
    })
}

/// A JPEG's facts (`dpi` as `float(info["dpi"][0])`).
pub fn jpeg_header(data: &[u8]) -> R<Header> {
    let info = jpeg_open(data)?;
    if info.malformed_mpo {
        return refused("a malformed MPO: Pillow warns and opens it as a JPEG");
    }
    if info.size.0 == 0 || info.size.1 == 0 {
        return refused("not identified by this driver"); // ImageFile: a size of 0
    }
    // (an infinite XResolution gives inf, which tmi stores as Python does)
    let dpi = info.dpi.map(|(d, _)| d.to_f64());
    Ok(Header { format: "jpeg", size: info.size, dpi, icc: info.icc_profile.is_some(), mode: None })
}

/// A value of `info["dpi"]` as Python holds it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PyNum {
    Int(i128),
    Float(f64),
    /// An IFDRational with a non-zero denominator (num, den).
    Rat(i64, i64),
}

impl PyNum {
    /// `float(x)` (a rational correctly rounded, as `float(Fraction)`).
    pub fn to_f64(self) -> f64 {
        match self {
            PyNum::Int(v) => v as f64,
            PyNum::Float(v) => v,
            // both fit a double exactly (32-bit EXIF values), so the IEEE
            // quotient is the correctly rounded one
            PyNum::Rat(n, d) => n as f64 / d as f64,
        }
    }

    /// `round(x)`: half to even (exactly, for a rational); None where
    /// Python raises (inf, nan).
    pub fn round(self) -> Option<i128> {
        match self {
            PyNum::Int(v) => Some(v),
            PyNum::Float(v) => v.is_finite().then(|| v.round_ties_even() as i128),
            PyNum::Rat(n, d) => {
                let (mut n, mut d) = (n as i128, d as i128);
                if d < 0 {
                    n = -n;
                    d = -d;
                }
                let (q, r) = (n.div_euclid(d), n.rem_euclid(d));
                Some(match (2 * r).cmp(&d) {
                    std::cmp::Ordering::Less => q,
                    std::cmp::Ordering::Greater => q + 1,
                    std::cmp::Ordering::Equal => q + (q & 1),
                })
            }
        }
    }
}

/// What `Image.open(jpeg)` reads from the markers before the scan, as tmi
/// uses it (import and write-back).
#[derive(Clone, Debug, Default)]
pub struct JpegInfo {
    pub size: (usize, usize),
    /// "L", "RGB" or "CMYK".
    pub mode: &'static str,
    /// The last SOF's component count (`layers`).
    pub layer_count: u8,
    /// `layer`: (id, h, v, table) of every SOF's components, in order.
    pub layers: Vec<(u8, u8, u8, u8)>,
    pub progressive: bool,
    /// `info["dpi"]`.
    pub dpi: Option<(PyNum, PyNum)>,
    /// `info["icc_profile"]` (Some(None): the wrong number of parts).
    pub icc_profile: Option<Option<Vec<u8>>>,
    pub exif: Option<Vec<u8>>,
    pub xmp: Option<Vec<u8>>,
    pub comment: Option<Vec<u8>>,
    pub adobe_transform: Option<u8>,
    /// `Image.open(data).format == "MPO"` (`jpeg_factory` turned it into an
    /// MpoImageFile; its frame 0 is the JPEG itself).
    pub mpo: bool,
    /// `jpeg_factory` warned "Image appears to be a malformed MPO file": the
    /// facts are right, but Python prints a warning that tmi does not, so
    /// the import hands such a file to Python.
    pub malformed_mpo: bool,
}

const SOF: [u8; 14] = [0xC0, 0xC1, 0xC2, 0xC3, 0xC5, 0xC6, 0xC7, 0xC9, 0xCA, 0xCB, 0xCD, 0xCE, 0xCF, 0xDE];
const SKIP: [u8; 6] = [0xC4, 0xCC, 0xDA, 0xDC, 0xDD, 0xDF];

fn no_handler(low: u8) -> bool {
    low == 0xC8 || (0xD0..0xDA).contains(&low) || (0xF0..0xFE).contains(&low)
}

/// `Image.open(jpeg)`: JpegImageFile._open's marker walk and handlers,
/// `_read_dpi_from_exif`, then `jpeg_factory` (MPO or JPEG). Err where
/// Pillow fails to open the file, except for a size of 0 (ImageFile's check:
/// the caller's; tmi.raster read such a header for the write-back).
pub fn jpeg_open(data: &[u8]) -> R<JpegInfo> {
    let n = data.len();
    let mut info = JpegInfo::default();
    let mut pos = 3usize;
    let mut marker_byte: Option<u8> = Some(0xFF);
    let mut icclist: Vec<&[u8]> = Vec::new();
    let mut have_size = false;
    let mut app1: Vec<&[u8]> = Vec::new();
    let mut mp: Option<&[u8]> = None;
    loop {
        let mb = match marker_byte {
            Some(b) => b,
            None => {
                if pos >= n {
                    return refused("no scan: IndexError");
                }
                pos += 1;
                data[pos - 1]
            }
        };
        if mb != 0xFF {
            marker_byte = None; // skip non-0xFF junk
            continue;
        }
        if pos >= n {
            return refused("no scan: struct.error");
        }
        let low = data[pos];
        pos += 1;
        if low == 0xFF {
            marker_byte = Some(0xFF);
            continue;
        }
        if low == 0x00 || no_handler(low) {
            marker_byte = None;
            continue;
        }
        if !(SOF.contains(&low) || SKIP.contains(&low) || low == 0xDB || low == 0xFE || (0xE0..=0xEF).contains(&low)) {
            return refused("no marker found");
        }
        if pos + 2 > n {
            return refused("marker length: struct.error");
        }
        let length = be16(data, pos) as i64 - 2;
        pos += 2;
        let seg_size = length.max(0) as usize;
        if pos + seg_size > n {
            return refused("Truncated File Read");
        }
        let s = &data[pos..pos + seg_size];
        pos += seg_size;
        if (0xE0..=0xEF).contains(&low) {
            if low == 0xE1 {
                app1.push(s);
            }
            if low == 0xE0 && s.starts_with(b"JFIF") {
                if s.len() < 7 {
                    return refused("JFIF: struct.error");
                }
                if s.len() >= 12 {
                    let unit = s[7];
                    let (dx, dy) = (be16(s, 8) as i128, be16(s, 10) as i128);
                    if unit == 1 {
                        info.dpi = Some((PyNum::Int(dx), PyNum::Int(dy)));
                    } else if unit == 2 {
                        info.dpi = Some((PyNum::Float(dx as f64 * 2.54), PyNum::Float(dy as f64 * 2.54)));
                    }
                }
            } else if low == 0xE1 && s.starts_with(b"Exif\0\0") {
                match &mut info.exif {
                    Some(e) => e.extend_from_slice(&s[6..]),
                    None => info.exif = Some(s.to_vec()),
                }
            } else if low == 0xE1 && s.starts_with(b"http://ns.adobe.com/xap/1.0/\x00") {
                info.xmp = Some(s.splitn(2, |&b| b == 0).nth(1).unwrap_or(&[]).to_vec());
            } else if low == 0xE2 && s.starts_with(b"FPXR\0") {
                // info["flashpix"]
            } else if low == 0xE2 && s.starts_with(b"ICC_PROFILE\0") {
                icclist.push(s);
            } else if low == 0xED && s.starts_with(b"Photoshop 3.0\x00") {
                photoshop_resources(s)?;
            } else if low == 0xEE && s.starts_with(b"Adobe") {
                if s.len() < 7 {
                    return refused("Adobe: struct.error");
                }
                if s.len() > 11 {
                    info.adobe_transform = Some(s[11]);
                }
            } else if low == 0xE2 && s.starts_with(b"MPF\0") {
                mp = Some(&s[4..]);
            }
        } else if low == 0xFE {
            info.comment = Some(s.to_vec());
        } else if SOF.contains(&low) {
            if s.len() < 5 {
                return refused("SOF: struct.error");
            }
            info.size = (be16(s, 3) as usize, be16(s, 1) as usize);
            have_size = true;
            if s[0] != 8 {
                return refused(format!("cannot handle {}-bit layers", s[0]));
            }
            let Some(&count) = s.get(5) else { return refused("SOF: IndexError") };
            info.mode = match count {
                1 => "L",
                3 => "RGB",
                4 => "CMYK",
                c => return refused(format!("cannot handle {c}-layer images")),
            };
            info.layer_count = count;
            if matches!(low, 0xC2 | 0xC6 | 0xCA | 0xCE) {
                info.progressive = true;
            }
            if !icclist.is_empty() {
                icclist.sort();
                let Some(&parts) = icclist[0].get(13) else { return refused("ICC: IndexError") };
                info.icc_profile =
                    Some((parts as usize == icclist.len()).then(|| icclist.iter().flat_map(|p| p[14..].iter().copied()).collect()));
                icclist.clear();
            }
            let mut i = 6;
            while i < s.len() {
                if s.len() - i < 3 {
                    return refused("SOF layers: IndexError");
                }
                info.layers.push((s[i], s[i + 1] / 16, s[i + 1] & 15, s[i + 2]));
                i += 3;
            }
        } else if low == 0xDB {
            let mut q = s;
            while !q.is_empty() {
                let precision = if q[0] / 16 == 0 { 1 } else { 2 };
                let qt_length = 1 + precision * 64;
                if q.len() < qt_length {
                    return refused("bad quantization table marker");
                }
                q = &q[qt_length..];
            }
        }
        if low == 0xDA {
            break;
        }
        marker_byte = None;
    }
    if info.mode.is_empty() || !have_size {
        return refused("not identified by this driver");
    }
    if info.dpi.is_none() {
        if let Some(e) = &info.exif {
            let d = exif_dpi(e)?;
            info.dpi = Some((d, d));
        }
    }
    if let Some(mp) = mp {
        (info.mpo, info.malformed_mpo) = is_mpo(mp, &app1)?;
    }
    Ok(info)
}

/// APP13 "Photoshop 3.0": the image resource walk. Its struct.errors end the
/// walk quietly; reading the name length past the end is an IndexError that
/// fails `open`.
fn photoshop_resources(s: &[u8]) -> R<()> {
    let mut offset = 14usize;
    while s.get(offset..offset.saturating_add(4)) == Some(b"8BIM") {
        offset += 4;
        if offset + 2 > s.len() {
            return Ok(()); // code: struct.error
        }
        let code = be16(s, offset);
        offset += 2;
        let Some(&name_len) = s.get(offset) else { return refused("Photoshop resources: IndexError") };
        offset += 1 + name_len as usize;
        offset += offset & 1;
        if offset.saturating_add(4) > s.len() {
            return Ok(()); // size: struct.error
        }
        let size = be32(s, offset) as usize;
        offset += 4;
        let block = &s[offset.min(s.len())..offset.saturating_add(size).min(s.len())];
        if code == 0x03ED && block.len() < 14 {
            return Ok(()); // ResolutionInfo: struct.error
        }
        offset = offset.saturating_add(size);
        offset += offset & 1;
    }
    Ok(())
}

/// A TIFF directory as `ImageFileDirectory_v2(head).load(fp)` reads it.
struct Ifd<'a> {
    little: bool,
    /// tag -> (type, data), later entries replacing earlier ones.
    tags: Vec<(u16, u16, &'a [u8])>,
}

/// TiffImagePlugin's `_load_dispatch`: the byte size of each value type it
/// reads (others are skipped).
fn type_size(t: u16) -> Option<usize> {
    Some(match t {
        1 | 2 | 6 | 7 => 1,
        3 | 8 => 2,
        4 | 9 | 11 | 13 => 4,
        5 | 10 | 12 | 16 => 8,
        _ => return None,
    })
}

/// `ImageFileDirectory_v2(head)` then `fp.seek(info.next); info.load(fp)`
/// over `d`: Err(()) where the constructor raises (SyntaxError: not a TIFF
/// header; struct.error: a short header, or BigTIFF's next offset, which
/// the 8-byte head does not hold). `load` itself never raises: a short read
/// ends it with the tags read so far (a warning).
fn ifd(d: &[u8]) -> Result<Ifd<'_>, ()> {
    let head = &d[..d.len().min(8)];
    const PREFIXES: [&[u8]; 6] = [b"MM\x00\x2a", b"II\x2a\x00", b"MM\x2a\x00", b"II\x00\x2a", b"MM\x00\x2b", b"II\x2b\x00"];
    if !PREFIXES.iter().any(|p| head.starts_with(p)) {
        return Err(());
    }
    let little = &head[..2] == b"II";
    if head[2] == 43 || head.len() < 8 {
        return Err(());
    }
    let u16_at = |at: usize| -> u16 {
        let b = [d[at], d[at + 1]];
        if little { u16::from_le_bytes(b) } else { u16::from_be_bytes(b) }
    };
    let u32_at = |at: usize| -> u32 {
        let b: [u8; 4] = d[at..at + 4].try_into().unwrap();
        if little { u32::from_le_bytes(b) } else { u32::from_be_bytes(b) }
    };
    let mut out = Ifd { little, tags: Vec::new() };
    let start = u32_at(4) as usize;
    if start.saturating_add(2) > d.len() {
        return Ok(out);
    }
    let count = u16_at(start) as usize;
    let mut at = start + 2;
    for _ in 0..count {
        if at + 12 > d.len() {
            return Ok(out);
        }
        let (tag, typ, cnt) = (u16_at(at), u16_at(at + 2), u32_at(at + 4) as usize);
        let raw = &d[at + 8..at + 12];
        let value_at = at + 8;
        at += 12;
        let Some(unit) = type_size(typ) else { continue };
        let size = cnt * unit;
        let value: &[u8] = if size > 4 {
            let offset = u32_at(value_at) as usize;
            match d.get(offset..offset.saturating_add(size)) {
                Some(v) => v,
                None => return Ok(out), // _safe_read: "Truncated File Read"
            }
        } else {
            &raw[..size]
        };
        if value.is_empty() {
            continue;
        }
        match out.tags.iter_mut().find(|t| t.0 == tag) {
            Some(t) => *t = (tag, typ, value),
            None => out.tags.push((tag, typ, value)),
        }
    }
    Ok(out)
}

/// A tag's value as `ifd[tag]` gives it for a tag TiffTags defines with one
/// value (`_setitem`: the first of several values, a warning).
#[derive(Clone, Debug)]
enum Val<'a> {
    Int(i128),
    Float(f64),
    /// IFDRational (num, den); den 0 is nan.
    Rat(i64, i64),
    Bytes(&'a [u8]),
    /// ASCII, latin-1: the bytes (a trailing NUL dropped).
    Str(&'a [u8]),
}

impl Ifd<'_> {
    fn get(&self, tag: u16, enum_names: &[(&str, i128)]) -> Option<Val<'_>> {
        let &(_, typ, v) = self.tags.iter().find(|t| t.0 == tag)?;
        let little = self.little;
        let rd = |b: &[u8]| -> u64 {
            let mut x = 0u64;
            if little {
                for (i, &byte) in b.iter().enumerate() {
                    x |= (byte as u64) << (8 * i);
                }
            } else {
                for &byte in b {
                    x = (x << 8) | byte as u64;
                }
            }
            x
        };
        Some(match typ {
            1 | 7 => Val::Bytes(v),
            2 => {
                let s = v.strip_suffix(b"\0").unwrap_or(v);
                // cvt_enum: a str naming one of the tag's values becomes it
                match enum_names.iter().find(|(name, _)| name.as_bytes() == s) {
                    Some(&(_, value)) => Val::Int(value),
                    None => Val::Str(s),
                }
            }
            3 => Val::Int(rd(&v[..2]) as u16 as i128),
            4 | 13 => Val::Int(rd(&v[..4]) as u32 as i128),
            16 => Val::Int(rd(&v[..8]) as i128),
            6 => Val::Int(v[0] as i8 as i128),
            8 => Val::Int(rd(&v[..2]) as u16 as i16 as i128),
            9 => Val::Int(rd(&v[..4]) as u32 as i32 as i128),
            5 => Val::Rat(rd(&v[..4]) as u32 as i64, rd(&v[4..8]) as u32 as i64),
            10 => Val::Rat(rd(&v[..4]) as u32 as i32 as i64, rd(&v[4..8]) as u32 as i32 as i64),
            11 => Val::Float(f32::from_bits(rd(&v[..4]) as u32) as f64),
            12 => Val::Float(f64::from_bits(rd(&v[..8]))),
            _ => return None,
        })
    }
}

/// `JpegImageFile._read_dpi_from_exif`: `getexif()` (Exif.load), then
/// ResolutionUnit and XResolution as Python computes with them; every
/// exception it catches gives (72, 72). Err for the IndexError it does not
/// catch (an XResolution of one byte or one digit), which fails `open`.
fn exif_dpi(exif: &[u8]) -> R<PyNum> {
    const FALLBACK: PyNum = PyNum::Int(72);
    let mut d = exif;
    while d.starts_with(b"Exif\0\0") {
        d = &d[6..];
    }
    if d.is_empty() {
        return Ok(FALLBACK); // no directory: KeyError
    }
    let Ok(dir) = ifd(d) else { return Ok(FALLBACK) };
    let Some(unit) = dir.get(0x0128, &[("none", 1), ("inch", 2), ("cm", 3)]) else { return Ok(FALLBACK) };
    let Some(xres) = dir.get(0x011A, &[]) else { return Ok(FALLBACK) };
    // dpi = float(x[0]) / x[1], or x itself on a TypeError
    let dpi = match xres {
        Val::Int(v) => PyNum::Int(v),
        Val::Float(f) => PyNum::Float(f),
        Val::Rat(_, 0) => return Ok(FALLBACK), // nan: ValueError
        Val::Rat(num, den) => PyNum::Rat(num, den),
        Val::Bytes(b) => {
            if b.len() < 2 {
                return refused("EXIF XResolution: IndexError");
            }
            if b[1] == 0 {
                return Ok(FALLBACK); // ZeroDivisionError
            }
            PyNum::Float(b[0] as f64 / b[1] as f64)
        }
        Val::Str(s) => {
            // float(s[0]) (a ValueError unless an ASCII digit), then s[1]
            match s.first() {
                None => return refused("EXIF XResolution: IndexError"),
                Some(c) if !c.is_ascii_digit() => return Ok(FALLBACK),
                Some(_) if s.len() == 1 => return refused("EXIF XResolution: IndexError"),
                Some(_) => return Ok(FALLBACK), // float / str: TypeError, then isnan(str)
            }
        }
    };
    if let PyNum::Float(f) = dpi {
        if f.is_nan() {
            return Ok(FALLBACK);
        }
    }
    let cm = match unit {
        Val::Int(u) => u == 3,
        Val::Float(f) => f == 3.0,
        Val::Rat(num, den) => den != 0 && num as i128 == 3 * den as i128,
        Val::Bytes(_) | Val::Str(_) => false,
    };
    Ok(if cm { PyNum::Float(dpi.to_f64() * 2.54) } else { dpi })
}

/// `jpeg_factory`'s test: `_getmp()` read and more than one image, and no
/// Ultra HDR gain map (an APP1 with `hdrgm:Version`). A malformed MP index
/// (SyntaxError: a warning) or an odd value type (TypeError) leaves a JPEG;
/// an MP entry list shorter than its count is a struct.error that fails
/// `open`.
/// Returns (is an MPO, Pillow warned it is malformed).
fn is_mpo(mp: &[u8], app1: &[&[u8]]) -> R<(bool, bool)> {
    const MALFORMED: (bool, bool) = (false, true);
    let big = mp.starts_with(b"MM\x00\x2a");
    let Ok(dir) = ifd(mp) else { return Ok(MALFORMED) }; // unreadable directory: SyntaxError
    let Some(quant) = dir.get(0xB001, &[]) else { return Ok(MALFORMED) }; // no number of images
    let Some(entries) = dir.get(0xB002, &[]) else { return Ok(MALFORMED) }; // bad MP entry
    let Val::Int(quant) = quant else { return Ok((false, false)) }; // range(x): TypeError
    let mut i = 0i128;
    while i < quant {
        let Val::Bytes(raw) = entries else { return Ok((false, false)) }; // unpack_from: TypeError
        let at = (i * 16) as usize;
        let Some(e) = raw.get(at..at + 16) else { return refused("MP entries: struct.error") };
        let attr = if big { u32::from_be_bytes(e[..4].try_into().unwrap()) } else { u32::from_le_bytes(e[..4].try_into().unwrap()) };
        if (attr >> 24) & 7 != 0 {
            return Ok(MALFORMED); // unsupported picture format: SyntaxError
        }
        i += 1;
    }
    if quant <= 1 {
        return Ok((false, false));
    }
    Ok((!app1.iter().any(|s| s.windows(16).any(|w| w == b" hdrgm:Version=\"")), false))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rational_rounding_is_exact() {
        assert_eq!(PyNum::Rat(5, 2).round(), Some(2));
        assert_eq!(PyNum::Rat(7, 2).round(), Some(4));
        assert_eq!(PyNum::Rat(-5, 2).round(), Some(-2));
        assert_eq!(PyNum::Rat(5, -2).round(), Some(-2));
        assert_eq!(PyNum::Rat(2_500_000_001, 1_000_000_000).round(), Some(3));
        assert_eq!(PyNum::Float(2.5).round(), Some(2));
        assert_eq!(PyNum::Float(f64::INFINITY).round(), None);
    }

    #[test]
    fn photoshop_name_length_past_the_end_fails() {
        assert!(photoshop_resources(b"Photoshop 3.0\x008BIM\x03\xed").is_err());
        assert!(photoshop_resources(b"Photoshop 3.0\x008BIM\x03").is_ok());
        assert!(photoshop_resources(b"Photoshop 3.0\x008BIM\x03\xed\x00\x00\x00\x00\x00\x02ab").is_ok());
    }
}
