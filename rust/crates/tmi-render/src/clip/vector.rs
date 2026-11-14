//! ベクター layers (tmiref/clip_vector.py): the strokes re-stroked from
//! their vector data, as Python draws them, bit for bit.
//!
//! * The plan (`_vector_plan`, `_layer_samples`, `_trim_end`): the samples
//!   of every stroke's spline in float64, in numpy's operation order. It is
//!   computed once per render and shared by the bands: the arc lengths are
//!   summed across all strokes of the layer (one `cumsum`), so a stroke's
//!   samples depend on the strokes before it.
//! * The drawing (`_draw_stroke`, `_soften`, `render_vector_layer`): each
//!   stroke is supersampled with Pillow's polygon / ellipse fill, reduced,
//!   softened in float32 and composited over the earlier strokes. Every
//!   step is local to a pixel's row and its two neighbours, and a stroke
//!   leaves the pixels outside its own rectangle alone, so a band draws only
//!   the strokes that reach it, and only the rows it needs: the same bytes
//!   as Python's one drawing of the whole layer cropped to the band
//!   (clip.py `_render_vector_buffer` and `render_vector_layer(window=)`
//!   rely on the same fact).

use std::sync::Arc;

use tmi_clip::model::Node;
use tmi_clip::vector::{Brush, Pressure, Stroke};
use tmi_numpy::sum::{add_reduceat, cumsum};
use tmi_pillow::chops::multiply;
use tmi_pillow::draw::{coord_ok, ellipse, polygon_with, LCanvas, PolyScratch};
use tmi_pillow::over_pixel;
use tmi_pillow::reduce::{from_sum, multiplier};

use super::{memo, Ctx, Res};
use crate::buffer::{intersect, Buf, Layer, Rect};

const WIDTH_K: f64 = 1.98;
const WIDTH_B: f64 = -1.0;
const LEGACY_K: f64 = 1.77;
const PEN_FLAG: u32 = 0x2000;
const EDGE_BLUR: f64 = 0.08;
const DAB_SPACING: f64 = 0.16;
const ERASE_MODE: i64 = 27;
const CORNER_FLAG: u32 = 0x1;
const WIDTH_FIXED_FLAG: u32 = 0x1000;
const HAIR_K: f64 = 1.18;
const HAIR_R: f64 = 0.8;
const MONO_SHIFT: [f64; 2] = [-0.5, 0.5];
const CURVE_SAMPLES: usize = 256;
const SS: i64 = 4;
const TOL: f64 = 0.05;
const MAX_STEP: f64 = 0.75;
const JOIN_GAP: f64 = 0.09;

/// One stroke ready to draw (`vector_plan`'s tuples).
pub struct Planned {
    pub xy: Vec<[f64; 2]>,
    pub r: Vec<f64>,
    pub blur: bool,
    /// `_stroke_bounds` within the canvas: the rectangle it draws over.
    pub reach: Rect,
    pub color: [u8; 3],
    pub opacity: f64,
    pub erase: bool,
}

pub type Plan = Vec<Planned>;

/// `np.hypot` (libm's `hypot`, as numpy calls it).
#[inline]
fn hypot(x: f64, y: f64) -> f64 {
    x.hypot(y)
}

/// `_radius`; `hair` (mono layers): a pen stroke never drops below the
/// hairline floor min(HAIR_K * w, HAIR_R).
#[inline]
fn radius(w: f64, pen: bool, hair: bool) -> f64 {
    if pen {
        let body = 0.5 * (WIDTH_K * w + WIDTH_B).max(0.0);
        if hair {
            body.max((HAIR_K * w).min(HAIR_R))
        } else {
            body
        }
    } else {
        0.5 * LEGACY_K * w
    }
}

/// `_curve`: the effector graph's control polygon read as a clamped
/// uniform quadratic B-spline, sampled for `interp` (numpy's operation
/// order: `linspace`, then `(1-t)**2*a + 2*(1-t)*t*b + t*t*c`).
fn curve(x: &[f64], y: &[f64]) -> (Vec<f64>, Vec<f64>) {
    let n = x.len();
    if n < 3 {
        return (x.to_vec(), y.to_vec());
    }
    let k = n - 2;
    let (mut px, mut py) = (Vec::new(), Vec::new());
    for s in 0..k {
        let a = [x[s], y[s]];
        let b = [x[s + 1], y[s + 1]];
        let c = [x[s + 2], y[s + 2]];
        let start = if s == 0 { a } else { [(a[0] + b[0]) / 2.0, (a[1] + b[1]) / 2.0] };
        let end = if s == k - 1 { c } else { [(b[0] + c[0]) / 2.0, (b[1] + c[1]) / 2.0] };
        let endpoint = s == k - 1;
        let div = if endpoint { CURVE_SAMPLES - 1 } else { CURVE_SAMPLES } as f64;
        let step = 1.0 / div;
        for i in 0..CURVE_SAMPLES {
            let t = if endpoint && i == CURVE_SAMPLES - 1 { 1.0 } else { i as f64 * step + 0.0 };
            let u = 1.0 - t;
            let q = [0, 1].map(|d| u * u * start[d] + 2.0 * u * t * b[d] + t * t * end[d]);
            px.push(q[0]);
            py.push(q[1]);
        }
    }
    (px, py)
}

