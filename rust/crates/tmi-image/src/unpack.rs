//! libImaging's image memory, raw-mode unpackers (Unpack.c) and raw decoder
//! (RawDecode.c), for the decoders that hand Pillow rows of packed samples
//! (BMP, TIFF, PSD).
//!
//! [`ImBuf`] holds pixels as Pillow's core image does: one byte per pixel
//! for "1" (0 / 255), "L" and "P", two for "I;16", four (the bands in
//! slots 0..3, "LA" in 0 and 3) for the multi-band modes. [`ImBuf::into_image`]
//! gives the `tobytes()` layout `tmi_pillow::image::Image` holds.

use tmi_pillow::image::{Image, Mode, Palette};

/// Bytes per pixel in Pillow's image memory.
pub(crate) fn pixel_size(mode: Mode) -> usize {
    match mode {
        Mode::One | Mode::L | Mode::P => 1,
        Mode::I16 | Mode::I16B => 2,
        Mode::LA | Mode::La | Mode::RGB | Mode::RGBA | Mode::RGBa | Mode::CMYK | Mode::I => 4,
    }
}

/// A Pillow core image (`Image.core.new(mode, size)`: zero-filled).
pub(crate) struct ImBuf {
    pub mode: Mode,
    pub w: usize,
    pub h: usize,
    pub ps: usize,
    pub data: Vec<u8>,
}

impl ImBuf {
    pub fn new(mode: Mode, w: usize, h: usize) -> ImBuf {
        let ps = pixel_size(mode);
        ImBuf { mode, w, h, ps, data: vec![0; w * h * ps] }
    }

    /// Row `y` from column `x` on.
    pub fn row_from(&mut self, y: usize, x: usize) -> &mut [u8] {
        let line = self.w * self.ps;
        &mut self.data[y * line + x * self.ps..(y + 1) * line]
    }

    /// `ImageOps.exif_transpose` for an EXIF orientation (2..=8; others
    /// leave the image as it is): Pillow's transpose of the core image.
    pub fn orient(self, orientation: u64) -> ImBuf {
        let (w, h, ps) = (self.w, self.h, self.ps);
        // (output size, source pixel of output (x, y))
        let swap = matches!(orientation, 5..=8);
        let (ow, oh) = if swap { (h, w) } else { (w, h) };
        let src = |x: usize, y: usize| -> (usize, usize) {
            match orientation {
                2 => (w - 1 - x, y),             // FLIP_LEFT_RIGHT
                3 => (w - 1 - x, h - 1 - y),     // ROTATE_180
                4 => (x, h - 1 - y),             // FLIP_TOP_BOTTOM
                5 => (y, x),                     // TRANSPOSE
                6 => (y, h - 1 - x),             // ROTATE_270
                7 => (w - 1 - y, h - 1 - x),     // TRANSVERSE
                _ => (w - 1 - y, x),             // 8: ROTATE_90
            }
        };
        if !(2..=8).contains(&orientation) {
            return self;
        }
        let mut data = vec![0u8; self.data.len()];
        for y in 0..oh {
            for x in 0..ow {
                let (sx, sy) = src(x, y);
                let s = (sy * w + sx) * ps;
                let d = (y * ow + x) * ps;
                data[d..d + ps].copy_from_slice(&self.data[s..s + ps]);
            }
        }
        ImBuf { mode: self.mode, w: ow, h: oh, ps, data }
    }

    /// The `tobytes()` layout, as a `tmi_pillow` image.
    pub fn into_image(self, palette: Option<Palette>) -> Image {
        let n = self.w * self.h;
        let data = match self.mode {
            Mode::RGB => {
                let mut v = Vec::with_capacity(n * 3);
                for p in self.data.chunks_exact(4) {
                    v.extend_from_slice(&p[..3]);
                }
                v
            }
            Mode::LA | Mode::La => {
                let mut v = Vec::with_capacity(n * 2);
                for p in self.data.chunks_exact(4) {
                    v.push(p[0]);
                    v.push(p[3]);
                }
                v
            }
            _ => self.data,
        };
        let mut img = Image::new(self.mode, self.w, self.h, data);
        if self.mode == Mode::P {
            img.palette = Some(palette.unwrap_or_else(|| Palette::from_rgb(&[])));
        }
        img
    }
}

