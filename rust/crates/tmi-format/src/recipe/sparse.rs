//! The tight decode of `pixel-source::tiles`: the `LayerCrop` the full decode
//! (`decode_tiles` into a `width * height` canvas, then `LayerCrop::from_full`)
//! gives, byte for byte, without the canvas (PLUGINS.md "recipe の疎なデコード").
//!
//! - Tiles whose rectangles (cut to the bitmap) meet form a group, laid down
//!   in tile order with the full decode's own writes (`paste`, or
//!   `over_pixel`; the first tile of a group over zeros is a copy of its
//!   pixels whose alpha is not 0, which is what `over_pixel` gives there).
//!   Groups never share a pixel: they are decoded and laid down in parallel,
//!   each tile decoded once, straight into one buffer.
//! - The buffer is the crop itself when each tile's own box of what it
//!   leaves behind (alpha not 0 for `over`, any byte not 0 otherwise) adds
//!   up to the crop: `over` (every pixel `over_pixel` writes has a non-zero
//!   alpha), or no two tiles meeting. That union is found first from the
//!   tiles that can reach each edge (`edge_box`), whose pixels are kept for
//!   laying them down. Otherwise (`replace` with tiles laid over others,
//!   where a later tile may put zeros back) the buffer is the box the tiles
//!   cover, cut in place to its non-zero box at the end.
//! - Every tile the full decode decodes (a size, content that is not
//!   `empty`) is decoded, wherever it lies: a tile outside the bitmap still
//!   fails the import when its data is bad. The error is the one the full
//!   decode meets first in tile order (a tile's own decode error before its
//!   `reject` overlap); tiles after a known error are not decoded.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use rayon::prelude::*;
use tmi_import::crop::LayerCrop;

use super::{clip, corrupt, decode_tile, DecodeEnv, BATCH};
use crate::types::*;

type Rect = (usize, usize, usize, usize);

/// A group of at most this many tiles is decoded on one thread, tile by
/// tile (each decoded tile laid down and freed at once); a larger one a
/// batch at a time in parallel.
const SERIAL_GROUP: usize = 32;

/// A tile's rectangle inside a `w` x `h` bitmap (x0, y0, x1, y1).
fn rect_in(w: usize, h: usize, t: &Tile) -> Option<Rect> {
    let (_, _, dx, dy, cw, ch) = clip(w, h, t.width as usize, t.height as usize, t.x as i64, t.y as i64)?;
    Some((dx, dy, dx + cw, dy + ch))
}

/// Whether the full decode decodes `t` (`decode_tile` gives pixels).
fn decodes(t: &Tile) -> bool {
    t.width != 0 && t.height != 0 && !matches!(t.content, TileContent::Empty)
}

/// A `fill` whose writes leave zeros where nothing else is: transparent
/// under `over` (`over_pixel` skips it), all zero under `paste`.
fn leaves_nothing(t: &Tile, over: bool) -> bool {
    match t.content {
        TileContent::Fill(c) => c.a == 0 && (over || (c.r, c.g, c.b) == (0, 0, 0)),
        _ => false,
    }
}

fn meets(a: &Rect, b: &Rect) -> bool {
    a.0 < b.2 && b.0 < a.2 && a.1 < b.3 && b.1 < a.3
}

fn union(a: Option<Rect>, b: Rect) -> Rect {
    match a {
        None => b,
        Some(a) => (a.0.min(b.0), a.1.min(b.1), a.2.max(b.2), a.3.max(b.3)),
    }
}

/// Every pair `(i, j)`, `i < j`, of rectangles that meet.
fn meeting_pairs(rects: &[(usize, Rect)]) -> Vec<(usize, usize)> {
    let mut by_top: Vec<&(usize, Rect)> = rects.iter().collect();
    by_top.sort_unstable_by_key(|(i, r)| (r.1, r.0, *i));
    let mut pairs = Vec::new();
    for (k, (i, a)) in by_top.iter().enumerate() {
        for (j, b) in &by_top[k + 1..] {
            if b.1 >= a.3 {
                break;
            }
            if meets(a, b) {
                pairs.push(((*i).min(*j), (*i).max(*j)));
            }
        }
    }
    pairs
}

fn find(parent: &mut [usize], mut i: usize) -> usize {
    while parent[i] != i {
        parent[i] = parent[parent[i]];
        i = parent[i];
    }
    i
}