/// `_dab_size`.
fn dab_size(s: &Stroke, b: &Brush) -> Vec<f64> {
    let p: Vec<f64> = s.f.iter().map(|f| f[0].max(0.0).min(1.0)).collect();
    let mut k: Vec<f64> = match &b.pressure {
        Pressure::Constant => vec![1.0; p.len()],
        Pressure::Linear => p.clone(),
        Pressure::Curve(mn, g) => {
            let gv = match g {
                Some((gx, gy)) => {
                    let (cx, cy) = curve(gx, gy);
                    tmi_numpy::interp(&p, &cx, &cy)
                }
                None => p.clone(),
            };
            let one_minus = 1.0 - mn;
            gv.iter().map(|v| mn + one_minus * v).collect()
        }
    };
    let t = if 0.0 < b.thick && b.thick < 1.0 { b.thick } else { 1.0 };
    // 線幅修正 points: f5 carries the size factor, and the thickness is in it
    // too (applied once more below)
    for (kv, &fl) in k.iter_mut().zip(&s.pflags) {
        if fl & WIDTH_FIXED_FLAG != 0 {
            *kv = t;
        }
    }
    k.iter().zip(&s.f).map(|(kv, f)| s.size * kv * t * f[4] * f[5]).collect()
}

/// `_stroke_bounds` within `bounds`.
fn stroke_bounds(xy: &[[f64; 2]], r: &[f64], bounds: Rect) -> Option<Rect> {
    if xy.is_empty() || !r.iter().any(|&v| v > 0.02) {
        return None;
    }
    let rmax = r.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let (mut minx, mut miny, mut maxx, mut maxy) = (f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY);
    for p in xy {
        minx = minx.min(p[0]);
        miny = miny.min(p[1]);
        maxx = maxx.max(p[0]);
        maxy = maxy.max(p[1]);
    }
    let b = [
        (minx - rmax - 1.0).floor() as i64,
        (miny - rmax - 1.0).floor() as i64,
        (maxx + rmax + 1.0).ceil() as i64,
        (maxy + rmax + 1.0).ceil() as i64,
    ];
    intersect(b, bounds)
}

struct Samples {
    xy: Vec<[f64; 2]>,
    r: Vec<f64>,
    /// Each stroke's slice of `xy` / `r`.
    cuts: Vec<(usize, usize)>,
}

/// The most spline samples one layer's strokes may take (about 100 bytes
/// each while planned). A segment takes up to two a pixel of its length, so
/// this is some 30 million pixels of strokes; a file's widths that jump by
/// millions between points ask for billions.
const MAX_SAMPLES: usize = 1 << 26;

