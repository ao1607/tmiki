//! numpy's float64 sums: `np.add.reduce` / `np.add.reduceat` use the
//! pairwise summation of `DOUBLE_pairwise_sum` (8 accumulators per block of
//! up to 128, halving above), `np.cumsum` adds left to right.

const PW_BLOCKSIZE: usize = 128;

/// `DOUBLE_pairwise_sum` of a contiguous run.
pub fn pairwise_sum(a: &[f64]) -> f64 {
    let n = a.len();
    if n < 8 {
        let mut res = -0.0f64;
        for &v in a {
            res += v;
        }
        res
    } else if n <= PW_BLOCKSIZE {
        let mut r = [0.0f64; 8];
        r.copy_from_slice(&a[..8]);
        let mut i = 8;
        while i < n - (n % 8) {
            for k in 0..8 {
                r[k] += a[i + k];
            }
            i += 8;
        }
        let mut res = ((r[0] + r[1]) + (r[2] + r[3])) + ((r[4] + r[5]) + (r[6] + r[7]));
        while i < n {
            res += a[i];
            i += 1;
        }
        res
    } else {
        let mut n2 = n / 2;
        n2 -= n2 % 8;
        pairwise_sum(&a[..n2]) + pairwise_sum(&a[n2..])
    }
}

/// `np.add.reduceat(a, indices)` for strictly increasing `indices` inside
/// `a`: each segment's first element plus the pairwise sum of the rest.
pub fn add_reduceat(a: &[f64], indices: &[usize]) -> Vec<f64> {
    indices
        .iter()
        .enumerate()
        .map(|(k, &s)| {
            let e = indices.get(k + 1).copied().unwrap_or(a.len());
            if e > s + 1 {
                a[s] + pairwise_sum(&a[s + 1..e])
            } else {
                a[s]
            }
        })
        .collect()
}

/// `np.cumsum` of float64.
pub fn cumsum(a: &[f64]) -> Vec<f64> {
    let mut acc = 0.0f64;
    a.iter()
        .map(|&v| {
            acc += v;
            acc
        })
        .collect()
}
