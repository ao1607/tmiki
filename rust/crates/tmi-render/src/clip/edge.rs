//! フチ (EffectEdge, clip_style.py:374-446, stage 1b): an outline of the
//! layer's alpha dilated by a disk, under the layer. `_disk_max` is an exact
//! maximum (any order gives the same bytes); the fractional width is mixed in
//! float32 as numpy does.

use rayon::prelude::*;
use tmi_clip::model::Style;
use tmi_pillow::over::over_pixel;

use super::tone::section;
use super::Res;
use crate::buffer::{intersect, Buf, Rect};

/// Widths beyond this are declined (Python would grind through them).
const MAX_WIDTH: f64 = 1024.0;

#[derive(Clone, Copy, Debug)]
pub struct Edge {
    pub width: f64,
    pub rgb: [u8; 3],
}

impl Edge {
    /// `_edge_margin`: how far the outline reads and draws beyond the layer.
    pub fn margin(&self) -> i64 {
        self.width.ceil() as i64 + 1
    }
}

/// `_u32_to_u8` of a u32.
fn u8_of(v: u32) -> u8 {
    ((v as u64 * 255 + 0x7FFF_FFFF) / 0xFFFF_FFFF).min(255) as u8
}

/// `parse_edge_effect(layer_style(...))`: the enabled フチ, else None.
pub fn parse_edge_effect(style: &Style, main_id: i64) -> Res<Option<Edge>> {
    let Some(b) = style.get("LayerEffectInfo").blob()? else {
        return Ok(None);
    };
    if b.is_empty() || !style.get("LayerEffectAttached").truthy()? {
        return Ok(None);
    }
    let Some(p) = section(b, "EffectEdge") else {
        return Ok(None);
    };
    // struct.error in Python (raised outside the layer's try in _edge_margin)
    let f = b.get(p..p + 24).ok_or_else(|| format!("layer {main_id}: unreadable edge effect"))?;
    let u = |at: usize| u32::from_be_bytes(f[at..at + 4].try_into().unwrap());
    let width = f64::from_be_bytes(f[4..12].try_into().unwrap());
    if u(0) == 0 || width <= 0.0 {
        return Ok(None);
    }
    if !(width <= MAX_WIDTH) {
        return Err(format!("layer {main_id}: edge width {width}"));
    }
    Ok(Some(Edge { width, rgb: [u8_of(u(12)), u8_of(u(16)), u8_of(u(20))] }))
}

/// `_hmax` of one row, max-ed into `out`: max over [x - hw, x + hw] (zero
/// outside). `g` and `h` are scratch.
fn hmax_row_into(row: &[u8], hw: usize, out: &mut [u8], g: &mut Vec<u8>, h: &mut Vec<u8>) {
    let n = row.len();
    if hw == 0 {
        for (o, &v) in out.iter_mut().zip(row) {
            *o = (*o).max(v);
        }
        return;
    }
    // van Herk / Gil-Werman over the zero-padded row.
    let len = 2 * hw + 1;
    let padded = n + 2 * hw;
    g.clear();
    g.resize(padded, 0);
    h.clear();
    h.resize(padded, 0);
    g[hw..hw + n].copy_from_slice(row);
    h[hw..hw + n].copy_from_slice(row);
    for i in 1..padded {
        if i % len != 0 {
            g[i] = g[i].max(g[i - 1]);
        }
    }
    for i in (0..padded - 1).rev() {
        if (i + 1) % len != 0 {
            h[i] = h[i].max(h[i + 1]);
        }
    }
    for (x, o) in out.iter_mut().enumerate() {
        *o = (*o).max(h[x].max(g[x + len - 1]));
    }
}

/// `_disk_max(a, r)` of a w x h plane (see `apply_edge` for the r > h): the max
/// over the disk rows `dy`, each a horizontal max of half-width
/// `floor(sqrt(r² - dy²) + 1e-9)`.
fn disk_max(a: &[u8], w: usize, h: usize, r: i64) -> Vec<u8> {
    if r <= 0 {
        return a.to_vec();
    }
    let hws: Vec<usize> = (-r..=r).map(|dy| (((r * r - dy * dy) as f64).sqrt() + 1e-9).floor() as usize).collect();
    let mut distinct = hws.clone();
    distinct.sort_unstable();
    distinct.dedup();
    let which: Vec<usize> = hws.iter().map(|hw| distinct.iter().position(|d| d == hw).unwrap()).collect();
    let nonzero: Vec<bool> = a.chunks_exact(w).map(|row| row.iter().any(|&v| v != 0)).collect();
    // Blocks of output rows: each source row's horizontal maxima are made
    // once per block and half-width, then combined over the disk rows.
    let block = (4 * r as usize).max(64);
    let mut out = vec![0u8; w * h];
    out.par_chunks_mut(w * block).enumerate().for_each(|(b, dst)| {
        let y0 = b * block;
        let rows = dst.len() / w;
        let s0 = (y0 as i64 - r).max(0) as usize;
        let s1 = ((y0 + rows) as i64 + r).min(h as i64) as usize;
        let mut planes = vec![vec![0u8; (s1 - s0) * w]; distinct.len()];
        let (mut g, mut hh) = (Vec::new(), Vec::new());
        for sy in s0..s1 {
            if !nonzero[sy] {
                continue;
            }
            let row = &a[sy * w..(sy + 1) * w];
            for (k, &hw) in distinct.iter().enumerate() {
                hmax_row_into(row, hw, &mut planes[k][(sy - s0) * w..(sy - s0 + 1) * w], &mut g, &mut hh);
            }
        }
        for yy in 0..rows {
            let y = (y0 + yy) as i64;
            let o = &mut dst[yy * w..(yy + 1) * w];
            for (i, dy) in (-r..=r).enumerate() {
                let sy = y + dy;
                if sy < 0 || sy >= h as i64 || !nonzero[sy as usize] {
                    continue;
                }
                let src = &planes[which[i]][(sy as usize - s0) * w..(sy as usize - s0 + 1) * w];
                for (d, &s) in o.iter_mut().zip(src) {
                    *d = (*d).max(s);
                }
            }
        }
    });
    out
}

