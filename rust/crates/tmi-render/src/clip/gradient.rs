//! Gradient fills (tmiref/clip_gradient.py, stage 1b): the colour model
//! `gradient_mix` evaluated on numpy's 16385-point grid, then per pixel the
//! linear interpolation of that table and CSP's 4x4 ordered dither.
//!
//! Every step is numpy's float64 arithmetic in numpy's order: the matrix
//! products go through OpenBLAS (`tmi_numpy::matmul3`, fused multiply-adds),
//! `**`, `np.cbrt` and the trigonometry call the C runtime as numpy does, and
//! the derived matrices are numpy's own values (bit patterns, checked against
//! clip_gradient.py by tests/test_rust_kernels.py).

use tmi_numpy::linalg::{fma_kernels, matmul3};
use tmi_numpy::{interp, linspace};

use super::Res;
use crate::buffer::{size, Buf, Rect};

/// `_EASE`: f(t) at k / 64.
const EASE: [f64; 65] = [
    0.00000, 0.00631, 0.01338, 0.02097, 0.02931, 0.03813, 0.04753, 0.05759, 0.06825, 0.07948, 0.09116, 0.10374, 0.11671, 0.13015, 0.14431,
    0.15895, 0.17474, 0.19058, 0.20698, 0.22367, 0.24174, 0.26022, 0.27903, 0.29854, 0.31867, 0.33921, 0.36035, 0.38184, 0.40465, 0.42790,
    0.45124, 0.47527, 0.50000, 0.52473, 0.54876, 0.57210, 0.59535, 0.61816, 0.63965, 0.66079, 0.68133, 0.70146, 0.72097, 0.73978, 0.75826,
    0.77633, 0.79302, 0.80942, 0.82526, 0.84105, 0.85569, 0.86985, 0.88329, 0.89626, 0.90884, 0.92052, 0.93175, 0.94241, 0.95247, 0.96187,
    0.97069, 0.97903, 0.98662, 0.99369, 1.00000,
];

// numpy's values of the module's matrices (`_LMS` row-normalised, the
// `np.linalg.inv`s), as float64 bit patterns.
const LMS: [[u64; 3]; 3] = [
    [0x3fe22363b256ffc1, 0x3fdb674d1633482c, 0x3f747ae147ae147b],
    [0x3fafd36f7e3d1cc1, 0x3feafec56d5cfaad, 0x3fb8201cd5f99c39],
    [0x3f831b9b66f9335d, 0x3fb33482be8bc16a, 0x3fed4d013a92a305],
];
const LMS_INV: [[u64; 3]; 3] = [
    [0x3ffde1a6297fc618, 0xbfee9596453879da, 0x3fb6924f91c76d57],
    [0xbfc1809dd041f086, 0x3ff442db816355d1, 0xbfc0963e3ad8be00],
    [0xbf8007789d42e618, 0xbfb8112f2a9a0960, 0x3ff1a121e3e42662],
];
const OK_M1: [[u64; 3]; 3] = [
    [0x3fda61d629f2e197, 0x3fe129a2d9e60e32, 0x3faa572112081026],
    [0x3fcb1fa76156a7c5, 0x3fe5c84a69936914, 0x3fbb7e5df0497455],
    [0x3fb69afd7a044c17, 0x3fd207ae728a2f45, 0x3fe428c9177a5edb],
];
const OK_M2: [[u64; 3]; 3] = [
    [0x3fcaf02a3fe8a4fa, 0x3fe9655120032aad, 0xbf70add9bd572b38],
    [0x3fffa5e1bfffde12, 0xc0036dc1bffe5d3e, 0x3fdcd686fff371a5],
    [0x3f9a869680b729e0, 0x3fe90c776001f502, 0xbfe9e0ac0001353d],
];
const OK_M1_INV: [[u64; 3]; 3] = [
    [0x40104e955db6ebdf, 0xc00a76317e8b3a09, 0x3fcd906c31204ccd],
    [0xbff44b85a6094545, 0x4004e0c87cf2bec5, 0xbfd5d82d4f29248f],
    [0xbf712fea677e77d2, 0xbfe68267c11007ef, 0x3ffb5263caeaacf7],
];
const OK_M2_INV: [[u64; 3]; 3] = [
    [0x3fefffffff2b0a81, 0x3fd95d992fe38806, 0x3fcb9f75219cc810],
    [0x3ff0000002625996, 0xbfbb0611710080c0, 0xbfb058bf485b8813],
    [0x3ff000000ead0f39, 0xbfb6e86f739b7095, 0xbff4a9ecbd4621c4],
];

fn mat(bits: &[[u64; 3]; 3]) -> [[f64; 3]; 3] {
    bits.map(|r| r.map(f64::from_bits))
}

/// The matrices as float64 (for the tests): LMS, LMS_INV, OK_M1, OK_M2,
/// OK_M1_INV, OK_M2_INV.
pub fn matrices() -> [[[f64; 3]; 3]; 6] {
    [mat(&LMS), mat(&LMS_INV), mat(&OK_M1), mat(&OK_M2), mat(&OK_M1_INV), mat(&OK_M2_INV)]
}

