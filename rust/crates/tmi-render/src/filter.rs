//! Correction-layer filters (`_decode_filter_layer` and the `_filter_*`
//! builders, clip.py:3149-3510), computed as numpy computes them: float32
//! where the Python works on the float32 image (a Python float operand is
//! cast to float32 first), float64 where it builds float64 tables.
//!
//! A per-channel filter becomes the 3 x 256 LUT `_filter_lut` would give;
//! the two that mix channels (hue/saturation, binarise) are evaluated per
//! colour.

use tmi_numpy::{interp, linspace};

/// Samples per quadratic span of a tone curve (CSP's chords: 32).
pub const TONE_CURVE_SAMPLES: usize = 33;

pub const IDENTITY_LEVELS: [u16; 5] = [0, 0x7FFF, 0xFFFF, 0, 0xFFFF];

#[derive(Clone, Debug)]
pub enum Filter {
    /// [R0..R255, G0.., B0..], already clipped to [0, 1].
    Lut(Vec<f32>),
    HueSat { hue: i64, sat: i64, lum: i64 },
    Binarize { threshold: f32 },
}

#[derive(Clone, Debug)]
pub enum Decoded {
    /// Not a correction layer, or one that changes nothing: drawn as a layer.
    None,
    /// A filter id tmiki does not render: warned about, drawn as a layer.
    Unsupported(u32),
    Filter(Filter),
}

fn be32(d: &[u8], at: usize) -> Option<u32> {
    d.get(at..at + 4).map(|b| u32::from_be_bytes(b.try_into().unwrap()))
}

fn bei32(d: &[u8], at: usize) -> Option<i32> {
    be32(d, at).map(|v| v as i32)
}

fn be16(d: &[u8], at: usize) -> Option<u16> {
    d.get(at..at + 2).map(|b| u16::from_be_bytes([b[0], b[1]]))
}

/// The float32 inputs `_filter_lut` builds: uint8 -> float32 / 255.
#[inline]
pub fn unit(v: u8) -> f32 {
    v as f32 / 255.0f32
}

/// `np.clip((rgb * 255.0).round().astype(np.int32), 0, 255)` on float32.
#[inline]
fn index(v: f32) -> usize {
    let r = (v * 255.0f32).round_ties_even();
    (r as i64).clamp(0, 255) as usize
}

fn clip01(v: f32) -> f32 {
    if v < 0.0 {
        0.0
    } else if v > 1.0 {
        1.0
    } else {
        v
    }
}

/// A per-channel filter as a LUT: `per(c, x)` for x = v / 255 (float32).
fn lut_of(per: impl Fn(usize, f32) -> f32) -> Filter {
    let mut lut = Vec::with_capacity(768);
    for c in 0..3 {
        for v in 0..=255u8 {
            lut.push(clip01(per(c, unit(v))));
        }
    }
    Filter::Lut(lut)
}

/// `_decode_filter_layer` on a FilterLayerInfo blob (None: not a filter).
pub fn decode(blob: &[u8]) -> Decoded {
    if blob.len() < 8 {
        return Decoded::None;
    }
    let kind = be32(blob, 0).unwrap();
    let size = be32(blob, 4).unwrap() as usize;
    let data = &blob[8..(8usize.saturating_add(size)).min(blob.len())];
    let built = match kind {
        1 => brightness_contrast(data),
        2 => levels(data),
        3 => tone_curve(data),
        4 => hue_saturation(data),
        5 => color_balance(data),
        6 => Some(lut_of(|_, x| 1.0f32 - x)),
        7 => posterize(data),
        8 => Some(binarize(data)),
        k => return Decoded::Unsupported(k),
    };
    match built {
        Some(f) => Decoded::Filter(f),
        None => Decoded::None,
    }
}

fn posterize(data: &[u8]) -> Option<Filter> {
    let levels = if data.len() >= 4 { be32(data, 0).unwrap() } else { 0 };
    if levels < 2 {
        return None;
    }
    let span = (levels as f64 - 1.0) as f32;
    Some(lut_of(move |_, x| (x * span).round_ties_even() / span))
}

fn binarize(data: &[u8]) -> Filter {
    let t = if data.len() >= 4 { be32(data, 0).unwrap() } else { 128 };
    Filter::Binarize { threshold: (t as f64 / 255.0) as f32 }
}