/// `_layer_samples` (Err past `MAX_SAMPLES`).
fn layer_samples(strokes: &[&Stroke], wd_per: &[Vec<f64>], ox: i64, oy: i64, hair: bool) -> Res<Samples> {
    let sizes: Vec<usize> = strokes.iter().map(|s| s.xy.len()).collect();
    let mut p: Vec<[f64; 2]> = Vec::new();
    let mut wd: Vec<f64> = Vec::new();
    let mut corner: Vec<bool> = Vec::new();
    let mut sid_pt: Vec<usize> = Vec::new();
    for (k, s) in strokes.iter().enumerate() {
        for (i, xy) in s.xy.iter().enumerate() {
            p.push([xy[0] + ox as f64, xy[1] + oy as f64]);
            corner.push(s.pflags[i] & CORNER_FLAG != 0);
            sid_pt.push(k);
        }
        wd.extend_from_slice(&wd_per[k]);
    }
    let pen: Vec<bool> = strokes.iter().map(|s| s.flags & PEN_FLAG != 0).collect();
    let rr: Vec<f64> = wd.iter().zip(&sid_pt).map(|(&w, &k)| radius(w, pen[k], hair)).collect();
    let mut start = Vec::with_capacity(sizes.len());
    let mut acc = 0usize;
    for &n in &sizes {
        start.push(acc);
        acc += n;
    }
    let end: Vec<usize> = start.iter().zip(&sizes).map(|(&a, &n)| a + n - 1).collect();

    // Segments: every point but its stroke's last.
    struct Seg {
        i: usize,
        sid: usize,
        p1: [f64; 2],
        p2: [f64; 2],
        m1: [f64; 2],
        m2: [f64; 2],
        m: usize,
    }
    let mut segs: Vec<Seg> = Vec::new();
    for i in 0..p.len() {
        let sid = sid_pt[i];
        if i == end[sid] {
            continue;
        }
        let mut im1 = (i as i64 - 1).max(start[sid] as i64) as usize;
        let mut ip2 = (i + 2).min(end[sid]);
        if corner[i] {
            im1 = i;
        }
        if corner[i + 1] {
            ip2 = i + 1;
        }
        let (p0, p1, p2, p3) = (p[im1], p[i], p[i + 1], p[ip2]);
        let (m1, m2) = if pen[sid] {
            centripetal(p0, p1, p2, p3)
        } else {
            (
                [0.5 * (p2[0] - p0[0]), 0.5 * (p2[1] - p0[1])],
                [0.5 * (p3[0] - p1[0]), 0.5 * (p3[1] - p1[1])],
            )
        };
        let c = [p2[0] - p1[0], p2[1] - p1[1]];
        let chord = hypot(c[0], c[1]);
        let dev = (hypot(m1[0] - c[0], m1[1] - c[1]) + hypot(m2[0] - c[0], m2[1] - c[1])) / 8.0;
        let mut m = (dev / TOL).sqrt().ceil();
        m = m.max(((rr[i + 1] - rr[i]).abs() / MAX_STEP).ceil());
        let hi = (1.0f64).max((chord / 0.5).ceil());
        m = m.max(1.0).min(hi);
        segs.push(Seg { i, sid, p1, p2, m1, m2, m: m as i64 as usize });
    }

    // The samples of every segment, all strokes at once.
    let total: usize = segs.iter().map(|s| s.m).fold(0usize, usize::saturating_add);
    if total > MAX_SAMPLES {
        return Err(format!("{total} stroke samples (at most {MAX_SAMPLES} a layer)"));
    }
    let mut xy: Vec<[f64; 2]> = Vec::with_capacity(total);
    let mut ts: Vec<f64> = Vec::with_capacity(total);
    let mut seg_of: Vec<usize> = Vec::with_capacity(total);
    let mut first: Vec<usize> = Vec::with_capacity(segs.len());
    for (g, s) in segs.iter().enumerate() {
        first.push(xy.len());
        for j in 0..s.m {
            let t = j as f64 / s.m as f64;
            let t2 = t * t;
            let t3 = t * t * t;
            let h00 = 2.0 * t3 - 3.0 * t2 + 1.0;
            let h10 = t3 - 2.0 * t2 + t;
            let h01 = -2.0 * t3 + 3.0 * t2;
            let h11 = t3 - t2;
            let v = [0, 1].map(|c| h00 * s.p1[c] + h10 * s.m1[c] + h01 * s.p2[c] + h11 * s.m2[c]);
            xy.push(v);
            ts.push(t);
            seg_of.push(g);
        }
    }
    let mut step = Vec::with_capacity(total);
    for k in 0..total {
        let last = k + 1 == total || seg_of[k + 1] != seg_of[k];
        let nxt = if last { segs[seg_of[k]].p2 } else { xy[k + 1] };
        step.push(hypot(nxt[0] - xy[k][0], nxt[1] - xy[k][1]));
    }
    let cum = cumsum(&step);
    let mut r_s = Vec::with_capacity(total);
    if !segs.is_empty() {
        let sums = add_reduceat(&step, &first);
        let seg_end: Vec<f64> = (0..segs.len())
            .map(|g| cum[first.get(g + 1).copied().unwrap_or(total) - 1])
            .collect();
        let seg_start: Vec<f64> = seg_end.iter().zip(&sums).map(|(e, s)| e - s).collect();
        for k in 0..total {
            let g = seg_of[k];
            let span = seg_end[g] - seg_start[g];
            let pen_s = pen[segs[g].sid];
            let u = if span > 1e-12 && pen_s { (cum[k] - step[k] - seg_start[g]) / span.max(1e-12) } else { ts[k] };
            let a = segs[g].i;
            r_s.push(radius(wd[a] * (1.0 - u) + wd[a + 1] * u, pen_s, hair));
        }
    }

    // Each stroke's samples (in order), then its last point.
    let mut out_xy = Vec::with_capacity(total + strokes.len());
    let mut out_r = Vec::with_capacity(total + strokes.len());
    let mut cuts = Vec::with_capacity(strokes.len());
    let mut k = 0usize;
    for (sid, &e) in end.iter().enumerate() {
        let a = out_xy.len();
        while k < total && segs[seg_of[k]].sid == sid {
            out_xy.push(xy[k]);
            out_r.push(r_s[k]);
            k += 1;
        }
        out_xy.push(p[e]);
        out_r.push(rr[e]);
        cuts.push((a, out_xy.len()));
    }
    Ok(Samples { xy: out_xy, r: out_r, cuts })
}

