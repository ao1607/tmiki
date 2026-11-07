//! PNG decode to the image `PIL.Image.open(data)` gives after `load()`
//! (PngImagePlugin 12.3), frame 0 of an APNG included.
//!
//! | bits, colour type | Pillow mode | unpacking |
//! |---|---|---|
//! | 1, grey | "1" | 0 / 255 |
//! | 2 / 4, grey | "L" | scaled (x 0x55 / x 0x11) |
//! | 16, grey | "I;16" | big-endian to native |
//! | 16, RGB | "RGB" | the high byte of each sample |
//! | 1-8, palette | "P" | indices, PLTE as an RGB palette |
//! | 16, grey + alpha | "RGBA" | (L, L, L, A), high bytes |
//! | 16, RGBA | "RGBA" | high bytes |
//!
//! The chunks are walked as `PngImageFile._open` and `load()` walk them
//! ([`scan`], [`load_facts`]): PngStream's handlers with their checks (a
//! failing one fails `open` before the image data and `load` after it), the
//! CRCs before the image data, `load_end`'s walk after it (stopping at the
//! next fcTL of an animation). tRNS anywhere before that point becomes
//! `info["transparency"]` as `chunk_tRNS` reads it (one read after the image
//! data still counts: `convert("RGBA")` runs after `load()`).
//!
//! Frame 0 (Pillow's rule): the image data `_open` stopped at (the first
//! IDAT, or an fdAT before any IDAT), decoded into the rectangle of the fcTL
//! read before it (the whole image without one), the rest of the image 0;
//! the data continues over the IDAT / fdAT / DDAT chunks that follow. A file
//! whose layout the `png` crate reads the same way (one IHDR, the image data
//! in IDAT chunks, no fcTL before it) goes through the crate; anything else,
//! and anything the crate refuses, through [`zip_decode`], which inflates
//! and unfilters as Pillow's ZipDecode.c does (a zlib stream that ends at a
//! row boundary leaves the remaining rows 0; data after the last row is not
//! read).
//!
//! What Pillow puts in `info` from text chunks under keys tmi reads (a tEXt
//! named "dpi", ...) is not reproduced: tmi's header reading never did.

use std::io::Cursor;

use png::Transformations;
use tmi_pillow::image::{Image, Mode, Palette, Transparency};

#[derive(Debug)]
pub enum PngError {
    Decode(String),
    /// A bit depth / colour type pair Pillow has no mode for.
    Unsupported,
}

fn err<T>(msg: impl Into<String>) -> Result<T, PngError> {
    Err(PngError::Decode(msg.into()))
}

pub struct DecodedPng {
    pub image: Image,
    /// `is_animated` (an APNG of more than one frame; the image is frame 0).
    pub animated: bool,
}

fn be16(s: &[u8], at: usize) -> i64 {
    match s.get(at..at + 2) {
        Some(b) => u16::from_be_bytes([b[0], b[1]]) as i64,
        None => 0,
    }
}

fn be32(s: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([s[at], s[at + 1], s[at + 2], s[at + 3]])
}

/// `_simple_palette.match(s)`: `^\xff*\x00\xff*$`, where `$` also matches
/// before a final newline.
fn simple_palette(s: &[u8]) -> bool {
    let full = |s: &[u8]| s.iter().filter(|&&v| v != 0xff).count() == 1 && s.contains(&0);
    full(s) || (s.last() == Some(&b'\n') && full(&s[..s.len() - 1]))
}

/// PngImagePlugin's MAX_TEXT_CHUNK and MAX_TEXT_MEMORY.
const MAX_TEXT_CHUNK: usize = 1 << 20;
const MAX_TEXT_MEMORY: usize = 64 * MAX_TEXT_CHUNK;

/// `_safe_zlib_decompress(s)`: Ok(Some(text)), Ok(None) for a zlib error
/// (the handlers catch it), Err for more than MAX_TEXT_CHUNK bytes (a
/// ValueError Pillow raises).
fn safe_zlib(s: &[u8]) -> Result<Option<Vec<u8>>, PngError> {
    let mut d = flate2::Decompress::new(true);
    let mut out = Vec::with_capacity(MAX_TEXT_CHUNK.min(s.len().saturating_mul(4).max(1024)));
    loop {
        let before_in = d.total_in() as usize;
        let before_out = d.total_out() as usize;
        if out.len() == out.capacity() {
            let more = (out.capacity() * 2).min(MAX_TEXT_CHUNK) - out.capacity();
            out.reserve_exact(more);
        }
        match d.decompress_vec(&s[before_in.min(s.len())..], &mut out, flate2::FlushDecompress::None) {
            Err(_) => return Ok(None),
            Ok(status) => {
                let (done_in, done_out) = (d.total_in() as usize, d.total_out() as usize);
                if status == flate2::Status::StreamEnd || done_in >= s.len() {
                    return Ok(Some(out));
                }
                if done_out >= MAX_TEXT_CHUNK {
                    // decompress(s, MAX_TEXT_CHUNK) left an unconsumed tail
                    return err("Decompressed data too large for PngImagePlugin.MAX_TEXT_CHUNK");
                }
                if done_in == before_in && done_out == before_out && out.len() < out.capacity() {
                    return Ok(Some(out)); // no progress: incomplete stream
                }
            }
        }
    }
}

/// CRC-32 (PNG / zlib).
struct Crc32(u32);

