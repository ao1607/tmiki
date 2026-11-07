//! `Image.convert(mode)` (PIL/Image.py) over libImaging/Convert.c, for the
//! modes Pillow's PNG and JPEG decoders produce and the targets tmi asks
//! for (mostly "RGBA"), including the Python-level rules around it:
//!
//! - `info["transparency"]` on "1" / "L" / "I" / "I;16" (to "LA" / "RGBA")
//!   and "RGB" (to "La" / "LA" / "RGBa" / "RGBA") goes through
//!   `convert_transparent`: the plain conversion, then each pixel equal to
//!   the transparent colour gets alpha 0 (all channels 0 when the target is
//!   premultiplied). The colour is compared after the conversion and only
//!   its low 8 bits count (`r & 0xff`), so a 16-bit grey or RGB key matches
//!   pixels whose 8-bit value equals the key's low byte.
//! - "P" with transparency to "LA" / "RGBA": the palette's alphas are set
//!   from the bytes (`putpalettealphas`) or the one index (`putpalettealpha`).
//! - A pair with no converter is retried through the source's base mode
//!   ("I;16" -> "L" -> "RGBA": 16-bit grey clamps to 255 above 255, it is
//!   not scaled).

use crate::image::{Image, Mode, Palette, Transparency};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConvertError {
    /// "conversion from X to Y not supported" (also what this module does
    /// not implement).
    Unsupported(Mode, Mode),
    /// A transparency value `convert_transparent` cannot parse.
    BadTransparency,
}

#[inline]
fn muldiv255(a: u32, b: u32) -> u32 {
    let tmp = a * b + 128;
    ((tmp >> 8) + tmp) >> 8
}