fn brightness_contrast(data: &[u8]) -> Option<Filter> {
    if data.len() < 8 {
        return None;
    }
    let (b, c) = (bei32(data, 0).unwrap(), bei32(data, 4).unwrap());
    if b == 0 && c == 0 {
        return None;
    }
    let bf = (b as f64 / 255.0) as f32;
    let cf = if c > 0 {
        if c < 100 {
            100.0 / (100.0 - c as f64)
        } else {
            1e9
        }
    } else {
        (0.0f64).max((100.0 + c as f64) / 100.0)
    } as f32;
    Some(lut_of(move |_, x| {
        let mut out = x + bf;
        if c != 0 {
            out = (out - 0.5f32) * cf + 0.5f32;
        }
        out
    }))
}

/// `_tone_curve_lut`: a quadratic spline through the control points,
/// resampled at x = i / 255 (float64).
pub fn tone_curve_lut(points: &[(f64, f64)]) -> Vec<f64> {
    let xs: Vec<f64> = (0..256).map(|i| i as f64 / 255.0).collect();
    let clip = |v: Vec<f64>| v.into_iter().map(|x| x.max(0.0).min(1.0)).collect::<Vec<f64>>();
    if points.len() < 2 {
        return xs;
    }
    if points.len() == 2 {
        return clip(interp(&xs, &[points[0].0, points[1].0], &[points[0].1, points[1].1]));
    }
    let interior = &points[1..points.len() - 1];
    // CSP flattens each quadratic span into 32 chords (33 samples, §15.5)
    let t = linspace(0.0, 1.0, TONE_CURVE_SAMPLES);
    let mut anchor = points[0];
    let mut bx = Vec::with_capacity(TONE_CURVE_SAMPLES * interior.len());
    let mut by = Vec::with_capacity(TONE_CURVE_SAMPLES * interior.len());
    for (i, &ctrl) in interior.iter().enumerate() {
        let nxt = if i < interior.len() - 1 {
            ((ctrl.0 + interior[i + 1].0) / 2.0, (ctrl.1 + interior[i + 1].1) / 2.0)
        } else {
            points[points.len() - 1]
        };
        for &tt in &t {
            let u = 1.0 - tt;
            let a = u * u;
            let b = 2.0 * u * tt;
            let c = tt * tt;
            bx.push(a * anchor.0 + b * ctrl.0 + c * nxt.0);
            by.push(a * anchor.1 + b * ctrl.1 + c * nxt.1);
        }
        anchor = nxt;
    }
    let mut order: Vec<usize> = (0..bx.len()).collect();
    order.sort_by(|&p, &q| bx[p].partial_cmp(&bx[q]).unwrap_or(std::cmp::Ordering::Equal));
    let sx: Vec<f64> = order.iter().map(|&k| bx[k]).collect();
    let sy: Vec<f64> = order.iter().map(|&k| by[k]).collect();
    clip(interp(&xs, &sx, &sy))
}

fn tone_curve(data: &[u8]) -> Option<Filter> {
    let channel = |ch: usize| -> Option<Vec<f64>> {
        let off = ch * 130;
        if off + 2 > data.len() {
            return None;
        }
        let count = be16(data, off).unwrap() as usize;
        if !(2..=32).contains(&count) || off + 2 + count * 4 > data.len() {
            return None;
        }
        let pts: Vec<(f64, f64)> = (0..count)
            .map(|k| {
                let p = off + 2 + k * 4;
                (be16(data, p).unwrap() as f64 / 65535.0, be16(data, p + 2).unwrap() as f64 / 65535.0)
            })
            .collect();
        if pts.iter().all(|&(x, y)| (x - y).abs() < 1e-6) {
            return None;
        }
        Some(tone_curve_lut(&pts))
    };
    let master = channel(0);
    let curves = [channel(1), channel(2), channel(3)];
    if master.is_none() && curves.iter().all(Option::is_none) {
        return None;
    }
    let mut luts: Vec<Vec<f32>> = Vec::new();
    for curve in &curves {
        let mut lut8: Vec<usize> = match curve {
            Some(cv) => cv.iter().map(|&y| csp16_to_8(y)).collect(),
            None => (0..256).collect(),
        };
        if let Some(m) = &master {
            lut8 = lut8.iter().map(|&i| csp16_to_8(m[i])).collect();
        }
        luts.push(lut8.into_iter().map(|v| (v as f64 / 255.0) as f32).collect());
    }
    Some(lut_of(move |c, x| luts[c][index(x)]))
}

/// `_csp16_to_8`: CSP's 16-bit corrections back to 8 bit,
/// round(clip(y, 0, 1) * 65535) >> 8.
fn csp16_to_8(y: f64) -> usize {
    let y = y.max(0.0).min(1.0);
    (((y * 65535.0 + 0.5).floor() as i64) >> 8) as usize
}