/// `_centripetal_tangents` for one segment.
fn centripetal(p0: [f64; 2], p1: [f64; 2], p2: [f64; 2], p3: [f64; 2]) -> ([f64; 2], [f64; 2]) {
    let sub = |a: [f64; 2], b: [f64; 2]| [a[0] - b[0], a[1] - b[1]];
    let (a, b, c) = (sub(p1, p0), sub(p2, p1), sub(p3, p2));
    let d0 = hypot(a[0], a[1]).sqrt();
    let d1 = hypot(b[0], b[1]).sqrt();
    let d2 = hypot(c[0], c[1]).sqrt();
    let eps = 1e-9;
    let s1 = d1.max(eps);
    let p20 = sub(p2, p0);
    let p31 = sub(p3, p1);
    let m1 = if d0 > eps {
        [0, 1].map(|k| d1 * (a[k] / d0.max(eps) - p20[k] / (d0 + d1).max(eps)) + b[k] * (d1 / s1))
    } else {
        [0.0, 0.0]
    };
    let m2 = if d2 > eps {
        [0, 1].map(|k| d1 * (c[k] / d2.max(eps) - p31[k] / (d1 + d2).max(eps)) + b[k] * (d1 / s1))
    } else {
        [0.0, 0.0]
    };
    (m1, m2)
}

/// `_trim_end`.
fn trim_end(s: &Stroke, w_last: f64, xy: &[[f64; 2]], r: &[f64]) -> (Vec<[f64; 2]>, Vec<f64>) {
    let keep = || (xy.to_vec(), r.to_vec());
    let Some(ph) = &s.phase else {
        return keep();
    };
    if xy.len() < 2 {
        return keep();
    }
    let spacing = DAB_SPACING * w_last;
    let recess = spacing - ph[ph.len() - 1];
    if !(0.0 < recess && recess < spacing) {
        return keep();
    }
    let seg: Vec<f64> = xy.windows(2).map(|w| hypot(w[1][0] - w[0][0], w[1][1] - w[0][1])).collect();
    let mut cum = vec![0.0];
    cum.extend(cumsum(&seg));
    let stop = cum[cum.len() - 1] - recess;
    if stop <= 0.0 {
        return keep();
    }
    let j = cum.partition_point(|&c| c <= stop); // searchsorted(side="right")
    let t = (stop - cum[j - 1]) / seg[j - 1].max(1e-12);
    let end_xy = [0, 1].map(|c| xy[j - 1][c] + t * (xy[j][c] - xy[j - 1][c]));
    let end_r = r[j - 1] + t * (r[j] - r[j - 1]);
    let mut out_xy = xy[..j].to_vec();
    out_xy.push(end_xy);
    let mut out_r = r[..j].to_vec();
    out_r.push(end_r);
    (out_xy, out_r)
}

