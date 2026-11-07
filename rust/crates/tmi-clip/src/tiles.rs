//! CLIP offscreen tile decode (P3-b; total since RUST_MIGRATION.md 1e).
//!
//! Mirrors `iter_tiles` + `_tile_to_image` + the `alpha_composite` of each
//! tile onto a transparent canvas in the Python decoder (now
//! `tmiref/clip.py`, `_python_offscreen_buffer`). Compositing a tile onto a
//! fully transparent canvas with Pillow yields the source pixel when its
//! alpha is non-zero and (0, 0, 0, 0) otherwise, so that is what we write;
//! tiles that overlap (a repeated block, a tile larger than its cell) are
//! composited over each other in file order, as Pillow does.
//!
//! Every payload has an answer: pixels (plus the unknown tile sizes met,
//! which Python warns about and draws transparent), or `Err` with why the
//! offscreen is unreadable, wherever Python raises (malformed records, zlib
//! errors, a 1-bit tile whose rows Pillow cannot unpack).

use flate2::{Decompress, FlushDecompress, Status};
use tmi_pillow::over::over_pixel;
use rayon::prelude::*;

pub const TILE_SIZE: usize = 256;
pub(crate) const BEGIN_NAME: &str = "BlockDataBeginChunk";

/// Exposed beyond this crate so `tmi-format`'s CLIP recipe (PLUGINS.md P3)
/// can describe each tile's span and dimensions without decoding it.
pub struct RawTile<'a> {
    pub block_index: usize,
    pub bytes_per_tile: usize,
    pub tile_w: usize,
    pub tile_h: usize,
    pub compressed: &'a [u8],
}

struct DecodedTile {
    x0: usize,
    y0: usize,
    x1: usize,
    y1: usize,
    // RGBA for the clipped rect (x0..x1, y0..y1), row-major.
    rgba: Vec<u8>,
    // Alpha bbox in canvas coordinates, None when fully transparent.
    bbox: Option<(usize, usize, usize, usize)>,
}

pub(crate) fn be_u32(buf: &[u8], off: usize) -> Option<usize> {
    let b = buf.get(off..off + 4)?;
    Some(u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize)
}

pub(crate) fn be_u64(buf: &[u8], off: usize) -> Option<u64> {
    let b = buf.get(off..off + 8)?;
    Some(u64::from_be_bytes(b.try_into().ok()?))
}

/// Python-style slice: clamps both ends instead of failing.
pub(crate) fn py_slice(buf: &[u8], start: usize, end: usize) -> &[u8] {
    let s = start.min(buf.len());
    let e = end.min(buf.len()).max(s);
    &buf[s..e]
}

pub(crate) fn begin_name_utf16be() -> Vec<u8> {
    BEGIN_NAME.encode_utf16().flat_map(|u| u.to_be_bytes()).collect()
}

pub fn scan_tiles(payload: &[u8]) -> Option<Vec<RawTile<'_>>> {
    if payload.get(8..16)? != b"extrnlid" {
        return None;
    }
    let inner_size = be_u64(payload, 48)?;
    let inner_end = 56usize.saturating_add(usize::try_from(inner_size).ok()?);
    let inner = py_slice(payload, 56, inner_end);
    let begin = begin_name_utf16be();

    let mut tiles = Vec::new();
    let mut p = 0usize;
    while p + 8 <= inner.len() {
        let rec_size = be_u32(inner, p)?;
        let name_len = be_u32(inner, p + 4)?;
        if name_len * 2 + 8 > rec_size || p + rec_size > inner.len() {
            break;
        }
        let name = &inner[p + 8..p + 8 + 2 * name_len];
        if name != begin.as_slice() {
            // Python decodes every record name; invalid UTF-16 raises there.
            let units: Vec<u16> = name.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
            String::from_utf16(&units).ok()?;
            p += rec_size;
            continue;
        }
        let body = py_slice(inner, p + 46, p + rec_size);
        let block_index = be_u32(body, 0)?;
        let bytes_per_tile = be_u32(body, 4)?;
        let tile_w = be_u32(body, 8)?;
        let tile_h = be_u32(body, 12)?;
        let has_data = be_u32(body, 16)?;
        if has_data != 0 {
            let data_size = be_u32(body, 20)?;
            let blob = py_slice(body, 24, 24 + data_size);
            tiles.push(RawTile {
                block_index,
                bytes_per_tile,
                tile_w,
                tile_h,
                compressed: py_slice(blob, 4, blob.len()),
            });
        }
        p += rec_size;
    }
    Some(tiles)
}