impl Crc32 {
    fn new() -> Self {
        Crc32(0xFFFF_FFFF)
    }
    fn update(&mut self, data: &[u8]) {
        for &b in data {
            let mut c = (self.0 ^ b as u32) & 0xFF;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            }
            self.0 = (self.0 >> 8) ^ c;
        }
    }
    fn finish(&self) -> u32 {
        self.0 ^ 0xFFFF_FFFF
    }
}

/// PngImagePlugin's `_MODES`: the mode and the (bits, colour type) of the
/// rawmode.
pub fn pillow_mode(depth: u8, colour: u8) -> Option<&'static str> {
    Some(match (depth, colour) {
        (1, 0) => "1",
        (2, 0) | (4, 0) | (8, 0) => "L",
        (16, 0) => "I;16",
        (8, 2) | (16, 2) => "RGB",
        (1, 3) | (2, 3) | (4, 3) | (8, 3) => "P",
        (8, 4) => "LA",
        (16, 4) | (8, 6) | (16, 6) => "RGBA",
        _ => return None,
    })
}

fn is_cid(c: &[u8]) -> bool {
    c.len() == 4 && c.iter().all(|&b| b.is_ascii_alphanumeric() || b == b'_')
}

/// A chunk header as `ChunkStream.read` reads it: its type, where its data
/// starts and its declared length (the data may run past the end).
#[derive(Clone, Copy, Debug)]
pub struct Chunk {
    pub cid: [u8; 4],
    pub pos: usize,
    pub length: usize,
}

/// `ChunkStream.read()` at `at`: None where it raises (fewer than 4 bytes:
/// struct.error; a type that is not four word characters: SyntaxError).
fn read_chunk(data: &[u8], at: usize) -> Option<Chunk> {
    let s = data.get(at..)?;
    if s.len() < 4 {
        return None;
    }
    let cid = &s[4.min(s.len())..8.min(s.len())];
    if !is_cid(cid) {
        return None;
    }
    Some(Chunk { cid: [cid[0], cid[1], cid[2], cid[3]], pos: at + 8, length: be32(s, 0) as usize })
}

impl Chunk {
    /// `_safe_read(fp, length)` of the data: None where it would be short
    /// (OSError "Truncated File Read").
    fn body<'a>(&self, data: &'a [u8]) -> Option<&'a [u8]> {
        data.get(self.pos..self.pos.checked_add(self.length)?)
    }
    fn next(&self) -> usize {
        self.pos.saturating_add(self.length).saturating_add(4)
    }
}

