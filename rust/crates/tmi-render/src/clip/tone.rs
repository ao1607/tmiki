//! トーン (EffectTone, tmiref/clip_tone.py, stage 1b): the halftone
//! threshold matrix of `ToneScreen` and `apply_tone`, integer for integer
//! where Python works on int64 and in numpy's float64 order elsewhere (the
//! spot function's `np.cos` is the C runtime's `cos`, as is Rust's; the 2x2
//! `np.linalg.inv` is `tmi_numpy::inv2`).

use tmi_clip::model::Style;

use super::Res;
use crate::buffer::Buf;

const TAU: f64 = 2.0 * std::f64::consts::PI;
const EPS: (f64, f64) = (1e-7, 0.8e-7);
const EPS_AXIS: (f64, f64) = (1e-7, 1e-7);
const MIN_LEVELS: i64 = 128;
/// Templates beyond this many pixels per cell are declined (Python would
/// build them, slowly; no file has one).
const MAX_AREA: i64 = 1 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    Circle,
    Square,
    Line,
}

#[derive(Clone, Copy, Debug)]
pub struct Tone {
    pub lpi: f64,
    pub angle: f64,
    pub shape: Shape,
    /// EffectTone's unknown u32 at +16 (`tone["unknown"]["u16"]` in tmiref): nonzero
    /// selects the whole-cell, pixel-corner screen of layer_types MainId 17.
    pub u16: u32,
}

/// Offset just past `u32 len + UTF-16BE name` (first occurrence), or None.
pub fn section(blob: &[u8], name: &str) -> Option<usize> {
    let mut key = (name.len() as u32).to_be_bytes().to_vec();
    key.extend(name.encode_utf16().flat_map(|u| u.to_be_bytes()));
    blob.windows(key.len()).position(|w| w == key.as_slice()).map(|i| i + key.len())
}

fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(b[at..at + 4].try_into().unwrap())
}

fn bef64(b: &[u8], at: usize) -> f64 {
    f64::from_be_bytes(b[at..at + 8].try_into().unwrap())
}

/// `parse_tone_effect(layer_style(...))`: the settings of an attached,
/// enabled トーン, else None.
pub fn parse_tone_effect(style: &Style) -> Res<Option<Tone>> {
    if !style.get("LayerEffectAttached").truthy()? {
        return Ok(None);
    }
    let Some(b) = style.get("LayerEffectInfo").blob()? else {
        return Ok(None);
    };
    if b.is_empty() {
        return Ok(None);
    }
    let Some(p) = section(b, "EffectTone") else {
        return Ok(None);
    };
    if b.len() < p + 56 || be32(b, p) == 0 {
        return Ok(None);
    }
    let shape = match be32(b, p + 12) {
        2 => Shape::Square,
        4 => Shape::Line,
        _ => Shape::Circle,
    };
    let lpi = bef64(b, p + 24);
    let angle = be32(b, p + 40) as f64;
    if !(lpi > 0.0 && lpi.is_finite()) {
        return Ok(None);
    }
    Ok(Some(Tone { lpi, angle, shape, u16: be32(b, p + 16) }))
}

/// Python's `//` on int64.
#[inline]
fn floor_div(a: i64, b: i64) -> i64 {
    a.div_euclid(b) - if b < 0 && a.rem_euclid(b) != 0 { 1 } else { 0 }
}

pub struct Screen {
    v1: (i64, i64),
    v2: (i64, i64),
    half: i64,
    eps: (f64, f64),
    det: i64,
    n: i64,
    /// Threshold levels `N`.
    pub levels: i64,
    bayer: Vec<i64>,
    tx0: i64,
    ty0: i64,
    lut_w: i64,
    lut_h: i64,
    lut: Vec<i64>,
}

/// `_bayer(n)` (row major, n x n).
fn bayer(n: i64) -> Vec<i64> {
    let mut m = vec![0i64];
    let mut k = 1;
    while k < n {
        let size = 2 * k;
        let mut next = vec![0i64; (size * size) as usize];
        for y in 0..k {
            for x in 0..k {
                let v = 4 * m[(y * k + x) as usize];
                next[(y * size + x) as usize] = v;
                next[(y * size + x + k) as usize] = v + 2;
                next[((y + k) * size + x) as usize] = v + 3;
                next[((y + k) * size + x + k) as usize] = v + 1;
            }
        }
        m = next;
        k = size;
    }
    m
}

