//! CPython float operations whose results tmiki depends on.

/// `round(x)` of a Python float (ties to even), as an integer.
#[inline]
pub fn py_round(x: f64) -> i64 {
    x.round_ties_even() as i64
}

/// Python's / numpy's floating `%` (`float_rem`, `npy_divmod`): the result
/// takes the sign of the divisor; a zero result is `copysign(0, b)`.
#[inline]
pub fn py_mod(a: f64, b: f64) -> f64 {
    let m = a % b; // fmod
    if b == 0.0 {
        return m;
    }
    if m != 0.0 {
        if (b < 0.0) != (m < 0.0) {
            m + b
        } else {
            m
        }
    } else {
        0.0f64.copysign(b)
    }
}

/// `math.floor(x)` / `np.floor`.
#[inline]
pub fn floor(x: f64) -> f64 {
    x.floor()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mod_sign_follows_divisor() {
        assert_eq!(py_mod(-1.0, 6.0), 5.0);
        assert_eq!(py_mod(7.5, 6.0), 1.5);
        assert!(py_mod(-6.0, 6.0) == 0.0 && py_mod(-6.0, 6.0).is_sign_positive());
        assert_eq!(py_round(2.5), 2);
        assert_eq!(py_round(3.5), 4);
    }
}
