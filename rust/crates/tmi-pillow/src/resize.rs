//! `Image.resize`, `Image.reduce` and `Image.thumbnail` (PIL/Image.py
//! 12.3) over `resample` and `reduce`, for 8-bit modes.
//!
//! What the Python layer adds:
//! - "LA" / "RGBA" are premultiplied ("La" / "RGBa", `MULDIV255`), resized,
//!   then unpremultiplied (`255 * v / a`, clipped). The recursive call
//!   drops `reducing_gap`, so an RGBA image is never reduced first, even by
//!   `thumbnail()`. The crop shortcut of `_resize` also round-trips through
//!   the premultiplied mode.
//! - For other modes, `reducing_gap` reduces by
//!   `int(box_w / size_w / gap) or 1` (and the same for y) over the
//!   filter-support-expanded box, then resamples the rest with the box
//!   mapped into the reduced image.
//! - An image more than 100 times as tall as wide that shrinks vertically
//!   is resampled in two calls (height first).
//! - The box is a Python float tuple until `_resize` turns it to float32.

use crate::convert::{premultiply_la, premultiply_rgba, unpremultiply_la, unpremultiply_rgba};
use crate::image::{Image, Mode};
use crate::reduce::reduce_box;
use crate::resample::{core_resize, crop, resample_sparse_with, Filter, ResizeError, Window};

fn bands(mode: Mode) -> usize {
    mode.bytes_per_pixel()
}

fn core(img: &Image, size: (usize, usize), filter: Filter, bx: [f64; 4]) -> Result<Image, ResizeError> {
    let b = [bx[0] as f32, bx[1] as f32, bx[2] as f32, bx[3] as f32];
    let data = core_resize(&img.data, img.width, img.height, bands(img.mode), size.0, size.1, filter, b)?;
    Ok(Image::new(img.mode, size.0, size.1, data))
}

/// `_get_safe_box`.
fn safe_box(img: &Image, size: (usize, usize), filter: Filter, bx: [f64; 4]) -> [usize; 4] {
    let support = filter.support() - 0.5;
    let sx = (bx[2] - bx[0]) / size.0 as f64;
    let sy = (bx[3] - bx[1]) / size.1 as f64;
    let (ux, uy) = (support * sx, support * sy);
    [
        ((bx[0] - ux).trunc() as i64).max(0) as usize,
        ((bx[1] - uy).trunc() as i64).max(0) as usize,
        ((bx[2] + ux).ceil() as i64).min(img.width as i64) as usize,
        ((bx[3] + uy).ceil() as i64).min(img.height as i64) as usize,
    ]
}

/// `Image.reduce(factor, box)` (box as (x0, y0, x1, y1)).
pub fn reduce(img: &Image, factor: (usize, usize), bx: Option<[usize; 4]>) -> Result<Image, ResizeError> {
    if !img.mode.is_u8_bands() {
        return Err(ResizeError::BadSize);
    }
    let bx = bx.unwrap_or([0, 0, img.width, img.height]);
    if factor == (1, 1) && bx == [0, 0, img.width, img.height] {
        return Ok(img.clone());
    }
    let (premul, unpremul): (Option<fn(&mut [u8])>, Option<fn(&mut [u8])>) = match img.mode {
        Mode::RGBA => (Some(premultiply_rgba), Some(unpremultiply_rgba)),
        Mode::LA => (Some(premultiply_la), Some(unpremultiply_la)),
        _ => (None, None),
    };
    let mut src = img.clone();
    if let Some(f) = premul {
        f(&mut src.data);
    }
    if factor.0 < 1 || factor.1 < 1 || bx[2] > img.width || bx[3] > img.height || bx[2] <= bx[0] || bx[3] <= bx[1] {
        return Err(ResizeError::BadBox);
    }
    let mut out = if factor == (1, 1) {
        let data = crop(&src.data, src.width, bands(src.mode), bx[0], bx[1], bx[2], bx[3]);
        Image::new(src.mode, bx[2] - bx[0], bx[3] - bx[1], data)
    } else {
        let (data, w, h) = reduce_box(&src.data, src.width, bands(src.mode), factor.0, factor.1, [bx[0], bx[1], bx[2] - bx[0], bx[3] - bx[1]]);
        Image::new(src.mode, w, h, data)
    };
    if let Some(f) = unpremul {
        f(&mut out.data);
    }
    Ok(out)
}