/// The `info` entries and PngStream state tmi reads, as the handlers leave
/// them.
#[derive(Clone, Debug, Default)]
pub struct PngInfo {
    /// `im_size` (IHDR).
    pub size: (u32, u32),
    /// `im_mode` (None: no IHDR with a known bit depth / colour type).
    pub mode: Option<&'static str>,
    /// The (bits, colour type) of `im_rawmode`.
    pub raw: (u8, u8),
    /// `info["interlace"]` (any IHDR with a non-zero interlace byte).
    pub interlace: bool,
    /// The PLTE read while the mode was "P" (`im_palette`).
    pub palette: Option<Vec<u8>>,
    pub transparency: Option<Transparency>,
    /// `info["dpi"]`.
    pub dpi: Option<(f64, f64)>,
    /// `info["icc_profile"]` (Some(None): a damaged profile).
    pub icc_profile: Option<Option<Vec<u8>>>,
    /// `info["exif"]`.
    pub exif: Option<Vec<u8>>,
    /// `im_n_frames` (a valid acTL).
    pub n_frames: Option<u32>,
    seq_num: Option<i64>,
    /// `info["bbox"]` (an fcTL).
    pub bbox: Option<(u64, u64, u64, u64)>,
    text_memory: usize,
    ihdr_count: usize,
    ihdr_plain: bool,
    /// An acTL, fcTL or fdAT was read.
    apng_chunks: bool,
    /// Every value `info` got under the keys image_ops' write-back reads
    /// from a PNG opened by Pillow, in order: the handlers' and the text
    /// chunks' (a tEXt "comment" becomes `info["comment"]`).
    pub info_log: Vec<(&'static str, InfoVal)>,
}

/// A value PngImagePlugin puts in `info`.
#[derive(Clone, Debug, PartialEq)]
pub enum InfoVal {
    /// pHYs: (x, y) dots per inch.
    Dpi(f64, f64),
    /// iCCP (None: a damaged profile).
    Icc(Option<Vec<u8>>),
    Bytes(Vec<u8>),
    /// A text chunk's value (latin-1 or UTF-8 decoded).
    Str(String),
}

const LOGGED: [&str; 7] = ["dpi", "icc_profile", "exif", "comment", "xmp", "progressive", "progression"];

fn note(info: &mut PngInfo, key: &[u8], val: InfoVal) {
    if let Some(k) = LOGGED.iter().find(|k| k.as_bytes() == key) {
        info.info_log.push((k, val));
    }
}

fn latin1(b: &[u8]) -> String {
    b.iter().map(|&c| c as char).collect()
}

/// Where `_open` stopped and how `load()` continues: the chunk that ended
/// the walk and the frame-0 tile.
#[derive(Clone, Debug)]
pub struct PngScan {
    /// After `Image.open` (no load).
    pub info: PngInfo,
    /// The image data chunk `_open` stopped at (IDAT, or an fdAT): its type,
    /// where its data starts (after an fdAT's sequence number) and its
    /// declared data length.
    pub first: Chunk,
    /// The tile: (x0, y0, x1, y1).
    pub tile: (u64, u64, u64, u64),
    /// `default_image`: an APNG whose IDAT is not one of its frames.
    pub default_image: bool,
    /// `n_frames` (`im_n_frames or 1`, plus the default image).
    pub n_frames: u64,
}

impl PngScan {
    pub fn is_animated(&self) -> bool {
        self.n_frames > 1
    }
}

/// Which handler checks to apply: `_open`'s (CRC checked by the caller) or
/// `load_end`'s.
fn handle(info: &mut PngInfo, c: &Chunk, data: &[u8]) -> Result<(), PngError> {
    let short = || err::<()>("Truncated File Read");
    let read = || c.body(data);
    match &c.cid {
        b"IHDR" => {
            let Some(s) = read() else { return short() };
            if c.length < 13 {
                return err("Truncated IHDR chunk");
            }
            info.size = (be32(s, 0), be32(s, 4));
            if let Some(m) = pillow_mode(s[8], s[9]) {
                info.mode = Some(m);
                info.raw = (s[8], s[9]);
            }
            if s[12] != 0 {
                info.interlace = true;
            }
            if s[11] != 0 {
                return err("unknown filter category");
            }
            info.ihdr_count += 1;
            info.ihdr_plain = c.length == 13 && s[10] == 0 && s[12] <= 1;
        }
        b"PLTE" => {
            let Some(s) = read() else { return short() };
            if info.mode == Some("P") {
                info.palette = Some(s.to_vec());
            }
        }
        b"tRNS" => {
            let Some(s) = read() else { return short() };
            match info.mode {
                Some("P") => {
                    if simple_palette(s) {
                        if let Some(i) = s.iter().position(|&v| v == 0) {
                            info.transparency = Some(Transparency::Int(i as i64));
                        }
                    } else {
                        info.transparency = Some(Transparency::Bytes(s.to_vec()));
                    }
                }
                Some(m @ ("1" | "L" | "I;16" | "RGB")) => {
                    if s.len() < if m == "RGB" { 6 } else { 2 } {
                        return err("tRNS: struct.error");
                    }
                    info.transparency = Some(match m {
                        "1" => Transparency::Int(if be16(s, 0) != 0 { 255 } else { 0 }),
                        "RGB" => Transparency::Rgb(be16(s, 0), be16(s, 2), be16(s, 4)),
                        _ => Transparency::Int(be16(s, 0)),
                    });
                }
                _ => {}
            }
        }
        b"gAMA" => {
            let Some(s) = read() else { return short() };
            if s.len() < 4 {
                return err("gAMA: struct.error");
            }
        }
        b"cHRM" => {
            let Some(s) = read() else { return short() };
            if s.len() % 4 != 0 {
                return err("cHRM: struct.error");
            }
        }
        b"sRGB" => {
            if read().is_none() {
                return short();
            }
            if c.length < 1 {
                return err("Truncated sRGB chunk");
            }
        }
        b"pHYs" => {
            let Some(s) = read() else { return short() };
            if c.length < 9 {
                return err("Truncated pHYs chunk");
            }
            if s[8] == 1 {
                info.dpi = Some((be32(s, 0) as f64 * 0.0254, be32(s, 4) as f64 * 0.0254));
                note(info, b"dpi", InfoVal::Dpi(be32(s, 0) as f64 * 0.0254, be32(s, 4) as f64 * 0.0254));
            }
        }
        b"tEXt" => {
            let Some(s) = read() else { return short() };
            let (k, v) = match s.iter().position(|&b| b == 0) {
                Some(i) => (&s[..i], &s[i + 1..]),
                None => (s, &[][..]),
            };
            if !k.is_empty() {
                note(info, k, if k == b"exif" { InfoVal::Bytes(v.to_vec()) } else { InfoVal::Str(latin1(v)) });
                text_memory(info, v.len())?;
            }
        }
        b"zTXt" => {
            let Some(s) = read() else { return short() };
            let (k, v) = match s.iter().position(|&b| b == 0) {
                Some(i) => (&s[..i], &s[i + 1..]),
                None => (s, &[][..]),
            };
            if v.first().copied().unwrap_or(0) != 0 {
                return err(format!("Unknown compression method {} in zTXt chunk", v[0]));
            }
            let text = safe_zlib(v.get(1..).unwrap_or(&[]))?.unwrap_or_default();
            if !k.is_empty() {
                note(info, k, InfoVal::Str(latin1(&text)));
                text_memory(info, text.len())?;
            }
        }
        b"iTXt" => {
            let Some(s) = read() else { return short() };
            let Some(i) = s.iter().position(|&b| b == 0) else { return Ok(()) };
            let r = &s[i + 1..];
            if r.len() < 2 {
                return Ok(());
            }
            let (cf, cm, r) = (r[0], r[1], &r[2..]);
            let mut parts = r.splitn(3, |&b| b == 0);
            let (Some(lang), Some(tk), Some(v)) = (parts.next(), parts.next(), parts.next()) else { return Ok(()) };
            let v: Vec<u8> = if cf != 0 {
                if cm != 0 {
                    return Ok(());
                }
                match safe_zlib(v)? {
                    Some(t) => t,
                    None => return Ok(()),
                }
            } else {
                v.to_vec()
            };
            if &s[..i] == b"XML:com.adobe.xmp" {
                note(info, b"xmp", InfoVal::Bytes(v.clone()));
            }
            let (Ok(_), Ok(_), Ok(text)) = (std::str::from_utf8(lang), std::str::from_utf8(tk), std::str::from_utf8(&v)) else {
                return Ok(());
            };
            note(info, &s[..i], InfoVal::Str(text.to_string()));
            text_memory(info, text.chars().count())?;
        }
        b"eXIf" => {
            let Some(s) = read() else { return short() };
            info.exif = Some([&b"Exif\x00\x00"[..], s].concat());
            note(info, b"exif", InfoVal::Bytes([&b"Exif\x00\x00"[..], s].concat()));
        }
        b"iCCP" => {
            let Some(s) = read() else { return short() };
            // i = s.find(b"\0"); comp_method = s[i + 1] (i = -1: s[0])
            let at = s.iter().position(|&b| b == 0).map_or(0, |i| i + 1);
            let Some(&method) = s.get(at) else { return err("iCCP: IndexError") };
            if method != 0 {
                return err(format!("Unknown compression method {method} in iCCP chunk"));
            }
            info.icc_profile = Some(safe_zlib(&s[at + 1..])?);
            note(info, b"icc_profile", InfoVal::Icc(info.icc_profile.clone().flatten()));
        }
        b"acTL" => {
            info.apng_chunks = true;
            let Some(s) = read() else { return short() };
            if c.length < 8 {
                return err("APNG contains truncated acTL chunk");
            }
            if info.n_frames.is_some() {
                info.n_frames = None; // "Invalid APNG" (a warning)
                return Ok(());
            }
            let n = be32(s, 0);
            if n == 0 || n > 0x8000_0000 {
                return Ok(()); // "Invalid APNG" (a warning)
            }
            info.n_frames = Some(n);
        }
        b"fcTL" => {
            info.apng_chunks = true;
            let Some(s) = read() else { return short() };
            if c.length < 26 {
                return err("APNG contains truncated fcTL chunk");
            }
            let seq = be32(s, 0) as i64;
            if (info.seq_num.is_none() && seq != 0) || info.seq_num.is_some_and(|n| n != seq - 1) {
                return err("APNG contains frame sequence errors");
            }
            info.seq_num = Some(seq);
            let (w, h, px, py) = (be32(s, 4) as u64, be32(s, 8) as u64, be32(s, 12) as u64, be32(s, 16) as u64);
            if px + w > info.size.0 as u64 || py + h > info.size.1 as u64 {
                return err("APNG contains invalid frames");
            }
            info.bbox = Some((px, py, px + w, py + h));
        }
        b"fdAT" => {
            info.apng_chunks = true;
            if c.length < 4 {
                return err("APNG contains truncated fDAT chunk");
            }
            let Some(s) = data.get(c.pos..c.pos.saturating_add(4)) else { return short() };
            let seq = be32(s, 0) as i64;
            if info.seq_num != Some(seq - 1) {
                return err("APNG contains frame sequence errors");
            }
            info.seq_num = Some(seq);
        }
        b"IDAT" | b"IEND" => {}
        _ => {
            if read().is_none() {
                return short();
            }
        }
    }
    Ok(())
}

fn text_memory(info: &mut PngInfo, n: usize) -> Result<(), PngError> {
    info.text_memory += n;
    if info.text_memory > MAX_TEXT_MEMORY {
        return err("Too much memory used in text chunks");
    }
    Ok(())
}

/// `Image.open(png)`: PngImageFile._open's walk to the image data (handlers,
/// CRCs) and ImageFile's checks after it. Err where `open` fails.
pub fn scan(data: &[u8]) -> Result<PngScan, PngError> {
    match scan_open(data)? {
        Ok(s) => Ok(s),
        Err(_) => err("cannot load this image"), // no tile: load() raises
    }
}

/// `scan`, telling `open` failing (the outer Err) from `open` succeeding on a
/// file with no image data (an IEND first; `load()` raises): the inner Err
/// holds what `open` read.
pub fn scan_open(data: &[u8]) -> Result<Result<PngScan, PngInfo>, PngError> {
    if !data.starts_with(b"\x89PNG\r\n\x1a\n") {
        return err("not a PNG file");
    }
    let mut info = PngInfo::default();
    let mut pos = 8usize;
    let first = loop {
        let Some(c) = read_chunk(data, pos) else { return err("broken PNG file") };
        handle(&mut info, &c, data)?;
        match &c.cid {
            b"IDAT" => break c,
            b"fdAT" => break Chunk { cid: c.cid, pos: c.pos + 4, length: c.length - 4 },
            b"IEND" => {
                if info.mode.is_none() || info.size.0 == 0 || info.size.1 == 0 {
                    return err("not identified by this driver");
                }
                return Ok(Err(info));
            }
            _ => {}
        }
        // PngStream.crc: the CRC of the chunk just read
        let body = c.body(data).expect("read by the handler");
        let Some(crc) = data.get(c.pos + c.length..c.pos + c.length + 4) else {
            return err("broken PNG file (incomplete checksum)");
        };
        let mut h = Crc32::new();
        h.update(&c.cid);
        h.update(body);
        if h.finish() != be32(crc, 0) {
            return err(format!("broken PNG file (bad header checksum in {:?})", String::from_utf8_lossy(&c.cid)));
        }
        pos = c.next();
    };
    if info.mode.is_none() || info.size.0 == 0 || info.size.1 == 0 {
        return err("not identified by this driver");
    }
    // chunk_IDAT: the tile is the fcTL's rectangle, else the whole image
    // (and an APNG's IDAT is then its default image)
    let (tile, default_image) = match info.bbox {
        Some(b) => (b, false),
        None => ((0, 0, info.size.0 as u64, info.size.1 as u64), info.n_frames.is_some()),
    };
    let n_frames = info.n_frames.unwrap_or(1) as u64 + (info.n_frames.is_some() && default_image) as u64;
    Ok(Ok(PngScan { info, first, tile, default_image, n_frames }))
}

/// The image data of frame 0 as `load_read` hands it over: the data of the
/// first chunk, then of each IDAT / DDAT / fdAT that directly follows (an
/// fdAT's sequence number checked and skipped). Each part says whether its
/// chunk ran past the end of the file. Err for an fdAT `load_read` refuses.
fn data_run<'a>(data: &'a [u8], scan: &PngScan, info: &mut PngInfo) -> Result<(Vec<(&'a [u8], bool)>, usize), PngError> {
    let mut parts = Vec::new();
    let mut c = scan.first;
    loop {
        let end = c.pos.saturating_add(c.length);
        parts.push((&data[c.pos.min(data.len())..end.min(data.len())], end > data.len()));
        let next = end.saturating_add(4);
        let Some(n) = read_chunk(data, next) else { return Ok((parts, next)) };
        match &n.cid {
            b"IDAT" | b"DDAT" => c = n,
            b"fdAT" => {
                handle(info, &n, data)?;
                c = Chunk { cid: n.cid, pos: n.pos + 4, length: n.length - 4 };
            }
            _ => return Ok((parts, next)),
        }
    }
}

/// `load_end`'s walk after the image data: the chunks from `at` on, until
/// IEND, a header it cannot read, or (an animation) the next fcTL.
fn load_end(data: &[u8], mut at: usize, animated: bool, info: &mut PngInfo) -> Result<(), PngError> {
    loop {
        let Some(c) = read_chunk(data, at) else { return Ok(()) };
        if &c.cid == b"IEND" || (&c.cid == b"fcTL" && animated) {
            return Ok(());
        }
        handle(info, &c, data)?;
        // IDAT / fdAT: chunk_IDAT raises EOFError, then the data is skipped
        if c.body(data).is_none() {
            return err("Truncated File Read");
        }
        at = c.next();
    }
}

/// The `info` (and the frame-0 data) after `load()`, without decoding the
/// pixels. `finished`: the part of the data run the decoder finished in
/// (needed only when the run's last chunk is cut short: if the decoder
/// stops before it, `load_end` meets that chunk and fails).
pub fn load_facts(data: &[u8], scan: &PngScan) -> Result<PngInfo, PngError> {
    let mut info = scan.info.clone();
    let (parts, after) = data_run(data, scan, &mut info)?;
    if parts.last().is_some_and(|p| p.1) {
        // the file ends inside the image data: Pillow is fine only if the
        // decoder finishes in that last chunk
        let (w, h) = tile_size(scan)?;
        let finished = zip_inflate(&parts, w, h, scan.info.raw, scan.info.interlace)?.1;
        if finished + 1 < parts.len() {
            return err("Truncated File Read");
        }
        return Ok(info);
    }
    load_end(data, after, scan.is_animated(), &mut info)?;
    Ok(info)
}

fn tile_size(scan: &PngScan) -> Result<(usize, usize), PngError> {
    let (x0, y0, x1, y1) = scan.tile;
    if x1 <= x0 || y1 <= y0 {
        return err("tile cannot extend outside image");
    }
    Ok(((x1 - x0) as usize, (y1 - y0) as usize))
}

fn channels(colour: u8) -> usize {
    match colour {
        2 => 3,
        4 => 2,
        6 => 4,
        _ => 1,
    }
}

const ADAM7: [(usize, usize, usize, usize); 7] = [(0, 0, 8, 8), (4, 0, 8, 8), (0, 4, 4, 8), (2, 0, 4, 4), (0, 2, 2, 4), (1, 0, 2, 2), (0, 1, 1, 2)];

/// The rows ZipDecode reads for a `w` x `h` tile: (pass, width, height) of
/// each non-empty pass (one pass when not interlaced).
fn passes(w: usize, h: usize, interlaced: bool) -> Vec<(usize, usize, usize)> {
    if !interlaced {
        return vec![(usize::MAX, w, h)];
    }
    ADAM7
        .iter()
        .enumerate()
        .filter_map(|(i, &(x0, y0, dx, dy))| {
            let pw = if w > x0 { (w - x0).div_ceil(dx) } else { 0 };
            let ph = if h > y0 { (h - y0).div_ceil(dy) } else { 0 };
            (pw > 0 && ph > 0).then_some((i, pw, ph))
        })
        .collect()
}

/// ZipDecode.c over the data run: the unfiltered rows of each pass, packed
/// as stored (`bits` per sample), and the index of the part the last row
/// came out of. A zlib stream that ends at a row boundary ends the image
/// there (the rest stays 0); one that ends inside a row, or data that runs
/// out, is "image file is truncated"; a zlib error or an unknown filter
/// type fails the decode.
fn zip_inflate(parts: &[(&[u8], bool)], w: usize, h: usize, raw: (u8, u8), interlaced: bool) -> Result<(Vec<Vec<u8>>, usize), PngError> {
    let bpp_bits = raw.0 as usize * channels(raw.1);
    let pixel_bytes = bpp_bits.div_ceil(8).max(1);
    let plan = passes(w, h, interlaced);
    let mut out: Vec<Vec<u8>> = plan.iter().map(|&(_, pw, ph)| Vec::with_capacity((pw * bpp_bits).div_ceil(8) * ph)).collect();
    let mut d = flate2::Decompress::new(true);
    let (mut pass, mut row) = (0usize, 0usize);
    let row_len = |p: usize| (plan[p].1 * bpp_bits).div_ceil(8) + 1;
    let mut buf: Vec<u8> = vec![0; row_len(0)];
    let mut filled = 0usize;
    let mut prev: Vec<u8> = vec![0; row_len(0) - 1];
    // ImageFile.load hands the decoder 64 KiB blocks of each chunk's data;
    // the call that completes the last row inflates on through the rest of
    // its block only (so the zlib checksum is checked when it is there).
    let blocks = parts.iter().enumerate().flat_map(|(index, (part, _))| part.chunks(65536).map(move |b| (index, b)));
    for (index, block) in blocks {
        let mut input = block;
        while !input.is_empty() {
            let (before_in, before_out) = (d.total_in(), d.total_out());
            let status = d
                .decompress(input, &mut buf[filled..], flate2::FlushDecompress::None)
                .map_err(|e| PngError::Decode(format!("decoder error -3 ({e})")))?;
            input = &input[(d.total_in() - before_in) as usize..];
            filled += (d.total_out() - before_out) as usize;
            let stream_end = status == flate2::Status::StreamEnd;
            if filled < buf.len() {
                if stream_end || (d.total_in() == before_in && d.total_out() == before_out) {
                    // the stream ended inside a row: no more data is taken
                    return err("image file is truncated");
                }
                continue;
            }
            // a whole row: unfilter it against the pass's previous row
            let filter = buf[0];
            let cur = &mut buf[1..];
            unfilter(filter, cur, &prev, pixel_bytes)?;
            out[pass].extend_from_slice(cur);
            prev.clear();
            prev.extend_from_slice(cur);
            filled = 0;
            row += 1;
            if row == plan[pass].2 {
                pass += 1;
                row = 0;
                if pass == plan.len() {
                    return Ok((out, index));
                }
                prev = vec![0; row_len(pass) - 1];
                buf = vec![0; row_len(pass)];
            }
            if stream_end {
                return Ok((out, index)); // the image and the data end together
            }
        }
    }
    err("image file is truncated")
}

fn unfilter(filter: u8, cur: &mut [u8], prev: &[u8], bpp: usize) -> Result<(), PngError> {
    match filter {
        0 => {}
        1 => {
            for i in bpp..cur.len() {
                cur[i] = cur[i].wrapping_add(cur[i - bpp]);
            }
        }
        2 => {
            for i in 0..cur.len() {
                cur[i] = cur[i].wrapping_add(prev[i]);
            }
        }
        3 => {
            for i in 0..cur.len() {
                let left = if i >= bpp { cur[i - bpp] as u16 } else { 0 };
                cur[i] = cur[i].wrapping_add(((left + prev[i] as u16) / 2) as u8);
            }
        }
        4 => {
            for i in 0..cur.len() {
                let a = if i >= bpp { cur[i - bpp] as i16 } else { 0 };
                let b = prev[i] as i16;
                let c = if i >= bpp { prev[i - bpp] as i16 } else { 0 };
                let p = a + b - c;
                let (pa, pb, pc) = ((p - a).abs(), (p - b).abs(), (p - c).abs());
                let pred = if pa <= pb && pa <= pc { a } else if pb <= pc { b } else { c };
                cur[i] = cur[i].wrapping_add(pred as u8);
            }
        }
        _ => return err("broken PNG file (unknown filter type)"),
    }
    Ok(())
}

/// The packed rows of the whole `w` x `h` tile (rows of `ceil(w * bits /
/// 8)` bytes), the passes put in place; rows the data did not reach stay 0.
fn deinterlace(rows: &[Vec<u8>], w: usize, h: usize, raw: (u8, u8), interlaced: bool) -> Vec<u8> {
    let bpp_bits = raw.0 as usize * channels(raw.1);
    let line = (w * bpp_bits).div_ceil(8);
    let mut out = vec![0u8; line * h];
    if !interlaced {
        let n = rows[0].len().min(out.len());
        out[..n].copy_from_slice(&rows[0][..n]);
        return out;
    }
    for (k, &(p, pw, ph)) in passes(w, h, true).iter().enumerate() {
        let (x0, y0, dx, dy) = ADAM7[p];
        let pline = (pw * bpp_bits).div_ceil(8);
        let done = rows[k].len() / pline.max(1);
        for j in 0..ph.min(done) {
            let src = &rows[k][j * pline..(j + 1) * pline];
            let y = y0 + j * dy;
            let dst = &mut out[y * line..(y + 1) * line];
            for i in 0..pw {
                let x = x0 + i * dx;
                if bpp_bits >= 8 {
                    let b = bpp_bits / 8;
                    dst[x * b..(x + 1) * b].copy_from_slice(&src[i * b..(i + 1) * b]);
                } else {
                    let v = (src[i * bpp_bits / 8] >> (8 - bpp_bits - (i * bpp_bits) % 8)) & ((1u16 << bpp_bits) - 1) as u8;
                    let shift = 8 - bpp_bits - (x * bpp_bits) % 8;
                    dst[x * bpp_bits / 8] |= v << shift;
                }
            }
        }
    }
    out
}

/// Unpack `w` samples of `bits` each from `row` (MSB first).
fn unpack_bits(row: &[u8], w: usize, bits: usize, scale: u8) -> impl Iterator<Item = u8> + '_ {
    let mask = ((1u16 << bits) - 1) as u8;
    (0..w).map(move |x| {
        let bit = x * bits;
        let v = (row[bit / 8] >> (8 - bits - bit % 8)) & mask;
        v * scale
    })
}

