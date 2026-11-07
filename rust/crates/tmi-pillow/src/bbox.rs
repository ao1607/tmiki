//! `Image.getbbox()` of one 8-bit channel (`getchannel("A").getbbox()`):
//! the smallest box holding every non-zero value, None when there is none.

/// Box `(x0, y0, x1, y1)` of the pixels whose byte at `channel` (of
/// `pixel_bytes`) is non-zero, in a `width`-pixel-wide image.
pub fn channel_bbox(data: &[u8], width: usize, pixel_bytes: usize, channel: usize) -> Option<(usize, usize, usize, usize)> {
    if width == 0 {
        return None;
    }
    let stride = width * pixel_bytes;
    let height = data.len() / stride;
    let row_any = |y: usize| data[y * stride..(y + 1) * stride].chunks_exact(pixel_bytes).any(|p| p[channel] != 0);
    let y0 = (0..height).find(|&y| row_any(y))?;
    let y1 = (y0..height).rev().find(|&y| row_any(y)).unwrap() + 1;
    let (mut x0, mut x1) = (width, 0usize);
    for y in y0..y1 {
        let row = &data[y * stride..(y + 1) * stride];
        // Only the columns outside the box found so far can widen it.
        if let Some(x) = (0..x0).find(|&x| row[x * pixel_bytes + channel] != 0) {
            x0 = x;
        }
        if let Some(x) = (x1.max(x0)..width).rev().find(|&x| row[x * pixel_bytes + channel] != 0) {
            x1 = x + 1;
        }
    }
    Some((x0, y0, x1, y1))
}

/// Alpha bbox of an RGBA image.
#[inline]
pub fn alpha_bbox(rgba: &[u8], width: usize) -> Option<(usize, usize, usize, usize)> {
    channel_bbox(rgba, width, 4, 3)
}

/// `Image.getbbox(alpha_only=False)` (and `getbbox()` of modes without
/// alpha): the box of the pixels with any non-zero byte among their
/// `pixel_bytes`.
pub fn bbox_any(data: &[u8], width: usize, pixel_bytes: usize) -> Option<(usize, usize, usize, usize)> {
    if width == 0 {
        return None;
    }
    let stride = width * pixel_bytes;
    let height = data.len() / stride;
    let nz = |row: &[u8], x: usize| row[x * pixel_bytes..(x + 1) * pixel_bytes].iter().any(|&v| v != 0);
    let row_any = |y: usize| data[y * stride..(y + 1) * stride].iter().any(|&v| v != 0);
    let y0 = (0..height).find(|&y| row_any(y))?;
    let y1 = (y0..height).rev().find(|&y| row_any(y)).unwrap() + 1;
    let (mut x0, mut x1) = (width, 0usize);
    for y in y0..y1 {
        let row = &data[y * stride..(y + 1) * stride];
        if let Some(x) = (0..x0).find(|&x| nz(row, x)) {
            x0 = x;
        }
        if let Some(x) = (x1.max(x0)..width).rev().find(|&x| nz(row, x)) {
            x1 = x + 1;
        }
    }
    Some((x0, y0, x1, y1))
}

/// `getextrema()` of each band of an 8-bit image: (min, max) per band, or
/// None for an empty image.
pub fn extrema(data: &[u8], bands: usize) -> Option<Vec<(u8, u8)>> {
    if data.is_empty() {
        return None;
    }
    let mut out = vec![(255u8, 0u8); bands];
    for px in data.chunks_exact(bands) {
        for (e, &v) in out.iter_mut().zip(px) {
            e.0 = e.0.min(v);
            e.1 = e.1.max(v);
        }
    }
    Some(out)
}
#[cfg(test)]
mod tests {
    use super::*;

    fn brute(data: &[u8], w: usize) -> Option<(usize, usize, usize, usize)> {
        let h = data.len() / (4 * w);
        let mut b: Option<(usize, usize, usize, usize)> = None;
        for y in 0..h {
            for x in 0..w {
                if data[(y * w + x) * 4 + 3] != 0 {
                    b = Some(match b {
                        None => (x, y, x + 1, y + 1),
                        Some(o) => (o.0.min(x), o.1.min(y), o.2.max(x + 1), o.3.max(y + 1)),
                    });
                }
            }
        }
        b
    }

    #[test]
    fn matches_brute_force() {
        let mut seed = 12345u64;
        let mut next = || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seed >> 33) as usize
        };
        for _ in 0..2000 {
            let (w, h) = (1 + next() % 9, 1 + next() % 9);
            let mut data = vec![0u8; w * h * 4];
            for _ in 0..next() % 4 {
                let p = next() % (w * h);
                data[p * 4 + 3] = 1 + (next() % 255) as u8;
                data[p * 4] = 7;
            }
            assert_eq!(alpha_bbox(&data, w), brute(&data, w));
        }
    }
}