/// `_vector_plan` of a layer's strokes over `canvas` (the full canvas);
/// `mono`: the layer's 表現色 is モノクロ (hairline floor, MONO_SHIFT).
pub fn plan(node: &Node, strokes: &[Stroke], brushes: &dyn Fn(u32) -> Res<Brush>, canvas: Rect, mono: bool) -> Res<Plan> {
    let strokes: Vec<&Stroke> = strokes.iter().filter(|s| s.size > 0.0 && !s.xy.is_empty()).collect();
    if strokes.is_empty() {
        return Ok(Vec::new());
    }
    let mut brush_of = Vec::with_capacity(strokes.len());
    for s in &strokes {
        let fin = |v: f64| v.is_finite() && v.abs() < 1e6;
        if !s.xy.iter().all(|p| fin(p[0]) && fin(p[1])) || !s.f.iter().all(|f| f[0].is_finite() && fin(f[4]) && fin(f[5])) {
            return Err(format!("layer {}: odd stroke values", node.main_id));
        }
        if let Some(ph) = &s.phase {
            if !ph.iter().all(|v| v.is_finite()) {
                return Err(format!("layer {}: odd dab phase", node.main_id));
            }
        }
        brush_of.push(brushes(s.brush)?);
    }
    let wd: Vec<Vec<f64>> = strokes.iter().zip(&brush_of).map(|(s, b)| dab_size(s, b)).collect();
    let mut samples = layer_samples(&strokes, &wd, node.offset_x, node.offset_y, mono).map_err(|e| format!("layer {}: {e}", node.main_id))?;
    if mono {
        for p in samples.xy.iter_mut() {
            p[0] += MONO_SHIFT[0];
            p[1] += MONO_SHIFT[1];
        }
    }
    let mut out = Vec::new();
    for (k, s) in strokes.iter().enumerate() {
        let (a, b) = samples.cuts[k];
        let pen = s.flags & PEN_FLAG != 0;
        let (sxy, sr) = if pen {
            trim_end(s, wd[k][wd[k].len() - 1], &samples.xy[a..b], &samples.r[a..b])
        } else {
            (samples.xy[a..b].to_vec(), samples.r[a..b].to_vec())
        };
        if !sxy.iter().all(|p| coord_ok(p[0]) && coord_ok(p[1])) || !sr.iter().all(|v| v.is_finite() && v.abs() < 1e6) {
            return Err(format!("layer {}: stroke out of range", node.main_id));
        }
        if let Some(reach) = stroke_bounds(&sxy, &sr, canvas) {
            out.push(Planned {
                xy: sxy,
                r: sr,
                blur: pen,
                reach,
                color: s.color,
                opacity: s.opacity,
                erase: brush_of[k].composite == ERASE_MODE,
            });
        }
    }
    Ok(out)
}

/// `_draw_stroke(xy, r, bounds, blur)` with the stroke's opacity table
/// applied (`render_vector_layer`'s `point`): the "L" coverage over the
/// stroke's rectangle and that rectangle, or None when it draws nothing.
/// For the differential tests; `blur` is EDGE_BLUR or none.
pub fn draw_stroke_coverage(xy: &[[f64; 2]], r: &[f64], bounds: Rect, blur: bool, opacity: f64) -> Option<(Vec<u8>, Rect)> {
    let reach = stroke_bounds(xy, r, bounds)?;
    let p = Planned { xy: xy.to_vec(), r: r.to_vec(), blur, reach, color: [0; 3], opacity, erase: false };
    Some((coverage(&p, reach).px, reach))
}

/// A stroke's coverage over a canvas rectangle: "L" rows, and per row the
/// columns `[a, b)` (canvas x) outside which it is 0.
struct Coverage {
    px: Vec<u8>,
    w: usize,
    ext: Vec<(i64, i64)>,
}