/// An unpacker: `bits` per pixel of input, `f(out, in, pixels)`.
#[derive(Clone, Copy)]
pub(crate) struct Unpacker {
    pub bits: usize,
    pub f: fn(&mut [u8], &[u8], usize),
}

fn bits_msb(out: &mut [u8], inp: &[u8], pixels: usize, bits: usize, f: impl Fn(u8) -> u8) {
    let per = 8 / bits;
    let mask = ((1u16 << bits) - 1) as u8;
    for i in 0..pixels {
        let b = inp[i / per];
        let shift = 8 - bits - (i % per) * bits;
        out[i] = f((b >> shift) & mask);
    }
}

fn reverse_bits(b: u8) -> u8 {
    b.reverse_bits()
}

fn unpack1(o: &mut [u8], i: &[u8], n: usize) {
    bits_msb(o, i, n, 1, |v| if v != 0 { 255 } else { 0 })
}
fn unpack1i(o: &mut [u8], i: &[u8], n: usize) {
    bits_msb(o, i, n, 1, |v| if v != 0 { 0 } else { 255 })
}
fn unpack1r(o: &mut [u8], i: &[u8], n: usize) {
    for k in 0..n {
        o[k] = if (reverse_bits(i[k / 8]) >> (7 - k % 8)) & 1 != 0 { 255 } else { 0 };
    }
}
fn unpack1ir(o: &mut [u8], i: &[u8], n: usize) {
    for k in 0..n {
        o[k] = if (reverse_bits(i[k / 8]) >> (7 - k % 8)) & 1 != 0 { 0 } else { 255 };
    }
}
fn unpack18(o: &mut [u8], i: &[u8], n: usize) {
    for k in 0..n {
        o[k] = if i[k] > 0 { 255 } else { 0 };
    }
}
fn unpack_l2(o: &mut [u8], i: &[u8], n: usize) {
    bits_msb(o, i, n, 2, |v| v * 0x55)
}
fn unpack_l2i(o: &mut [u8], i: &[u8], n: usize) {
    bits_msb(o, i, n, 2, |v| 0xff - v * 0x55)
}
fn unpack_l2r(o: &mut [u8], i: &[u8], n: usize) {
    for k in 0..n {
        let b = reverse_bits(i[k / 4]);
        o[k] = ((b >> (6 - (k % 4) * 2)) & 3) * 0x55;
    }
}
fn unpack_l2ir(o: &mut [u8], i: &[u8], n: usize) {
    for k in 0..n {
        let b = reverse_bits(i[k / 4]);
        o[k] = 0xff - ((b >> (6 - (k % 4) * 2)) & 3) * 0x55;
    }
}
fn unpack_l4(o: &mut [u8], i: &[u8], n: usize) {
    bits_msb(o, i, n, 4, |v| v * 0x11)
}
fn unpack_l4i(o: &mut [u8], i: &[u8], n: usize) {
    bits_msb(o, i, n, 4, |v| 0xff - v * 0x11)
}
fn unpack_l4r(o: &mut [u8], i: &[u8], n: usize) {
    for k in 0..n {
        let b = reverse_bits(i[k / 2]);
        o[k] = ((b >> (4 - (k % 2) * 4)) & 15) * 0x11;
    }
}
fn unpack_l4ir(o: &mut [u8], i: &[u8], n: usize) {
    for k in 0..n {
        let b = reverse_bits(i[k / 2]);
        o[k] = 0xff - ((b >> (4 - (k % 2) * 4)) & 15) * 0x11;
    }
}
fn copy1(o: &mut [u8], i: &[u8], n: usize) {
    o[..n].copy_from_slice(&i[..n]);
}
fn unpack_li(o: &mut [u8], i: &[u8], n: usize) {
    for k in 0..n {
        o[k] = !i[k];
    }
}
fn unpack_lr(o: &mut [u8], i: &[u8], n: usize) {
    for k in 0..n {
        o[k] = reverse_bits(i[k]);
    }
}
fn unpack_p1(o: &mut [u8], i: &[u8], n: usize) {
    bits_msb(o, i, n, 1, |v| v)
}
fn unpack_p2(o: &mut [u8], i: &[u8], n: usize) {
    bits_msb(o, i, n, 2, |v| v)
}
fn unpack_p4(o: &mut [u8], i: &[u8], n: usize) {
    bits_msb(o, i, n, 4, |v| v)
}
fn copy2(o: &mut [u8], i: &[u8], n: usize) {
    o[..2 * n].copy_from_slice(&i[..2 * n]);
}
fn copy4(o: &mut [u8], i: &[u8], n: usize) {
    o[..4 * n].copy_from_slice(&i[..4 * n]);
}