/// `bbox::bbox_any(buf, w, 4)`, its rows scanned in parallel.
fn nonzero_box(buf: &[u8], w: usize) -> Option<Rect> {
    let rows: Vec<Option<(usize, usize)>> = buf
        .par_chunks(w * 4)
        .map(|row| {
            let first = row.chunks_exact(4).position(|p| p != [0, 0, 0, 0])?;
            let last = row.chunks_exact(4).rposition(|p| p != [0, 0, 0, 0]).unwrap();
            Some((first, last + 1))
        })
        .collect();
    let y0 = rows.iter().position(Option::is_some)?;
    let y1 = rows.iter().rposition(Option::is_some).unwrap() + 1;
    let (x0, x1) = rows[y0..y1].iter().flatten().fold((usize::MAX, 0), |(a, b), &(f, l)| (a.min(f), b.max(l)));
    Some((x0, y0, x1, y1))
}

/// `buf` (rows of `w` pixels) cut to `b` where it lies, without a second
/// buffer: row y of the cut moves to y times its width, which is never past
/// where it comes from. Rows move in waves, in parallel: a wave is the rows
/// whose new places all end before the first of their old places begins.
pub(super) fn cut_in_place(buf: &mut Vec<u8>, w: usize, b: Rect) {
    let cw = (b.2 - b.0) * 4;
    let rows = b.3 - b.1;
    let from = |y: usize| ((y + b.1) * w + b.0) * 4;
    let ptr = OutPtr(buf.as_mut_ptr());
    let mut y = 0;
    while y < rows {
        let wave_end = (from(y) / cw).min(rows);
        if wave_end <= y + 1 {
            buf.copy_within(from(y)..from(y) + cw, y * cw);
            y += 1;
            continue;
        }
        (y..wave_end).into_par_iter().for_each(|r| {
            let ptr = ptr;
            // SAFETY: every row of the wave is written to [r * cw, (r + 1) * cw),
            // which ends at or before `from(y)`, and read from `from(r) >=
            // from(y)`: no write meets a read, and the writes are disjoint.
            unsafe { std::ptr::copy_nonoverlapping(ptr.0.add(from(r)), ptr.0.add(r * cw), cw) };
        });
        y = wave_end;
    }
    buf.truncate(cw * rows);
}

#[derive(Clone, Copy)]
struct OutPtr(*mut u8);
// SAFETY: the groups write pairwise disjoint pixels of the buffer (see `decode`).
unsafe impl Send for OutPtr {}
unsafe impl Sync for OutPtr {}

/// The first error in tile order among the tiles decoded so far; tiles after
/// it (or after `limit`, the first `reject` overlap) need not be decoded.
struct FirstError {
    index: AtomicUsize,
    error: Mutex<Option<(usize, ReadError)>>,
}

impl FirstError {
    fn skips(&self, i: usize) -> bool {
        i > self.index.load(Ordering::Relaxed)
    }

    fn note(&self, i: usize, e: ReadError) {
        let mut slot = self.error.lock().unwrap_or_else(|p| p.into_inner());
        if slot.as_ref().is_none_or(|(j, _)| i < *j) {
            *slot = Some((i, e));
            self.index.fetch_min(i, Ordering::Relaxed);
        }
    }
}

