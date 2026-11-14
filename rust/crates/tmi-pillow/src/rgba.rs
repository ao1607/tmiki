//! Whole-image operations on packed RGBA8 buffers as Pillow does them:
//! `crop` (zeros outside the image), `paste` (a straight copy),
//! `alpha_composite(im, dest)` at an offset, and the "visible difference"
//! box of tmiki's diff (`ImageChops` difference / lighter / multiply plus
//! `getbbox`). Coordinates may lie outside the image; what falls outside is
//! dropped (or, for `crop`, zero).

use rayon::prelude::*;

use crate::chops::multiply;
use crate::over::over_pixel;

/// `Image.crop((x0, y0, x1, y1))` of a `w` x `h` RGBA image: the box's
/// pixels, transparent black where the box leaves the image.
pub fn crop(src: &[u8], w: usize, h: usize, x0: i64, y0: i64, x1: i64, y1: i64) -> Vec<u8> {
    let (cw, ch) = ((x1 - x0).max(0) as usize, (y1 - y0).max(0) as usize);
    let mut out = vec![0u8; cw * ch * 4];
    let (sx0, sx1) = (x0.max(0), x1.min(w as i64));
    let (sy0, sy1) = (y0.max(0), y1.min(h as i64));
    if sx0 >= sx1 || sy0 >= sy1 {
        return out;
    }
    let n = (sx1 - sx0) as usize * 4;
    for y in sy0..sy1 {
        let s = (y as usize * w + sx0 as usize) * 4;
        let d = ((y - y0) as usize * cw + (sx0 - x0) as usize) * 4;
        out[d..d + n].copy_from_slice(&src[s..s + n]);
    }
    out
}

/// The part of a `sw` x `sh` source placed at (`x`, `y`) that lies inside a
/// `dw` x `dh` target: (source x, source y, target x, target y, w, h).
fn clip(dw: usize, dh: usize, sw: usize, sh: usize, x: i64, y: i64) -> Option<(usize, usize, usize, usize, usize, usize)> {
    let tx0 = x.max(0);
    let ty0 = y.max(0);
    let tx1 = (x + sw as i64).min(dw as i64);
    let ty1 = (y + sh as i64).min(dh as i64);
    if tx0 >= tx1 || ty0 >= ty1 {
        return None;
    }
    Some(((tx0 - x) as usize, (ty0 - y) as usize, tx0 as usize, ty0 as usize, (tx1 - tx0) as usize, (ty1 - ty0) as usize))
}

/// `Image.paste(src, (x, y))` of RGBA into RGBA: every byte copied.
pub fn paste(dst: &mut [u8], dw: usize, dh: usize, src: &[u8], sw: usize, sh: usize, x: i64, y: i64) {
    let Some((sx, sy, tx, ty, w, h)) = clip(dw, dh, sw, sh, x, y) else { return };
    for r in 0..h {
        let s = ((sy + r) * sw + sx) * 4;
        let d = ((ty + r) * dw + tx) * 4;
        dst[d..d + w * 4].copy_from_slice(&src[s..s + w * 4]);
    }
}

/// `Image.alpha_composite(src, dest=(x, y))`: Pillow's source-over of `src`
/// onto `dst` where they overlap.
pub fn over(dst: &mut [u8], dw: usize, dh: usize, src: &[u8], sw: usize, sh: usize, x: i64, y: i64) {
    let Some((sx, sy, tx, ty, w, h)) = clip(dw, dh, sw, sh, x, y) else { return };
    let rows: Vec<(usize, &mut [u8])> = dst
        .chunks_exact_mut(dw * 4)
        .enumerate()
        .skip(ty)
        .take(h)
        .map(|(i, row)| (i - ty, &mut row[tx * 4..(tx + w) * 4]))
        .collect();
    rows.into_par_iter().for_each(|(r, row)| {
        let s = &src[((sy + r) * sw + sx) * 4..((sy + r) * sw + sx + w) * 4];
        for (d, p) in row.chunks_exact_mut(4).zip(s.chunks_exact(4)) {
            over_pixel(d, p);
        }
    });
}

/// Whether a pixel counts in tmiki's visible difference of two RGBA images:
/// its alpha differs, or a colour channel differs by enough to show at the
/// larger of the two alphas (`multiply(|a - b|, max alpha) != 0`).
#[inline]
fn visibly_differs(a: &[u8], b: &[u8]) -> bool {
    if a[3] != b[3] {
        return true;
    }
    let alpha = a[3];
    (0..3).any(|c| multiply(a[c].abs_diff(b[c]), alpha) != 0)
}

