//! `Image.reduce(n)` of an "L" image whose sides are multiples of `n`
//! (libImaging/Reduce.c, the n x n box): each output pixel is
//! `((sum + n²/2) * m) >> 24` in 32-bit arithmetic, with the float32
//! multiplier `m = (u32)(2^32 / (256 * n²))`. Checked against Pillow for
//! every block sum (tests/test_rust_kernels.py).

/// `division_UINT32(n², 8)`.
pub fn multiplier(n: u32) -> u32 {
    let max_dividend = (1u32 << 8) * n * n;
    let max_int = 4_294_967_296.0f32;
    (max_int / max_dividend as f32) as u32
}

/// One output pixel from the sum of its n x n block.
#[inline]
pub fn from_sum(sum: u32, n: u32, mult: u32) -> u8 {
    let amend = n * n / 2;
    ((sum + amend).wrapping_mul(mult) >> 24) as u8
}

/// Reduce rows `[r0, r1)` of the output (all columns) of an image
/// `src_w` pixels wide whose first held row is source row `src_row0`
/// (`src` must hold every source row the output rows read).
pub fn reduce_rows(src: &[u8], src_w: usize, src_row0: usize, n: usize, r0: usize, r1: usize) -> Vec<u8> {
    let out_w = src_w / n;
    let mult = multiplier(n as u32);
    let mut out = vec![0u8; out_w * (r1 - r0)];
    let mut sums = vec![0u32; out_w];
    for (oy, row) in (r0..r1).zip(out.chunks_exact_mut(out_w.max(1))) {
        sums.iter_mut().for_each(|s| *s = 0);
        for sy in oy * n..oy * n + n {
            let line = &src[(sy - src_row0) * src_w..(sy - src_row0 + 1) * src_w];
            for (s, block) in sums.iter_mut().zip(line.chunks_exact(n)) {
                *s += block.iter().map(|&v| v as u32).sum::<u32>();
            }
        }
        for (o, &s) in row.iter_mut().zip(&sums) {
            *o = from_sum(s, n as u32, mult);
        }
    }
    out
}

/// `Image.reduce(n)` of a whole `w` x `h` image (`w`, `h` multiples of n).
pub fn reduce(src: &[u8], w: usize, h: usize, n: usize) -> Vec<u8> {
    reduce_rows(src, w, 0, n, 0, h / n)
}

/// `division_UINT32(divider, 8)`.
fn division(divider: u32) -> u32 {
    let max_dividend = (1u32 << 8).wrapping_mul(divider);
    (4_294_967_296.0f32 / max_dividend as f32) as u32
}

/// `ImagingReduce(im, xscale, yscale, box)` of an 8-bit image with `bands`
/// bytes per pixel; `bx` is (left, top, width, height) within the image.
/// Every specialised kernel of Reduce.c (1xN, Nx1, 2x2, 3x3, 4x4, 5x5,
/// NxN and the corners) computes `((sum + n/2) * division(n)) >> 24` for
/// the n source pixels of its block (the power-of-two ones as a shift,
/// the same value), so one loop does them all. The last column and row
/// take the partial blocks.
pub fn reduce_box(src: &[u8], w: usize, bands: usize, xscale: usize, yscale: usize, bx: [usize; 4]) -> (Vec<u8>, usize, usize) {
    let [bx0, by0, bw, bh] = bx;
    let out_w = bw.div_ceil(xscale);
    let out_h = bh.div_ceil(yscale);
    let stride = w * bands;
    let mut out = vec![0u8; out_w * out_h * bands];
    let mut sums = vec![0u32; bands];
    for oy in 0..out_h {
        let ys = by0 + oy * yscale;
        let ye = (ys + yscale).min(by0 + bh);
        for ox in 0..out_w {
            let xs = bx0 + ox * xscale;
            let xe = (xs + xscale).min(bx0 + bw);
            let n = ((ye - ys) * (xe - xs)) as u32;
            let mult = division(n);
            sums.iter_mut().for_each(|s| *s = n / 2);
            for y in ys..ye {
                let row = &src[y * stride + xs * bands..y * stride + xe * bands];
                for px in row.chunks_exact(bands) {
                    for (s, &v) in sums.iter_mut().zip(px) {
                        *s += v as u32;
                    }
                }
            }
            let o = &mut out[(oy * out_w + ox) * bands..(oy * out_w + ox + 1) * bands];
            for (ov, &s) in o.iter_mut().zip(&sums) {
                *ov = (s.wrapping_mul(mult) >> 24) as u8;
            }
        }
    }
    (out, out_w, out_h)
}
