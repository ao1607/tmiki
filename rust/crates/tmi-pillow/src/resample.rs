//! `ImagingResample` (libImaging/Resample.c) for 8-bit images of 1 to 4
//! bands, packed as `tobytes()` gives them: separable convolution, the
//! horizontal pass first (over only the source rows the vertical pass
//! reads), coefficients in f64 normalised to 22-bit fixed point, sums in
//! i32 starting at 0.5 and clipped to 0..=255.
//!
//! The box is float32 (the `(ffff)` of `_imaging.c` `_resize`), and so is
//! `in1 - in0`; everything after that is f64, in the C order. The filters
//! call the C runtime's `sin` / `cos` (UCRT on Windows, as Pillow's MSVC
//! build does).

/// Pillow's resampling filters (NEAREST is a transform, not here).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Filter {
    Box,
    Bilinear,
    Hamming,
    Bicubic,
    Lanczos,
}

impl Filter {
    /// `Image.Resampling` value (BOX 4, BILINEAR 2, HAMMING 5, BICUBIC 3,
    /// LANCZOS 1).
    pub fn from_pil(v: u32) -> Option<Filter> {
        Some(match v {
            1 => Filter::Lanczos,
            2 => Filter::Bilinear,
            3 => Filter::Bicubic,
            4 => Filter::Box,
            5 => Filter::Hamming,
            _ => return None,
        })
    }

    /// `struct filter.support`, also `Image._filters_support`.
    pub fn support(self) -> f64 {
        match self {
            Filter::Box => 0.5,
            Filter::Bilinear | Filter::Hamming => 1.0,
            Filter::Bicubic => 2.0,
            Filter::Lanczos => 3.0,
        }
    }

    #[inline]
    fn eval(self, x: f64) -> f64 {
        match self {
            Filter::Box => {
                if x > -0.5 && x <= 0.5 {
                    1.0
                } else {
                    0.0
                }
            }
            Filter::Bilinear => {
                let x = x.abs();
                if x < 1.0 {
                    1.0 - x
                } else {
                    0.0
                }
            }
            Filter::Hamming => {
                let mut x = if x < 0.0 { -x } else { x };
                if x == 0.0 {
                    return 1.0;
                }
                if x >= 1.0 {
                    return 0.0;
                }
                x *= std::f64::consts::PI;
                x.sin() / x * (0.54f32 as f64 + 0.46f32 as f64 * x.cos())
            }
            Filter::Bicubic => {
                const A: f64 = -0.5;
                let x = if x < 0.0 { -x } else { x };
                if x < 1.0 {
                    ((A + 2.0) * x - (A + 3.0)) * x * x + 1.0
                } else if x < 2.0 {
                    (((x - 5.0) * x + 8.0) * x - 4.0) * A
                } else {
                    0.0
                }
            }
            Filter::Lanczos => {
                if (-3.0..3.0).contains(&x) {
                    sinc(x) * sinc(x / 3.0)
                } else {
                    0.0
                }
            }
        }
    }
}

#[inline]
fn sinc(x: f64) -> f64 {
    if x == 0.0 {
        return 1.0;
    }
    let x = x * std::f64::consts::PI;
    x.sin() / x
}

use rayon::prelude::*;

const PRECISION_BITS: u32 = 32 - 8 - 2;

/// The coefficients of one pass: for each output index, the first source
/// index and the count, and `ksize` fixed-point weights.
struct Coeffs {
    ksize: usize,
    bounds: Vec<(i32, i32)>,
    kk: Vec<i32>,
}