/// `image_ops._visible_difference_bbox(a, b)` of two `w`-wide RGBA images
/// of the same size: the box of `ImageChops.difference(a, b)`'s alpha
/// together with the boxes of each colour band of the difference
/// multiplied by `ImageChops.lighter` of the alphas. None when nothing
/// differs visibly.
pub fn visible_difference_bbox(a: &[u8], b: &[u8], w: usize) -> Option<(usize, usize, usize, usize)> {
    assert_eq!(a.len(), b.len());
    if w == 0 || a.is_empty() {
        return None;
    }
    let stride = w * 4;
    const BAND: usize = 32;
    a.par_chunks(stride * BAND)
        .zip(b.par_chunks(stride * BAND))
        .enumerate()
        .filter_map(|(band, (ra, rb))| {
            let mut found: Option<(usize, usize, usize, usize)> = None;
            for (i, (la, lb)) in ra.chunks_exact(stride).zip(rb.chunks_exact(stride)).enumerate() {
                if la == lb {
                    continue;
                }
                let y = band * BAND + i;
                let mut first = None;
                let mut last = 0;
                for (x, (pa, pb)) in la.chunks_exact(4).zip(lb.chunks_exact(4)).enumerate() {
                    if pa != pb && visibly_differs(pa, pb) {
                        first.get_or_insert(x);
                        last = x;
                    }
                }
                if let Some(x0) = first {
                    found = Some(match found {
                        None => (x0, y, last + 1, y + 1),
                        Some(f) => (f.0.min(x0), f.1, f.2.max(last + 1), y + 1),
                    });
                }
            }
            found
        })
        .reduce_with(|p, q| (p.0.min(q.0), p.1.min(q.1), p.2.max(q.2), p.3.max(q.3)))
}

/// Whether R == G == B in every pixel.
pub fn is_gray(rgba: &[u8]) -> bool {
    rgba.par_chunks(1 << 16).all(|c| c.chunks_exact(4).all(|p| p[0] == p[1] && p[1] == p[2]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn brute(a: &[u8], b: &[u8], w: usize) -> Option<(usize, usize, usize, usize)> {
        let h = a.len() / (4 * w);
        let mut out: Option<(usize, usize, usize, usize)> = None;
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) * 4;
                let (pa, pb) = (&a[i..i + 4], &b[i..i + 4]);
                let vis = pa[3].max(pb[3]);
                let hit = pa[3].abs_diff(pb[3]) != 0 || (0..3).any(|c| multiply(pa[c].abs_diff(pb[c]), vis) != 0);
                if hit {
                    out = Some(match out {
                        None => (x, y, x + 1, y + 1),
                        Some(o) => (o.0.min(x), o.1.min(y), o.2.max(x + 1), o.3.max(y + 1)),
                    });
                }
            }
        }
        out
    }

    #[test]
    fn visible_bbox_matches_brute_force() {
        let mut seed = 99u64;
        let mut next = || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seed >> 33) as usize
        };
        for _ in 0..500 {
            let (w, h) = (1 + next() % 40, 1 + next() % 80);
            let a: Vec<u8> = (0..w * h * 4).map(|_| (next() % 4) as u8 * 60).collect();
            let mut b = a.clone();
            for _ in 0..next() % 5 {
                let p = next() % (w * h * 4);
                b[p] = (next() % 256) as u8;
            }
            assert_eq!(visible_difference_bbox(&a, &b, w), brute(&a, &b, w));
        }
    }

    #[test]
    fn crop_paste_over_clip() {
        let src: Vec<u8> = (0..3 * 2 * 4).map(|i| i as u8 + 1).collect();
        let c = crop(&src, 3, 2, -1, 1, 2, 3);
        assert_eq!(c.len(), 3 * 2 * 4);
        assert_eq!(&c[0..4], &[0, 0, 0, 0]);
        assert_eq!(&c[4..8], &src[12..16]);
        assert!(c[12..].iter().all(|&v| v == 0));
        let mut dst = vec![0u8; 2 * 2 * 4];
        paste(&mut dst, 2, 2, &src, 3, 2, -1, 1);
        assert_eq!(&dst[8..12], &src[4..8]);
        assert_eq!(&dst[0..8], &[0; 8]);
        let mut dst2 = vec![0u8; 2 * 2 * 4];
        over(&mut dst2, 2, 2, &src, 3, 2, -1, 1);
        let mut want = [0u8; 4];
        over_pixel(&mut want, &src[4..8]);
        assert_eq!(&dst2[8..12], &want);
    }
}