#[inline]
fn clip8(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

/// ITU-R 601-2 luma in 16.16 fixed point (`L24 >> 16`).
#[inline]
fn l24(r: u8, g: u8, b: u8) -> u8 {
    ((r as u32 * 19595 + g as u32 * 38470 + b as u32 * 7471 + 0x8000) >> 16) as u8
}

/// `rgba2rgbA` / `la2lA`: undo premultiplication of one value.
#[inline]
pub fn unpremultiply(v: u8, a: u8) -> u8 {
    if a == 255 || a == 0 {
        v
    } else {
        clip8((255 * v as i32) / a as i32)
    }
}

/// `rgbA2rgba` / `lA2la`: premultiply one value.
#[inline]
pub fn premultiply(v: u8, a: u8) -> u8 {
    muldiv255(v as u32, a as u32) as u8
}

/// RGBA -> RGBa in place.
pub fn premultiply_rgba(px: &mut [u8]) {
    for p in px.chunks_exact_mut(4) {
        let a = p[3];
        p[0] = premultiply(p[0], a);
        p[1] = premultiply(p[1], a);
        p[2] = premultiply(p[2], a);
    }
}

/// RGBa -> RGBA in place.
pub fn unpremultiply_rgba(px: &mut [u8]) {
    for p in px.chunks_exact_mut(4) {
        let a = p[3];
        p[0] = unpremultiply(p[0], a);
        p[1] = unpremultiply(p[1], a);
        p[2] = unpremultiply(p[2], a);
    }
}

/// LA -> La in place (2 bytes per pixel).
pub fn premultiply_la(px: &mut [u8]) {
    for p in px.chunks_exact_mut(2) {
        p[0] = premultiply(p[0], p[1]);
    }
}

/// La -> LA in place.
pub fn unpremultiply_la(px: &mut [u8]) {
    for p in px.chunks_exact_mut(2) {
        p[0] = unpremultiply(p[0], p[1]);
    }
}

/// `cmyk2rgb` for one pixel.
#[inline]
pub fn cmyk_to_rgb(c: u8, m: u8, y: u8, k: u8) -> [u8; 3] {
    let nk = 255 - k as i32;
    let f = |v: u8| clip8(nk - muldiv255(v as u32, nk as u32) as i32);
    [f(c), f(m), f(y)]
}

/// `getmodebase`.
fn base_mode(m: Mode) -> Mode {
    match m {
        Mode::One | Mode::L | Mode::I | Mode::LA | Mode::La | Mode::I16 | Mode::I16B => Mode::L,
        Mode::P => Mode::P,
        Mode::RGB | Mode::RGBA | Mode::RGBa | Mode::CMYK => Mode::RGB,
    }
}

/// Each source pixel (`from.bytes_per_pixel()` bytes) to RGBA (4 bytes),
/// the "RGB / RGBA / LA" converters that write a full 4-byte pixel: the
/// target keeps the bands it has.
fn map_pixels(img: &Image, to: Mode, f: impl Fn(&[u8]) -> [u8; 4]) -> Image {
    let bpp = img.mode.bytes_per_pixel();
    let n = img.width * img.height;
    let out_bpp = to.bytes_per_pixel();
    let mut data = Vec::with_capacity(n * out_bpp);
    for p in img.data.chunks_exact(bpp) {
        let v = f(p);
        match to {
            Mode::RGBA | Mode::RGBa | Mode::CMYK => data.extend_from_slice(&v),
            Mode::RGB => data.extend_from_slice(&v[..3]),
            // 2-band images keep bands 0 and 3 of the 4-byte pixel.
            Mode::LA | Mode::La => {
                data.push(v[0]);
                data.push(v[3]);
            }
            Mode::L | Mode::One | Mode::P => data.push(v[0]),
            _ => unreachable!(),
        }
    }
    Image::new(to, img.width, img.height, data)
}

fn i16_clamp_l(lo: u8, hi: u8) -> u8 {
    if hi != 0 {
        255
    } else {
        lo
    }
}

/// libImaging `convert()` (no transparency handling, no fallback): the
/// converter table, `frompalette` for "P".
fn core_convert(img: &Image, to: Mode) -> Result<Image, ConvertError> {
    use Mode::*;
    let from = img.mode;
    if from == to {
        let mut out = Image::new(to, img.width, img.height, img.data.clone());
        out.palette = img.palette.clone();
        return Ok(out);
    }
    let unsupported = Err(ConvertError::Unsupported(from, to));
    if from == P {
        let pal = img.palette.clone().unwrap_or_else(|| Palette::from_rgb(&[]));
        let e = &pal.entries;
        return Ok(match to {
            One => map_pixels(img, One, |p| {
                let c = e[p[0] as usize];
                let l = c[0] as i32 * 299 + c[1] as i32 * 587 + c[2] as i32 * 114;
                [if l >= 128000 { 255 } else { 0 }, 0, 0, 0]
            }),
            L => map_pixels(img, L, |p| {
                let c = e[p[0] as usize];
                [l24(c[0], c[1], c[2]), 0, 0, 0]
            }),
            LA => map_pixels(img, LA, |p| {
                let c = e[p[0] as usize];
                let l = l24(c[0], c[1], c[2]);
                [l, l, l, c[3]]
            }),
            RGB => map_pixels(img, RGB, |p| {
                let c = e[p[0] as usize];
                [c[0], c[1], c[2], 255]
            }),
            RGBA => map_pixels(img, RGBA, |p| e[p[0] as usize]),
            _ => return unsupported,
        });
    }
    let out = match (from, to) {
        (One, L) => map_pixels(img, L, |p| [if p[0] != 0 { 255 } else { 0 }, 0, 0, 0]),
        (One, LA | RGB | RGBA) => map_pixels(img, to, |p| {
            let v = if p[0] != 0 { 255 } else { 0 };
            [v, v, v, 255]
        }),
        (L, One) => map_pixels(img, One, |p| [if p[0] >= 128 { 255 } else { 0 }, 0, 0, 0]),
        (L, LA | RGB | RGBA) => map_pixels(img, to, |p| [p[0], p[0], p[0], 255]),
        (LA, L) => map_pixels(img, L, |p| [p[0], 0, 0, 0]),
        (LA, La) => map_pixels(img, La, |p| {
            let v = premultiply(p[0], p[1]);
            [v, v, v, p[1]]
        }),
        (LA, RGB | RGBA) => map_pixels(img, to, |p| [p[0], p[0], p[0], p[1]]),
        (La, LA) => map_pixels(img, LA, |p| {
            let v = unpremultiply(p[0], p[1]);
            [v, v, v, p[1]]
        }),
        (I, L) => map_pixels(img, L, |p| [i32::from_le_bytes([p[0], p[1], p[2], p[3]]).clamp(0, 255) as u8, 0, 0, 0]),
        (I, RGB | RGBA) => map_pixels(img, to, |p| {
            let v = i32::from_le_bytes([p[0], p[1], p[2], p[3]]).clamp(0, 255) as u8;
            [v, v, v, 255]
        }),
        (RGB, L) => map_pixels(img, L, |p| [l24(p[0], p[1], p[2]), 0, 0, 0]),
        (RGB, LA | La) => map_pixels(img, to, |p| {
            let l = l24(p[0], p[1], p[2]);
            [l, l, l, 255]
        }),
        (RGB, RGBA | RGBa) => map_pixels(img, to, |p| [p[0], p[1], p[2], 255]),
        (RGB, CMYK) => map_pixels(img, CMYK, |p| [!p[0], !p[1], !p[2], 0]),
        (RGBA, L) => map_pixels(img, L, |p| [l24(p[0], p[1], p[2]), 0, 0, 0]),
        (RGBA, LA) => map_pixels(img, LA, |p| {
            let l = l24(p[0], p[1], p[2]);
            [l, l, l, p[3]]
        }),
        (RGBA, RGB) => map_pixels(img, RGB, |p| [p[0], p[1], p[2], 255]),
        (RGBA, RGBa) => map_pixels(img, RGBa, |p| [premultiply(p[0], p[3]), premultiply(p[1], p[3]), premultiply(p[2], p[3]), p[3]]),
        (RGBA, CMYK) => map_pixels(img, CMYK, |p| [!p[0], !p[1], !p[2], 0]),
        (RGBa, RGBA) => map_pixels(img, RGBA, |p| [unpremultiply(p[0], p[3]), unpremultiply(p[1], p[3]), unpremultiply(p[2], p[3]), p[3]]),
        (RGBa, RGB) => map_pixels(img, RGB, |p| [unpremultiply(p[0], p[3]), unpremultiply(p[1], p[3]), unpremultiply(p[2], p[3]), 255]),
        (CMYK, RGB | RGBA) => map_pixels(img, to, |p| {
            let c = cmyk_to_rgb(p[0], p[1], p[2], p[3]);
            [c[0], c[1], c[2], 255]
        }),
        (I16, L) => map_pixels(img, L, |p| [i16_clamp_l(p[0], p[1]), 0, 0, 0]),
        (I16B, L) => map_pixels(img, L, |p| [i16_clamp_l(p[1], p[0]), 0, 0, 0]),
        (I16, RGB) => map_pixels(img, RGB, |p| {
            let v = i16_clamp_l(p[0], p[1]);
            [v, v, v, 255]
        }),
        (I16, I) | (I16B, I) => {
            let le = from == I16;
            let mut data = Vec::with_capacity(img.width * img.height * 4);
            for p in img.data.chunks_exact(2) {
                let v = if le { u16::from_le_bytes([p[0], p[1]]) } else { u16::from_be_bytes([p[0], p[1]]) } as i32;
                data.extend_from_slice(&v.to_le_bytes());
            }
            Image::new(I, img.width, img.height, data)
        }
        _ => return unsupported,
    };
    Ok(out)
}

/// `ImagingConvertTransparent(im, mode, r, g, b)`.
fn convert_transparent(img: &Image, to: Mode, r: i64, g: i64, b: i64) -> Result<Image, ConvertError> {
    use Mode::*;
    let (g, b) = if matches!(img.mode, One | I | I16 | L) { (r, r) } else { (g, b) };
    let key = [(r & 0xff) as u8, (g & 0xff) as u8, (b & 0xff) as u8];
    let premultiplied = matches!(to, RGBa | La);
    // RGB -> LA / La compares the key with the source pixel, the others
    // with the converted one.
    let source_key = img.mode == RGB && matches!(to, LA | La);
    // The converter's 4-byte pixel.
    let grey = |v: u8| [v, v, v, 255];
    let pixel: Box<dyn Fn(&[u8]) -> [u8; 4]> = match (img.mode, to) {
        (RGB, RGBA | RGBa) => Box::new(|p: &[u8]| [p[0], p[1], p[2], 255]),
        (RGB, LA | La) => Box::new(move |p: &[u8]| grey(l24(p[0], p[1], p[2]))),
        (One, RGBA | LA) => Box::new(move |p: &[u8]| grey(if p[0] != 0 { 255 } else { 0 })),
        (L, RGBA | LA) => Box::new(move |p: &[u8]| grey(p[0])),
        (I, RGBA | LA) => Box::new(move |p: &[u8]| grey(i32::from_le_bytes([p[0], p[1], p[2], p[3]]).clamp(0, 255) as u8)),
        (I16, RGBA | LA) => Box::new(move |p: &[u8]| grey(i16_clamp_l(p[0], p[1]))),
        _ => return Err(ConvertError::Unsupported(img.mode, to)),
    };
    let bpp = img.mode.bytes_per_pixel();
    let obpp = to.bytes_per_pixel();
    let mut data = Vec::with_capacity(img.width * img.height * obpp);
    for p in img.data.chunks_exact(bpp) {
        let mut v = pixel(p);
        // rgbT2a: the 4-byte pixel (or the RGB source, whose padding byte
        // is 255) equal to (r, g, b, 255) becomes (r, g, b, 0), or 0.
        let probe = if source_key { [p[0], p[1], p[2], 255] } else { v };
        if probe == [key[0], key[1], key[2], 255] {
            v = if premultiplied { [0; 4] } else { [key[0], key[1], key[2], 0] };
        }
        if obpp == 4 {
            data.extend_from_slice(&v);
        } else {
            data.push(v[0]);
            data.push(v[3]);
        }
    }
    Ok(Image::new(to, img.width, img.height, data))
}

/// `Image.convert(mode)` (no matrix; dithering never applies to the
/// supported pairs except RGB/L -> "1", which is not supported here).
pub fn convert(img: &Image, to: Mode) -> Result<Image, ConvertError> {
    use Mode::*;
    if to == img.mode {
        return Ok(img.clone());
    }
    let mut src: std::borrow::Cow<Image> = std::borrow::Cow::Borrowed(img);
    if let Some(t) = &img.transparency {
        let promote = (matches!(img.mode, One | L | I | I16) && matches!(to, LA | RGBA))
            || (img.mode == RGB && matches!(to, La | LA | RGBa | RGBA));
        if promote {
            return match *t {
                Transparency::Int(v) => convert_transparent(img, to, v, 0, 0),
                Transparency::Rgb(r, g, b) => convert_transparent(img, to, r, g, b),
                Transparency::Bytes(_) => Err(ConvertError::BadTransparency),
            };
        }
        if img.mode == P && matches!(to, LA | RGBA) {
            let mut p = img.clone();
            let mut pal = p.palette.take().unwrap_or_else(|| Palette::from_rgb(&[]));
            match t {
                // `putpalettealphas` / `putpalettealpha` refuse more than
                // 256 alphas and an index outside the palette.
                Transparency::Bytes(a) if a.len() <= 256 => {
                    for (e, &v) in pal.entries.iter_mut().zip(a.iter()) {
                        e[3] = v;
                    }
                }
                Transparency::Int(i) if (0..256).contains(i) => pal.entries[*i as usize][3] = 0,
                Transparency::Bytes(_) | Transparency::Int(_) => return Err(ConvertError::BadTransparency),
                Transparency::Rgb(..) => return Err(ConvertError::BadTransparency),
            }
            p.palette = Some(pal);
            p.transparency = None;
            src = std::borrow::Cow::Owned(p);
        }
    }
    match core_convert(&src, to) {
        Ok(out) => Ok(out),
        Err(e) => {
            let base = base_mode(src.mode);
            if base == src.mode {
                return Err(e);
            }
            let mid = core_convert(&src, base)?;
            core_convert(&mid, to)
        }
    }
}
