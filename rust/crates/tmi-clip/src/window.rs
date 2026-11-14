//! Partial decodes of an offscreen: the tile records a window needs
//! (`_offscreen_tile_index` / `_offscreen_tiles_in_box`, clip.py:1010-1083),
//! a structural check of a whole offscreen without inflating it, and layer
//! mask tiles (`iter_tiles` + `_tile_alpha`, clip.py:409-441, :2633-2650).

use crate::tiles::{be_u32, be_u64, begin_name_utf16be, inflate_exact, one_bit_rows_ok, py_slice, scan_tiles, tile_kind, TILE_SIZE};

/// `_offscreen_tile_index`: the payload's first 48 bytes, its inner stream
/// and the BlockDataBeginChunk records `(block, offset, size)` in file order.
pub struct TileIndex<'a> {
    pub header: &'a [u8],
    pub inner: &'a [u8],
    pub records: Vec<(usize, usize, usize)>,
}

pub fn tile_index(payload: &[u8]) -> Option<TileIndex<'_>> {
    if payload.get(8..16)? != b"extrnlid" {
        return None;
    }
    let inner_size = be_u64(payload, 48)?;
    let inner = py_slice(payload, 56, 56usize.saturating_add(usize::try_from(inner_size).ok()?));
    let begin = begin_name_utf16be();
    let mut records = Vec::new();
    let mut p = 0usize;
    while p + 8 <= inner.len() {
        let rec_size = be_u32(inner, p)?;
        let name_len = be_u32(inner, p + 4)?;
        if name_len * 2 + 8 > rec_size || p + rec_size > inner.len() {
            break;
        }
        if rec_size >= 50 && inner[p + 8..p + 8 + 2 * name_len] == begin[..] {
            records.push((be_u32(inner, p + 46)?, p, rec_size));
        }
        p += rec_size;
    }
    Some(TileIndex { header: &payload[..48], inner, records })
}

/// `_offscreen_tiles_in_box`: a stand-in payload holding only the records
/// whose 256-pixel cell overlaps `bx` (offscreen pixels; with `invert`, the
/// records that do not), in file order. None when no record is picked.
pub fn subset_payload(index: &TileIndex<'_>, tile_cols: usize, bx: [i64; 4], invert: bool) -> Option<Vec<u8>> {
    let cols = tile_cols.max(1) as i64;
    let t = TILE_SIZE as i64;
    let first = bx[1].div_euclid(t) - 1;
    let last = (bx[3] - 1).div_euclid(t);
    let mut body = Vec::new();
    let mut any = false;
    for &(block, off, size) in &index.records {
        let block = block as i64;
        let row = block / cols;
        if !invert && !(first <= row && row <= last) {
            continue;
        }
        let tx = (block % cols) * t;
        let ty = row * t;
        let hit = tx < bx[2] && ty < bx[3] && tx + t > bx[0] && ty + t > bx[1];
        if hit != invert {
            body.extend_from_slice(&index.inner[off..off + size]);
            any = true;
        }
    }
    if !any {
        return None;
    }
    let mut out = Vec::with_capacity(56 + body.len());
    out.extend_from_slice(index.header);
    out.extend_from_slice(&(body.len() as u64).to_be_bytes());
    out.extend_from_slice(&body);
    Some(out)
}

/// Whether windows of this payload (`subset_payload`) decode to the crops
/// of its whole decode, short of inflating its tiles: the records scan,
/// a painted tile has a column to sit in, and every tile stays inside its
/// own 256-pixel cell (a window picks tiles by cell; tiles of one cell,
/// repeated blocks included, come along together in file order). Whether
/// each tile inflates is seen when it is decoded.
pub fn windows_decode_like_whole(payload: &[u8], tile_cols: usize) -> bool {
    let Some(tiles) = scan_tiles(payload) else {
        return false;
    };
    (tile_cols > 0 || tiles.is_empty()) && tiles.iter().all(|t| t.tile_w <= TILE_SIZE && t.tile_h <= TILE_SIZE)
}