/// `precompute_coeffs` then `normalize_coeffs_8bpc`.
fn precompute(in_size: i32, in0: f32, in1: f32, out_size: usize, filter: Filter) -> Coeffs {
    let scale = (in1 - in0) as f64 / out_size as f64;
    let filterscale = if scale < 1.0 { 1.0 } else { scale };
    let support = filter.support() * filterscale;
    let ksize = (support.ceil() as i32 * 2 + 1) as usize;
    let inv_filterscale = 1.0 / filterscale;
    let mut bounds = Vec::with_capacity(out_size);
    let mut pre = vec![0f64; out_size * ksize];
    for xx in 0..out_size {
        let center = in0 as f64 + (xx as f64 + 0.5) * scale;
        let mut ww = 0.0f64;
        let mut xmin = (center - support + 0.5) as i32;
        if xmin < 0 {
            xmin = 0;
        }
        let mut xmax = (center + support + 0.5) as i32;
        if xmax > in_size {
            xmax = in_size;
        }
        xmax -= xmin;
        let k = &mut pre[xx * ksize..(xx + 1) * ksize];
        let n = xmax.max(0) as usize;
        for (x, kv) in k.iter_mut().enumerate().take(n) {
            let w = filter.eval(((x as i32 + xmin) as f64 - center + 0.5) * inv_filterscale);
            *kv = w;
            ww += w;
        }
        if ww != 0.0 {
            for kv in k.iter_mut().take(n) {
                *kv /= ww;
            }
        }
        bounds.push((xmin, xmax));
    }
    let one = (1u32 << PRECISION_BITS) as f64;
    let kk = pre
        .iter()
        .map(|&v| if v < 0.0 { (-0.5 + v * one) as i32 } else { (0.5 + v * one) as i32 })
        .collect();
    Coeffs { ksize, bounds, kk }
}

#[inline]
fn clip8(ss: i32) -> u8 {
    (ss >> PRECISION_BITS).clamp(0, 255) as u8
}

const HALF: i32 = 1 << (PRECISION_BITS - 1);

/// Horizontal pass: rows `offset..offset + rows` of `src` (`w` wide).
fn horizontal(src: &[u8], w: usize, bands: usize, offset: usize, rows: usize, c: &Coeffs) -> Vec<u8> {
    let out_w = c.bounds.len();
    let mut out = vec![0u8; out_w * rows * bands];
    let stride = w * bands;
    // Rows are independent: split them over the pool (the same sums in the
    // same order, so the same bytes).
    out.par_chunks_mut((out_w * bands).max(1)).enumerate().for_each(|(yy, orow)| {
        let line = &src[(yy + offset) * stride..(yy + offset + 1) * stride];
        for xx in 0..out_w {
            let (xmin, xmax) = c.bounds[xx];
            let k = &c.kk[xx * c.ksize..(xx + 1) * c.ksize];
            let base = xmin.max(0) as usize * bands;
            for b in 0..bands {
                let mut ss = HALF;
                for x in 0..xmax.max(0) as usize {
                    ss = ss.wrapping_add((line[base + x * bands + b] as i32).wrapping_mul(k[x]));
                }
                orow[xx * bands + b] = clip8(ss);
            }
        }
    });
    out
}

/// Vertical pass over `src` (`w` wide); `bounds` already shifted to `src`.
fn vertical(src: &[u8], w: usize, bands: usize, c: &Coeffs) -> Vec<u8> {
    let out_h = c.bounds.len();
    let stride = w * bands;
    let mut out = vec![0u8; stride * out_h];
    out.par_chunks_mut(stride.max(1)).enumerate().for_each(|(yy, orow)| {
        let mut acc = vec![HALF; stride];
        let (ymin, ymax) = c.bounds[yy];
        let k = &c.kk[yy * c.ksize..(yy + 1) * c.ksize];
        for y in 0..ymax.max(0) as usize {
            let row = (y as i32 + ymin) as usize;
            let line = &src[row * stride..(row + 1) * stride];
            let kv = k[y];
            for (a, &v) in acc.iter_mut().zip(line) {
                *a = a.wrapping_add((v as i32).wrapping_mul(kv));
            }
        }
        for (o, &a) in orow.iter_mut().zip(&acc) {
            *o = clip8(a);
        }
    });
    out
}

/// `ImagingResample(im, xsize, ysize, filter, box)` of an 8-bit image `w`
/// x `h` with `bands` bytes per pixel (1 = L, 2 = LA/La, 3 = RGB, 4 =
/// RGBA/RGBa/CMYK...). Every band is resampled; Pillow's 2-band images
/// resample bands 0 and 3 of their 4-byte storage, the same two values.
///
/// Computed by `resample_sparse` over the whole image (its loops skip zero
/// pixels and use AVX2 when there is; the bytes are `resample_pillow`'s).
pub fn resample(src: &[u8], w: usize, h: usize, bands: usize, out_w: usize, out_h: usize, filter: Filter, bx: [f32; 4]) -> Vec<u8> {
    assert_eq!(src.len(), w * h * bands);
    #[cfg(test)]
    if tests::REFERENCE.with(|r| r.get()) {
        return resample_pillow(src, w, h, bands, out_w, out_h, filter, bx);
    }
    if w == 0 || h == 0 || out_w == 0 || out_h == 0 {
        return resample_pillow(src, w, h, bands, out_w, out_h, filter, bx);
    }
    let win = Window { data: src, stride: w * bands, rect: (0, 0, w, h) };
    resample_sparse(Some(win), w, h, bands, out_w, out_h, filter, bx)
}