/// The coverage ("L", after reduce / soften / opacity) of `p` over the
/// canvas rectangle `want` (inside `p.reach`): `_draw_stroke` cropped.
///
/// Only the columns the drawing reached are reduced and softened: elsewhere
/// every step maps 0 to 0 (`reduce` of a blank block is 0, the blur of a
/// blank neighbourhood is 0, the opacity table keeps 0).
fn coverage(p: &Planned, want: Rect) -> Coverage {
    let [x0, y0, x1, y1] = p.reach;
    let (w, h) = (x1 - x0, y1 - y0);
    let rmax = p.r.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let mut ss = SS;
    while ss < 16 && rmax * (ss as f64) < 4.0 && (w as i128) * (h as i128) * ((2 * ss) as i128).pow(2) <= 1 << 26 {
        ss *= 2;
    }
    // Reduced rows / columns needed (the blur reads one more each side).
    let m = if p.blur { 1 } else { 0 };
    let (ox0, oy0, ox1, oy1) = (want[0] - x0, want[1] - y0, want[2] - x0, want[3] - y0);
    let (rx0, ry0, rx1, ry1) = ((ox0 - m).max(0), (oy0 - m).max(0), (ox1 + m).min(w), (oy1 + m).min(h));
    let region = [(rx0 * ss) as i32, (ry0 * ss) as i32, (rx1 * ss) as i32, (ry1 * ss) as i32];
    let mut c = LCanvas::new((w * ss) as i32, (h * ss) as i32, region);
    draw_stroke(&mut c, p, x0, y0, ss);

    // Image.reduce(ss) over the reached blocks (stroke-image coordinates).
    let n = ss as usize;
    let mult = multiplier(ss as u32);
    let rw = (rx1 - rx0) as usize;
    let held_w = rw * n;
    let mut red = vec![0u8; rw * (ry1 - ry0) as usize];
    let mut red_ext: Vec<(i64, i64)> = vec![(i64::MAX, i64::MIN); (ry1 - ry0) as usize];
    let mut sums: Vec<u32> = Vec::new();
    for ry in ry0..ry1 {
        let (mut a, mut b) = (i32::MAX, i32::MIN);
        for k in 0..n {
            let e = c.ext[(ry - ry0) as usize * n + k];
            a = a.min(e.0);
            b = b.max(e.1);
        }
        if a > b {
            continue;
        }
        let (ba, bb) = ((a as i64 / ss), (b as i64 / ss)); // blocks, inclusive
        sums.clear();
        sums.resize((bb - ba + 1) as usize, 0);
        for k in 0..n {
            let row = (ry - ry0) as usize * n + k;
            let line = &c.px[row * held_w..(row + 1) * held_w];
            let seg = &line[((ba - rx0) * ss) as usize..((bb - rx0 + 1) * ss) as usize];
            for (s, block) in sums.iter_mut().zip(seg.chunks_exact(n)) {
                *s += block.iter().map(|&v| v as u32).sum::<u32>();
            }
        }
        let dst = &mut red[(ry - ry0) as usize * rw..(ry - ry0 + 1) as usize * rw];
        for (x, &s) in (ba..=bb).zip(&sums) {
            dst[(x - rx0) as usize] = from_sum(s, ss as u32, mult);
        }
        red_ext[(ry - ry0) as usize] = (ba, bb + 1);
    }
    let at = |x: i64, y: i64| red[(y - ry0) as usize * rw + (x - rx0) as usize];

    let ow = (ox1 - ox0) as usize;
    let oh = (oy1 - oy0) as usize;
    let mut out = vec![0u8; ow * oh];
    let mut ext: Vec<(i64, i64)> = vec![(0, 0); oh];
    let span = |y: i64| -> (i64, i64) {
        if y < ry0 || y >= ry1 {
            (i64::MAX, i64::MIN)
        } else {
            red_ext[(y - ry0) as usize]
        }
    };
    if p.blur {
        // `_soften`: float32, numpy's order of additions.
        let k = EDGE_BLUR as f32;
        let c0 = (1.0 - 2.0 * EDGE_BLUR) as f32;
        let mut b = vec![0f32; rw + 2];
        for y in oy0..oy1 {
            // b is non-zero only where the rows y-1..y+1 are.
            let (mut ba, mut bb) = span(y);
            for s in [span(y - 1), span(y + 1)] {
                ba = ba.min(s.0);
                bb = bb.max(s.1);
            }
            if ba >= bb {
                continue;
            }
            for x in ba..bb {
                let mut v = at(x, y) as f32 * c0;
                if y >= 1 {
                    v += k * at(x, y - 1) as f32;
                }
                if y < h - 1 {
                    v += k * at(x, y + 1) as f32;
                }
                b[(x - rx0) as usize] = v;
            }
            let bv = |x: i64| if x < ba || x >= bb { 0.0 } else { b[(x - rx0) as usize] };
            let (ca, cb) = ((ba - 1).max(ox0), (bb + 1).min(ox1));
            let row = &mut out[(y - oy0) as usize * ow..(y - oy0 + 1) as usize * ow];
            for x in ca..cb {
                let mut v = bv(x) * c0;
                if x >= 1 {
                    v += k * bv(x - 1);
                }
                if x < w - 1 {
                    v += k * bv(x + 1);
                }
                row[(x - ox0) as usize] = tmi_numpy::rint_to_u8(v);
            }
            ext[(y - oy0) as usize] = (ca.max(ox0), cb.max(ca));
        }
    } else {
        for y in oy0..oy1 {
            let (a, b) = span(y);
            let (a, b) = (a.max(ox0), b.min(ox1));
            if a >= b {
                continue;
            }
            let row = &mut out[(y - oy0) as usize * ow..(y - oy0 + 1) as usize * ow];
            for x in a..b {
                row[(x - ox0) as usize] = at(x, y);
            }
            ext[(y - oy0) as usize] = (a, b);
        }
    }
    if p.opacity < 1.0 {
        let lut: Vec<u8> = (0..256).map(|v| tmi_numpy::py_round(v as f64 * p.opacity) as u8).collect();
        for v in out.iter_mut() {
            *v = lut[*v as usize];
        }
    }
    // canvas columns
    for e in ext.iter_mut() {
        *e = (e.0 + x0, e.1 + x0);
    }
    Coverage { px: out, w: ow, ext }
}