/// `decode_tiles(env, t, w, h)` cut by `LayerCrop::from_full`, exactly.
pub fn decode(env: &DecodeEnv, t: &Tiled, w: usize, h: usize) -> Result<LayerCrop, ReadError> {
    let tiles = &t.tiles;
    let over = t.overlap == Overlap::Over;
    let none = || Ok(LayerCrop { size: (w, h), rect: None, rgba: Vec::new() });
    // what the full decode places, cut to the bitmap
    let placed: Vec<(usize, Rect)> =
        tiles.iter().enumerate().filter(|(_, tile)| decodes(tile)).filter_map(|(i, tile)| Some((i, rect_in(w, h, tile)?))).collect();
    let pairs = meeting_pairs(&placed);
    let first_overlap = if t.overlap == Overlap::Reject { pairs.iter().map(|p| p.1).min() } else { None };

    // groups of tiles whose rectangles meet, each in tile order
    let mut parent: Vec<usize> = (0..tiles.len()).collect();
    for &(i, j) in &pairs {
        let (a, b) = (find(&mut parent, i), find(&mut parent, j));
        if a != b {
            parent[a.max(b)] = a.min(b);
        }
    }
    let mut groups: Vec<Vec<(usize, Rect)>> = Vec::new();
    let mut slot: Vec<Option<usize>> = vec![None; tiles.len()];
    for &(i, r) in &placed {
        let g = find(&mut parent, i);
        let s = *slot[g].get_or_insert_with(|| {
            groups.push(Vec::new());
            groups.len() - 1
        });
        groups[s].push((i, r));
    }
    // decoded for their errors only
    let mut is_placed = vec![false; tiles.len()];
    for &(i, _) in &placed {
        is_placed[i] = true;
    }
    let outside: Vec<usize> = (0..tiles.len()).filter(|&i| decodes(&tiles[i]) && !is_placed[i]).collect();

    let first = FirstError { index: AtomicUsize::new(first_overlap.unwrap_or(usize::MAX)), error: Mutex::new(None) };
    let decode_one = |i: usize| -> Option<Vec<u8>> {
        if first.skips(i) {
            return None;
        }
        match decode_tile(env, &tiles[i]) {
            Ok(px) => px,
            Err(e) => {
                first.note(i, e);
                None
            }
        }
    };

    // The buffer. When each tile's own box of what it leaves behind adds up
    // to the crop (`over`, or no tile laid over another), the crop is found
    // first, from the tiles on its edges, and is the buffer; otherwise the
    // box the tiles cover is, cut to its non-zero box at the end.
    let exact = over || groups.iter().all(|g| g.len() == 1);
    let mut cached: Vec<Option<Option<Vec<u8>>>> = Vec::new();
    let u = if exact {
        let (b, kept) = edge_box(tiles, &placed, over, &decode_one);
        cached = kept;
        b
    } else {
        placed.iter().filter(|&&(i, _)| !leaves_nothing(&tiles[i], over)).fold(None, |c, &(_, r)| Some(union(c, r)))
    };
    let u = u.unwrap_or((0, 0, 0, 0));
    let (uw, uh) = (u.2 - u.0, u.3 - u.1);
    let mut buf = vec![0u8; uw * uh * 4];
    let out = OutPtr(buf.as_mut_ptr());
    let pixels_of = |i: usize| -> Option<std::borrow::Cow<[u8]>> {
        match cached.get(i) {
            Some(Some(Some(px))) => Some(std::borrow::Cow::Borrowed(px)),
            // decoded already, leaves nothing (or failed)
            Some(Some(None)) => None,
            _ => decode_one(i).map(std::borrow::Cow::Owned),
        }
    };

    // `fresh`: the first tile of its group, laid over zeros, where `over_pixel`
    // gives the source pixel when its alpha is not 0 and leaves the zeros
    // otherwise (`over_pixel_onto_zero_is_the_source`)
    let lay = |i: usize, r: Rect, px: &[u8], fresh: bool| {
        let out = out;
        let tile = &tiles[i];
        let (x0, y0, x1, y1) = (r.0.max(u.0), r.1.max(u.1), r.2.min(u.2), r.3.min(u.3));
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        let tw = tile.width as usize;
        let n = (x1 - x0) * 4;
        for y in y0..y1 {
            let s = (((y as i64 - tile.y as i64) as usize) * tw + (x0 as i64 - tile.x as i64) as usize) * 4;
            let src = &px[s..s + n];
            let d = ((y - u.1) * uw + (x0 - u.0)) * 4;
            // SAFETY: the row [x0, x1) of y lies inside the buffer and inside
            // this tile's group's rectangles, which no other group meets.
            let dst = unsafe { std::slice::from_raw_parts_mut(out.0.add(d), n) };
            if over && fresh {
                for (dp, sp) in dst.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
                    if sp[3] != 0 {
                        dp.copy_from_slice(sp);
                    }
                }
            } else if over {
                for (dp, sp) in dst.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
                    tmi_pillow::over::over_pixel(dp, sp);
                }
            } else {
                dst.copy_from_slice(src);
            }
        }
    };
    rayon::join(
        || {
            outside.par_iter().for_each(|&i| {
                decode_one(i);
            })
        },
        || {
            groups.par_iter().for_each(|members| {
                if members.len() <= SERIAL_GROUP {
                    for (n, &(i, r)) in members.iter().enumerate() {
                        if let Some(px) = pixels_of(i) {
                            lay(i, r, &px, n == 0);
                        }
                    }
                    return;
                }
                for (b, batch) in members.chunks(BATCH).enumerate() {
                    let decoded: Vec<_> = batch.par_iter().map(|&(i, _)| pixels_of(i)).collect();
                    for (n, (&(i, r), px)) in batch.iter().zip(decoded).enumerate() {
                        if first.skips(i) {
                            return;
                        }
                        if let Some(px) = px {
                            lay(i, r, &px, b == 0 && n == 0);
                        }
                    }
                }
            })
        },
    );
    drop(cached);

    let error = first.error.into_inner().unwrap_or_else(|p| p.into_inner());
    match (error, first_overlap) {
        (Some((i, e)), k) if k.is_none_or(|k| i <= k) => return Err(e),
        (_, Some(k)) => {
            let tile = &tiles[k];
            return Err(corrupt(format!("tiles overlap at ({}, {}) and overlap is reject", tile.x, tile.y)));
        }
        _ => {}
    }
    if uw == 0 || uh == 0 {
        return none();
    }
    if exact {
        return Ok(LayerCrop { size: (w, h), rect: Some(u), rgba: buf });
    }
    let Some(b) = nonzero_box(&buf, uw) else { return none() };
    if b == (0, 0, uw, uh) {
        return Ok(LayerCrop { size: (w, h), rect: Some(u), rgba: buf });
    }
    cut_in_place(&mut buf, uw, b);
    Ok(LayerCrop { size: (w, h), rect: Some((u.0 + b.0, u.1 + b.1, u.0 + b.2, u.1 + b.3)), rgba: buf })
}