/// `Image.resize(size, filter, box, reducing_gap)`.
pub fn resize(img: &Image, size: (usize, usize), filter: Filter, bx: Option<[f64; 4]>, reducing_gap: Option<f64>) -> Result<Image, ResizeError> {
    if !img.mode.is_u8_bands() {
        return Err(ResizeError::BadSize);
    }
    let full = [0.0, 0.0, img.width as f64, img.height as f64];
    let mut bx = bx.unwrap_or(full);
    if (img.width, img.height) == size && bx == full {
        return Ok(img.clone());
    }
    match img.mode {
        Mode::RGBA | Mode::LA => {
            // Straight from `img`, premultiplied row by row as it is read
            // (`resize_sparse` with the whole image as its window), unless
            // Pillow crops or splits the call.
            if direct(img.width, img.height, size, bx) && img.width > 0 && img.height > 0 && !reference() {
                let win = Window { data: &img.data, stride: img.width * bands(img.mode), rect: (0, 0, img.width, img.height) };
                return resize_sparse(img.mode, img.width, img.height, Some(win), size, filter, Some(bx));
            }
            let mut pre = img.clone();
            if img.mode == Mode::RGBA {
                premultiply_rgba(&mut pre.data);
            } else {
                premultiply_la(&mut pre.data);
            }
            // The recursive `im.resize(size, resample, box)` (no gap); its
            // own early return cannot fire (same test as above).
            let mut out = resize_inner(&pre, size, filter, bx)?;
            if img.mode == Mode::RGBA {
                unpremultiply_rgba(&mut out.data);
            } else {
                unpremultiply_la(&mut out.data);
            }
            return Ok(out);
        }
        _ => {}
    }
    let mut cur = std::borrow::Cow::Borrowed(img);
    if let Some(gap) = reducing_gap {
        let fx = ((bx[2] - bx[0]) / size.0 as f64 / gap).trunc() as i64;
        let fy = ((bx[3] - bx[1]) / size.1 as f64 / gap).trunc() as i64;
        let fx = if fx == 0 { 1 } else { fx } as usize;
        let fy = if fy == 0 { 1 } else { fy } as usize;
        if fx > 1 || fy > 1 {
            let rb = safe_box(img, size, filter, bx);
            cur = std::borrow::Cow::Owned(reduce(img, (fx, fy), Some(rb))?);
            bx = [
                (bx[0] - rb[0] as f64) / fx as f64,
                (bx[1] - rb[1] as f64) / fy as f64,
                (bx[2] - rb[0] as f64) / fx as f64,
                (bx[3] - rb[1] as f64) / fy as f64,
            ];
        }
    }
    resize_inner(&cur, size, filter, bx)
}

/// The end of `Image.resize`: the tall-image split, then `_resize`.
fn resize_inner(img: &Image, size: (usize, usize), filter: Filter, bx: [f64; 4]) -> Result<Image, ResizeError> {
    if img.height > img.width * 100 && size.1 < img.height {
        let first = core(img, (img.width, size.1), filter, [0.0, bx[1], img.width as f64, bx[3]])?;
        return core(&first, size, filter, [bx[0], 0.0, bx[2], size.1 as f64]);
    }
    core(img, size, filter, bx)
}

/// Whether `resize` of a `w` x `h` image to `size` over `bx` is one
/// `ImagingResample` call: not `resize_inner`'s split (more than 100 times
/// as tall as wide), not `core_resize`'s refusals or its crop shortcut.
fn direct(w: usize, h: usize, size: (usize, usize), bx: [f64; 4]) -> bool {
    let f = [bx[0] as f32, bx[1] as f32, bx[2] as f32, bx[3] as f32];
    let tall = h > w * 100 && size.1 < h;
    let crops = f[0] - (f[0] as i32) as f32 == 0.0
        && f[2] - f[0] == size.0 as f32
        && f[1] - (f[1] as i32) as f32 == 0.0
        && f[3] - f[1] == size.1 as f32;
    let bad_box = f[0] < 0.0 || f[1] < 0.0 || f[2] > w as f32 || f[3] > h as f32 || f[2] - f[0] < 0.0 || f[3] - f[1] < 0.0;
    !(tall || size.0 < 1 || size.1 < 1 || bad_box || crops)
}

