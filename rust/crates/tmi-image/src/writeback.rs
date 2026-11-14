//! The colour mode a PNG / JPEG written back into a user's file gets
//! (issue #21; tmiki's `image_ops._fit_mode` before stage 2b): the original's
//! mode where the new RGBA pixels fit it exactly, else RGBA (PNG) / RGB
//! (JPEG). Every PNG candidate is checked the way Python checked it: its
//! `convert("RGBA")` (`tmi_pillow::convert`, Pillow-exact, colour keys
//! included) must give back exactly the RGBA pixels.

use std::collections::HashMap;

use tmi_pillow::convert::convert;
use tmi_pillow::image::{Image, Mode, Palette, Transparency};
use tmi_pillow::rgba::is_gray;

/// The original file's mode, palette (its entries, RGB) and
/// `info["transparency"]`, as Pillow opened it.
pub struct Source<'a> {
    pub mode: Mode,
    pub palette: Option<&'a [u8]>,
    pub transparency: Option<Transparency>,
}

fn gray_mode(m: Mode) -> bool {
    matches!(m, Mode::One | Mode::L | Mode::LA | Mode::La | Mode::I | Mode::I16 | Mode::I16B)
}

fn alpha_mode(m: Mode) -> bool {
    matches!(m, Mode::RGBA | Mode::LA | Mode::RGBa | Mode::La)
}

fn channel(rgba: &[u8], c: usize) -> Vec<u8> {
    rgba.chunks_exact(4).map(|p| p[c]).collect()
}

/// `_to_palette`: the pixels as indices into the source palette (the first
/// entry of equal ones), its transparency kept; None when a pixel is not
/// exactly an entry (colour and alpha).
fn to_palette(rgba: &Image, palette: &[u8], transparency: &Option<Transparency>) -> Option<(Image, usize)> {
    let count = palette.len() / 3;
    if count == 0 {
        return None;
    }
    let mut alpha = vec![255u8; count];
    match transparency {
        Some(Transparency::Int(t)) if (*t as usize) < count && *t >= 0 => alpha[*t as usize] = 0,
        Some(Transparency::Bytes(b)) => {
            for (a, &v) in alpha.iter_mut().zip(b.iter()) {
                *a = v;
            }
        }
        _ => {}
    }
    let mut index: HashMap<[u8; 4], u8> = HashMap::with_capacity(count);
    for i in (0..count.min(256)).rev() {
        index.insert([palette[3 * i], palette[3 * i + 1], palette[3 * i + 2], alpha[i]], i as u8);
    }
    let mut out = Vec::with_capacity(rgba.width * rgba.height);
    let mut last: Option<([u8; 4], u8)> = None;
    for p in rgba.data.chunks_exact(4) {
        let key = [p[0], p[1], p[2], p[3]];
        let i = match last {
            Some((k, i)) if k == key => i,
            _ => {
                let i = *index.get(&key)?;
                last = Some((key, i));
                i
            }
        };
        out.push(i);
    }
    let mut img = Image::new(Mode::P, rgba.width, rgba.height, out);
    img.palette = Some(Palette::from_rgb(&palette[..count * 3]));
    img.transparency = transparency.clone();
    Some((img, count))
}

/// Whether an ICC profile's data colour space suits an image of `mode`.
fn icc_fits(profile: &[u8], mode: Mode) -> bool {
    let space = profile.get(16..20).unwrap_or(b"");
    match mode {
        Mode::One | Mode::L | Mode::LA => space == b"GRAY",
        Mode::RGB | Mode::RGBA | Mode::P => space == b"RGB ",
        _ => false,
    }
}

fn family(mode: Mode) -> u8 {
    if gray_mode(mode) {
        0
    } else if mode == Mode::CMYK {
        1
    } else {
        2
    }
}

/// Whether the original's ICC profile goes into the written file: it
/// describes the written mode, or that mode kept the original's colour
/// family (a CMYK JPEG written as RGB drops its CMYK profile).
pub fn keeps_icc(profile: &[u8], written: Mode, original: Mode) -> bool {
    icc_fits(profile, written) || family(written) == family(original)
}

/// The iCCP chunk data Pillow writes: name "ICC Profile", method 0, the
/// zlib-compressed profile.
pub fn iccp_chunk(profile: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::new(6));
    z.write_all(profile).unwrap();
    [&b"ICC Profile\0\0"[..], &z.finish().unwrap()].concat()
}

/// The APP2 segments Pillow writes for an ICC profile: "ICC_PROFILE\0",
/// sequence number, count, then at most 65519 bytes of the profile each.
pub fn icc_segments(profile: &[u8]) -> Vec<Vec<u8>> {
    let parts: Vec<&[u8]> = profile.chunks(65533 - 14).collect();
    let n = parts.len();
    parts.iter().enumerate().map(|(i, p)| [&b"ICC_PROFILE\0"[..], &[(i + 1) as u8, n as u8], p].concat()).collect()
}

