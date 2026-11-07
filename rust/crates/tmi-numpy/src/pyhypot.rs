//! CPython 3.13's `math.hypot(x, y)` (`vector_norm` in Modules/
//! mathmodule.c): not libm's `hypot` but a scaled, compensated sum of
//! squares with one Newton correction, whose last bit can differ from
//! libm's. Reproduced step for step; checked against the venv's
//! `math.hypot` by tests/test_rust_kernels.py.

/// `dl_mul`: the error-free product (hi, lo) with `hi + lo == x * y`.
#[inline]
fn dl_mul(x: f64, y: f64) -> (f64, f64) {
    let z = x * y;
    (z, x.mul_add(y, -z))
}

/// `dl_fast_sum`: compensated sum of `a + b` with `|a| >= |b|`.
#[inline]
fn dl_fast_sum(a: f64, b: f64) -> (f64, f64) {
    let x = a + b;
    (x, (a - x) + b)
}

/// `frexp(v).1` for a finite, nonzero, normal or subnormal `v`.
fn frexp_exp(v: f64) -> i32 {
    let bits = v.to_bits();
    let e = ((bits >> 52) & 0x7ff) as i32;
    if e == 0 {
        // subnormal: normalise first
        return frexp_exp(v * 2f64.powi(54)) - 54;
    }
    e - 1022
}

fn vector_norm(vec: &mut [f64], max: f64, found_nan: bool) -> f64 {
    if max.is_infinite() {
        return max;
    }
    if found_nan {
        return f64::NAN;
    }
    if max == 0.0 || vec.len() <= 1 {
        return max;
    }
    let max_e = frexp_exp(max);
    if max_e < -1023 {
        for v in vec.iter_mut() {
            *v /= f64::MIN_POSITIVE;
        }
        return f64::MIN_POSITIVE * vector_norm(vec, max / f64::MIN_POSITIVE, found_nan);
    }
    let scale = 2f64.powi(-max_e);
    let (mut csum, mut frac1, mut frac2) = (1.0f64, 0.0f64, 0.0f64);
    for &v in vec.iter() {
        let x = v * scale;
        let pr = dl_mul(x, x);
        let sm = dl_fast_sum(csum, pr.0);
        csum = sm.0;
        frac1 += pr.1;
        frac2 += sm.1;
    }
    let mut h = (csum - 1.0 + (frac1 + frac2)).sqrt();
    let pr = dl_mul(-h, h);
    let sm = dl_fast_sum(csum, pr.0);
    csum = sm.0;
    frac1 += pr.1;
    frac2 += sm.1;
    let x = csum - 1.0 + (frac1 + frac2);
    h += x / (2.0 * h);
    h / scale
}

/// `math.hypot(x, y)`.
pub fn py_hypot(x: f64, y: f64) -> f64 {
    let mut v = [x.abs(), y.abs()];
    let found_nan = v[0].is_nan() || v[1].is_nan();
    let mut max = 0.0f64;
    for &a in &v {
        if a > max {
            max = a;
        }
    }
    vector_norm(&mut v, max, found_nan)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_values() {
        assert_eq!(py_hypot(3.0, 4.0), 5.0);
        assert_eq!(py_hypot(0.0, 0.0), 0.0);
        assert_eq!(py_hypot(-5.0, 0.0), 5.0);
        assert_eq!(py_hypot(f64::INFINITY, f64::NAN), f64::INFINITY);
        assert!(py_hypot(1.0, f64::NAN).is_nan());
        assert_eq!(py_hypot(1e-310, 0.0), 1e-310);
        assert_eq!(frexp_exp(1.0), 1);
        assert_eq!(frexp_exp(0.75), 0);
    }
}