/// Tests compute the reference with Pillow's loops throughout.
fn reference() -> bool {
    #[cfg(test)]
    if crate::resample::tests::REFERENCE.with(|r| r.get()) {
        return true;
    }
    false
}

/// `resize(img, size, filter, bx, None)` of the `w` x `h` image `img` of
/// `mode` that is 0 in every byte outside `win` (None: everywhere), without
/// building `img`: `resample_sparse` does the resampling (see there why the
/// bytes are the same). An image Pillow would crop instead of resample, or
/// resample in two calls (more than 100 times as tall as wide), is built
/// and handed to `resize`.
pub fn resize_sparse(mode: Mode, w: usize, h: usize, win: Option<Window>, size: (usize, usize), filter: Filter, bx: Option<[f64; 4]>) -> Result<Image, ResizeError> {
    let b = bands(mode);
    let dense = || -> Image {
        let data = match &win {
            Some(win) => win.to_dense(w, h, b),
            None => vec![0u8; w * h * b],
        };
        Image::new(mode, w, h, data)
    };
    if !mode.is_u8_bands() {
        return Err(ResizeError::BadSize);
    }
    let full = [0.0, 0.0, w as f64, h as f64];
    let bxv = bx.unwrap_or(full);
    if (w, h) == size && bxv == full {
        return Ok(dense());
    }
    let f = [bxv[0] as f32, bxv[1] as f32, bxv[2] as f32, bxv[3] as f32];
    if !direct(w, h, size, bxv) {
        #[cfg(test)]
        tests::FALLBACKS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return resize(&dense(), size, filter, bx, None);
    }
    let (premul, unpremul): (Option<fn(&mut [u8])>, Option<fn(&mut [u8])>) = match mode {
        Mode::RGBA => (Some(premultiply_rgba), Some(unpremultiply_rgba)),
        Mode::LA => (Some(premultiply_la), Some(unpremultiply_la)),
        _ => (None, None),
    };
    // The window premultiplied row by row as it is read (0 stays 0 outside).
    let data = resample_sparse_with(win, w, h, b, size.0, size.1, filter, f, premul);
    let mut out = Image::new(mode, size.0, size.1, data);
    if let Some(u) = unpremul {
        u(&mut out.data);
    }
    Ok(out)
}

/// The size `Image.thumbnail(size)` gives a `w` x `h` image, or None when it
/// leaves the image alone.
pub fn thumbnail_size(w: usize, h: usize, size: (f64, f64)) -> Option<(usize, usize)> {
    let (mut x, mut y) = (size.0.floor() as i64, size.1.floor() as i64);
    if x >= w as i64 && y >= h as i64 {
        return None;
    }
    let aspect = w as f64 / h as f64;
    // min(floor, ceil, key=...) keeps the first of equal keys.
    let pick = |number: f64, key: &dyn Fn(i64) -> f64| -> i64 {
        let (f, c) = (number.floor() as i64, number.ceil() as i64);
        let best = if key(c) < key(f) { c } else { f };
        best.max(1)
    };
    if x as f64 / y as f64 >= aspect {
        let yf = y as f64;
        x = pick(yf * aspect, &|n| (aspect - n as f64 / yf).abs());
    } else {
        let xf = x as f64;
        y = pick(xf / aspect, &|n| if n == 0 { 0.0 } else { (aspect - xf / n as f64).abs() });
    }
    Some((x as usize, y as usize))
}