/// An offscreen may be bigger than its canvas (a layer pushed past the
/// edge), but not without bound: its declared size is what `decode_offscreen`
/// sizes its output by, and a hostile file declares anything. Paths that know
/// the pixel limit use it (`tmi_format::builtin_clip`); the renderers, which
/// only know the canvas, hold an offscreen to this many canvases (or 2^24
/// pixels, whichever is more). Returns why it is refused.
pub const OFFSCREEN_CANVASES: u64 = 64;

pub fn offscreen_oversize(pixel_w: usize, pixel_h: usize, canvas_w: usize, canvas_h: usize) -> Option<String> {
    let pixels = (pixel_w as u128) * (pixel_h as u128);
    let canvas = (canvas_w as u128) * (canvas_h as u128);
    let allowed = (canvas * OFFSCREEN_CANVASES as u128).max(1 << 24);
    (pixels > allowed).then(|| format!("offscreen of {pixel_w}x{pixel_h} pixels is far larger than the {canvas_w}x{canvas_h} canvas"))
}

/// The most bytes one tile can hold: 5 per pixel (alpha plane + BGRA) of the
/// largest tile CSP writes (256 x 256).
pub const MAX_TILE_BYTES: usize = TILE_SIZE * TILE_SIZE * 5;

/// The most bytes a tile may declare. Twice `MAX_TILE_BYTES`, so a tile of an
/// unknown size a little over it still reaches `tile_kind` and is drawn
/// transparent with a warning, as the reference does.
pub const MAX_TILE_DECLARED: usize = 2 * MAX_TILE_BYTES;

pub(crate) fn inflate_exact(src: &[u8], expected: usize) -> Option<Vec<u8>> {
    // A declared size is not trusted: no real tile is larger than this, and
    // `expected` is allocated up front (and by every tile in parallel).
    if expected > MAX_TILE_DECLARED {
        return None;
    }
    // One spare byte so an oversized stream is detected instead of truncated.
    let mut out = Vec::with_capacity(expected + 1);
    let mut d = Decompress::new(true);
    let status = d.decompress_vec(src, &mut out, FlushDecompress::Finish).ok()?;
    if status != Status::StreamEnd || out.len() != expected {
        return None;
    }
    Some(out)
}

#[inline]
fn bit(plane: &[u8], row_bytes: usize, x: usize, y: usize) -> u8 {
    if plane[y * row_bytes + x / 8] & (0x80 >> (x % 8)) != 0 {
        255
    } else {
        0
    }
}

/// `_tile_to_image`'s encoding of a tile with `pixels` pixels and `bpt`
/// bytes (same dispatch order): 1 = 1-bit alpha, 2 = 1-bit alpha + 1-bit
/// grey, 3 = 8-bit alpha, 4 = alpha + grey planes, 5 = alpha plane + BGRA;
/// None for an unknown size (warned about, drawn transparent).
pub fn tile_kind(bpt: usize, pixels: usize) -> Option<u8> {
    Some(if bpt == pixels / 8 {
        1
    } else if bpt == pixels / 4 {
        2
    } else if bpt == pixels {
        3
    } else if bpt == pixels * 2 {
        4
    } else if bpt == pixels * 5 {
        5
    } else {
        return None;
    })
}

/// Why a 1-bit tile is unreadable: Pillow unpacks "1" rows of
/// `ceil(w / 8)` bytes, more than `w * h / 8` holds unless `w % 8 == 0`
/// ("not enough image data"). A tile with no pixels unpacks nothing.
pub(crate) fn one_bit_rows_ok(kind: u8, tw: usize, th: usize) -> bool {
    !((kind == 1 || kind == 2) && tw % 8 != 0 && tw * th != 0)
}

