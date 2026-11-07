//! `np.interp` and `np.linspace` on float64, as numpy 2.5 computes them
//! (numpy/_core/src/multiarray/compiled_base.c `arr_interp`,
//! numpy/_core/function_base.py `linspace`).

const LIKELY_IN_CACHE_SIZE: isize = 8;

/// numpy's `binary_search_with_guess`, step for step (so that even an
/// unsorted `xp` gives numpy's answer): on sorted input, the last index `j`
/// with `xp[j] <= x`, -1 below `xp[0]`, `len` above `xp[len - 1]`.
fn search(key: f64, arr: &[f64], mut guess: isize) -> isize {
    let len = arr.len() as isize;
    let a = |i: isize| arr[i as usize];
    let mut imin: isize = 0;
    let mut imax: isize = len;
    if key > a(len - 1) {
        return len;
    } else if key < a(0) {
        return -1;
    }
    if len <= 4 {
        let mut i = 1;
        while i < len && key >= a(i) {
            i += 1;
        }
        return i - 1;
    }
    if guess > len - 3 {
        guess = len - 3;
    }
    if guess < 1 {
        guess = 1;
    }
    if key < a(guess) {
        if key < a(guess - 1) {
            imax = guess - 1;
            if guess > LIKELY_IN_CACHE_SIZE && key >= a(guess - LIKELY_IN_CACHE_SIZE) {
                imin = guess - LIKELY_IN_CACHE_SIZE;
            }
        } else {
            return guess - 1;
        }
    } else if key < a(guess + 1) {
        return guess;
    } else if key < a(guess + 2) {
        return guess + 1;
    } else {
        imin = guess + 2;
        if guess < len - LIKELY_IN_CACHE_SIZE - 1 && key < a(guess + LIKELY_IN_CACHE_SIZE) {
            imax = guess + LIKELY_IN_CACHE_SIZE;
        }
    }
    while imin < imax {
        let imid = imin + ((imax - imin) >> 1);
        if key >= a(imid) {
            imin = imid + 1;
        } else {
            imax = imid;
        }
    }
    imin - 1
}

/// `np.interp(x, xp, fp)` with the default `left` / `right` (the end values).
/// `xp` must be sorted (as numpy requires) and as long as `fp`.
pub fn interp(x: &[f64], xp: &[f64], fp: &[f64]) -> Vec<f64> {
    assert!(!xp.is_empty() && xp.len() == fp.len());
    let len = xp.len();
    let (lval, rval) = (fp[0], fp[len - 1]);
    if len == 1 {
        return x.iter().map(|&v| if v < xp[0] { lval } else if v > xp[0] { rval } else { fp[0] }).collect();
    }
    let mut j: isize = 0;
    x.iter()
        .map(|&v| {
            if v.is_nan() {
                return v;
            }
            j = search(v, xp, j);
            if j == -1 {
                lval
            } else if j == len as isize {
                rval
            } else {
                let j = j as usize;
                if j == len - 1 || xp[j] == v {
                    fp[j]
                } else {
                    let slope = (fp[j + 1] - fp[j]) / (xp[j + 1] - xp[j]);
                    let mut res = slope * (v - xp[j]) + fp[j];
                    if res.is_nan() {
                        res = slope * (v - xp[j + 1]) + fp[j + 1];
                        if res.is_nan() && fp[j] == fp[j + 1] {
                            res = fp[j];
                        }
                    }
                    res
                }
            }
        })
        .collect()
}

/// `np.linspace(start, stop, num)` (endpoint included, float64).
pub fn linspace(start: f64, stop: f64, num: usize) -> Vec<f64> {
    if num == 0 {
        return Vec::new();
    }
    let div = num - 1;
    let delta = stop - start;
    let mut y: Vec<f64> = (0..num).map(|i| i as f64).collect();
    if div > 0 {
        let step = delta / div as f64;
        if step == 0.0 {
            for v in &mut y {
                *v = (*v / div as f64) * delta;
            }
        } else {
            for v in &mut y {
                *v *= step;
            }
        }
    } else {
        // numpy: step = nan; y = y * delta
        for v in &mut y {
            *v *= delta;
        }
    }
    for v in &mut y {
        *v += start;
    }
    if num > 1 {
        y[num - 1] = stop;
    }
    y
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ends_and_ties() {
        let xp = [0.0, 0.5, 0.5, 1.0];
        let fp = [0.0, 1.0, 2.0, 3.0];
        assert_eq!(interp(&[-1.0, 0.0, 0.5, 2.0, 1.0], &xp, &fp), vec![0.0, 0.0, 2.0, 3.0, 3.0]);
        let l = linspace(0.0, 1.0, 5);
        assert_eq!(l, vec![0.0, 0.25, 0.5, 0.75, 1.0]);
    }
}