/// `ImagingResample` with Pillow's loops as Resample.c has them (the
/// reference `resample` is tested against).
pub fn resample_pillow(src: &[u8], w: usize, h: usize, bands: usize, out_w: usize, out_h: usize, filter: Filter, bx: [f32; 4]) -> Vec<u8> {
    assert_eq!(src.len(), w * h * bands);
    let need_h = out_w != w || bx[0] != 0.0 || bx[2] != out_w as f32;
    let need_v = out_h != h || bx[1] != 0.0 || bx[3] != out_h as f32;
    let mut cv = precompute(h as i32, bx[1], bx[3], out_h, filter);
    let ybox_first = cv.bounds[0].0;
    let last = cv.bounds[out_h - 1];
    let ybox_last = last.0 + last.1;
    let mut cur: Option<(Vec<u8>, usize)> = None;
    if need_h {
        let ch = precompute(w as i32, bx[0], bx[2], out_w, filter);
        for b in cv.bounds.iter_mut() {
            b.0 -= ybox_first;
        }
        let rows = (ybox_last - ybox_first).max(0) as usize;
        cur = Some((horizontal(src, w, bands, ybox_first as usize, rows, &ch), out_w));
    }
    if need_v {
        let (data, cw) = match &cur {
            Some((d, cw)) => (d.as_slice(), *cw),
            None => (src, w),
        };
        return vertical(data, cw, bands, &cv);
    }
    match cur {
        Some((d, _)) => d,
        None => src.to_vec(),
    }
}

/// The pixels of an image that is 0 in every byte outside the rectangle
/// `rect` = (x0, y0, x1, y1) (in the image, x0 < x1, y0 < y1): rectangle
/// row `r` is `data[r * stride..][..(x1 - x0) * bands]`.
#[derive(Clone, Copy)]
pub struct Window<'a> {
    pub data: &'a [u8],
    pub stride: usize,
    pub rect: (usize, usize, usize, usize),
}

impl<'a> Window<'a> {
    /// The part inside a `w` x `h` image of a `cw` x `ch` image (`data`,
    /// packed, `bands` bytes a pixel) placed at (`x`, `y`); None when
    /// nothing of it is inside.
    pub fn placed(data: &'a [u8], cw: usize, ch: usize, bands: usize, x: i64, y: i64, w: usize, h: usize) -> Option<Window<'a>> {
        assert_eq!(data.len(), cw * ch * bands);
        let (x0, y0) = (x.max(0), y.max(0));
        let (x1, y1) = ((x + cw as i64).min(w as i64), (y + ch as i64).min(h as i64));
        if x0 >= x1 || y0 >= y1 {
            return None;
        }
        let stride = cw * bands;
        let at = (y0 - y) as usize * stride + (x0 - x) as usize * bands;
        Some(Window { data: &data[at..], stride, rect: (x0 as usize, y0 as usize, x1 as usize, y1 as usize) })
    }

    /// Row `y` (image coordinates, inside the rectangle) of the rectangle.
    #[inline]
    pub fn row(&self, y: usize, bands: usize) -> &'a [u8] {
        let (x0, y0, x1, _) = self.rect;
        let at = (y - y0) * self.stride;
        &self.data[at..at + (x1 - x0) * bands]
    }

    /// The whole `w` x `h` image (zeros outside the rectangle).
    pub fn to_dense(&self, w: usize, h: usize, bands: usize) -> Vec<u8> {
        let mut out = vec![0u8; w * h * bands];
        let (x0, y0, x1, y1) = self.rect;
        for y in y0..y1 {
            out[(y * w + x0) * bands..(y * w + x1) * bands].copy_from_slice(self.row(y, bands));
        }
        out
    }
}