/// Packed rows (`line` bytes each, as stored) to Pillow's mode and bytes.
fn unpack(buf: &[u8], line: usize, w: usize, h: usize, raw: (u8, u8)) -> Result<(Mode, Vec<u8>), PngError> {
    let rows = |y: usize| &buf[y * line..(y + 1) * line];
    let n = w * h;
    let (colour, bits) = (raw.1, raw.0);
    Ok(match (colour, bits) {
        (0, 1) => (Mode::One, (0..h).flat_map(|y| unpack_bits(rows(y), w, 1, 255)).collect()),
        (0, 2) => (Mode::L, (0..h).flat_map(|y| unpack_bits(rows(y), w, 2, 0x55)).collect()),
        (0, 4) => (Mode::L, (0..h).flat_map(|y| unpack_bits(rows(y), w, 4, 0x11)).collect()),
        (0, 8) => (Mode::L, (0..h).flat_map(|y| rows(y)[..w].to_vec()).collect()),
        (0, 16) => {
            let mut v = Vec::with_capacity(n * 2);
            for y in 0..h {
                for s in rows(y)[..w * 2].chunks_exact(2) {
                    v.push(s[1]);
                    v.push(s[0]);
                }
            }
            (Mode::I16, v)
        }
        (2, 8) => (Mode::RGB, (0..h).flat_map(|y| rows(y)[..w * 3].to_vec()).collect()),
        (2, 16) => (Mode::RGB, (0..h).flat_map(|y| rows(y)[..w * 6].iter().step_by(2).copied().collect::<Vec<_>>()).collect()),
        (3, b) => (Mode::P, (0..h).flat_map(|y| unpack_bits(rows(y), w, b as usize, 1)).collect()),
        (4, 8) => (Mode::LA, (0..h).flat_map(|y| rows(y)[..w * 2].to_vec()).collect()),
        (4, 16) => {
            let mut v = Vec::with_capacity(n * 4);
            for y in 0..h {
                for s in rows(y)[..w * 4].chunks_exact(4) {
                    v.extend_from_slice(&[s[0], s[0], s[0], s[2]]);
                }
            }
            (Mode::RGBA, v)
        }
        (6, 8) => (Mode::RGBA, (0..h).flat_map(|y| rows(y)[..w * 4].to_vec()).collect()),
        (6, 16) => (Mode::RGBA, (0..h).flat_map(|y| rows(y)[..w * 8].iter().step_by(2).copied().collect::<Vec<_>>()).collect()),
        _ => return Err(PngError::Unsupported),
    })
}