/// One painted tile of a layer mask: its alpha plane at (x, y); empty for a
/// tile of an unknown size, which is 0 over its `w` x `h`.
pub struct MaskTile {
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
    pub alpha: Vec<u8>,
}

fn utf16_ok(name: &[u8]) -> bool {
    let units: Vec<u16> = name.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
    String::from_utf16(&units).is_ok()
}

/// The painted tiles of a mask offscreen, in file order, as
/// `_render_mask_attenuation` collects them, and the unknown tile sizes
/// `(bytes per tile, pixels)` among them (warned about; their alpha is 0).
/// `Err` wherever Python raises (malformed records, zlib errors, 1-bit
/// tiles whose width is not a multiple of 8): the mask is unreadable.
///
/// A tile of an unknown size holds no alpha (`alpha` is empty: 0 over its
/// rectangle), and the tiles kept are held to twice the 256-pixel cells of
/// the `pixel_w` x `pixel_h` offscreen (CSP writes one tile a cell).
pub fn mask_tiles(payload: &[u8], tile_cols: usize, pixel_w: usize, pixel_h: usize) -> Result<(Vec<MaskTile>, Vec<(usize, usize)>), String> {
    mask_tiles_opt(payload, tile_cols, pixel_w, pixel_h).ok_or_else(|| "malformed mask tiles".to_string())?
}