/// The output indices `[first, last + 1)` whose source range (`bounds`)
/// meets `[lo, hi)`; `(0, 0)` when none does.
fn touched(bounds: &[(i32, i32)], lo: usize, hi: usize) -> (usize, usize) {
    let mut first = None;
    let mut last = 0;
    for (i, &(min, n)) in bounds.iter().enumerate() {
        let (s, e) = (min.max(0) as usize, (min.max(0) + n.max(0)) as usize);
        if s < hi && lo < e {
            first.get_or_insert(i);
            last = i + 1;
        }
    }
    match first {
        Some(f) => (f, last),
        None => (0, 0),
    }
}

/// `resample(full, w, h, bands, out_w, out_h, filter, bx)` of the `w` x `h`
/// image `full` that is 0 outside `win` (`None`: 0 everywhere), without
/// `full`: the same coefficient tables (computed over the whole image, so
/// the normalisation near its edges is the same), the same fixed-point sums
/// and the same clipped 8-bit intermediate image, but only where they can
/// differ from 0.
///
/// Why the bytes are the same: a source byte outside `win` is 0, so its
/// product with any weight is 0 and leaving it out of a sum changes nothing
/// (i32 addition is associative and commutative). An intermediate byte of a
/// row outside `win`, or of a column whose source range misses `win`'s
/// columns, is `clip8(HALF) = (1 << 21) >> 22 = 0`; the vertical pass, for
/// the same reason, needs only the intermediate rows inside `win` (those are
/// computed exactly as Pillow computes them, from the full-image horizontal
/// coefficients), and an output byte whose rows or columns miss them is 0.
/// When a pass is skipped (`need_h` / `need_v` false) the other pass reads
/// the source as Pillow does.
pub fn resample_sparse(win: Option<Window>, w: usize, h: usize, bands: usize, out_w: usize, out_h: usize, filter: Filter, bx: [f32; 4]) -> Vec<u8> {
    resample_sparse_with(win, w, h, bands, out_w, out_h, filter, bx, None)
}