const C1: f64 = 3424.0 / 4096.0;
const C2: f64 = 2413.0 / 128.0;
const C3: f64 = 2392.0 / 128.0;
const M1: f64 = 2610.0 / 16384.0;
const M2: f64 = 2523.0 / 32.0;
const PQ_WHITE_NITS: [f64; 5] = [46533.8, 5822.3, 1124.1, 275.2, 84.67];

/// `np.sign` (0 for ±0).
#[inline]
fn sign(x: f64) -> f64 {
    if x > 0.0 {
        1.0
    } else if x < 0.0 {
        -1.0
    } else if x == 0.0 {
        0.0
    } else {
        x
    }
}

/// `np.maximum(x, 0)` (NaN propagates; -0.0 >= 0 keeps -0.0).
#[inline]
fn max0(x: f64) -> f64 {
    if x >= 0.0 || x.is_nan() {
        x
    } else {
        0.0
    }
}

/// `np.clip(x, lo, hi)`.
#[inline]
fn clip(x: f64, lo: f64, hi: f64) -> f64 {
    if x.is_nan() {
        return x;
    }
    let y = if x >= lo { x } else { lo };
    if y <= hi {
        y
    } else {
        hi
    }
}

fn to_lin(c: f64) -> f64 {
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((max0(c) + 0.055) / 1.055).powf(2.4)
    }
}

fn from_lin(c: f64) -> f64 {
    if c <= 0.0031308 {
        c * 12.92
    } else {
        1.055 * max0(c).powf(1.0 / 2.4) - 0.055
    }
}

fn pq(y: f64) -> f64 {
    let a = y.abs().powf(M1);
    sign(y) * ((C1 + C2 * a) / (1.0 + C3 * a)).powf(M2)
}

fn pq_inv(e: f64) -> f64 {
    let p = e.abs().powf(1.0 / M2);
    sign(e) * (max0(p - C1) / (C2 - C3 * p)).powf(1.0 / M1)
}

fn white_scale(level: u32) -> f64 {
    PQ_WHITE_NITS[level.min(4) as usize] / 10000.0
}

/// `_encode`: 0..255 sRGB -> mixing space.
fn encode(rgb8: [f64; 3], mode: u32, level: u32) -> [f64; 3] {
    let c = rgb8.map(|v| v / 255.0);
    if mode == 0 {
        return c;
    }
    let lin = c.map(to_lin);
    match mode {
        3 => lin,
        2 => matmul3(matmul3(lin, &mat(&OK_M1)).map(f64::cbrt), &mat(&OK_M2)),
        _ => {
            let ws = white_scale(level);
            matmul3(lin, &mat(&LMS)).map(|v| pq(v * ws))
        }
    }
}

/// `_decode`: mixing space -> 0..255 sRGB float, gamut-clipped.
fn decode(x: [f64; 3], mode: u32, level: u32) -> [f64; 3] {
    let out = if mode == 0 {
        x
    } else {
        let lin = match mode {
            3 => x,
            2 => matmul3(matmul3(x, &mat(&OK_M2_INV)).map(|v| v.powf(3.0)), &mat(&OK_M1_INV)),
            _ => {
                let ws = white_scale(level);
                matmul3(x.map(|v| pq_inv(v) / ws), &mat(&LMS_INV))
            }
        };
        lin.map(|v| from_lin(clip(v, 0.0, 1.0)))
    };
    out.map(|v| clip(v, 0.0, 1.0) * 255.0)
}

/// `gradient_mix(stops, t, mode, level)`: straight-alpha RGBA (0..255
/// float) at each `t`. `stops` sorted by position.
pub fn gradient_mix(stops: &[(f64, [u8; 4])], t: &[f64], mode: u32, level: u32) -> Vec<[f64; 4]> {
    let t: Vec<f64> = t.iter().map(|&v| clip(v, 0.0, 1.0)).collect();
    let col: Vec<[f64; 4]> = stops.iter().map(|s| s.1.map(|v| v as f64)).collect();
    if stops.len() == 1 {
        return vec![col[0]; t.len()];
    }
    let mode = if mode <= 3 { mode } else { 0 };
    let pos: Vec<f64> = stops.iter().map(|s| s.0).collect();
    let enc: Vec<[f64; 3]> = col.iter().map(|c| encode([c[0], c[1], c[2]], mode, level)).collect();
    let n = stops.len();
    let mut segs = Vec::with_capacity(t.len());
    let mut local = Vec::with_capacity(t.len());
    for &v in &t {
        // np.searchsorted(pos, v, side="right") - 1, clipped
        let right = pos.partition_point(|&p| p <= v) as i64;
        let seg = (right - 1).clamp(0, n as i64 - 2) as usize;
        let (p0, p1) = (pos[seg], pos[seg + 1]);
        let span = p1 - p0;
        let l = if span > 0.0 { (v - p0) / span } else { 1.0 };
        segs.push(seg);
        local.push(clip(l, 0.0, 1.0));
    }
    let ease_x = linspace(0.0, 1.0, EASE.len());
    let w = interp(&local, &ease_x, &EASE);
    segs.iter()
        .zip(&w)
        .map(|(&seg, &w)| {
            let (e0, e1) = (enc[seg], enc[seg + 1]);
            let mixed = [0, 1, 2].map(|c| e0[c] + (e1[c] - e0[c]) * w);
            let rgb = decode(mixed, mode, level);
            let a = col[seg][3] + (col[seg + 1][3] - col[seg][3]) * w;
            [rgb[0], rgb[1], rgb[2], a]
        })
        .collect()
}