/// Four output bytes per pixel from `step` input bytes.
fn px4(o: &mut [u8], i: &[u8], n: usize, step: usize, f: impl Fn(&[u8]) -> [u8; 4]) {
    for k in 0..n {
        o[k * 4..k * 4 + 4].copy_from_slice(&f(&i[k * step..k * step + step]));
    }
}

fn unpack_bgr(o: &mut [u8], i: &[u8], n: usize) {
    px4(o, i, n, 3, |p| [p[2], p[1], p[0], 255])
}
fn unpack_bgr15(o: &mut [u8], i: &[u8], n: usize) {
    px4(o, i, n, 2, |p| {
        let v = p[0] as u32 + ((p[1] as u32) << 8);
        let c = |s: u32| ((v >> s) & 31) * 255 / 31;
        [c(10) as u8, c(5) as u8, c(0) as u8, 255]
    })
}
fn unpack_bgr16(o: &mut [u8], i: &[u8], n: usize) {
    px4(o, i, n, 2, |p| {
        let v = p[0] as u32 + ((p[1] as u32) << 8);
        [(((v >> 11) & 31) * 255 / 31) as u8, (((v >> 5) & 63) * 255 / 63) as u8, ((v & 31) * 255 / 31) as u8, 255]
    })
}
fn unpack_bgrx(o: &mut [u8], i: &[u8], n: usize) {
    px4(o, i, n, 4, |p| [p[2], p[1], p[0], 255])
}
fn unpack_bgxr(o: &mut [u8], i: &[u8], n: usize) {
    px4(o, i, n, 4, |p| [p[3], p[1], p[0], 255])
}
fn unpack_xbgr(o: &mut [u8], i: &[u8], n: usize) {
    px4(o, i, n, 4, |p| [p[3], p[2], p[1], 255])
}
fn unpack_bgra(o: &mut [u8], i: &[u8], n: usize) {
    px4(o, i, n, 4, |p| [p[2], p[1], p[0], p[3]])
}
fn unpack_abgr(o: &mut [u8], i: &[u8], n: usize) {
    px4(o, i, n, 4, |p| [p[3], p[2], p[1], p[0]])
}
fn unpack_bgar(o: &mut [u8], i: &[u8], n: usize) {
    px4(o, i, n, 4, |p| [p[3], p[1], p[0], p[2]])
}