/// `resample_sparse` of the image whose pixels are `win`'s after `prep`
/// (applied to each row as it is read; it must keep 0 bytes 0, as the
/// premultiplication of RGBA and LA does), without the converted copy.
#[allow(clippy::too_many_arguments)]
pub fn resample_sparse_with(win: Option<Window>, w: usize, h: usize, bands: usize, out_w: usize, out_h: usize, filter: Filter, bx: [f32; 4], prep: Option<fn(&mut [u8])>) -> Vec<u8> {
    let mut out = vec![0u8; out_w * out_h * bands];
    let Some(win) = win else { return out };
    let (x0, y0, x1, y1) = win.rect;
    assert!(x0 < x1 && x1 <= w && y0 < y1 && y1 <= h);
    let need_h = out_w != w || bx[0] != 0.0 || bx[2] != out_w as f32;
    let need_v = out_h != h || bx[1] != 0.0 || bx[3] != out_h as f32;
    let cv = precompute(h as i32, bx[1], bx[3], out_h, filter);
    // Output rows (= intermediate rows when there is no vertical pass) and
    // the intermediate rows they read, both inside `win`'s rows.
    let (oy0, oy1, ry0, ry1) = if need_v {
        let (a, b) = touched(&cv.bounds, y0, y1);
        if a == b {
            return out;
        }
        let lo = cv.bounds[a..b].iter().map(|&(m, _)| m.max(0) as usize).min().unwrap();
        let hi = cv.bounds[a..b].iter().map(|&(m, n)| (m.max(0) + n.max(0)) as usize).max().unwrap();
        (a, b, lo.max(y0), hi.min(y1))
    } else {
        (y0, y1, y0, y1)
    };
    // Intermediate (= output) columns that can be non-zero.
    let ch = need_h.then(|| precompute(w as i32, bx[0], bx[2], out_w, filter));
    let (mx0, mx1) = match &ch {
        Some(c) => touched(&c.bounds, x0, x1),
        None => (x0, x1),
    };
    if mx0 == mx1 || ry0 >= ry1 {
        return out;
    }
    let mw = (mx1 - mx0) * bands;
    // The intermediate image's rows ry0..ry1, columns mx0..mx1.
    let mid: Vec<u8> = match &ch {
        Some(c) => {
            let sc = Scatter::new(c, x0, x1, mx0, mx1);
            let mut mid = vec![0u8; mw * (ry1 - ry0)];
            mid.par_chunks_mut(mw).enumerate().for_each_init(|| (Vec::new(), Vec::new()), |(acc, line), (r, mrow)| {
                let src = win.row(ry0 + r, bands);
                let src = match prep {
                    Some(p) => {
                        line.clear();
                        line.extend_from_slice(src);
                        p(line);
                        &line[..]
                    }
                    None => src,
                };
                horizontal_sparse_row(src, bands, &sc, mrow, acc);
            });
            mid
        }
        None => {
            let mut mid = Vec::with_capacity(mw * (ry1 - ry0));
            for y in ry0..ry1 {
                mid.extend_from_slice(win.row(y, bands));
            }
            if let Some(p) = prep {
                mid.par_chunks_mut(mw).for_each(p);
            }
            mid
        }
    };
    let ostride = out_w * bands;
    let rows = &mut out[oy0 * ostride..oy1 * ostride];
    if !need_v {
        for (r, orow) in rows.chunks_exact_mut(ostride).enumerate() {
            orow[mx0 * bands..mx1 * bands].copy_from_slice(&mid[r * mw..(r + 1) * mw]);
        }
        return out;
    }
    // Each intermediate row's non-zero bytes (first, last + 1): the sums
    // skip all-zero rows and keep to the columns some row reaches (a zero
    // term changes no sum; a sum of none is clip8(HALF) = 0, already there).
    let spans: Vec<Option<(usize, usize)>> = mid
        .par_chunks(mw)
        .map(|row| {
            let first = row.iter().position(|&v| v != 0)?;
            let last = row.iter().rposition(|&v| v != 0).unwrap();
            Some((first, last + 1))
        })
        .collect();
    rows.par_chunks_mut(ostride).enumerate().for_each_init(Vec::new, |acc, (r, orow)| {
        let yy = oy0 + r;
        let (ymin, n) = cv.bounds[yy];
        let (smin, smax) = (ymin.max(0) as usize, (ymin.max(0) + n.max(0)) as usize);
        let (lo, hi) = (smin.max(ry0), smax.min(ry1));
        if lo >= hi {
            return;
        }
        let reach = spans[lo - ry0..hi - ry0].iter().flatten().copied().reduce(|p, q| (p.0.min(q.0), p.1.max(q.1)));
        let Some((c0, c1)) = reach else { return };
        let k = &cv.kk[yy * cv.ksize..(yy + 1) * cv.ksize];
        acc.clear();
        acc.resize(c1 - c0, HALF);
        for y in lo..hi {
            if spans[y - ry0].is_none() {
                continue;
            }
            let line = &mid[(y - ry0) * mw + c0..(y - ry0) * mw + c1];
            accumulate(acc, line, k[y - smin]);
        }
        for (o, &a) in orow[mx0 * bands + c0..mx0 * bands + c1].iter_mut().zip(acc.iter()) {
            *o = clip8(a);
        }
    });
    out
}

/// The horizontal coefficients turned around: for each source pixel of
/// `x0..x1`, the intermediate columns of `mx0..mx1` whose source range holds
/// it (relative to `mx0`) and its weight in each, in column order. Each
/// column's weights are `precompute`'s, unchanged.
struct Scatter {
    start: Vec<u32>,
    cols: Vec<u32>,
    ks: Vec<i32>,
    width: usize,
}

impl Scatter {
    fn new(c: &Coeffs, x0: usize, x1: usize, mx0: usize, mx1: usize) -> Scatter {
        let n = x1 - x0;
        let range = |xx: usize| {
            let (xmin, cnt) = c.bounds[xx];
            let (smin, smax) = (xmin.max(0) as usize, (xmin.max(0) + cnt.max(0)) as usize);
            (smin, smin.max(x0), smax.min(x1))
        };
        let mut start = vec![0u32; n + 1];
        for xx in mx0..mx1 {
            let (_, lo, hi) = range(xx);
            for x in lo..hi {
                start[x - x0 + 1] += 1;
            }
        }
        for i in 0..n {
            start[i + 1] += start[i];
        }
        let total = start[n] as usize;
        let mut fill: Vec<u32> = start[..n].to_vec();
        let (mut cols, mut ks) = (vec![0u32; total], vec![0i32; total]);
        for xx in mx0..mx1 {
            let (smin, lo, hi) = range(xx);
            for x in lo..hi {
                let at = fill[x - x0] as usize;
                fill[x - x0] += 1;
                cols[at] = (xx - mx0) as u32;
                ks[at] = c.kk[xx * c.ksize + (x - smin)];
            }
        }
        Scatter { start, cols, ks, width: mx1 - mx0 }
    }
}