impl Screen {
    /// `get_screen(tone, dpi)` (`screen_vectors` + `ToneScreen`).
    pub fn new(tone: &Tone, dpi: f64) -> Res<Screen> {
        let p0 = dpi / tone.lpi;
        let min_period = 2.0 * 2.0f64.sqrt();
        if !p0.is_finite() {
            return Err("tone period is not finite".into());
        }
        let period = if min_period > p0 { min_period } else { p0 };
        let th = tone.angle * (std::f64::consts::PI / 180.0);
        let fx = (period * th.cos() + 0.5).floor();
        let fy = (period * th.sin() + 0.5).floor();
        if fx.abs() > 4096.0 || fy.abs() > 4096.0 {
            return Err("tone cell too large".into());
        }
        let mut v1 = (fx as i64, fy as i64);
        if v1 == (0, 0) {
            v1 = (1, 0);
        }
        let v2 = (-v1.1, v1.0);
        let clamped = p0 < min_period;
        let axis = v1.0 == 0 || v1.1 == 0;
        let eps = if axis { EPS_AXIS } else { EPS };
        let mut half = clamped && !axis;
        let mut min_levels = MIN_LEVELS;
        if tone.u16 != 0 {
            // layer_types MainId 17 (EffectTone u16 = 2): whole-cell levels,
            // sampled at pixel corners even though the screen is clamped.
            half = false;
            min_levels = 1;
        }
        Screen::build(v1, v2, tone.shape, half, eps, min_levels)
    }

    fn build(v1: (i64, i64), v2: (i64, i64), shape: Shape, half: bool, eps: (f64, f64), min_levels: i64) -> Res<Screen> {
        let det = v1.0 * v2.1 - v1.1 * v2.0;
        let area = det.abs();
        if area == 0 || area > MAX_AREA {
            return Err("tone cell out of range".into());
        }
        let mut n = 1;
        while area * n * n < min_levels {
            n *= 2;
        }
        let mut s = Screen {
            v1,
            v2,
            half: half as i64,
            eps,
            det,
            n,
            levels: area * n * n,
            bayer: bayer(n),
            tx0: 0,
            ty0: 0,
            lut_w: 0,
            lut_h: 0,
            lut: Vec::new(),
        };
        s.template(shape, area)?;
        Ok(s)
    }

    /// `_cell_coords`: (i, j, s numerator, t numerator, d).
    #[inline]
    fn cell(&self, x: i64, y: i64) -> (i64, i64, i64, i64, i64) {
        let (v1, v2) = (self.v1, self.v2);
        let xx = 2 * x + self.half;
        let yy = 2 * y + self.half;
        let mut sn = v2.1 * xx - v2.0 * yy;
        let mut tn = -v1.1 * xx + v1.0 * yy;
        let mut d = 2 * self.det;
        if d < 0 {
            (sn, tn, d) = (-sn, -tn, -d);
        }
        if self.half != 0 {
            let (a, b) = self.eps;
            let sg = if self.det > 0 { 1.0 } else { -1.0 };
            let ds = sg * (v2.1 as f64 * a - v2.0 as f64 * b);
            let dt = sg * (-v1.1 as f64 * a + v1.0 as f64 * b);
            let sgn = |v: f64| {
                if v > 0.0 {
                    1
                } else if v < 0.0 {
                    -1
                } else {
                    0
                }
            };
            sn = sn * 2 - sgn(ds);
            tn = tn * 2 - sgn(dt);
            d *= 2;
        }
        let i = floor_div(sn, d);
        let j = floor_div(tn, d);
        (i, j, sn - i * d, tn - j * d, d)
    }