fn unpack_rgb(o: &mut [u8], i: &[u8], n: usize) {
    px4(o, i, n, 3, |p| [p[0], p[1], p[2], 255])
}
fn unpack_rgbr(o: &mut [u8], i: &[u8], n: usize) {
    px4(o, i, n, 3, |p| [reverse_bits(p[0]), reverse_bits(p[1]), reverse_bits(p[2]), 255])
}
fn unpack_la(o: &mut [u8], i: &[u8], n: usize) {
    px4(o, i, n, 2, |p| [p[0], p[0], p[0], p[1]])
}
fn copy4skip1(o: &mut [u8], i: &[u8], n: usize) {
    px4(o, i, n, 5, |p| [p[0], p[1], p[2], p[3]])
}
fn copy4skip2(o: &mut [u8], i: &[u8], n: usize) {
    px4(o, i, n, 6, |p| [p[0], p[1], p[2], p[3]])
}
/// unpackRGBa and friends: `CLIP8(v * 255 / a)`, 0 for a = 0.
#[inline]
fn unpremul(r: u8, g: u8, b: u8, a: u8) -> [u8; 4] {
    match a {
        0 => [0; 4],
        255 => [r, g, b, a],
        _ => {
            let f = |v: u8| (v as u32 * 255 / a as u32).min(255) as u8;
            [f(r), f(g), f(b), a]
        }
    }
}
pub(crate) fn unpack_rgba_premul(o: &mut [u8], i: &[u8], n: usize) {
    px4(o, i, n, 4, |p| unpremul(p[0], p[1], p[2], p[3]))
}
fn unpack_rgba_premul_skip1(o: &mut [u8], i: &[u8], n: usize) {
    px4(o, i, n, 5, |p| unpremul(p[0], p[1], p[2], p[3]))
}
fn unpack_rgba_premul_skip2(o: &mut [u8], i: &[u8], n: usize) {
    px4(o, i, n, 6, |p| unpremul(p[0], p[1], p[2], p[3]))
}
fn unpack_rgba_premul16l(o: &mut [u8], i: &[u8], n: usize) {
    px4(o, i, n, 8, |p| unpremul(p[1], p[3], p[5], p[7]))
}
fn unpack_rgba_premul16b(o: &mut [u8], i: &[u8], n: usize) {
    px4(o, i, n, 8, |p| unpremul(p[0], p[2], p[4], p[6]))
}
fn unpack_rgb16l(o: &mut [u8], i: &[u8], n: usize) {
    px4(o, i, n, 6, |p| [p[1], p[3], p[5], 255])
}
fn unpack_rgb16b(o: &mut [u8], i: &[u8], n: usize) {
    px4(o, i, n, 6, |p| [p[0], p[2], p[4], 255])
}
fn unpack_rgba16l(o: &mut [u8], i: &[u8], n: usize) {
    px4(o, i, n, 8, |p| [p[1], p[3], p[5], p[7]])
}
fn unpack_rgba16b(o: &mut [u8], i: &[u8], n: usize) {
    px4(o, i, n, 8, |p| [p[0], p[2], p[4], p[6]])
}
fn unpack_l16b(o: &mut [u8], i: &[u8], n: usize) {
    for k in 0..n {
        o[k] = i[2 * k];
    }
}
fn swap2(o: &mut [u8], i: &[u8], n: usize) {
    for k in 0..n {
        o[2 * k] = i[2 * k + 1];
        o[2 * k + 1] = i[2 * k];
    }
}
fn unpack_i16r(o: &mut [u8], i: &[u8], n: usize) {
    for k in 0..2 * n {
        o[k] = reverse_bits(i[k]);
    }
}

/// `band{b}` / `band{b}16L` / `band{b}16B`: one band of a 4-byte pixel
/// from 1 / 2-byte samples.
pub(crate) fn band(b: usize, bytes: usize, big: bool) -> fn(&mut [u8], &[u8], usize) {
    macro_rules! bandfn {
        ($b:expr, $step:expr, $at:expr) => {{
            fn f(o: &mut [u8], i: &[u8], n: usize) {
                for k in 0..n {
                    o[k * 4 + $b] = i[k * $step + $at];
                }
            }
            f
        }};
    }
    match (b, bytes, big) {
        (0, 1, _) => bandfn!(0, 1, 0),
        (1, 1, _) => bandfn!(1, 1, 0),
        (2, 1, _) => bandfn!(2, 1, 0),
        (3, 1, _) => bandfn!(3, 1, 0),
        (0, 2, false) => bandfn!(0, 2, 1),
        (1, 2, false) => bandfn!(1, 2, 1),
        (2, 2, false) => bandfn!(2, 2, 1),
        (3, 2, false) => bandfn!(3, 2, 1),
        (0, 2, true) => bandfn!(0, 2, 0),
        (1, 2, true) => bandfn!(1, 2, 0),
        (2, 2, true) => bandfn!(2, 2, 0),
        _ => bandfn!(3, 2, 0),
    }
}