/// One row of the horizontal pass of `resample_sparse`: the intermediate
/// columns of `sc` from the source row `line` (pixels from column `sc.x0`).
///
/// Pillow sums each column over its source range; here each non-zero
/// source pixel adds its products to the columns that hold it. Every
/// column gets the same terms but the zero ones (which add nothing), and
/// i32 wrapping addition does not depend on the order, so each sum, and
/// its clip8, is Pillow's. A column no pixel reaches is clip8(HALF) = 0,
/// as `mrow` already is.
fn horizontal_sparse_row(line: &[u8], bands: usize, sc: &Scatter, mrow: &mut [u8], acc: &mut Vec<i32>) {
    #[cfg(target_arch = "x86_64")]
    if simd::avx2() {
        // SAFETY: the CPU has AVX2 (checked above).
        return unsafe { simd::horizontal_sparse_row(line, bands, sc, mrow, acc) };
    }
    horizontal_sparse_row_impl(line, bands, sc, mrow, acc)
}

#[inline(always)]
fn horizontal_sparse_row_impl(line: &[u8], bands: usize, sc: &Scatter, mrow: &mut [u8], acc: &mut Vec<i32>) {
    let n = line.len() / bands;
    let Some(first) = line.chunks_exact(bands).position(|p| p.iter().any(|&v| v != 0)) else { return };
    acc.clear();
    acc.resize(sc.width * bands, HALF);
    for x in first..n {
        let p = &line[x * bands..(x + 1) * bands];
        if p.iter().all(|&v| v == 0) {
            continue;
        }
        let (s, e) = (sc.start[x] as usize, sc.start[x + 1] as usize);
        for (&col, &kv) in sc.cols[s..e].iter().zip(&sc.ks[s..e]) {
            let a = &mut acc[col as usize * bands..(col as usize + 1) * bands];
            for (av, &pv) in a.iter_mut().zip(p) {
                *av = av.wrapping_add((pv as i32).wrapping_mul(kv));
            }
        }
    }
    for (o, &a) in mrow.iter_mut().zip(acc.iter()) {
        *o = clip8(a);
    }
}

/// `acc[i] += line[i] * kv` (wrapping), the vertical pass's inner loop.
#[inline]
fn accumulate(acc: &mut [i32], line: &[u8], kv: i32) {
    #[cfg(target_arch = "x86_64")]
    if simd::avx2() {
        // SAFETY: the CPU has AVX2 (checked above).
        return unsafe { simd::accumulate(acc, line, kv) };
    }
    accumulate_impl(acc, line, kv)
}

#[inline(always)]
fn accumulate_impl(acc: &mut [i32], line: &[u8], kv: i32) {
    for (a, &v) in acc.iter_mut().zip(line) {
        *a = a.wrapping_add((v as i32).wrapping_mul(kv));
    }
}

/// The same loops compiled for AVX2, picked at run time (`avx2`). They are
/// integer sums, so the vector code gives the scalar code's values.
#[cfg(target_arch = "x86_64")]
mod simd {
    use super::*;

    pub fn avx2() -> bool {
        #[cfg(test)]
        if tests::SCALAR.with(|s| s.get()) {
            return false;
        }
        std::is_x86_feature_detected!("avx2")
    }