/// One tile's contribution: nothing (outside the offscreen, or no pixels),
/// an unknown size `(bpt, pixels)`, or pixels.
enum TilePixels {
    Nothing,
    Unknown(usize, usize),
    Pixels(DecodedTile),
}

fn decode_tile(tile: &RawTile<'_>, cols: usize, pixel_w: usize, pixel_h: usize) -> Result<TilePixels, String> {
    let (tw, th) = (tile.tile_w, tile.tile_h);
    // A tile over 256 x 256 runs into the next cell, as the reference draws
    // it; what it may allocate is held by its declared bytes.
    let pixels = tw.checked_mul(th).ok_or("tile size overflows")?;
    let bpt = tile.bytes_per_tile;
    if bpt > MAX_TILE_DECLARED {
        return Err(format!("tile {} declares {bpt} bytes for {pixels} pixels", tile.block_index));
    }
    // `iter_tiles` inflates before `_tile_to_image` looks at the size.
    let data = inflate_exact(tile.compressed, bpt)
        .ok_or_else(|| format!("tile {} does not inflate to {bpt} bytes", tile.block_index))?;
    let Some(kind) = tile_kind(bpt, pixels) else {
        return Ok(TilePixels::Unknown(bpt, pixels));
    };
    if !one_bit_rows_ok(kind, tw, th) {
        return Err(format!("1-bit tile {} is {tw} pixels wide: not enough image data", tile.block_index));
    }

    let tx = (tile.block_index % cols) * TILE_SIZE;
    let ty = (tile.block_index / cols) * TILE_SIZE;
    let x0 = tx.min(pixel_w);
    let y0 = ty.min(pixel_h);
    let x1 = tx.saturating_add(tw).min(pixel_w);
    let y1 = ty.saturating_add(th).min(pixel_h);
    if x0 >= x1 || y0 >= y1 {
        return Ok(TilePixels::Nothing);
    }
    let (cw, ch) = (x1 - x0, y1 - y0);
    let mut rgba = vec![0u8; cw * ch * 4];
    let row_bytes = tw / 8;
    let half = pixels / 8;
    let (mut bx0, mut by0, mut bx1, mut by1) = (usize::MAX, usize::MAX, 0usize, 0usize);
    if kind == 5 {
        // The common colour tile: an alpha plane, then BGRA pixels.
        let (alpha, bgra) = data.split_at(pixels);
        for ly in 0..ch {
            let sy = y0 - ty + ly;
            let sx0 = x0 - tx;
            let arow = &alpha[sy * tw + sx0..sy * tw + sx0 + cw];
            let crow = &bgra[(sy * tw + sx0) * 4..(sy * tw + sx0 + cw) * 4];
            let orow = &mut rgba[ly * cw * 4..(ly + 1) * cw * 4];
            let (mut first, mut last) = (usize::MAX, 0usize);
            for (lx, ((o, s), &a)) in orow.chunks_exact_mut(4).zip(crow.chunks_exact(4)).zip(arow).enumerate() {
                if a != 0 {
                    o[0] = s[2];
                    o[1] = s[1];
                    o[2] = s[0];
                    o[3] = a;
                    if first == usize::MAX {
                        first = lx;
                    }
                    last = lx;
                }
            }
            if first != usize::MAX {
                bx0 = bx0.min(x0 + first);
                bx1 = bx1.max(x0 + last + 1);
                by0 = by0.min(y0 + ly);
                by1 = y0 + ly + 1;
            }
        }
        let bbox = if bx0 == usize::MAX { None } else { Some((bx0, by0, bx1, by1)) };
        return Ok(TilePixels::Pixels(DecodedTile { x0, y0, x1, y1, rgba, bbox }));
    }
    for ly in 0..ch {
        let sy = y0 - ty + ly;
        for lx in 0..cw {
            let sx = x0 - tx + lx;
            let i = sy * tw + sx;
            let (r, g, b, a) = match kind {
                1 => (255, 255, 255, bit(&data, row_bytes, sx, sy)),
                2 => {
                    let gray = bit(&data[half..], row_bytes, sx, sy);
                    (gray, gray, gray, bit(&data, row_bytes, sx, sy))
                }
                3 => (255, 255, 255, data[i]),
                4 => {
                    let gray = data[pixels + i];
                    (gray, gray, gray, data[i])
                }
                _ => {
                    let px = &data[pixels + i * 4..pixels + i * 4 + 4];
                    (px[2], px[1], px[0], data[i])
                }
            };
            if a == 0 {
                continue;
            }
            let o = (ly * cw + lx) * 4;
            rgba[o] = r;
            rgba[o + 1] = g;
            rgba[o + 2] = b;
            rgba[o + 3] = a;
            let (cx, cy) = (x0 + lx, y0 + ly);
            bx0 = bx0.min(cx);
            by0 = by0.min(cy);
            bx1 = bx1.max(cx + 1);
            by1 = by1.max(cy + 1);
        }
    }
    let bbox = if bx0 == usize::MAX { None } else { Some((bx0, by0, bx1, by1)) };
    Ok(TilePixels::Pixels(DecodedTile { x0, y0, x1, y1, rgba, bbox }))
}