/// `ImagingFindUnpacker(mode, rawmode)` for the pairs the P0 decoders ask
/// for; None where Pillow has no unpacker ("unknown raw mode") or this
/// module does not carry it. The native-order ("N") raw modes are those of
/// a little-endian machine.
pub(crate) fn find(mode: Mode, raw: &str) -> Option<Unpacker> {
    use Mode::*;
    let u = |bits: usize, f: fn(&mut [u8], &[u8], usize)| Some(Unpacker { bits, f });
    let band_of = |s: &str| -> Option<(usize, usize, bool)> {
        Some(match s {
            "R" | "C" => (0, 1, false),
            "G" | "M" => (1, 1, false),
            "B" | "Y" => (2, 1, false),
            "A" | "K" => (3, 1, false),
            "R;16L" | "R;16N" => (0, 2, false),
            "G;16L" | "G;16N" => (1, 2, false),
            "B;16L" | "B;16N" => (2, 2, false),
            "A;16L" | "A;16N" => (3, 2, false),
            "R;16B" => (0, 2, true),
            "G;16B" => (1, 2, true),
            "B;16B" => (2, 2, true),
            "A;16B" => (3, 2, true),
            _ => return None,
        })
    };
    // RGB / RGBA take R G B (A) bands; CMYK takes C M Y K (8-bit only).
    if let Some((b, bytes, big)) = band_of(raw) {
        let cmyk_name = matches!(raw, "C" | "M" | "Y" | "K");
        let ok = match mode {
            RGB => !cmyk_name && b < 3,
            RGBA => !cmyk_name,
            CMYK => cmyk_name,
            _ => false,
        };
        return if ok { u(8 * bytes, band(b, bytes, big)) } else { None };
    }
    if mode == CMYK {
        // band{b}I: an inverted band (Photoshop's CMYK planes).
        let inv: Option<fn(&mut [u8], &[u8], usize)> = match raw {
            "C;I" => Some(|o, i, n| (0..n).for_each(|k| o[k * 4] = !i[k])),
            "M;I" => Some(|o, i, n| (0..n).for_each(|k| o[k * 4 + 1] = !i[k])),
            "Y;I" => Some(|o, i, n| (0..n).for_each(|k| o[k * 4 + 2] = !i[k])),
            "K;I" => Some(|o, i, n| (0..n).for_each(|k| o[k * 4 + 3] = !i[k])),
            _ => None,
        };
        if let Some(f) = inv {
            return u(8, f);
        }
    }
    match (mode, raw) {
        (L, "L;16B") | (P, "PX") => u(16, unpack_l16b),
        (LA, "LA") => u(16, unpack_la),
        (RGB, "RGB") => u(24, unpack_rgb),
        (RGB, "RGB;R") => u(24, unpack_rgbr),
        (RGB, "RGBXX") | (RGBA, "RGBAX") | (CMYK, "CMYKX") => u(40, copy4skip1),
        (RGB, "RGBXXX") | (RGBA, "RGBAXX") | (CMYK, "CMYKXX") => u(48, copy4skip2),
        (RGBA, "RGBa") => u(32, unpack_rgba_premul),
        (RGBA, "RGBaX") => u(40, unpack_rgba_premul_skip1),
        (RGBA, "RGBaXX") => u(48, unpack_rgba_premul_skip2),
        (RGBA, "RGBa;16L") | (RGBA, "RGBa;16N") => u(64, unpack_rgba_premul16l),
        (RGBA, "RGBa;16B") => u(64, unpack_rgba_premul16b),
        (RGB, "RGB;16L") | (RGB, "RGB;16N") => u(48, unpack_rgb16l),
        (RGB, "RGB;16B") => u(48, unpack_rgb16b),
        (RGB, "RGBX;16L") | (RGB, "RGBX;16N") | (RGBA, "RGBA;16L") | (RGBA, "RGBA;16N") | (CMYK, "CMYK;16L") | (CMYK, "CMYK;16N") => {
            u(64, unpack_rgba16l)
        }
        (RGB, "RGBX;16B") | (RGBA, "RGBA;16B") | (CMYK, "CMYK;16B") => u(64, unpack_rgba16b),
        (I16, "I;16N") => u(16, copy2),
        (I16B, "I;16N") | (I16, "I;16B") => u(16, swap2),
        (I16, "I;16R") => u(16, unpack_i16r),
        (One, "1") => u(1, unpack1),
        (One, "1;I") => u(1, unpack1i),
        (One, "1;R") => u(1, unpack1r),
        (One, "1;IR") => u(1, unpack1ir),
        (One, "1;8") => u(8, unpack18),
        (L, "L;2") => u(2, unpack_l2),
        (L, "L;2I") => u(2, unpack_l2i),
        (L, "L;2R") => u(2, unpack_l2r),
        (L, "L;2IR") => u(2, unpack_l2ir),
        (L, "L;4") => u(4, unpack_l4),
        (L, "L;4I") => u(4, unpack_l4i),
        (L, "L;4R") => u(4, unpack_l4r),
        (L, "L;4IR") => u(4, unpack_l4ir),
        (L, "L") => u(8, copy1),
        (L, "L;I") => u(8, unpack_li),
        (L, "L;R") => u(8, unpack_lr),
        (P, "P;1") => u(1, unpack_p1),
        (P, "P;2") => u(2, unpack_p2),
        (P, "P;4") => u(4, unpack_p4),
        (P, "P") | (P, "L") => u(8, copy1),
        (P, "P;R") => u(8, unpack_lr),
        (I16, "I;16") | (I16B, "I;16B") => u(16, copy2),
        (RGB, "BGR") | (RGBA, "BGR") => u(24, unpack_bgr),
        (RGB, "BGR;15") | (RGB, "BGR;5") => u(16, unpack_bgr15),
        (RGB, "BGR;16") => u(16, unpack_bgr16),
        (RGB, "BGRX") => u(32, unpack_bgrx),
        (RGB, "BGXR") => u(32, unpack_bgxr),
        (RGB, "XBGR") => u(32, unpack_xbgr),
        (RGBA, "RGBA") | (RGB, "RGBX") | (CMYK, "CMYK") => u(32, copy4),
        (RGBA, "BGRA") => u(32, unpack_bgra),
        (RGBA, "ABGR") => u(32, unpack_abgr),
        (RGBA, "BGAR") => u(32, unpack_bgar),
        _ => None,
    }
}

