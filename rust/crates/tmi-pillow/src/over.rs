//! `Image.alpha_composite`: Pillow's `ImagingAlphaComposite`
//! (libImaging/AlphaComposite.c), integer for integer. The formula has not
//! changed since Pillow 4.0.

const PRECISION_BITS: u32 = 7;

#[inline]
fn shift_for_div255(a: u32) -> u32 {
    ((a >> 8) + a) >> 8
}

/// Pillow's `alpha_composite` of `src` over `dst`, in place.
#[inline]
pub fn over_pixel(dst: &mut [u8], src: &[u8]) {
    let sa = src[3] as u32;
    if sa == 0 {
        return;
    }
    if sa == 255 {
        // coef1 = 255 * 128, coef2 = 0: the source pixel exactly (checked
        // against the transcription below for every value).
        dst[..4].copy_from_slice(&src[..4]);
        return;
    }
    let da = dst[3] as u32;
    let blend = da * (255 - sa);
    let outa255 = sa * 255 + blend;
    let coef1 = sa * 255 * 255 * (1 << PRECISION_BITS) / outa255;
    let coef2 = 255 * (1 << PRECISION_BITS) - coef1;
    for c in 0..3 {
        let tmp = src[c] as u32 * coef1 + dst[c] as u32 * coef2;
        dst[c] = (shift_for_div255(tmp + (0x80 << PRECISION_BITS)) >> PRECISION_BITS) as u8;
    }
    dst[3] = shift_for_div255(outa255 + 0x80) as u8;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A straight transcription of Pillow's C, for comparison.
    fn pillow(dst: [u8; 4], src: [u8; 4]) -> [u8; 4] {
        if src[3] == 0 {
            return dst;
        }
        let (sa, da) = (src[3] as u32, dst[3] as u32);
        let blend = da * (255 - sa);
        let outa255 = sa * 255 + blend;
        let coef1 = sa * 255 * 255 * 128 / outa255;
        let coef2 = 255 * 128 - coef1;
        let f = |s: u8, d: u8| {
            let t = s as u32 * coef1 + d as u32 * coef2 + (0x80 << 7);
            ((((t >> 8) + t) >> 8) >> 7) as u8
        };
        let a = outa255 + 0x80;
        [f(src[0], dst[0]), f(src[1], dst[1]), f(src[2], dst[2]), (((a >> 8) + a) >> 8) as u8]
    }

    #[test]
    fn opaque_source_is_a_copy() {
        for da in 0..=255u8 {
            for c in 0..=255u8 {
                for d in [0u8, 1, 77, 128, 254, 255] {
                    let src = [c, 255 - c, c / 3, 255];
                    let dst = [d, c, 255 - d, da];
                    let mut out = dst;
                    over_pixel(&mut out, &src);
                    assert_eq!(out, pillow(dst, src));
                }
            }
        }
    }

    #[test]
    fn over_matches_transcription() {
        for sa in 0..=255u8 {
            for da in (0..=255u8).step_by(5) {
                for c in (0..=255u8).step_by(17) {
                    let src = [c, 255 - c, c / 2, sa];
                    let dst = [255 - c, c, 7, da];
                    let mut out = dst;
                    over_pixel(&mut out, &src);
                    assert_eq!(out, pillow(dst, src));
                }
            }
        }
    }
}