    #[target_feature(enable = "avx2")]
    pub unsafe fn horizontal_sparse_row(line: &[u8], bands: usize, sc: &Scatter, mrow: &mut [u8], acc: &mut Vec<i32>) {
        if bands != 4 {
            return horizontal_sparse_row_impl(line, bands, sc, mrow, acc);
        }
        use std::arch::x86_64::*;
        let n = line.len() / 4;
        let px = |x: usize| u32::from_le_bytes([line[x * 4], line[x * 4 + 1], line[x * 4 + 2], line[x * 4 + 3]]);
        let Some(first) = (0..n).position(|x| px(x) != 0) else { return };
        acc.clear();
        acc.resize(sc.width * 4, HALF);
        let base = acc.as_mut_ptr();
        for x in first..n {
            let v = px(x);
            if v == 0 {
                continue;
            }
            let p = _mm_cvtepu8_epi32(_mm_cvtsi32_si128(v as i32));
            let (s, e) = (sc.start[x] as usize, sc.start[x + 1] as usize);
            for (&col, &kv) in sc.cols[s..e].iter().zip(&sc.ks[s..e]) {
                assert!((col as usize) < sc.width);
                let at = base.add(col as usize * 4) as *mut __m128i;
                _mm_storeu_si128(at, _mm_add_epi32(_mm_loadu_si128(at), _mm_mullo_epi32(p, _mm_set1_epi32(kv))));
            }
        }
        for (o, &a) in mrow.iter_mut().zip(acc.iter()) {
            *o = clip8(a);
        }
    }

    #[target_feature(enable = "avx2")]
    pub unsafe fn accumulate(acc: &mut [i32], line: &[u8], kv: i32) {
        accumulate_impl(acc, line, kv)
    }
}

/// Crop `[x0, x1) x [y0, y1)` of a packed image (`ImagingCrop` within the
/// image; Pillow fills outside with 0, never reached here).
pub fn crop(src: &[u8], w: usize, bands: usize, x0: usize, y0: usize, x1: usize, y1: usize) -> Vec<u8> {
    let stride = w * bands;
    let mut out = Vec::with_capacity((x1 - x0) * (y1 - y0) * bands);
    for y in y0..y1 {
        out.extend_from_slice(&src[y * stride + x0 * bands..y * stride + x1 * bands]);
    }
    out
}

/// Why `_resize` refuses a call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResizeError {
    BadSize,
    BadBox,
}