fn mask_tiles_opt(payload: &[u8], tile_cols: usize, pixel_w: usize, pixel_h: usize) -> Option<Result<(Vec<MaskTile>, Vec<(usize, usize)>), String>> {
    if payload.get(8..16)? != b"extrnlid" {
        return None;
    }
    let inner_size = be_u64(payload, 48)?;
    let inner = py_slice(payload, 56, 56usize.saturating_add(usize::try_from(inner_size).ok()?));
    let begin = begin_name_utf16be();
    let mut raw = Vec::new();
    let mut p = 0usize;
    while p + 8 <= inner.len() {
        let rec_size = be_u32(inner, p)?;
        let name_len = be_u32(inner, p + 4)?;
        if name_len * 2 + 8 > rec_size || p + rec_size > inner.len() {
            break;
        }
        let name = &inner[p + 8..p + 8 + 2 * name_len];
        if name != begin.as_slice() {
            if !utf16_ok(name) {
                return None;
            }
            p += rec_size;
            continue;
        }
        let body = py_slice(inner, p + 46, p + rec_size);
        let block = be_u32(body, 0)?;
        let bpt = be_u32(body, 4)?;
        let tw = be_u32(body, 8)?;
        let th = be_u32(body, 12)?;
        let has_data = be_u32(body, 16)?;
        if has_data != 0 {
            let size = be_u32(body, 20)?;
            let blob = py_slice(body, 24, 24 + size);
            raw.push((block, bpt, tw, th, py_slice(blob, 4, blob.len())));
        }
        p += rec_size;
    }
    if raw.is_empty() {
        return Some(Ok((Vec::new(), Vec::new())));
    }
    if tile_cols == 0 {
        return Some(Err("tiles but no tile columns (ZeroDivisionError)".into()));
    }
    let cells = |n: usize| n.div_ceil(TILE_SIZE) as u128 * TILE_SIZE as u128;
    let held: u128 = raw
        .iter()
        .filter(|&&(_, bpt, tw, th, _)| tw.checked_mul(th).is_some_and(|px| tile_kind(bpt, px).is_some()))
        .map(|&(_, _, tw, th, _)| tw as u128 * th as u128)
        .sum();
    if held > 2 * cells(pixel_w) * cells(pixel_h) {
        return Some(Err(format!("mask tiles cover the {pixel_w}x{pixel_h} offscreen more than twice")));
    }
    // Sequential: masks are decoded once for every band waiting on them, and
    // a rayon wait here could run one of those bands on this very thread.
    let mut unknown = Vec::new();
    let mut tiles = Vec::with_capacity(raw.len());
    for &(block, bpt, tw, th, compressed) in &raw {
        let Some(pixels) = tw.checked_mul(th) else {
            return Some(Err("tile size overflows".into()));
        };
        let Some(data) = inflate_exact(compressed, bpt) else {
            return Some(Err(format!("tile {block} does not inflate to {bpt} bytes")));
        };
        let alpha = match tile_kind(bpt, pixels) {
            None => {
                // `_tile_to_image(tile).getchannel("A")`: warned, transparent.
                unknown.push((bpt, pixels));
                Vec::new()
            }
            Some(kind) if !one_bit_rows_ok(kind, tw, th) => {
                return Some(Err(format!("1-bit tile {block} is {tw} pixels wide: not enough image data")));
            }
            Some(1 | 2) => {
                let row_bytes = tw / 8;
                let mut out = vec![0u8; pixels];
                for y in 0..th {
                    for x in 0..tw {
                        if data[y * row_bytes + x / 8] & (0x80 >> (x % 8)) != 0 {
                            out[y * tw + x] = 255;
                        }
                    }
                }
                out
            }
            Some(_) => data[..pixels].to_vec(),
        };
        tiles.push(MaskTile { x: (block % tile_cols) * TILE_SIZE, y: (block / tile_cols) * TILE_SIZE, w: tw, h: th, alpha });
    }
    Some(Ok((tiles, unknown)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tiles::tests::{payload, record};

    #[test]
    fn mask_tiles_report_unknown_sizes_and_unreadable_data() {
        let p = payload(&[record(0, 4, 2, 2, Some(&[1, 2, 3, 4])), record(1, 3, 2, 2, Some(&[9, 9, 9]))]);
        let (tiles, unknown) = mask_tiles(&p, 2, 512, 256).unwrap();
        assert_eq!(unknown, vec![(3, 4)]);
        assert_eq!(tiles[0].alpha, vec![1, 2, 3, 4]);
        // (an unknown size holds no alpha: 0 over its rectangle)
        assert_eq!((tiles[1].x, tiles[1].w, tiles[1].alpha.clone()), (256, 2, vec![]));
        assert!(mask_tiles(&p, 0, 512, 256).is_err());
        let odd = payload(&[record(0, 2, 4, 4, Some(&[0xFF, 0xFF]))]);
        assert!(mask_tiles(&odd, 1, 4, 4).is_err());
        // An empty tile unpacks nothing, whatever its width.
        let (tiles, _) = mask_tiles(&payload(&[record(0, 0, 12, 0, Some(&[]))]), 1, 12, 1).unwrap();
        assert!(tiles[0].alpha.is_empty());
    }

    /// A 100-byte mask tile of an unknown size declaring 2^30 x 2^30 pixels:
    /// warned about and held as nothing, not allocated (2^60 bytes).
    #[test]
    fn a_mask_tile_of_an_unknown_size_declaring_exabytes_is_not_allocated() {
        let p = payload(&[record(0, 0, 1 << 30, 1 << 30, Some(&[]))]);
        let (tiles, unknown) = mask_tiles(&p, 1, 256, 256).unwrap();
        assert_eq!(unknown, vec![(0, 1 << 60)]);
        assert!(tiles[0].alpha.is_empty());
        // and known tiles repeated over the mask more than twice are refused
        let many = payload(&vec![record(0, 4, 2, 2, Some(&[1, 2, 3, 4])); 1 << 16]);
        assert!(mask_tiles(&many, 1, 256, 256).err().expect("refused").contains("more than twice"));
    }

    #[test]
    fn windows_need_tiles_inside_their_cells() {
        let small = payload(&[record(0, 4, 2, 2, Some(&[1; 4])), record(0, 4, 2, 2, Some(&[2; 4]))]);
        assert!(windows_decode_like_whole(&small, 1));
        assert!(!windows_decode_like_whole(&small, 0));
        let wide = payload(&[record(0, 257, 257, 1, Some(&[1; 257]))]);
        assert!(!windows_decode_like_whole(&wide, 2));
    }
}