    /// `_build_template`: each cell pixel's rank by the nudged spot function.
    fn template(&mut self, shape: Shape, area: i64) -> Res<()> {
        let (v1, v2) = (self.v1, self.v2);
        let cx = [0, v1.0, v2.0, v1.0 + v2.0];
        let cy = [0, v1.1, v2.1, v1.1 + v2.1];
        let (xa, xb) = (cx.iter().min().unwrap() - 1, cx.iter().max().unwrap() + 2);
        let (ya, yb) = (cy.iter().min().unwrap() - 1, cy.iter().max().unwrap() + 2);
        let mut pts: Vec<(i64, i64, i64, i64)> = Vec::new();
        let mut d = 1;
        for y in ya..yb {
            for x in xa..xb {
                let (i, j, sr, tr, dd) = self.cell(x, y);
                d = dd;
                if i == 0 && j == 0 {
                    pts.push((x, y, sr, tr));
                }
            }
        }
        if pts.len() as i64 != area {
            return Err("tone template does not tile".into());
        }
        let vi = tmi_numpy::inv2([[v1.0 as f64, v2.0 as f64], [v1.1 as f64, v2.1 as f64]]).ok_or("tone lattice is singular")?;
        let (ea, eb) = self.eps;
        let e1 = vi[0][0] * ea + vi[0][1] * eb;
        let e2 = vi[1][0] * ea + vi[1][1] * eb;
        let df = d as f64;
        let spot: Vec<f64> = pts
            .iter()
            .map(|&(_, _, s, t)| {
                let u1 = s as f64 / df - 0.5 - e1;
                let u2 = t as f64 / df - 0.5 - e2;
                match shape {
                    Shape::Square => -(u1.abs().max(u2.abs())),
                    Shape::Line => (TAU * u2).cos(),
                    Shape::Circle => (TAU * u1).cos() + (TAU * u2).cos(),
                }
            })
            .collect();
        if !spot.iter().all(|v| v.is_finite()) {
            return Err("tone spot is not finite".into());
        }
        // np.lexsort((S, T, -s)): by -s, then T, then S (stable).
        let mut order: Vec<usize> = (0..pts.len()).collect();
        order.sort_by(|&p, &q| (-spot[p]).partial_cmp(&-spot[q]).unwrap().then(pts[p].3.cmp(&pts[q].3)).then(pts[p].2.cmp(&pts[q].2)));
        let mut rank = vec![0i64; pts.len()];
        for (r, &k) in order.iter().enumerate() {
            rank[k] = r as i64;
        }
        self.tx0 = pts.iter().map(|p| p.0).min().unwrap();
        self.ty0 = pts.iter().map(|p| p.1).min().unwrap();
        self.lut_w = pts.iter().map(|p| p.0).max().unwrap() - self.tx0 + 1;
        self.lut_h = pts.iter().map(|p| p.1).max().unwrap() - self.ty0 + 1;
        self.lut = vec![-1; (self.lut_w * self.lut_h) as usize];
        for (k, p) in pts.iter().enumerate() {
            self.lut[((p.1 - self.ty0) * self.lut_w + (p.0 - self.tx0)) as usize] = rank[k];
        }
        Ok(())
    }

    /// `index`: the threshold index k of canvas pixel (x, y).
    #[inline]
    pub fn index(&self, x: i64, y: i64) -> Option<i64> {
        let (i, j, _, _, _) = self.cell(x, y);
        let rx = x - (i * self.v1.0 + j * self.v2.0) - self.tx0;
        let ry = y - (i * self.v1.1 + j * self.v2.1) - self.ty0;
        if rx < 0 || ry < 0 || rx >= self.lut_w || ry >= self.lut_h {
            return None;
        }
        let r = self.lut[(ry * self.lut_w + rx) as usize];
        if r < 0 {
            return None;
        }
        let n = self.n;
        Some(r * (n * n) + self.bayer[(j.rem_euclid(n) * n + i.rem_euclid(n)) as usize])
    }

    /// `ink_levels` of one grey value.
    #[inline]
    fn level(&self, grey: f64) -> i64 {
        let big_n = self.levels;
        let v = (255.0 - grey) / 255.0;
        let k = if v >= 1.0 { big_n as f64 } else { (big_n as f64 * v + 1.0 - (self.n * self.n) as f64 / 2.0).floor() };
        k.max(0.0).min(big_n as f64) as i64
    }
}

/// `apply_tone(image, x, y, tone, dpi)`: inked pixels opaque black, the rest
/// transparent.
pub fn apply_tone(buf: &Buf, tone: &Tone, dpi: f64) -> Res<Buf> {
    let screen = Screen::new(tone, dpi)?;
    let mut out = Buf { x: buf.x, y: buf.y, w: buf.w, h: buf.h, px: vec![0u8; buf.px.len()] };
    let mut levels: std::collections::HashMap<[u8; 4], i64> = std::collections::HashMap::new();
    for (k, (p, o)) in buf.px.chunks_exact(4).zip(out.px.chunks_exact_mut(4)).enumerate() {
        let (x, y) = (buf.x + (k % buf.w) as i64, buf.y + (k / buf.w) as i64);
        // (Python indexes every pixel; an index off the template would raise.)
        let idx = screen.index(x, y).ok_or("tone index outside the template")?;
        if p[3] == 0 {
            continue;
        }
        let key = [p[0], p[1], p[2], p[3]];
        let big_k = *levels.entry(key).or_insert_with(|| {
            let (r, g, b, a) = (p[0] as f64, p[1] as f64, p[2] as f64, p[3] as f64);
            let luma = r * 0.299 + g * 0.587 + b * 0.114;
            let dens = (255.0 - luma) * (a / 255.0);
            screen.level((255.0 - dens).round_ties_even())
        });
        if idx < big_k {
            o[3] = 255;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floor_div_matches_python() {
        for a in -20..20 {
            for b in [-7i64, -3, -1, 1, 2, 5] {
                let q = (a as f64 / b as f64).floor() as i64;
                assert_eq!(floor_div(a, b), q, "{a} // {b}");
            }
        }
        assert_eq!(bayer(2), vec![0, 2, 3, 1]);
    }
}
