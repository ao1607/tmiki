//! numpy's rounding: `np.round` / `np.rint` round half to even.

/// `clip(round_half_even(v * 255), 0, 255)` as a byte: the tail of numpy's
/// `np.clip(np.round(arr * 255.0), 0.0, 255.0).astype(np.uint8)` on float32.
///
/// `round_ties_even` is a libm call on the x86-64 baseline (no SSE4.1), which
/// made the regrade kernel memory-bound on a function call per channel.
/// Adding and subtracting 2^23 rounds an f32 to an integer, ties to even,
/// whenever the sum lands in [2^23, 2^24) (where the spacing is exactly 1);
/// for inputs in [-1, 0) it lands on a multiple of 0.5 at or below 0 instead,
/// which the final clamp turns into 0 exactly as it does the true rounding.
/// Clamping to [-1, 256] first keeps every input in that range (and maps NaN
/// to 0, as `as u8` did). `same_as_round_ties_even` checks every f32 in
/// between.
#[inline]
pub fn to_byte(v: f32) -> u8 {
    const MAGIC: f32 = 8_388_608.0; // 2^23
    let x = (v * 255.0).max(-1.0).min(256.0);
    ((x + MAGIC) - MAGIC).clamp(0.0, 255.0) as u8
}

/// `np.rint` / `np.round` (0 decimals) of a float32: half to even.
#[inline]
pub fn rint_f32(v: f32) -> f32 {
    v.round_ties_even()
}

/// `np.rint` / `np.round` (0 decimals) of a float64: half to even.
#[inline]
pub fn rint_f64(v: f64) -> f64 {
    v.round_ties_even()
}

/// `np.clip(np.rint(v), 0, 255).astype(np.uint8)` of a float32.
#[inline]
pub fn rint_to_u8(v: f32) -> u8 {
    let r = v.round_ties_even();
    if r.is_nan() {
        0
    } else {
        r.clamp(0.0, 255.0) as u8
    }
}

/// `rint_to_u8` without the libm call for the usual inputs: adding and
/// subtracting 2^23 rounds an f32 in [0, 2^22) to an integer, ties to even
/// (see `to_byte`); anything else takes `rint_to_u8`.
#[inline]
pub fn rint_to_u8_fast(v: f32) -> u8 {
    const MAGIC: f32 = 8_388_608.0; // 2^23
    if (0.0..4_194_304.0).contains(&v) {
        ((v + MAGIC) - MAGIC).min(255.0) as u8
    } else {
        rint_to_u8(v)
    }
}

#[cfg(test)]
mod tests {
    use super::{rint_to_u8, rint_to_u8_fast, to_byte};

    /// f32s over [-1, 2^23] by bit pattern, every half-integer up to 600 and
    /// its neighbours, and the special values.
    #[test]
    fn rint_fast_is_rint() {
        let check = |v: f32| assert_eq!(rint_to_u8_fast(v), rint_to_u8(v), "{v:e}");
        for bits in (0u32..=8_388_608.0f32.to_bits()).step_by(101).chain((0x8000_0000..=(-1.0f32).to_bits()).step_by(11)) {
            check(f32::from_bits(bits));
        }
        for k in 0..1200 {
            let v = k as f32 / 2.0;
            for d in -3i32..=3 {
                check(f32::from_bits((v.to_bits() as i32 + d).max(0) as u32));
            }
        }
        for v in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -0.0, 4_194_303.5, 4_194_304.0, 1e30] {
            check(v);
        }
    }

    /// Every f32 v with v * 255 in [-2, 300], plus the extremes.
    #[test]
    fn same_as_round_ties_even() {
        use rayon::prelude::*;
        let reference = |v: f32| (v * 255.0).round_ties_even().clamp(0.0, 255.0) as u8;
        // Checks bit patterns a..=b on the rayon pool; returns how many.
        let range = |a: u32, b: u32| -> u64 {
            let blocks: Vec<u32> = (a..=b).step_by(1 << 20).collect();
            blocks
                .par_iter()
                .map(|&s| {
                    let e = s.saturating_add((1 << 20) - 1).min(b);
                    for bits in s..=e {
                        let v = f32::from_bits(bits);
                        assert_eq!(to_byte(v), reference(v), "{v:e}");
                    }
                    (e - s) as u64 + 1
                })
                .sum()
        };
        // Negative floats: bit patterns grow toward more negative values.
        let mut checked = range(0x8000_0000u32, (-2.0f32 / 255.0).to_bits());
        checked += range(0, (300.0f32 / 255.0).to_bits());
        for v in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, f32::MAX, f32::MIN, 1e30, -1e30] {
            assert_eq!(to_byte(v), reference(v), "{v:e}");
        }
        assert!(checked > 2_000_000_000);
    }
}
