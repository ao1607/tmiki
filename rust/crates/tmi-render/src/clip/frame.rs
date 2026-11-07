//! コマ枠 folders (tmiref/clip_frame.py): the children clipped to the
//! frame interior and the border drawn on top, bit for bit.
//!
//! The scanline rasteriser (`_rasterize`) works row by row: a pixel row's
//! coverage comes from its four sub-rows' crossings (the winding count
//! returns to 0 at the end of every sub-row, the polygons being closed) and
//! one accumulator row. So a band computes only its own rows, with the
//! accumulations in numpy's order (`np.add.at` in four passes, then
//! `cumsum`), and gets the rows of Python's one rasterisation of the whole
//! shape. The border's painter's-order composite is per pixel as well.

use tmi_clip::model::Node;
use tmi_clip::vector::FrameObject;
use tmi_numpy::py_hypot;
use tmi_pillow::chops::multiply;
use tmi_pillow::over_pixel;

use super::{Ctx, Res};
use crate::buffer::{intersect, size, union, Buf, Layer, Rect};

const SUBROWS: i64 = 4;
const MITER_LIMIT: f64 = 10.0;

type Poly = Vec<(f64, f64)>;

/// `_signed_area`.
fn signed_area(poly: &[(f64, f64)]) -> f64 {
    let n = poly.len();
    let mut s = 0.0;
    for k in 0..n {
        let (ax, ay) = poly[k];
        let (bx, by) = poly[(k + 1) % n];
        s += ax * by - bx * ay;
    }
    s / 2.0
}

/// `_poly_bounds` (pad 1).
fn poly_bounds(polys: &[&[(f64, f64)]]) -> Rect {
    let (mut x0, mut y0, mut x1, mut y1) = (f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY);
    for p in polys {
        for &(x, y) in p.iter() {
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x);
            y1 = y1.max(y);
        }
    }
    [(x0 - 1.0).floor() as i64, (y0 - 1.0).floor() as i64, (x1 + 1.0).ceil() as i64, (y1 + 1.0).ceil() as i64]
}

/// `_miter_offsets`.
fn miter_offsets(pts: &[(f64, f64)], w: f64) -> Vec<(f64, f64)> {
    let n = pts.len();
    let flip = signed_area(pts) < 0.0;
    let mut out = Vec::with_capacity(n);
    for k in 0..n {
        let (px, py) = pts[(k + n - 1) % n];
        let (cx, cy) = pts[k];
        let (nx, ny) = pts[(k + 1) % n];
        let e0 = (cx - px, cy - py);
        let e1 = (nx - cx, ny - cy);
        let l0 = match py_hypot(e0.0, e0.1) {
            v if v == 0.0 => 1.0,
            v => v,
        };
        let l1 = match py_hypot(e1.0, e1.1) {
            v if v == 0.0 => 1.0,
            v => v,
        };
        let mut n0 = (e0.1 / l0, -e0.0 / l0);
        let mut n1 = (e1.1 / l1, -e1.0 / l1);
        if flip {
            n0 = (-n0.0, -n0.1);
            n1 = (-n1.0, -n1.1);
        }
        let (mut mx, mut my) = (n0.0 + n1.0, n0.1 + n1.1);
        let denom = 1.0 + n0.0 * n1.0 + n0.1 * n1.1;
        if denom < 1e-6 {
            out.push((n0.0 * w, n0.1 * w));
            continue;
        }
        mx = mx * w / denom;
        my = my * w / denom;
        let ml = py_hypot(mx, my);
        if ml > MITER_LIMIT * w {
            mx = mx * MITER_LIMIT * w / ml;
            my = my * MITER_LIMIT * w / ml;
        }
        out.push((mx, my));
    }
    out
}

/// `_stroke_quads`.
fn stroke_quads(pts: &[(f64, f64)], w: f64) -> Vec<Poly> {
    let m = miter_offsets(pts, w);
    let n = pts.len();
    (0..n)
        .map(|k| {
            let (a, b) = (pts[k], pts[(k + 1) % n]);
            let (ma, mb) = (m[k], m[(k + 1) % n]);
            let mut q = vec![(a.0 + ma.0, a.1 + ma.1), (b.0 + mb.0, b.1 + mb.1), (b.0 - mb.0, b.1 - mb.1), (a.0 - ma.0, a.1 - ma.1)];
            if signed_area(&q) < 0.0 {
                q.reverse();
            }
            q
        })
        .collect()
}

