//! `ImageChops` (libImaging/Chops.c) on 8-bit channels.

/// `ImageChops.multiply` of two "L" pixels: `in1 * in2 / 255`, truncated.
#[inline]
pub fn multiply(a: u8, b: u8) -> u8 {
    ((a as u32 * b as u32) / 255) as u8
}

/// `ImageChops.multiply` over two planes of the same length, into `a`.
pub fn multiply_into(a: &mut [u8], b: &[u8]) {
    for (x, &y) in a.iter_mut().zip(b) {
        *x = multiply(*x, y);
    }
}

/// `ImageChops.invert`: `255 - in`.
#[inline]
pub fn invert(a: u8) -> u8 {
    255 - a
}

/// `ImageChops.difference`: `|in1 - in2|`, per byte (every band).
pub fn difference(a: &[u8], b: &[u8]) -> Vec<u8> {
    a.iter().zip(b).map(|(&x, &y)| x.abs_diff(y)).collect()
}

/// `ImageChops.lighter`: the larger byte.
pub fn lighter(a: &[u8], b: &[u8]) -> Vec<u8> {
    a.iter().zip(b).map(|(&x, &y)| x.max(y)).collect()
}

/// `ImageChops.darker`: the smaller byte.
pub fn darker(a: &[u8], b: &[u8]) -> Vec<u8> {
    a.iter().zip(b).map(|(&x, &y)| x.min(y)).collect()
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multiply_is_symmetric_and_bounded() {
        for a in 0..=255u8 {
            assert_eq!(multiply(a, 255), a);
            assert_eq!(multiply(a, 0), 0);
            for b in 0..=255u8 {
                assert_eq!(multiply(a, b), multiply(b, a));
            }
        }
    }
}