/// The `png` crate's decode of a file laid out as it expects: the packed
/// rows of the whole image. None when it refuses (ZipDecode decides).
fn crate_decode(data: &[u8]) -> Option<(Vec<u8>, usize)> {
    // Pillow's zip decoder does not check the IDAT CRCs (scan checked the
    // others). The zlib checksum is checked: Pillow checks it when it comes
    // with the last row's data, and a file whose checksum is wrong goes to
    // zip_decode, which follows Pillow there.
    let mut options = png::DecodeOptions::default();
    options.set_ignore_crc(true);
    options.set_ignore_adler32(false);
    options.set_ignore_text_chunk(true);
    let mut dec = png::Decoder::new_with_options(Cursor::new(data), options);
    dec.set_transformations(Transformations::IDENTITY);
    let mut reader = dec.read_info().ok()?;
    let size = reader.output_buffer_size()?;
    let mut buf = vec![0u8; size];
    let frame = reader.next_frame(&mut buf).ok()?;
    let line = frame.line_size;
    buf.truncate(line * frame.height as usize);
    Some((buf, line))
}

pub fn decode_png(data: &[u8]) -> Result<DecodedPng, PngError> {
    decode_png_with(data, false)
}

/// `decode_png`; `zip_only`: never through the `png` crate (for tests).
pub fn decode_png_with(data: &[u8], zip_only: bool) -> Result<DecodedPng, PngError> {
    let scan = scan(data)?;
    let info = load_facts(data, &scan)?;
    let (w, h) = (scan.info.size.0 as usize, scan.info.size.1 as usize);
    let raw = scan.info.raw;
    let (tw, th) = tile_size(&scan)?;
    // The png crate reads frame 0 as Pillow does when the image data is
    // IDAT chunks of the one plain IHDR, for the whole image.
    let simple = !zip_only
        && scan.info.ihdr_count == 1
        && scan.info.ihdr_plain
        && &scan.first.cid == b"IDAT"
        && scan.info.bbox.is_none()
        && !scan.info.apng_chunks;
    let decoded = if simple { crate_decode(data).filter(|(b, line)| *line == (w * raw.0 as usize * channels(raw.1)).div_ceil(8) && b.len() == line * h) } else { None };
    let (mode, px) = match decoded {
        Some((buf, line)) => {
            // 8-bit grey, RGB, grey + alpha and RGBA rows are Pillow's bytes as they are
            let direct = matches!(raw, (8, 0) | (8, 2) | (8, 4) | (8, 6)) && line == w * channels(raw.1);
            if direct {
                let mode = match raw.1 {
                    0 => Mode::L,
                    2 => Mode::RGB,
                    4 => Mode::LA,
                    _ => Mode::RGBA,
                };
                (mode, buf)
            } else {
                unpack(&buf, line, w, h, raw)?
            }
        }
        None => {
            let mut info2 = scan.info.clone();
            let (parts, _) = data_run(data, &scan, &mut info2)?;
            let (rows, _) = zip_inflate(&parts, tw, th, raw, scan.info.interlace)?;
            let packed = deinterlace(&rows, tw, th, raw, scan.info.interlace);
            let line = (tw * raw.0 as usize * channels(raw.1)).div_ceil(8);
            let (mode, tile) = unpack(&packed, line, tw, th, raw)?;
            if (tw, th) == (w, h) {
                (mode, tile)
            } else {
                // the tile into a 0 image of the full size
                let bpp = mode.bytes_per_pixel();
                let mut full = vec![0u8; w * h * bpp];
                let (x0, y0) = (scan.tile.0 as usize, scan.tile.1 as usize);
                for y in 0..th {
                    let d = ((y0 + y) * w + x0) * bpp;
                    full[d..d + tw * bpp].copy_from_slice(&tile[y * tw * bpp..(y + 1) * tw * bpp]);
                }
                (mode, full)
            }
        }
    };
    let mut image = Image::new(mode, w, h, px);
    if mode == Mode::P {
        image.palette = Some(Palette::from_rgb(scan.info.palette.as_deref().unwrap_or(&[])));
    }
    image.transparency = match (mode, info.transparency) {
        (Mode::RGBA | Mode::LA, _) => None,
        (_, t) => t,
    };
    Ok(DecodedPng { image, animated: scan.is_animated() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use png::{BitDepth, ColorType};

    fn png(colour: png::ColorType, depth: png::BitDepth, w: u32, h: u32, data: &[u8], trns: Option<&[u8]>) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut e = png::Encoder::new(&mut out, w, h);
            e.set_color(colour);
            e.set_depth(depth);
            if let Some(t) = trns {
                e.set_trns(t.to_vec());
            }
            let mut wr = e.write_header().unwrap();
            wr.write_image_data(data).unwrap();
        }
        out
    }

    #[test]
    fn modes_as_pillow_gives_them() {
        let g16 = png(ColorType::Grayscale, BitDepth::Sixteen, 2, 1, &[0x01, 0x02, 0xff, 0x00], Some(&[0x00, 0x05]));
        let d = decode_png(&g16).unwrap().image;
        assert_eq!((d.mode, d.data.clone()), (Mode::I16, vec![0x02, 0x01, 0x00, 0xff]));
        assert_eq!(d.transparency, Some(Transparency::Int(5)));
        let g2 = png(ColorType::Grayscale, BitDepth::Two, 4, 1, &[0b00_01_10_11], None);
        assert_eq!(decode_png(&g2).unwrap().image.data, vec![0, 0x55, 0xaa, 0xff]);
        let rgba16: Vec<u8> = (0..16).collect();
        let d = decode_png(&png(ColorType::Rgba, BitDepth::Sixteen, 2, 1, &rgba16, None)).unwrap().image;
        assert_eq!(d.data, vec![0, 2, 4, 6, 8, 10, 12, 14]);
    }

    #[test]
    fn simple_palette_rule() {
        assert!(simple_palette(b"\xff\xff\x00\xff"));
        assert!(simple_palette(b"\xff\x00\n"));
        assert!(!simple_palette(b"\xff\x00\x00"));
        assert!(!simple_palette(b"\xff\x80"));
    }

    /// The crate and ZipDecode agree, interlaced or not, at every depth.
    #[test]
    fn zip_decode_matches_the_crate() {
        for (colour, depth, chans) in [
            (ColorType::Grayscale, BitDepth::One, 1),
            (ColorType::Grayscale, BitDepth::Two, 1),
            (ColorType::Grayscale, BitDepth::Four, 1),
            (ColorType::Grayscale, BitDepth::Sixteen, 2),
            (ColorType::Indexed, BitDepth::Four, 1),
            (ColorType::Rgb, BitDepth::Eight, 3),
            (ColorType::Rgba, BitDepth::Sixteen, 8),
        ] {
            for (w, h) in [(1u32, 1u32), (3, 2), (9, 11), (17, 5)] {
                let bits = depth as usize * if chans > 1 && depth as usize == 16 { chans / 2 } else { chans };
                let line = (w as usize * bits).div_ceil(8);
                let px: Vec<u8> = (0..line * h as usize).map(|i| (i * 37 % 251) as u8).collect();
                let mut out = Vec::new();
                {
                    let mut e = png::Encoder::new(&mut out, w, h);
                    e.set_color(colour);
                    e.set_depth(depth);
                    if colour == ColorType::Indexed {
                        e.set_palette(vec![0u8; 48]);
                    }
                    let mut wr = e.write_header().unwrap();
                    wr.write_image_data(&px).unwrap();
                }
                let scan = scan(&out).unwrap();
                let (crate_rows, crate_line) = crate_decode(&out).unwrap();
                let mut info = scan.info.clone();
                let (parts, _) = data_run(&out, &scan, &mut info).unwrap();
                let (rows, _) = zip_inflate(&parts, w as usize, h as usize, scan.info.raw, false).unwrap();
                let ours = deinterlace(&rows, w as usize, h as usize, scan.info.raw, false);
                assert_eq!(crate_line, line);
                assert_eq!(ours, crate_rows, "{colour:?} {depth:?} {w}x{h}");
            }
        }
    }
}