/// `_rasterize(polys, bounds)` for the canvas rows `[ya, yb)` (inside
/// `bounds`): float32 coverage, `(yb - ya)` rows of the bounds' width.
fn rasterize(polys: &[&[(f64, f64)]], bounds: Rect, ya: i64, yb: i64) -> Vec<f32> {
    let [x0, y0, x1, _] = bounds;
    let w = (x1 - x0) as usize;
    let mut out = vec![0f32; w * (yb - ya).max(0) as usize];
    if w == 0 || yb <= ya {
        return out;
    }
    // (x0, y0, x1, y1, dir) relative to the bounds, as `_rasterize` builds them.
    let mut edges: Vec<(f64, f64, f64, f64, i32)> = Vec::new();
    for poly in polys {
        let n = poly.len();
        for k in 0..n {
            let (ax, ay) = poly[k];
            let (bx, by) = poly[(k + 1) % n];
            if ay == by {
                continue;
            }
            let (e0, f0, e1, f1) = (ax - x0 as f64, ay - y0 as f64, bx - x0 as f64, by - y0 as f64);
            edges.push((e0, f0, e1, f1, if f1 > f0 { 1 } else { -1 }));
        }
    }
    if edges.is_empty() {
        return out;
    }
    let step = 1.0 / SUBROWS as f64;
    let wf = w as f64;
    let mut cross: Vec<(f64, i32)> = Vec::new();
    let mut spans: Vec<(f64, f64)> = Vec::new();
    let mut acc = vec![0f64; w + 2];
    for row in (ya - y0)..(yb - y0) {
        spans.clear();
        for s in row * SUBROWS..(row + 1) * SUBROWS {
            let ys = (s as f64 + 0.5) * step;
            cross.clear();
            for &(e0, f0, e1, f1, d) in &edges {
                let (lo, hi) = if f0 < f1 { (f0, f1) } else { (f1, f0) };
                if ys >= lo && ys < hi {
                    let t = (ys - f0) / (f1 - f0);
                    cross.push((e0 + t * (e1 - e0), d));
                }
            }
            cross.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap()); // stable: ties keep edge order
            let mut wind = 0i64;
            for k in 0..cross.len().saturating_sub(1) {
                wind += cross[k].1 as i64;
                if wind != 0 {
                    let a = cross[k].0.max(0.0).min(wf);
                    let b = cross[k + 1].0.max(0.0).min(wf);
                    if b > a {
                        spans.push((a, b));
                    }
                }
            }
        }
        acc.iter_mut().for_each(|v| *v = 0.0);
        // np.add.at, pass by pass, spans in order.
        for &(a, _) in &spans {
            let ia = a.floor();
            acc[ia as usize] += (1.0 - (a - ia)) * step;
        }
        for &(a, _) in &spans {
            let ia = a.floor();
            acc[ia as usize + 1] += (a - ia) * step;
        }
        for &(_, b) in &spans {
            let ib = b.floor();
            acc[ib as usize] += -(1.0 - (b - ib)) * step;
        }
        for &(_, b) in &spans {
            let ib = b.floor();
            acc[ib as usize + 1] += -(b - ib) * step;
        }
        let dst = &mut out[(row + y0 - ya) as usize * w..(row + y0 - ya + 1) as usize * w];
        let mut c = 0.0f64;
        for (o, &v) in dst.iter_mut().zip(&acc[..w]) {
            c += v;
            *o = c.max(0.0).min(1.0) as f32;
        }
    }
    out
}

/// `_rasterize(polys, bounds)` whole (for the differential tests).
pub fn rasterize_all(polys: &[Vec<(f64, f64)>], bounds: Rect) -> Vec<f32> {
    let refs: Vec<&[(f64, f64)]> = polys.iter().map(|p| &p[..]).collect();
    rasterize(&refs, bounds, bounds[1], bounds[3])
}

