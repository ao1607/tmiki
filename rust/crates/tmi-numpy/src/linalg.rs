//! The two linear-algebra calls the renderer makes through numpy's OpenBLAS
//! (scipy-openblas 0.3.34, DYNAMIC_ARCH, Haswell kernels on an FMA CPU):
//!
//! * `x @ m.T` for an (n, 3) float64 `x` and a 3 x 3 `m`, n >= 2: dgemm,
//!   each output element accumulated over k with fused multiply-adds,
//!   `fma(x2, m[j][2], fma(x1, m[j][1], x0 * m[j][0]))`. (numpy sends a
//!   single row, n == 1, through gemv, which sums differently: the renderer
//!   never has one.)
//! * `np.linalg.inv` of a 2 x 2 float64 matrix: `gesv` against the identity,
//!   i.e. OpenBLAS's getrf (partial pivoting, the column scaled by the
//!   reciprocal of the pivot) and getrs (two triangular solves whose
//!   packed diagonal holds reciprocals), without fused multiply-adds.
//!
//! Both were pinned against numpy bit for bit (tests/test_rust_kernels.py).
//! A CPU without FMA runs other OpenBLAS kernels, so callers of `matmul3`
//! check `fma_kernels()` and decline otherwise.

/// Whether numpy's OpenBLAS would pick FMA dgemm kernels on this CPU (the
/// `matmul3` arithmetic). Conservative: AVX2 and FMA both present.
pub fn fma_kernels() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        std::is_x86_feature_detected!("fma") && std::is_x86_feature_detected!("avx2")
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        false
    }
}

/// One row of `x @ m.T` (numpy float64 matmul through OpenBLAS dgemm).
#[inline]
pub fn matmul3(x: [f64; 3], m: &[[f64; 3]; 3]) -> [f64; 3] {
    [0, 1, 2].map(|j| {
        let acc = x[0] * m[j][0];
        let acc = x[1].mul_add(m[j][1], acc);
        x[2].mul_add(m[j][2], acc)
    })
}

/// `np.linalg.inv` of `[[a, b], [c, d]]` (row major), or None where numpy
/// raises (a zero pivot) or where the arithmetic could leave the path pinned
/// here (non-finite input, a pivot too small for the reciprocal path).
pub fn inv2(m: [[f64; 2]; 2]) -> Option<[[f64; 2]; 2]> {
    let [[a, b], [c, d]] = m;
    if !(a.is_finite() && b.is_finite() && c.is_finite() && d.is_finite()) {
        return None;
    }
    // getrf: idamax picks the first largest |.| of column 0.
    let swap = c.abs() > a.abs();
    let (p, q, s, t) = if swap { (c, d, a, b) } else { (a, b, c, d) };
    if !(p.abs() >= f64::MIN_POSITIVE) {
        return None;
    }
    let l = s * (1.0 / p);
    let u = t - l * q;
    if !(u.abs() >= f64::MIN_POSITIVE) || !l.is_finite() || !u.is_finite() {
        return None;
    }
    let (rp, ru) = (1.0 / p, 1.0 / u);
    let mut out = [[0.0f64; 2]; 2];
    for k in 0..2 {
        // Row i of P @ I: 1 where the permuted row is k.
        let row = |i: usize| -> f64 {
            let src = if swap { 1 - i } else { i };
            if src == k {
                1.0
            } else {
                0.0
            }
        };
        let y0 = row(0);
        let y1 = row(1) - l * y0;
        let x1 = y1 * ru;
        let x0 = (y0 - q * x1) * rp;
        out[0][k] = x0;
        out[1][k] = x1;
    }
    if out.iter().flatten().all(|v| v.is_finite()) {
        Some(out)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_integer_matrices() {
        let i = inv2([[5.0, -5.0], [5.0, 5.0]]).unwrap();
        assert_eq!(i, [[0.1, 0.1], [-0.1, 0.1]]);
        assert!(inv2([[1.0, 2.0], [2.0, 4.0]]).is_none());
        assert!(inv2([[0.0, 0.0], [0.0, 1.0]]).is_none());
    }
}