/// How RawDecode.c stopped short.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RawError {
    /// The stride is shorter than a row (IMAGING_CODEC_CONFIG).
    Config,
    /// The data ended before the last row (ImageFile.load: "image file is
    /// truncated"; set_as_raw: "not enough image data").
    Short,
}

/// RawDecode.c over all the bytes from the tile's offset to the end of the
/// file (ImageFile.load feeds it what it reads until the decoder is done or
/// a read comes back empty): `stride` 0 means rows of the unpacker's size;
/// `ystep` < 0 fills the tile bottom-up.
pub(crate) fn raw_decode(
    im: &mut ImBuf,
    (x0, y0, x1, y1): (usize, usize, usize, usize),
    un: Unpacker,
    stride: usize,
    ystep: i32,
    data: &[u8],
) -> Result<(), RawError> {
    let (xsize, ysize) = (x1 - x0, y1 - y0);
    let bytes = (xsize * un.bits).div_ceil(8);
    let skip = if stride != 0 {
        if stride < bytes {
            return Err(RawError::Config);
        }
        stride - bytes
    } else {
        0
    };
    let mut pos = 0usize;
    for k in 0..ysize {
        if k > 0 {
            pos += skip;
        }
        let Some(line) = data.get(pos..pos + bytes) else {
            return Err(RawError::Short);
        };
        let y = if ystep < 0 { ysize - 1 - k } else { k };
        (un.f)(im.row_from(y + y0, x0), line, xsize);
        pos += bytes;
    }
    Ok(())
}