/// The polygons and ellipses of `_draw_stroke` on the supersampled canvas.
fn draw_stroke(c: &mut LCanvas, p: &Planned, x0: i64, y0: i64, ss: i64) {
    let ssf = ss as f64;
    let pts: Vec<[f64; 2]> = p.xy.iter().map(|v| [(v[0] - x0 as f64) * ssf, (v[1] - y0 as f64) * ssf]).collect();
    let rr: Vec<f64> = p.r.iter().map(|v| v * ssf).collect();
    let n = pts.len();
    let (ry0, ry1) = (c.region[1], c.region[3]);
    let mut len = Vec::with_capacity(n.saturating_sub(1));
    let mut nz = Vec::with_capacity(n.saturating_sub(1));
    let mut nx = Vec::with_capacity(n.saturating_sub(1));
    let mut ny = Vec::with_capacity(n.saturating_sub(1));
    for k in 0..n.saturating_sub(1) {
        let (dx, dy) = (pts[k + 1][0] - pts[k][0], pts[k + 1][1] - pts[k][1]);
        let l = hypot(dx, dy);
        let z = l > 1e-9;
        let safe = if z { l } else { 1.0 };
        len.push(l);
        nz.push(z);
        nx.push(if z { -dy / safe } else { 0.0 });
        ny.push(if z { dx / safe } else { 0.0 });
    }
    let mut scratch = PolyScratch::default();
    for k in 0..n.saturating_sub(1) {
        let (ra, rb) = (rr[k], rr[k + 1]);
        if !(nz[k] && (ra > 0.1 || rb > 0.1)) {
            continue;
        }
        let (a, b) = (pts[k], pts[k + 1]);
        let q = [
            (a[0] + nx[k] * ra, a[1] + ny[k] * ra),
            (b[0] + nx[k] * rb, b[1] + ny[k] * rb),
            (b[0] - nx[k] * rb, b[1] - ny[k] * rb),
            (a[0] - nx[k] * ra, a[1] - ny[k] * ra),
        ];
        // Rows outside the held ones draw nothing there (scan lines and
        // horizontal edges stay within the edges' rows).
        let (lo, hi) = q.iter().fold((i32::MAX, i32::MIN), |(lo, hi), v| (lo.min(v.1 as i32), hi.max(v.1 as i32)));
        if hi < ry0 || lo >= ry1 {
            continue;
        }
        polygon_with(c, &q, &mut scratch);
    }
    let mut need = vec![false; n];
    need[0] = true;
    need[n - 1] = true;
    if n > 2 {
        for k in 0..n - 2 {
            let cosang = nx[k] * nx[k + 1] + ny[k] * ny[k + 1];
            let gap = rr[k + 1] * (1.0 - ((1.0 + cosang) / 2.0).max(0.0).min(1.0).sqrt());
            if gap > JOIN_GAP * ssf || !nz[k] || !nz[k + 1] {
                need[k + 1] = true;
            }
        }
    }
    for k in 0..n.saturating_sub(1) {
        if (rr[k + 1] - rr[k]).abs() > 0.3 * len[k] {
            need[k] = true;
            need[k + 1] = true;
        }
    }
    for k in 0..n {
        if need[k] && rr[k] > 0.5 {
            let (cx, cy, rk) = (pts[k][0], pts[k][1], rr[k]);
            ellipse(c, [cx - rk, cy - rk, cx + rk, cy + rk]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One stroke of 1000 points (64 KB of vector data) zig-zagging across
    /// ±990,000 with a width that jumps by millions between points: every
    /// segment asks for millions of samples (some 450 GB planned). Refused
    /// before they are allocated; a plain stroke still plans.
    #[test]
    fn a_stroke_whose_width_jumps_by_millions_is_refused_before_its_samples() {
        let node = Node {
            main_id: 2,
            layer_type: None,
            folder: false,
            visible: true,
            mask_enabled: false,
            opacity: 256,
            composite: 0,
            clip: false,
            offset_x: 0,
            offset_y: 0,
            render_mipmap: None,
            mask_mipmap: None,
            render_offset_x: 0,
            render_offset_y: 0,
            mask_offset_x: 0,
            mask_offset_y: 0,
            comic_frame: false,
            children: Vec::new(),
        };
        let stroke = |n: usize, far: f64, jump: f64| Stroke {
            flags: 0,
            brush: 1,
            size: 1e7,
            color: [0; 3],
            opacity: 1.0,
            xy: (0..n).map(|i| if i % 2 == 0 { [-far, -far] } else { [far, far] }).collect(),
            pflags: vec![0; n],
            f: (0..n).map(|i| [1.0, 0.0, 0.0, 0.0, if i % 2 == 0 { 0.0 } else { jump }, 1.0, 0.0]).collect(),
            phase: None,
        };
        let brushes = |_: u32| -> Res<Brush> { Ok(Brush { pressure: Pressure::Linear, thick: 1.0, composite: 0 }) };
        let e = plan(&node, &[stroke(1000, 990_000.0, 10.0)], &brushes, [0, 0, 100, 100], false).err().expect("refused");
        assert!(e.contains("stroke samples"), "{e}");
        assert!(plan(&node, &[stroke(3, 50.0, 1e-6)], &brushes, [0, 0, 100, 100], false).is_ok());
    }
}

impl Ctx<'_> {
    /// The layer's plan, once per render.
    fn vector_plan(&self, node: &Node, strokes: &[Stroke], full: Rect) -> Res<Arc<Plan>> {
        memo(&self.vector_plans, &node.main_id, || {
            let brushes = |id: u32| -> Res<Brush> {
                match self.doc.vectors.brushes.get(&id) {
                    Some(Ok(b)) => Ok(b.clone()),
                    Some(Err(e)) => Err(format!("brush {id}: {e}")),
                    None => Err(format!("brush {id} not loaded")),
                }
            };
            // `_is_mono`: an unreadable style counts as colour
            let mono = crate::style::colour_type(&self.doc.style(node.main_id)).unwrap_or(0) == crate::style::MONO;
            plan(node, strokes, &brushes, full, mono).map(Arc::new)
        })
    }

    /// Plan every vector layer at once on the pool before the bands start
    /// (each band needs them from its first layers on; Python's
    /// `_band_warmups`). Errors stay memoised for the band that asks.
    pub fn warm_vector_plans(&self) {
        use rayon::prelude::*;
        let full = self.full();
        let layers: Vec<(&Node, &[Stroke])> = self
            .doc
            .nodes
            .iter()
            .filter(|n| n.visible && !n.folder)
            .filter_map(|n| match self.doc.vectors.strokes.get(&n.main_id) {
                Some(Ok(s)) if !s.is_empty() => Some((n, &s[..])),
                _ => None,
            })
            .collect();
        layers.par_iter().for_each(|(n, s)| {
            let _ = self.vector_plan(n, s, full);
        });
    }

    /// A vector layer's pixels over `window` before its style
    /// (`render_vector_layer_buffer` + `_clip_layer_to_window`); None when
    /// the layer has no strokes (Python then synthesises it like any layer
    /// without pixels).
    pub fn vector_pixels(&self, node: &Node, full: Rect, window: Rect) -> Res<Option<Layer>> {
        let Some(strokes) = self.doc.vectors.strokes.get(&node.main_id) else {
            return Ok(None);
        };
        let strokes = strokes.as_ref().map_err(|e| format!("layer {}: {e}", node.main_id))?;
        if strokes.is_empty() {
            return Ok(None);
        }
        let plan = self.vector_plan(node, strokes, full)?;
        let mut union: Option<Rect> = None;
        let parts: Vec<(&Planned, Rect)> = plan
            .iter()
            .filter_map(|p| intersect(p.reach, window).map(|r| (p, r)))
            .collect();
        for (_, r) in &parts {
            union = Some(match union {
                None => *r,
                Some(u) => crate::buffer::union(u, *r),
            });
        }
        let Some(u) = union else {
            return Ok(Some(Layer::Empty));
        };
        let mut canvas = Buf::filled(u, [0, 0, 0, 0]);
        for (p, r) in parts {
            let cov = coverage(p, r);
            for y in r[1]..r[3] {
                let (a, b) = cov.ext[(y - r[1]) as usize];
                if a >= b {
                    continue; // nothing drawn on this row (0 changes nothing)
                }
                let at = (y - r[1]) as usize * cov.w + (a - r[0]) as usize;
                let crow = &cov.px[at..at + (b - a) as usize];
                let dst = canvas.row_mut(y, a, b);
                if p.erase {
                    for (d, &v) in dst.chunks_exact_mut(4).zip(crow) {
                        d[3] = multiply(d[3], 255 - v);
                    }
                } else {
                    for (d, &v) in dst.chunks_exact_mut(4).zip(crow) {
                        over_pixel(d, &[p.color[0], p.color[1], p.color[2], v]);
                    }
                }
            }
        }
        Ok(Some(Layer::from_trimmed(canvas.trim())))
    }
}
