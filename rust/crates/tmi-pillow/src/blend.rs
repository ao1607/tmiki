//! `Image.blend` (libImaging/Blend.c): `in1 + alpha * (in2 - in1)` per byte
//! in float32 (alpha is a C float), truncated to u8; outside 0..=1 the
//! value is clipped first.

/// `Image.blend(a, b, alpha)` of two equally long byte buffers (any 8-bit
/// mode; every byte is a band).
pub fn blend(a: &[u8], b: &[u8], alpha: f32) -> Vec<u8> {
    assert_eq!(a.len(), b.len());
    if alpha == 0.0 {
        return a.to_vec();
    }
    if alpha == 1.0 {
        return b.to_vec();
    }
    if (0.0..=1.0).contains(&alpha) {
        a.iter().zip(b).map(|(&x, &y)| (x as i32 as f32 + alpha * (y as i32 - x as i32) as f32) as u8).collect()
    } else {
        a.iter()
            .zip(b)
            .map(|(&x, &y)| {
                let t = x as i32 as f32 + alpha * (y as i32 - x as i32) as f32;
                if t <= 0.0 {
                    0
                } else if t >= 255.0 {
                    255
                } else {
                    t as u8
                }
            })
            .collect()
    }
}