/// `Image.thumbnail(size, filter, reducing_gap)` of an image that has no
/// `draft()` (anything but a JPEG still being read).
pub fn thumbnail(img: &Image, size: (f64, f64), filter: Filter, reducing_gap: Option<f64>) -> Result<Image, ResizeError> {
    match thumbnail_size(img.width, img.height, size) {
        None => Ok(img.clone()),
        Some(s) if s == (img.width, img.height) => Ok(img.clone()),
        Some(s) => resize(img, s, filter, None, reducing_gap),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub static FALLBACKS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> usize {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (self.0 >> 33) as usize
        }
        fn below(&mut self, n: usize) -> usize {
            self.next() % n.max(1)
        }
    }

    /// A crop rectangle of a `w` x `h` image: random, or one of the special
    /// shapes (touching an edge, 1 px, everything).
    fn rect(rng: &mut Rng, w: usize, h: usize) -> (usize, usize, usize, usize) {
        let span = |rng: &mut Rng, n: usize| -> (usize, usize) {
            match rng.below(6) {
                0 => (0, 1 + rng.below(n)),         // touches the start
                1 => (rng.below(n), n),             // touches the end
                2 => (0, n),                        // all of it
                3 => {
                    let a = rng.below(n);
                    (a, a + 1)                      // 1 px
                }
                _ => {
                    let a = rng.below(n);
                    (a, a + 1 + rng.below(n - a))
                }
            }
        };
        let (x0, x1) = span(rng, w);
        let (y0, y1) = span(rng, h);
        (x0, y0, x1, y1)
    }

    fn pixels(rng: &mut Rng, n: usize, bands: usize) -> Vec<u8> {
        let mut px = noise(rng, n, bands);
        // Line-art-like content half the time: long runs of zero bytes
        // (whole rows and columns of them too), strokes between.
        if rng.below(2) == 0 {
            let mut i = 0;
            while i < n {
                let run = 1 + rng.below(3 + n / 4);
                if rng.below(4) != 0 {
                    for b in &mut px[i * bands..(i + run).min(n) * bands] {
                        *b = 0;
                    }
                }
                i += run;
            }
        }
        px
    }

    fn noise(rng: &mut Rng, n: usize, bands: usize) -> Vec<u8> {
        // RGBA-like content: some transparent, some opaque, some partial.
        (0..n)
            .flat_map(|_| {
                let a = [0u8, 255, 255, 128, 1, 200][rng.below(6)];
                let mut p: Vec<u8> = (0..bands).map(|_| rng.below(256) as u8).collect();
                if bands == 4 || bands == 2 {
                    p[bands - 1] = a;
                }
                p
            })
            .collect()
    }

    fn check(rng: &mut Rng, mode: Mode, w: usize, h: usize, size: (usize, usize), filter: Filter, bx: Option<[f64; 4]>) {
        let b = bands(mode);
        let (x0, y0, x1, y1) = rect(rng, w, h);
        let crop = pixels(rng, (x1 - x0) * (y1 - y0), b);
        let win = Window::placed(&crop, x1 - x0, y1 - y0, b, x0 as i64, y0 as i64, w, h);
        let full = Image::new(mode, w, h, win.unwrap().to_dense(w, h, b));
        // the reference: Pillow's loops over the whole image
        let reference = &crate::resample::tests::REFERENCE;
        reference.with(|r| r.set(true));
        let want = resize(&full, size, filter, bx, None);
        reference.with(|r| r.set(false));
        // and the whole image through the new loops
        assert!(resize(&full, size, filter, bx, None).map(|i| i.data) == want.clone().map(|i| i.data));
        let got = resize_sparse(mode, w, h, win, size, filter, bx);
        assert_eq!(got.as_ref().map(|i| (i.width, i.height)), want.as_ref().map(|i| (i.width, i.height)));
        assert!(
            got.as_ref().map(|i| &i.data) == want.as_ref().map(|i| &i.data),
            "{mode:?} {w}x{h} rect {:?} -> {size:?} {filter:?} {bx:?}",
            (x0, y0, x1, y1)
        );
    }

    #[test]
    fn sparse_resize_equals_full_resize() {
        let mut rng = Rng(7);
        let filters = [Filter::Lanczos, Filter::Bicubic, Filter::Bilinear, Filter::Box, Filter::Hamming];
        // fewer in debug builds (cargo test --workspace); --release runs all
        let cases = if cfg!(debug_assertions) { 400 } else { 1500 };
        for i in 0..cases {
            let (w, h) = (1 + rng.below(90), 1 + rng.below(90));
            let filter = filters[i % filters.len()];
            let mode = [Mode::RGBA, Mode::RGBA, Mode::L, Mode::RGB, Mode::LA][rng.below(5)];
            // The shrinks diff.py makes (_fit to an edge), others, upscales.
            let edge = 1 + rng.below(w.max(h) + 4);
            let scale = edge as f64 / w.max(h) as f64;
            let fit = |v: usize| ((v as f64 * scale).round_ties_even() as usize).max(1);
            let size = match rng.below(4) {
                0 => (fit(w), fit(h)),
                1 => (1 + rng.below(2 * w), 1 + rng.below(2 * h)),
                2 => (w, 1 + rng.below(h)),
                _ => (1 + rng.below(w), h),
            };
            let bx = match rng.below(3) {
                0 => None,
                _ => {
                    let bx0 = rng.below(w) as f64 * [1.0, 0.5, 0.37][rng.below(3)];
                    let by0 = rng.below(h) as f64 * [1.0, 0.25][rng.below(2)];
                    let bx1 = (bx0 + 0.5 + rng.below(w) as f64 * 0.9).min(w as f64);
                    let by1 = (by0 + 0.5 + rng.below(h) as f64).min(h as f64);
                    Some([bx0, by0, bx1, by1])
                }
            };
            check(&mut rng, mode, w, h, size, filter, bx);
        }
        // most cases take the sparse path (not the built image)
        let n = FALLBACKS.load(std::sync::atomic::Ordering::Relaxed);
        assert!(n < cases / 5, "{n} fallbacks");
    }

    #[test]
    fn sparse_resize_of_diff_previews() {
        // The shapes diff.py resamples: a canvas to a 2048 / 1024 edge, the
        // side by side halves on the pair's grid (box), odd sizes.
        let mut rng = Rng(11);
        let shapes: &[(usize, usize)] = if cfg!(debug_assertions) { &[(1025, 1023), (4097, 33)] } else { &[(2101, 977), (977, 2101), (3001, 1501), (1025, 1023), (4097, 33)] };
        for &(w, h) in shapes {
            for edge in [2048usize, 1024] {
                let scale = edge as f64 / w.max(h) as f64;
                let fit = |v: usize| ((v as f64 * scale).round_ties_even() as usize).max(1);
                for _ in 0..3 {
                    check(&mut rng, Mode::RGBA, w, h, (fit(w), fit(h)), Filter::Lanczos, None);
                }
                let pair = (2 * w + 16, h);
                let ps_scale = edge as f64 / pair.0.max(pair.1) as f64;
                let ps = (((pair.0 as f64) * ps_scale).round_ties_even() as usize, ((pair.1 as f64) * ps_scale).round_ties_even() as usize);
                let step = pair.0 as f64 / ps.0 as f64;
                let left_w = (w as f64 / step).floor() as usize;
                let right_x = ((w + 16) as f64 / step).ceil() as usize;
                if left_w >= 1 && ps.0 > right_x {
                    let bl = [0.0, 0.0, (w as f64).min(left_w as f64 * step), h as f64];
                    check(&mut rng, Mode::RGBA, w, h, (left_w, ps.1), Filter::Lanczos, Some(bl));
                    let br = [(right_x as f64 * step - w as f64 - 16.0).max(0.0), 0.0, w as f64, h as f64];
                    check(&mut rng, Mode::RGBA, w, h, (ps.0 - right_x, ps.1), Filter::Lanczos, Some(br));
                }
            }
        }
    }

    #[test]
    fn sparse_resize_of_nothing_is_transparent() {
        let got = resize_sparse(Mode::RGBA, 50, 40, None, (10, 8), Filter::Lanczos, None).unwrap();
        assert_eq!(got.data, vec![0u8; 10 * 8 * 4]);
        let same = resize_sparse(Mode::RGBA, 5, 4, None, (5, 4), Filter::Lanczos, None).unwrap();
        assert_eq!(same.data, vec![0u8; 5 * 4 * 4]);
    }
}
