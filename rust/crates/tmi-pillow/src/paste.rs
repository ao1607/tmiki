//! `Image.paste(im, box, mask)` with an "L" mask (libImaging/Paste.c
//! `paste_mask_L`), which is also `Image.composite(image1, image2, mask)`:
//! a copy of `image2` with `image1` pasted through `mask`.

/// Pillow's `DIV255`: `(a + 128) / 255` rounded, without a division.
#[inline]
pub fn div255(a: u32) -> u32 {
    let tmp = a + 128;
    ((tmp >> 8) + tmp) >> 8
}

/// One channel of `paste_mask_L`: `out` (the destination) mixed towards `src`
/// by `mask`: `DIV255(out * (255 - mask) + src * mask)`.
#[inline]
pub fn blend_mask(out: u8, src: u8, mask: u8) -> u8 {
    div255(out as u32 * (255 - mask as u32) + src as u32 * mask as u32) as u8
}

/// `paste_mask_L` of RGBA `src` into RGBA `dst` (same size) through the "L"
/// plane `mask`: every channel, alpha included, is mixed by `blend_mask`.
pub fn paste_rgba_mask_l(dst: &mut [u8], src: &[u8], mask: &[u8]) {
    for ((d, s), &m) in dst.chunks_exact_mut(4).zip(src.chunks_exact(4)).zip(mask) {
        for c in 0..4 {
            d[c] = blend_mask(d[c], s[c], m);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ends_of_the_mask() {
        for a in 0..=255u8 {
            for b in 0..=255u8 {
                assert_eq!(blend_mask(a, b, 0), a);
                assert_eq!(blend_mask(a, b, 255), b);
            }
        }
    }
}