/// A decoded offscreen: the RGBA of its alpha bbox at (x, y) (`w == h == 0`
/// when nothing is painted), and the unknown tile sizes `(bytes per tile,
/// pixels)` it held, which Python warns about and draws transparent.
#[derive(Debug, Default)]
pub struct Offscreen {
    pub rgba: Vec<u8>,
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
    pub unknown_sizes: Vec<(usize, usize)>,
}

/// Decode an offscreen's tiles (all cores), or why it is unreadable.
#[allow(clippy::too_many_arguments)]
pub fn decode_offscreen(
    raw: &[u8],
    payload_off: usize,
    payload_size: usize,
    tile_cols: usize,
    tile_rows: usize,
    pixel_w: usize,
    pixel_h: usize,
) -> Result<Offscreen, String> {
    decode_offscreen_on(raw, payload_off, payload_size, tile_cols, tile_rows, pixel_w, pixel_h, true)
}

/// `decode_offscreen` with the tiles decoded on the calling thread only
/// (`parallel == false`): for code that must not let rayon steal other work
/// onto its thread (a value being computed once for several waiters).
/// (`tile_rows` does not matter: tiles sit by their block index.)
#[allow(clippy::too_many_arguments)]
pub fn decode_offscreen_on(
    raw: &[u8],
    payload_off: usize,
    payload_size: usize,
    tile_cols: usize,
    _tile_rows: usize,
    pixel_w: usize,
    pixel_h: usize,
    parallel: bool,
) -> Result<Offscreen, String> {
    let payload = payload_off
        .checked_add(payload_size)
        .and_then(|end| raw.get(payload_off..end))
        .ok_or("data block outside the file")?;
    let tiles = scan_tiles(payload).ok_or("malformed tile records")?;
    if tile_cols == 0 && !tiles.is_empty() {
        return Err("tiles but no tile columns (ZeroDivisionError)".into());
    }

    let decoded: Result<Vec<TilePixels>, String> = if parallel {
        tiles.par_iter().map(|t| decode_tile(t, tile_cols, pixel_w, pixel_h)).collect()
    } else {
        tiles.iter().map(|t| decode_tile(t, tile_cols, pixel_w, pixel_h)).collect()
    };
    let mut unknown_sizes = Vec::new();
    let mut painted = Vec::with_capacity(tiles.len());
    for t in decoded? {
        match t {
            TilePixels::Nothing => {}
            TilePixels::Unknown(bpt, pixels) => unknown_sizes.push((bpt, pixels)),
            TilePixels::Pixels(d) if d.bbox.is_some() => painted.push(d),
            TilePixels::Pixels(_) => {}
        }
    }
    // A repeated block or a tile larger than its cell lands on another
    // tile's pixels: those are composited over in file order.
    let mut seen = std::collections::HashSet::with_capacity(tiles.len());
    let overlap = tiles.iter().any(|t| t.tile_w > TILE_SIZE || t.tile_h > TILE_SIZE || !seen.insert(t.block_index));

    let mut bbox: Option<(usize, usize, usize, usize)> = None;
    for t in &painted {
        let b = t.bbox.unwrap();
        bbox = Some(match bbox {
            None => b,
            Some(u) => (u.0.min(b.0), u.1.min(b.1), u.2.max(b.2), u.3.max(b.3)),
        });
    }
    let Some((bx0, by0, bx1, by1)) = bbox else {
        return Ok(Offscreen { unknown_sizes, ..Offscreen::default() });
    };
    let (ow, oh) = (bx1 - bx0, by1 - by0);
    let mut out = vec![0u8; ow * oh * 4];
    for t in &painted {
        let cx0 = t.x0.max(bx0);
        let cx1 = t.x1.min(bx1);
        let cy0 = t.y0.max(by0);
        let cy1 = t.y1.min(by1);
        if cx0 >= cx1 || cy0 >= cy1 {
            continue;
        }
        let tw = t.x1 - t.x0;
        let n = (cx1 - cx0) * 4;
        for y in cy0..cy1 {
            let src = ((y - t.y0) * tw + (cx0 - t.x0)) * 4;
            let dst = ((y - by0) * ow + (cx0 - bx0)) * 4;
            if overlap {
                for (d, s) in out[dst..dst + n].chunks_exact_mut(4).zip(t.rgba[src..src + n].chunks_exact(4)) {
                    over_pixel(d, s);
                }
            } else {
                out[dst..dst + n].copy_from_slice(&t.rgba[src..src + n]);
            }
        }
    }
    Ok(Offscreen { rgba: out, x: bx0, y: by0, w: ow, h: oh, unknown_sizes })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A BlockDataBeginChunk record (the layout clip.py's `build_tile_record`
    /// writes), its tile data deflated when given.
    pub(crate) fn record(block: u32, bpt: u32, tw: u32, th: u32, data: Option<&[u8]>) -> Vec<u8> {
        let mut body = Vec::new();
        for v in [block, bpt, tw, th, data.is_some() as u32] {
            body.extend_from_slice(&v.to_be_bytes());
        }
        if let Some(d) = data {
            let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
            std::io::Write::write_all(&mut z, d).unwrap();
            let comp = z.finish().unwrap();
            let mut blob = (comp.len() as u32).to_le_bytes().to_vec();
            blob.extend_from_slice(&comp);
            body.extend_from_slice(&(blob.len() as u32).to_be_bytes());
            body.extend_from_slice(&blob);
        }
        let end: Vec<u8> = "BlockDataEndChunk".encode_utf16().flat_map(u16::to_be_bytes).collect();
        body.extend_from_slice(&17u32.to_be_bytes());
        body.extend_from_slice(&end);
        let name = begin_name_utf16be();
        let mut out = Vec::new();
        out.extend_from_slice(&((8 + name.len() + body.len()) as u32).to_be_bytes());
        out.extend_from_slice(&19u32.to_be_bytes());
        out.extend_from_slice(&name);
        out.extend_from_slice(&body);
        out
    }

    pub(crate) fn payload(records: &[Vec<u8>]) -> Vec<u8> {
        let inner: Vec<u8> = records.concat();
        let mut out = 0x28u64.to_be_bytes().to_vec();
        out.extend_from_slice(b"extrnlid");
        out.extend_from_slice(&[b'0'; 32]);
        out.extend_from_slice(&(inner.len() as u64).to_be_bytes());
        out.extend_from_slice(&inner);
        out
    }

    /// An 8-bit alpha tile (`bpt == pixels`): white at the given alpha.
    fn alpha_tile(w: usize, h: usize, a: impl Fn(usize, usize) -> u8) -> Vec<u8> {
        (0..h).flat_map(|y| (0..w).map(move |x| (x, y))).map(|(x, y)| a(x, y)).collect()
    }

    fn decode(p: &[u8], cols: usize, w: usize, h: usize) -> Result<Offscreen, String> {
        decode_offscreen(p, 0, p.len(), cols, 1, w, h)
    }

    #[test]
    fn unknown_tile_sizes_are_transparent_and_reported() {
        let ok = alpha_tile(4, 4, |x, _| if x == 1 { 200 } else { 0 });
        let p = payload(&[record(0, 16, 4, 4, Some(&ok)), record(1, 7, 4, 4, Some(&[1; 7]))]);
        let d = decode(&p, 2, 300, 4).unwrap();
        assert_eq!((d.x, d.y, d.w, d.h), (1, 0, 1, 4));
        assert_eq!(d.unknown_sizes, vec![(7, 16)]);
        assert!(d.rgba.chunks_exact(4).all(|p| p == [255, 255, 255, 200]));
        // An unknown size must still inflate to its byte count.
        let bad = payload(&[record(0, 9, 4, 4, Some(&[1; 7]))]);
        assert!(decode(&bad, 1, 4, 4).is_err());
    }

    #[test]
    fn overlapping_tiles_are_composited_in_file_order() {
        let a = alpha_tile(4, 1, |_, _| 128);
        let b = alpha_tile(4, 1, |x, _| if x < 2 { 255 } else { 64 });
        let p = payload(&[record(0, 4, 4, 1, Some(&a)), record(0, 4, 4, 1, Some(&b))]);
        let d = decode(&p, 1, 4, 1).unwrap();
        let mut want = Vec::new();
        for x in 0..4 {
            let mut px = [255, 255, 255, 128];
            over_pixel(&mut px, &[255, 255, 255, if x < 2 { 255 } else { 64 }]);
            want.extend_from_slice(&px);
        }
        assert_eq!(d.rgba, want);
        // A tile wider than its cell runs into the next one.
        let wide = alpha_tile(260, 1, |x, _| if x >= 256 { 255 } else { 0 });
        let p = payload(&[record(1, 4, 4, 1, Some(&a)), record(0, 260, 260, 1, Some(&wide))]);
        let d = decode(&p, 2, 512, 1).unwrap();
        assert_eq!((d.x, d.w), (256, 4));
        assert!(d.rgba.chunks_exact(4).all(|p| p == [255, 255, 255, 255]));
    }

    #[test]
    fn hostile_tile_declarations_are_refused_before_allocating() {
        // A declared 4 GiB tile (the zlib stream itself is tiny).
        let p = payload(&[record(0, u32::MAX, 4, 4, Some(&[0; 16]))]);
        let e = decode(&p, 1, 4, 4).unwrap_err();
        assert!(e.contains("declares"), "{e}");
        // A 32768 x 32768 tile of 1-bit data: 128 MiB declared and inflated.
        let big = 32768u32;
        let p = payload(&[record(0, big * big / 8, big, big, Some(&[0; 16]))]);
        let e = decode(&p, 1, 4, 4).unwrap_err();
        assert!(e.contains("declares"), "{e}");
        // The mask path (window.rs) refuses the same declarations.
        assert!(inflate_exact(&[0x78, 0x9c, 3, 0, 0, 0, 0, 1], MAX_TILE_DECLARED + 1).is_none());
    }

    #[test]
    fn unreadable_payloads_say_why() {
        let tile = alpha_tile(4, 4, |_, _| 9);
        // A 1-bit tile whose width is not a multiple of 8.
        let p = payload(&[record(0, 2, 4, 4, Some(&[0xFF, 0xFF]))]);
        assert!(decode(&p, 1, 4, 4).unwrap_err().contains("not enough image data"));
        // Data that is not a zlib stream.
        let mut p = payload(&[record(0, 16, 4, 4, Some(&tile))]);
        let n = p.len();
        p[n - 39] ^= 0xFF; // the last byte of the zlib stream (its checksum)
        assert!(decode(&p, 1, 4, 4).is_err());
        // No columns for a painted tile; none needed without one.
        let p = payload(&[record(0, 16, 4, 4, Some(&tile))]);
        assert!(decode(&p, 0, 4, 4).is_err());
        let d = decode(&payload(&[record(0, 16, 4, 4, None)]), 0, 4, 4).unwrap();
        assert_eq!((d.w, d.h), (0, 0));
        // An offscreen larger than its tile grid is fine.
        let d = decode(&p, 1, 600, 600).unwrap();
        assert_eq!((d.w, d.h), (4, 4));
    }
}