/// `_levels_lut`: (in_black, mid, in_white, out_black, out_white) -> LUT.
fn levels_lut(rec: [u16; 5]) -> Vec<f64> {
    let [ib, mid, iw, ob, ow] = rec.map(|v| v as f64 / 65535.0);
    let span = (iw - ib).max(1e-6);
    let p = ((mid - ib) / span).max(1e-6).min(1.0 - 1e-6);
    let gamma = 10.0f64.min(0.1f64.max(0.5f64.ln() / p.ln()));
    (0..256)
        .map(|i| {
            let t = ((i as f64 / 255.0 - ib) / span).max(0.0).min(1.0);
            // ndarray ** 0.5 is np.sqrt (numpy's fast_scalar_power)
            let tg = if gamma == 0.5 { t.sqrt() } else { t.powf(gamma) };
            ob + tg * (ow - ob)
        })
        .collect()
}

fn levels(data: &[u8]) -> Option<Filter> {
    if data.len() < 10 {
        return None;
    }
    let n = 4.min(data.len() / 10);
    let recs: Vec<[u16; 5]> = (0..n).map(|i| [0, 1, 2, 3, 4].map(|k| be16(data, i * 10 + k * 2).unwrap())).collect();
    if recs.iter().all(|r| *r == IDENTITY_LEVELS) {
        return None;
    }
    let master = levels_lut(recs[0]);
    let mut luts: Vec<Vec<f32>> = Vec::new();
    for c in 0..3 {
        if c + 1 < n && recs[c + 1] != IDENTITY_LEVELS {
            let lc = levels_lut(recs[c + 1]);
            luts.push(
                master
                    .iter()
                    .map(|&v| lc[((v * 255.0).round_ties_even() as i64).clamp(0, 255) as usize] as f32)
                    .collect(),
            );
        } else {
            luts.push(master.iter().map(|&v| v as f32).collect());
        }
    }
    Some(lut_of(move |c, x| luts[c][index(x)]))
}

fn color_balance(data: &[u8]) -> Option<Filter> {
    if data.len() < 40 {
        return None;
    }
    let vals: Vec<i64> = (0..10).map(|k| bei32(data, k * 4).unwrap() as i64).collect();
    let (preserve, shadows, mids, highs) = (vals[0], &vals[1..4], &vals[4..7], &vals[7..10]);
    if vals[1..].iter().all(|&v| v == 0) {
        return None;
    }
    // One 16-bit levels-and-gamma curve per channel (`_filter_color_balance`).
    let others = |c: usize| (0..3).filter(move |&o| o != c);
    let max_s = *shadows.iter().max().unwrap();
    let min_h = *highs.iter().min().unwrap();
    let mut luts: Vec<Vec<f32>> = Vec::new();
    for c in 0..3 {
        let (mut b, mut w, mut m) = (0.0f64, 1.0f64, 0.5f64);
        if preserve != 0 {
            b = ((max_s - shadows[c]) * 255) as f64 / 65535.0;
            w = 1.0 - ((highs[c] - min_h) * 255) as f64 / 65535.0;
            // sum(mids[o] ...) is an int sum, then / 2.0
            let s: i64 = others(c).map(|o| mids[o]).sum();
            m = 0.5 - 0.0015 * (mids[c] as f64 - s as f64 / 2.0);
        } else {
            let (s, h) = (shadows[c], highs[c]);
            if s > 0 {
                m -= 0.0008 * s as f64;
            } else if s < 0 {
                b = 0.96 * (-s) as f64 * 255.0 / 65535.0;
                m += 0.0012 * (-s) as f64;
            }
            if h > 0 {
                w = 1.0 - 0.96 * h as f64 * 255.0 / 65535.0;
                m -= 0.0012 * h as f64;
            } else if h < 0 {
                m += 0.0008 * (-h) as f64;
            }
            m -= 0.002 * mids[c] as f64;
        }
        let m = m.max(1e-6).min(1.0 - 1e-6);
        let g = (0.5f64.ln() / m.ln() * 4096.0 + 0.5).floor() / 4096.0;
        let span = (w - b).max(1e-9);
        let lut: Vec<f32> = (0..256)
            .map(|i| {
                let x = i as f64 * 257.0 / 65535.0;
                let t = ((x - b) / span).max(0.0).min(1.0);
                // ndarray ** scalar: numpy's fast_scalar_power for 0.5 / 1 / 2
                let y = if g == 1.0 {
                    t
                } else if g == 0.5 {
                    t.sqrt()
                } else if g == 2.0 {
                    t * t
                } else {
                    t.powf(g)
                };
                (csp16_to_8(y) as f64 / 255.0) as f32
            })
            .collect();
        luts.push(lut);
    }
    Some(lut_of(move |c, x| luts[c][index(x)]))
}