/// The image to write (and, for "P", its number of palette entries).
pub fn fit_mode(rgba: &Image, src: Option<&Source>, jpeg: bool) -> Result<(Image, usize), String> {
    assert_eq!(rgba.mode, Mode::RGBA);
    let (w, h) = (rgba.width, rgba.height);
    let gray = || Image::new(Mode::L, w, h, channel(&rgba.data, 0));
    let rgb = || convert(rgba, Mode::RGB).map_err(|e| format!("{e:?}"));
    if jpeg {
        let out = match src {
            Some(s) if gray_mode(s.mode) && is_gray(&rgba.data) => gray(),
            _ => rgb()?,
        };
        return Ok((out, 0));
    }
    let Some(src) = src else { return Ok((rgba.clone(), 0)) };
    let mode = src.mode;
    let key = &src.transparency;
    let has_alpha = alpha_mode(mode) || key.is_some();
    let mut candidates: Vec<Box<dyn Fn() -> Result<Option<(Image, usize)>, String>>> = Vec::new();
    if mode == Mode::P {
        candidates.push(Box::new(|| Ok(src.palette.and_then(|p| to_palette(rgba, p, &src.transparency)))));
    }
    let grey = gray_mode(mode) && is_gray(&rgba.data);
    if grey {
        if alpha_mode(mode) {
            candidates.push(Box::new(|| {
                let la: Vec<u8> = rgba.data.chunks_exact(4).flat_map(|p| [p[0], p[3]]).collect();
                Ok(Some((Image::new(Mode::LA, w, h, la), 0)))
            }));
        }
        if mode == Mode::One {
            candidates.push(Box::new(|| {
                let bits: Vec<u8> = rgba.data.chunks_exact(4).map(|p| if p[0] >= 128 { 255 } else { 0 }).collect();
                Ok(Some((Image::new(Mode::One, w, h, bits), 0)))
            }));
        }
        if mode == Mode::L {
            if let Some(Transparency::Int(k)) = key {
                let k = *k;
                candidates.push(Box::new(move || {
                    let mut g = gray();
                    g.transparency = Some(Transparency::Int(k));
                    Ok(Some((g, 0)))
                }));
            }
        }
        if !has_alpha {
            candidates.push(Box::new(|| Ok(Some((gray(), 0)))));
        }
    }
    if mode == Mode::RGB {
        if let Some(Transparency::Rgb(r, g, b)) = key {
            let t = Transparency::Rgb(*r, *g, *b);
            candidates.push(Box::new(move || {
                let mut out = rgb()?;
                out.transparency = Some(t.clone());
                Ok(Some((out, 0)))
            }));
        }
    }
    if !has_alpha {
        candidates.push(Box::new(|| Ok(Some((rgb()?, 0)))));
    }
    for make in candidates {
        let Some((out, n)) = make()? else { continue };
        let back = convert(&out, Mode::RGBA).map_err(|e| format!("{e:?}"))?;
        if back.data == rgba.data {
            return Ok((out, n));
        }
    }
    Ok((rgba.clone(), 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgba(px: &[[u8; 4]]) -> Image {
        Image::new(Mode::RGBA, px.len(), 1, px.iter().flatten().copied().collect())
    }

    #[test]
    fn modes_kept_where_the_pixels_fit() {
        let grey = rgba(&[[5, 5, 5, 255], [200, 200, 200, 255]]);
        let src = Source { mode: Mode::L, palette: None, transparency: None };
        assert_eq!(fit_mode(&grey, Some(&src), false).unwrap().0.mode, Mode::L);
        let coloured = rgba(&[[5, 6, 5, 255]]);
        assert_eq!(fit_mode(&coloured, Some(&src), false).unwrap().0.mode, Mode::RGB);
        let translucent = rgba(&[[5, 6, 5, 100]]);
        assert_eq!(fit_mode(&translucent, Some(&src), false).unwrap().0.mode, Mode::RGBA);
        let pal = [0u8, 0, 0, 255, 0, 0];
        let src = Source { mode: Mode::P, palette: Some(&pal), transparency: Some(Transparency::Int(0)) };
        let (out, n) = fit_mode(&rgba(&[[255, 0, 0, 255], [0, 0, 0, 0]]), Some(&src), false).unwrap();
        assert_eq!((out.mode, n, out.data), (Mode::P, 2, vec![1, 0]));
        assert_eq!(fit_mode(&rgba(&[[1, 0, 0, 255]]), Some(&src), false).unwrap().0.mode, Mode::RGBA);
        let src = Source { mode: Mode::RGB, palette: None, transparency: Some(Transparency::Rgb(1, 2, 3)) };
        let (out, _) = fit_mode(&rgba(&[[1, 2, 3, 0], [9, 9, 9, 255]]), Some(&src), false).unwrap();
        assert_eq!((out.mode, out.transparency), (Mode::RGB, Some(Transparency::Rgb(1, 2, 3))));
    }
}