/// `_BAYER`, indexed [(H - 1 - y) % 4][x % 4].
const BAYER: [[f64; 4]; 4] = [[0.0, 8.0, 2.0, 10.0], [12.0, 4.0, 14.0, 6.0], [3.0, 11.0, 1.0, 9.0], [15.0, 7.0, 13.0, 5.0]];
const LUT_STEPS: usize = 16384;

/// Whether a float is safe to square like Python's `x ** 2` (no subnormal
/// result where `pow` and a multiply could part, no OverflowError).
fn plain(x: f64) -> bool {
    x == 0.0 || (x.abs() > 1e-100 && x.abs() < 1e100)
}

/// The colour table `render_linear_gradient` interpolates (None: declined).
pub fn gradient_lut(stops: &[(f64, [u8; 4])], mode: u32, level: u32) -> Res<Vec<[f64; 4]>> {
    let effective = if stops.len() == 1 || mode > 3 { 0 } else { mode };
    if matches!(effective, 1 | 2) && !fma_kernels() {
        return Err("gradient colour mixing needs numpy's FMA BLAS kernels".into());
    }
    let lut = gradient_mix(stops, &linspace(0.0, 1.0, LUT_STEPS + 1), mode, level);
    if !lut.iter().flatten().all(|v| v.is_finite()) {
        return Err("gradient colour table is not finite".into());
    }
    Ok(lut)
}

/// `render_linear_gradient(stops, points, canvas, mode, level, region=window)`.
pub fn render_linear(stops: &[(f64, [u8; 4])], points: [f64; 4], canvas: (usize, usize), mode: u32, level: u32, window: Rect) -> Res<Buf> {
    check_points(points)?;
    render_with_lut(&gradient_lut(stops, mode, level)?, points, canvas, window)
}

fn check_points(points: [f64; 4]) -> Res<()> {
    let (dx, dy) = (points[2] - points[0], points[3] - points[1]);
    if !points.iter().all(|v| v.is_finite()) || !plain(dx) || !plain(dy) {
        return Err("gradient points out of range".into());
    }
    Ok(())
}

/// `render_linear` with the table already made (`gradient_lut`).
pub fn render_with_lut(lut: &[[f64; 4]], points: [f64; 4], canvas: (usize, usize), window: Rect) -> Res<Buf> {
    check_points(points)?;
    if lut.len() != LUT_STEPS + 1 {
        return Err("gradient table has the wrong size".into());
    }
    let [x0, y0, x1, y1] = points;
    let (dx, dy) = (x1 - x0, y1 - y0);
    let mut length2 = dx * dx + dy * dy;
    if length2 == 0.0 {
        length2 = 1.0;
    }
    let ch = canvas.1 as i64;
    let (w, h) = size(window);
    let ax: Vec<f64> = (window[0]..window[2]).map(|x| (x as f64 + 0.5 - x0) * dx).collect();
    let col4: Vec<usize> = (window[0]..window[2]).map(|x| x.rem_euclid(4) as usize).collect();
    let mut px = vec![0u8; w * h * 4];
    for (row, out) in px.chunks_exact_mut(w * 4).enumerate() {
        let y = window[1] + row as i64;
        let by = ((y as f64 + 0.5) - y0) * dy;
        let brow = &BAYER[(ch - 1 - y).rem_euclid(4) as usize];
        let thr = brow.map(|b| (b + 0.5) / 16.0);
        for (k, p) in out.chunks_exact_mut(4).enumerate() {
            let t = clip((ax[k] + by) / length2, 0.0, 1.0);
            let idx = t * LUT_STEPS as f64;
            let i0 = (idx as i64).min(LUT_STEPS as i64 - 1);
            let fr = idx - i0 as f64;
            let (a, b) = (&lut[i0 as usize], &lut[i0 as usize + 1]);
            let th = thr[col4[k]];
            for c in 0..4 {
                let v = a[c] * (1.0 - fr) + b[c] * fr;
                // floor(v + 1 - thr) clipped to a byte: v >= 0 (the table
                // is clipped to [0, 255], the weights to [0, 1]) and thr < 1,
                // so the value is positive and truncation is the floor.
                p[c] = ((v + 1.0 - th) as i64).clamp(0, 255) as u8;
            }
        }
    }
    Ok(Buf { x: window[0], y: window[1], w, h, px })
}