/// `np.round(v * 255).astype(np.uint8)` of a float32 in [0, 1].
#[inline]
fn byte(v: f32) -> u8 {
    tmi_numpy::to_byte(v)
}

fn check(objs: &[FrameObject], node: &Node) -> Res<()> {
    let ok = |v: f64| v.is_finite() && v.abs() < 1e7;
    for o in objs {
        if !o.points.iter().all(|&(x, y)| ok(x) && ok(y)) || !o.half_width.is_finite() || o.half_width.abs() > 1e6 {
            return Err(format!("folder {}: odd comic frame values", node.main_id));
        }
    }
    Ok(())
}

impl Ctx<'_> {
    /// `apply_frame_to_group_buffer`: the folder's composited children
    /// (`buf`, trimmed, None when there are none) clipped to the frame and
    /// the border on top, over `window` of the `full` canvas.
    pub fn frame_folder(&self, node: &Node, buf: Option<Buf>, full: Rect, window: Rect) -> Res<Layer> {
        let objs = match self.doc.vectors.frames.get(&node.main_id) {
            None => &[][..],
            Some(Ok(o)) => &o[..],
            Some(Err(e)) => return Err(format!("folder {}: {e}", node.main_id)),
        };
        if objs.is_empty() {
            return Ok(match buf {
                Some(b) => Layer::Buf(b),
                None => Layer::Empty,
            });
        }
        check(objs, node)?;
        let mut parts: Vec<Buf> = Vec::new();
        if let Some(mut b) = buf {
            let mask = self.frame_mask(objs, b.bounds(), full);
            for (p, m) in b.px.chunks_exact_mut(4).zip(mask) {
                p[3] = multiply(p[3], m);
            }
            parts.push(b);
        }
        if let Some(border) = frame_border(objs, full, window) {
            parts.push(border);
        }
        let Some(u) = parts.iter().map(Buf::bounds).reduce(union) else {
            return Ok(Layer::Empty);
        };
        let mut out = Buf::filled(u, [0, 0, 0, 0]);
        for p in &parts {
            for y in p.y..p.y + p.h as i64 {
                let src = p.row(y, p.x, p.x + p.w as i64);
                let dst = out.row_mut(y, p.x, p.x + p.w as i64);
                for (d, s) in dst.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
                    over_pixel(d, s);
                }
            }
        }
        Ok(Layer::from_trimmed(out.trim()))
    }

    /// `frame_mask_crop` over `crop` (inside the canvas): 255 inside the
    /// union of the frame polygons.
    fn frame_mask(&self, objs: &[FrameObject], crop: Rect, full: Rect) -> Vec<u8> {
        let (cw, ch) = size(crop);
        let mut out = vec![0u8; cw * ch];
        let polys: Vec<Poly> = objs
            .iter()
            .map(|o| {
                let mut p = o.points.clone();
                if signed_area(&p) <= 0.0 {
                    p.reverse();
                }
                p
            })
            .collect();
        let refs: Vec<&[(f64, f64)]> = polys.iter().map(|p| &p[..]).collect();
        let Some(b) = intersect(poly_bounds(&refs), full) else {
            return out;
        };
        let Some(o) = intersect(b, crop) else {
            return out;
        };
        let cov = rasterize(&refs, b, o[1], o[3]);
        let bw = (b[2] - b[0]) as usize;
        for y in o[1]..o[3] {
            for x in o[0]..o[2] {
                out[(y - crop[1]) as usize * cw + (x - crop[0]) as usize] = byte(cov[(y - o[1]) as usize * bw + (x - b[0]) as usize]);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rasterize_rows_are_independent() {
        let tri: Vec<(f64, f64)> = vec![(2.25, 1.5), (17.5, 4.75), (6.0, 13.125)];
        let sq: Vec<(f64, f64)> = vec![(10.5, 8.0), (14.5, 8.0), (14.5, 12.0), (10.5, 12.0)];
        let polys = [&tri[..], &sq[..]];
        let b = [1, 0, 19, 15];
        let whole = rasterize(&polys, b, 0, 15);
        for (a, z) in [(0, 15), (3, 9), (8, 9), (14, 15)] {
            let part = rasterize(&polys, b, a, z);
            assert_eq!(part, whole[(a as usize) * 18..(z as usize) * 18]);
        }
        // a square's inside is covered, its half-pixel edges by half
        let sq = rasterize(&[&sq[..]], b, 9, 10);
        assert_eq!(sq[12 - 1], 1.0);
        assert_eq!(sq[10 - 1], 0.5);
        assert_eq!(sq[15 - 1], 0.0);
    }
}

/// `frame_border` cropped to `window` (`shared_frame_border` / the
/// windowed crop of clip_frame.py): None when it draws nothing there.
fn frame_border(objs: &[FrameObject], full: Rect, window: Rect) -> Option<Buf> {
    struct Part {
        quads: Vec<Poly>,
        b: Rect,
        color: [u8; 3],
        opacity: f32,
        pts: Poly,
    }
    let mut layers: Vec<Part> = Vec::new();
    for o in objs {
        let w = o.half_width;
        if w <= 0.0 {
            continue;
        }
        let quads = stroke_quads(&o.points, w);
        let refs: Vec<&[(f64, f64)]> = quads.iter().map(|q| &q[..]).collect();
        let Some(b) = intersect(poly_bounds(&refs), full) else {
            continue;
        };
        layers.push(Part { quads, b, color: o.color, opacity: o.opacity as f32, pts: o.points.clone() });
    }
    let ub = layers.iter().map(|l| l.b).reduce(union)?;
    let v = intersect(ub, window)?;
    let mut img = Buf::filled(v, [0, 0, 0, 0]);
    for (k, l) in layers.iter().enumerate() {
        if k > 0 {
            if let Some(pb) = intersect(poly_bounds(&[&l.pts[..]]), ub) {
                if let Some(o) = intersect(pb, v) {
                    let cov = rasterize(&[&l.pts[..]], pb, o[1], o[3]);
                    let pw = (pb[2] - pb[0]) as usize;
                    for y in o[1]..o[3] {
                        let row = img.row_mut(y, o[0], o[2]);
                        for (i, px) in row.chunks_exact_mut(4).enumerate() {
                            let c = cov[(y - o[1]) as usize * pw + (o[0] - pb[0]) as usize + i];
                            px[3] = multiply(px[3], byte(1.0f32 - c));
                        }
                    }
                }
            }
        }
        let Some(o) = intersect(l.b, v) else {
            continue;
        };
        let (bw, oh) = ((l.b[2] - l.b[0]) as usize, (o[3] - o[1]) as usize);
        let mut cov = vec![0f32; bw * oh];
        for q in &l.quads {
            let Some(qb) = intersect(poly_bounds(&[&q[..]]), l.b) else {
                continue;
            };
            let Some(qo) = intersect(qb, [l.b[0], o[1], l.b[2], o[3]]) else {
                continue;
            };
            let part = rasterize(&[&q[..]], qb, qo[1], qo[3]);
            let qw = (qb[2] - qb[0]) as usize;
            for y in qo[1]..qo[3] {
                for x in qb[0]..qb[2] {
                    let at = (y - o[1]) as usize * bw + (x - l.b[0]) as usize;
                    cov[at] += part[(y - qo[1]) as usize * qw + (x - qb[0]) as usize];
                }
            }
        }
        for y in o[1]..o[3] {
            let row = img.row_mut(y, o[0], o[2]);
            for (i, px) in row.chunks_exact_mut(4).enumerate() {
                let c = cov[(y - o[1]) as usize * bw + (o[0] - l.b[0]) as usize + i];
                let a = byte(c.max(0.0).min(1.0) * l.opacity);
                over_pixel(px, &[l.color[0], l.color[1], l.color[2], a]);
            }
        }
    }
    // CSP draws the border without antialiasing: alpha cut at 128
    for p in img.px.chunks_exact_mut(4) {
        p[3] = if p[3] >= BORDER_ALPHA_THRESHOLD { 255 } else { 0 };
    }
    Some(img)
}

/// `clip_frame.BORDER_ALPHA_THRESHOLD`.
const BORDER_ALPHA_THRESHOLD: u8 = 128;