/// `_imaging.c` `_resize` with a resampling filter: the box checks, the
/// crop when the box is whole pixels of the output's size, else
/// `ImagingResample`.
pub fn core_resize(src: &[u8], w: usize, h: usize, bands: usize, out_w: usize, out_h: usize, filter: Filter, bx: [f32; 4]) -> Result<Vec<u8>, ResizeError> {
    if out_w < 1 || out_h < 1 {
        return Err(ResizeError::BadSize);
    }
    if bx[0] < 0.0 || bx[1] < 0.0 || bx[2] > w as f32 || bx[3] > h as f32 || bx[2] - bx[0] < 0.0 || bx[3] - bx[1] < 0.0 {
        return Err(ResizeError::BadBox);
    }
    if bx[0] - (bx[0] as i32) as f32 == 0.0
        && bx[2] - bx[0] == out_w as f32
        && bx[1] - (bx[1] as i32) as f32 == 0.0
        && bx[3] - bx[1] == out_h as f32
    {
        let (x0, y0) = (bx[0] as usize, bx[1] as usize);
        return Ok(crop(src, w, bands, x0, y0, bx[2] as usize, bx[3] as usize));
    }
    Ok(resample(src, w, h, bands, out_w, out_h, filter, bx))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    thread_local! {
        /// Set: `resample` on this thread is `resample_pillow` (the reference).
        pub static REFERENCE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        /// Set: this thread takes the scalar loops (as a CPU without AVX2 would).
        pub static SCALAR: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    #[test]
    fn scalar_and_vector_loops_agree() {
        let mut seed = 21u64;
        let mut next = |n: usize| {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seed >> 33) as usize % n
        };
        for _ in 0..if cfg!(debug_assertions) { 250 } else { 1000 } {
            let (w, h, bands) = (1 + next(120), 1 + next(120), [1, 2, 3, 4, 4][next(5)]);
            let data: Vec<u8> = (0..w * h * bands).map(|_| if next(3) == 0 { 0 } else { next(256) as u8 }).collect();
            let (ow, oh) = (1 + next(w + 3), 1 + next(h + 3));
            let bx = if next(2) == 0 {
                [0.0, 0.0, w as f32, h as f32]
            } else {
                let (a, b) = (next(w) as f32 * 0.7, next(h) as f32 * 0.3);
                [a, b, (a + 0.25 + next(w) as f32).min(w as f32), (b + 1.0 + next(h) as f32).min(h as f32)]
            };
            let f = [Filter::Lanczos, Filter::Bicubic, Filter::Bilinear, Filter::Box, Filter::Hamming][next(5)];
            let win = Window { data: &data, stride: w * bands, rect: (0, 0, w, h) };
            // the pool's threads take whichever loops their own flag says
            let pool = rayon::ThreadPoolBuilder::new().num_threads(1).start_handler(|_| SCALAR.with(|s| s.set(true))).build().unwrap();
            let scalar = pool.install(|| resample_sparse(Some(win), w, h, bands, ow, oh, f, bx));
            let vector = resample_sparse(Some(win), w, h, bands, ow, oh, f, bx);
            assert!(scalar == vector);
            assert!(vector == resample_pillow(&data, w, h, bands, ow, oh, f, bx));
        }
    }

    #[test]
    fn identity_and_crop() {
        let px: Vec<u8> = (0..5 * 4 * 4).map(|i| i as u8).collect();
        assert_eq!(resample(&px, 5, 4, 4, 5, 4, Filter::Lanczos, [0.0, 0.0, 5.0, 4.0]), px);
        let c = core_resize(&px, 5, 4, 4, 2, 2, Filter::Lanczos, [1.0, 1.0, 3.0, 3.0]).unwrap();
        assert_eq!(c, crop(&px, 5, 4, 1, 1, 3, 3));
        assert_eq!(core_resize(&px, 5, 4, 4, 2, 2, Filter::Lanczos, [1.0, 1.0, 6.0, 3.0]), Err(ResizeError::BadBox));
    }

    #[test]
    fn constant_stays_constant() {
        for f in [Filter::Box, Filter::Bilinear, Filter::Hamming, Filter::Bicubic, Filter::Lanczos] {
            let px = vec![77u8; 31 * 17 * 3];
            let out = resample(&px, 31, 17, 3, 9, 40, f, [0.0, 0.0, 31.0, 17.0]);
            assert!(out.iter().all(|&v| v == 77), "{f:?}");
        }
    }

    #[test]
    fn reduce_box_matches_square_reduce() {
        let px: Vec<u8> = (0..12 * 8).map(|i| (i * 13 % 256) as u8).collect();
        let (b, w, h) = crate::reduce::reduce_box(&px, 12, 1, 4, 4, [0, 0, 12, 8]);
        assert_eq!((w, h), (3, 2));
        assert_eq!(b, crate::reduce::reduce(&px, 12, 8, 4));
    }
}

#[cfg(test)]
mod bench {
    use super::*;

    /// `cargo test --release -p tmi-pillow bench_sparse -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_sparse() {
        let (w, h) = (5598usize, 7937usize);
        let mut seed = 3u64;
        let mut data = vec![0u8; w * h * 4];
        // strokes: runs of opaque pixels on a transparent page
        for y in 0..h {
            let mut x = 0;
            while x < w {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                let run = 1 + (seed >> 40) as usize % 200;
                if (seed >> 20) % 3 == 0 {
                    for p in data[(y * w + x) * 4..(y * w + (x + run).min(w)) * 4].chunks_exact_mut(4) {
                        p.copy_from_slice(&[(seed >> 8) as u8, 30, 40, 255]);
                    }
                }
                x += run;
            }
        }
        let bx = [0.0, 0.0, w as f32, h as f32];
        let pool = rayon::ThreadPoolBuilder::new().num_threads(1).build().unwrap();
        pool.install(|| for round in 0..2 {
            if round == 1 {
                for p in data.chunks_exact_mut(4) {
                    p[3] = 255;
                    p[0] |= 1;
                }
            }
            let t = std::time::Instant::now();
            let full = resample_pillow(&data, w, h, 4, 723, 1024, Filter::Lanczos, bx);
            let tf = t.elapsed();
            let t = std::time::Instant::now();
            let win = Window { data: &data, stride: w * 4, rect: (0, 0, w, h) };
            let sparse = resample_sparse(Some(win), w, h, 4, 723, 1024, Filter::Lanczos, bx);
            let ts = t.elapsed();
            assert!(full == sparse);
            eprintln!("full {tf:?}  sparse {ts:?}");
        });
    }
}