fn hue_saturation(data: &[u8]) -> Option<Filter> {
    if data.len() < 12 {
        return None;
    }
    let (hue, sat, lum) = (bei32(data, 0).unwrap() as i64, bei32(data, 4).unwrap() as i64, bei32(data, 8).unwrap() as i64);
    // `_filter_hue_saturation`: CSP's integer HSV; even an all-zero layer
    // changes some colours, so it is never skipped
    Some(Filter::HueSat { hue, sat, lum })
}
impl Filter {
    pub fn per_channel(&self) -> bool {
        matches!(self, Filter::Lut(_))
    }

    /// The filtered colour of one pixel, clipped to [0, 1] (float32), as
    /// `np.clip(_run_filter(filter_fn, rgb, raw), 0.0, 1.0)` gives it.
    pub fn colour(&self, rgb: [u8; 3]) -> [f32; 3] {
        match self {
            Filter::Lut(lut) => [0, 1, 2].map(|c| lut[c * 256 + rgb[c] as usize]),
            Filter::Binarize { threshold } => {
                let [r, g, b] = rgb.map(unit);
                let l = 0.2126f32 * r + 0.7152f32 * g + 0.0722f32 * b;
                let on = if l >= *threshold { 1.0f32 } else { 0.0 };
                [on, on, on]
            }
            &Filter::HueSat { hue, sat, lum } => hue_sat_pixel(rgb, hue, sat, lum),
        }
    }
}

const HSV_HK: i64 = 32768;
const HSV_SK: i64 = 65536;

/// `_filter_hue_saturation`'s integer HSV round trip for one colour.
fn hue_sat_pixel(rgb: [u8; 3], hue: i64, sat: i64, lum: i64) -> [f32; 3] {
    let (r, g, b) = (rgb[0] as i64, rgb[1] as i64, rgb[2] as i64);
    let mx = r.max(g).max(b);
    let ch = mx - r.min(g).min(b);
    let mut s = if mx > 0 { (ch * HSV_SK).div_euclid(mx.max(1)) } else { 0 };
    let mut v = mx;
    let mut h = if ch == 0 {
        0
    } else {
        let (base, num) = if r == mx {
            (0, g - b)
        } else if g == mx {
            (2, b - r)
        } else {
            (4, r - g)
        };
        // the hue fraction divides towards zero (a C division)
        let x = base * HSV_HK + num.signum() * ((num.abs() * HSV_HK) / ch.max(1));
        if x < 0 {
            x + 6 * HSV_HK
        } else {
            x
        }
    };
    let l = (lum.abs() * HSV_SK).div_euclid(100);
    let a = (sat.abs() * HSV_SK).div_euclid(100);
    if lum > 0 {
        v += ((255 - v) * l).div_euclid(HSV_SK);
        s -= (s * l).div_euclid(HSV_SK);
    } else if lum < 0 {
        v -= (v * l).div_euclid(HSV_SK);
    }
    if sat < 0 {
        let x = (s * a).div_euclid(HSV_SK);
        v -= (v * x).div_euclid(2 * HSV_SK);
        s -= x;
    } else if sat > 0 && s != 0 {
        let ds = ((HSV_SK - s) * a).div_euclid(HSV_SK);
        let dv = (v * ds).div_euclid(HSV_SK);
        if v + dv > 255 {
            s += (ds * (255 - v)).div_euclid(dv.max(1));
            v = 255;
        } else {
            s += ds;
            v += dv;
        }
    }
    h += (hue * HSV_HK).div_euclid(60);
    let h = h.rem_euclid(6 * HSV_HK);
    let i = h / HSV_HK;
    let f = h - i * HSV_HK;
    let p = (v * (HSV_SK - s)).div_euclid(HSV_SK);
    let q = (v * (HSV_SK - (s * f).div_euclid(HSV_HK))).div_euclid(HSV_SK);
    let t = (v * (HSV_SK - (s * (HSV_HK - f)).div_euclid(HSV_HK))).div_euclid(HSV_SK);
    let out = match i {
        0 => [v, t, p],
        1 => [q, v, p],
        2 => [p, v, t],
        3 => [p, q, v],
        4 => [t, p, v],
        _ => [v, p, q],
    };
    out.map(|x| (x as f64 / 255.0) as f32)
}