/// `apply_edge(buf, width, rgb, canvas_bounds)`: `buf` over its outline, or
/// `buf` itself where the outline leaves nothing.
pub fn apply_edge(buf: Buf, edge: &Edge, canvas_bounds: Rect) -> Res<Buf> {
    let r = edge.width.ceil() as i64;
    let grown = [buf.x - r, buf.y - r, buf.x + buf.w as i64 + r, buf.y + buf.h as i64 + r];
    let Some(bounds) = intersect(grown, canvas_bounds) else {
        return Ok(buf);
    };
    let mut canvas = Buf::filled(bounds, [0, 0, 0, 0]);
    canvas.paste(&buf);
    let (w, h) = (canvas.w, canvas.h);
    let alpha: Vec<u8> = canvas.px.chunks_exact(4).map(|p| p[3]).collect();
    let lo = edge.width.floor() as i64;
    let frac = edge.width - lo as f64;
    let wide = frac > 1e-3;
    // `_disk_max` with h < r < 2h for some disk row (h >= 2) slices rows that
    // do not line up: ValueError in Python. (h == 1 is fine: only dy = 0
    // reaches a row.)
    let misaligned = |r: i64| r > h as i64 && h >= 2;
    if misaligned(lo) || (wide && misaligned(lo + 1)) {
        return Err("edge wider than its canvas".into());
    }
    let mut level: Vec<f32> = disk_max(&alpha, w, h, lo).into_iter().map(|v| v as f32).collect();
    if wide {
        let f = frac as f32;
        for (e, v) in level.iter_mut().zip(disk_max(&alpha, w, h, lo + 1)) {
            let x = v as f32 * f;
            // np.maximum
            if !(*e >= x) {
                *e = x;
            }
        }
    }
    let [cr, cg, cb] = edge.rgb;
    let mut out = Buf { x: bounds[0], y: bounds[1], w, h, px: vec![0u8; w * h * 4] };
    for ((o, &e), s) in out.px.chunks_exact_mut(4).zip(&level).zip(canvas.px.chunks_exact(4)) {
        o.copy_from_slice(&[cr, cg, cb, tmi_numpy::round::rint_to_u8_fast(e)]);
        over_pixel(o, s);
    }
    Ok(match out.trim() {
        Some(t) => t,
        None => buf,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn brute_hmax(row: &[u8], hw: usize) -> Vec<u8> {
        (0..row.len())
            .map(|x| {
                let a = x.saturating_sub(hw);
                let b = (x + hw + 1).min(row.len());
                *row[a..b].iter().max().unwrap()
            })
            .collect()
    }

    /// `_disk_max` written out: the max over every (dy, dx) of the disk.
    fn brute_disk(a: &[u8], w: usize, h: usize, r: i64) -> Vec<u8> {
        let mut out = vec![0u8; w * h];
        for y in 0..h as i64 {
            for x in 0..w as i64 {
                let mut m = 0u8;
                for dy in -r..=r {
                    let hw = (((r * r - dy * dy) as f64).sqrt() + 1e-9).floor() as i64;
                    for dx in -hw..=hw {
                        let (sx, sy) = (x + dx, y + dy);
                        if sx >= 0 && sy >= 0 && sx < w as i64 && sy < h as i64 {
                            m = m.max(a[(sy * w as i64 + sx) as usize]);
                        }
                    }
                }
                out[(y * w as i64 + x) as usize] = if r <= 0 { a[(y * w as i64 + x) as usize] } else { m };
            }
        }
        out
    }

    #[test]
    fn disk_max_is_the_disk_max() {
        let mut seed = 7u32;
        let mut next = || {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
            (seed >> 16) as u8
        };
        for (w, h) in [(1, 1), (7, 1), (13, 9), (40, 70), (3, 150)] {
            let a: Vec<u8> = (0..w * h).map(|_| if next() < 60 { next() } else { 0 }).collect();
            for r in [0i64, 1, 2, 3, 5, 9, 17] {
                assert_eq!(disk_max(&a, w, h, r), brute_disk(&a, w, h, r), "{w}x{h} r {r}");
            }
        }
    }

    #[test]
    fn hmax_is_the_window_max() {
        let row: Vec<u8> = (0..97u32).map(|i| ((i * 37 + 11) % 251) as u8).collect();
        let (mut g, mut h) = (Vec::new(), Vec::new());
        for hw in 0..120 {
            let mut out = vec![0u8; row.len()];
            hmax_row_into(&row, hw, &mut out, &mut g, &mut h);
            assert_eq!(out, brute_hmax(&row, hw), "hw {hw}");
        }
    }
}