/// The box (bitmap coordinates) of what decoded tile `px` leaves inside `r`,
/// its rectangle in the bitmap: alpha not 0 (`over`), or any byte not 0.
fn contribution(px: &[u8], t: &Tile, r: &Rect, over: bool) -> Option<Rect> {
    let tw = t.width as usize;
    let (sx0, sy0) = ((r.0 as i64 - t.x as i64) as usize, (r.1 as i64 - t.y as i64) as usize);
    let cw = r.2 - r.0;
    let hit = |p: &[u8]| if over { p[3] != 0 } else { p != [0, 0, 0, 0] };
    let mut b: Option<Rect> = None;
    for row in 0..r.3 - r.1 {
        let s = ((sy0 + row) * tw + sx0) * 4;
        let line = &px[s..s + cw * 4];
        let Some(first) = line.chunks_exact(4).position(hit) else { continue };
        let last = line.chunks_exact(4).rposition(hit).unwrap();
        let y = r.1 + row;
        b = Some(union(b, (r.0 + first, y, r.0 + last + 1, y + 1)));
    }
    b
}

/// The union of every placed tile's `contribution`, decoding only the tiles
/// that can reach each edge: for the left edge, tiles in order of their
/// rectangle's left side, until that side is at or right of the best left
/// edge found (a tile leaves nothing left of its rectangle), and so on for
/// each side. The decoded tiles are kept for laying them down (Some(None):
/// decoded, nothing to lay down).
fn edge_box(
    tiles: &[Tile],
    placed: &[(usize, Rect)],
    over: bool,
    decode_one: &(dyn Fn(usize) -> Option<Vec<u8>> + Sync),
) -> (Option<Rect>, Vec<Option<Option<Vec<u8>>>>) {
    let mut kept: Vec<Option<Option<Vec<u8>>>> = vec![None; tiles.len()];
    let mut done = vec![false; tiles.len()];
    let mut boxes: Vec<Option<Rect>> = vec![None; tiles.len()];
    // each side as a quantity to minimise
    let sides: [fn(&Rect) -> i64; 4] = [|r| r.0 as i64, |r| r.1 as i64, |r| -(r.2 as i64), |r| -(r.3 as i64)];
    let chunk = rayon::current_num_threads().max(1) * 2;
    let mut found: Option<Rect> = None;
    for side in sides {
        let mut order: Vec<&(usize, Rect)> = placed.iter().collect();
        order.sort_by_key(|(i, r)| (side(r), *i));
        let mut best = found.map_or(i64::MAX, |b| side(&b));
        let mut at = 0;
        while at < order.len() && side(&order[at].1) < best {
            let mut end = at;
            while end < order.len() && end - at < chunk && side(&order[end].1) < best {
                end += 1;
            }
            let todo: Vec<(usize, Rect)> = order[at..end].iter().filter(|(i, _)| !done[*i]).map(|&&p| p).collect();
            let got: Vec<(usize, Option<Vec<u8>>, Option<Rect>)> = todo
                .par_iter()
                .map(|&(i, r)| {
                    let px = decode_one(i);
                    let b = px.as_ref().and_then(|px| contribution(px, &tiles[i], &r, over));
                    (i, px, b)
                })
                .collect();
            for (i, px, b) in got {
                done[i] = true;
                boxes[i] = b;
                // a tile that leaves nothing behind need not be laid down
                kept[i] = Some(px.filter(|_| b.is_some()));
            }
            for &&(i, _) in &order[at..end] {
                if let Some(b) = boxes[i] {
                    found = Some(union(found, b));
                    best = best.min(side(&b));
                }
            }
            at = end;
        }
    }
    (found, kept)
}
