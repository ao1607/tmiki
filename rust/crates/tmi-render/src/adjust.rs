//! Correction-layer regrade (composite step 2).
//!
//! Mirrors `_regrade_pixels` / `_regrade_uniform_by_colour` of
//! `tmiref/clip.py`. The filter arrives as a 3x256 f32 LUT that is
//! bit-identical to the numpy filter on the 256 possible 8-bit inputs (or,
//! for a filter that mixes channels, per pixel); CSP then mixes it in like a
//! normal layer over an opaque backdrop, in integers (research §15.7):
//!
//!   F   = floor(filtered * 255 + 0.5)            (f32 widened to f64)
//!   a'  = (mask * opacity256) >> 8               (mask 255 when absent)
//!   a'  = a' * base_alpha // 255                 (with a clipping base)
//!   out = b + sign(n) * ((2|n| + 255) // 510),   n = (F - b) * a'

use rayon::prelude::*;

/// The 8-bit filtered value of a LUT entry.
#[inline]
fn filtered8(v: f32) -> i64 {
    (v as f64 * 255.0 + 0.5).floor() as i64
}

/// `b + round((f - b) * a / 255)`, halves away from zero.
#[inline]
fn mix(b: u8, f: i64, a: i64) -> u8 {
    let b = b as i64;
    let n = (f - b) * a;
    (b + n.signum() * ((2 * n.abs() + 255) / 510)).clamp(0, 255) as u8
}

/// The weight of pixel `p`.
#[inline]
fn weight(op256: i64, mask: Option<&[u8]>, base_alpha: Option<&[u8]>, p: usize) -> i64 {
    let mut a = (mask.map_or(255, |m| m[p] as i64) * op256) >> 8;
    if let Some(b) = base_alpha {
        a = a * b[p] as i64 / 255;
    }
    a
}

/// Regrade the RGBA image `rgba` (`width` pixels wide) into `out` (same
/// size). `lut` holds 3 x 256 filtered values ([R0..R255, G.., B..]); `mask`
/// and `base_alpha` are optional one-byte-per-pixel planes.
pub fn regrade(
    out: &mut [u8],
    rgba: &[u8],
    width: usize,
    lut: &[f32],
    opacity: f32,
    mask: Option<&[u8]>,
    base_alpha: Option<&[u8]>,
) {
    let row = (width * 4).max(4);
    let op256 = (opacity as f64 * 256.0).round() as i64;
    let f: Vec<i64> = lut.iter().map(|&v| filtered8(v)).collect();
    out.par_chunks_mut(row).enumerate().for_each(|(y, dst)| {
        for x in 0..width {
            let p = y * width + x;
            let a = weight(op256, mask, base_alpha, p);
            let src = &rgba[p * 4..p * 4 + 4];
            for c in 0..3 {
                dst[x * 4 + c] = mix(src[c], f[c * 256 + src[c] as usize], a);
            }
            dst[x * 4 + 3] = src[3];
        }
    });
}

/// `regrade` with the filtered colour given per pixel (`filtered[p]`, from a
/// filter that mixes channels) instead of a per-channel LUT.
pub fn regrade_with(
    out: &mut [u8],
    rgba: &[u8],
    width: usize,
    filtered: &[[f32; 3]],
    opacity: f32,
    mask: Option<&[u8]>,
    base_alpha: Option<&[u8]>,
) {
    let row = (width * 4).max(4);
    let op256 = (opacity as f64 * 256.0).round() as i64;
    out.par_chunks_mut(row).enumerate().for_each(|(y, dst)| {
        for x in 0..width {
            let p = y * width + x;
            let a = weight(op256, mask, base_alpha, p);
            let src = &rgba[p * 4..p * 4 + 4];
            for c in 0..3 {
                dst[x * 4 + c] = mix(src[c], filtered8(filtered[p][c]), a);
            }
            dst[x * 4 + 3] = src[3];
        }
    });
}

#[cfg(test)]
mod tests {
    /// Full weight gives the filtered value itself.
    #[test]
    fn full_weight_is_the_filter() {
        for b in 0..=255u8 {
            for f in [0i64, 1, 127, 128, 254, 255] {
                assert_eq!(super::mix(b, f, 255) as i64, f);
            }
        }
    }
}
