//! PNG encode of the modes tmi writes back into a user's PNG (issue #21):
//! "1", "L", "LA", "P", "RGB" and "RGBA", with the chunks
//! `PngImagePlugin._save` (Pillow 12.3) would write and in its order:
//!
//! IHDR, the caller's chunks before the palette (iCCP, colour space, text),
//! PLTE, tRNS, the caller's chunks after it (pHYs, eXIf), IDAT, IEND.
//!
//! The palette and transparency follow `_save`: a "P" image is written with
//! 1, 2, 4 or 8 bits by its number of palette entries, PLTE holds those
//! entries, and `info["transparency"]` becomes tRNS (palette alphas cut to
//! the entries, a grey or RGB colour key as 16-bit values). The pixels are
//! deflated by mtpng (zlib level 6); the bytes are not Pillow's, which
//! nothing needs: this file goes to the user's working tree, not the store.

use mtpng::encoder::{Encoder, Options};
use mtpng::{ColorType, CompressionLevel, Header};
use tmi_pillow::image::{Image, Mode, Transparency};

const CHUNK_SIZE: usize = 256 * 1024;

fn io_err(e: std::io::Error) -> String {
    e.to_string()
}

/// Rows of `bits`-bit samples packed MSB first (one byte per pixel in).
fn pack(data: &[u8], w: usize, h: usize, bits: usize, sample: impl Fn(u8) -> u8) -> Vec<u8> {
    let row = (w * bits).div_ceil(8);
    let mut out = vec![0u8; row * h];
    for y in 0..h {
        for x in 0..w {
            let v = sample(data[y * w + x]);
            let bit = x * bits;
            out[y * row + bit / 8] |= v << (8 - bits - bit % 8);
        }
    }
    out
}

/// The PNG of `img`; `before` and `after` are (chunk type, data) written
/// before PLTE and after tRNS. `palette_len` is the number of entries of a
/// "P" image's palette (`len(im.palette.getdata()[1]) // 3`).
pub fn encode_png(img: &Image, palette_len: usize, before: &[(Vec<u8>, Vec<u8>)], after: &[(Vec<u8>, Vec<u8>)]) -> Result<Vec<u8>, String> {
    let (w, h) = (img.width, img.height);
    let mut colors = 0usize;
    let (colour, depth, rows): (ColorType, u8, std::borrow::Cow<[u8]>) = match img.mode {
        Mode::One => (ColorType::Greyscale, 1, pack(&img.data, w, h, 1, |v| (v != 0) as u8).into()),
        Mode::L => (ColorType::Greyscale, 8, (&img.data[..]).into()),
        Mode::LA => (ColorType::GreyscaleAlpha, 8, (&img.data[..]).into()),
        Mode::RGB => (ColorType::Truecolor, 8, (&img.data[..]).into()),
        Mode::RGBA => (ColorType::TruecolorAlpha, 8, (&img.data[..]).into()),
        Mode::P => {
            img.palette.as_ref().ok_or("a P image needs a palette")?;
            colors = palette_len.clamp(1, 256);
            let bits = match colors {
                0..=2 => 1,
                3..=4 => 2,
                5..=16 => 4,
                _ => 8,
            };
            let rows = if bits == 8 { (&img.data[..]).into() } else { pack(&img.data, w, h, bits, |v| v).into() };
            (ColorType::IndexedColor, bits as u8, rows)
        }
        m => return Err(format!("cannot write mode {} as PNG", m.name())),
    };
    let o16 = |v: i64| (v.clamp(0, 65535) as u16).to_be_bytes();
    let trns: Option<Vec<u8>> = match (&img.transparency, img.mode) {
        (None, _) => None,
        (Some(Transparency::Bytes(b)), Mode::P) => Some(b[..b.len().min(colors)].to_vec()),
        (Some(Transparency::Int(t)), Mode::P) => {
            let t = (*t).clamp(0, 255) as usize;
            let mut alpha = vec![0xffu8; t];
            alpha.push(0);
            alpha.truncate(colors);
            Some(alpha)
        }
        (Some(Transparency::Int(t)), Mode::One | Mode::L) => Some(o16(*t).to_vec()),
        (Some(Transparency::Rgb(r, g, b)), Mode::RGB) => Some([o16(*r), o16(*g), o16(*b)].concat()),
        (Some(_), m) => return Err(format!("cannot use transparency for mode {}", m.name())),
    };
    let palette: Option<Vec<u8>> = (img.mode == Mode::P).then(|| {
        let p = img.palette.as_ref().unwrap();
        p.entries.iter().take(colors).flat_map(|e| [e[0], e[1], e[2]]).collect()
    });
    if depth < 8 {
        // mtpng 0.4.1 rounds a row of less than whole bytes wrongly (it
        // tests `bits & 3`); these small images are written plainly.
        return Ok(plain_png(w, h, colour as u8, depth, &rows, before, palette.as_deref(), trns.as_deref(), after));
    }
    let mut header = Header::new();
    header.set_size(w as u32, h as u32).map_err(io_err)?;
    header.set_color(colour, depth).map_err(io_err)?;
    let mut options = Options::new();
    options.set_compression_level(CompressionLevel::Default).map_err(io_err)?;
    options.set_chunk_size(CHUNK_SIZE).map_err(io_err)?;
    // Not the caller's pool: this may run on its workers (tmi_codec::png_enc::pool).
    options.set_thread_pool(tmi_codec::png_enc::pool()).map_err(io_err)?;
    let mut enc = Encoder::new(Vec::with_capacity(rows.len() / 2 + 1024), &options);
    enc.write_header(&header).map_err(io_err)?;
    for (kind, data) in before {
        enc.write_chunk(kind, data).map_err(io_err)?;
    }
    if let Some(p) = &palette {
        enc.write_palette(p).map_err(io_err)?;
    }
    if let Some(t) = trns {
        enc.write_chunk(b"tRNS", &t).map_err(io_err)?;
    }
    for (kind, data) in after {
        enc.write_chunk(kind, data).map_err(io_err)?;
    }
    enc.write_image_rows(&rows).map_err(io_err)?;
    enc.finish().map_err(io_err)
}

fn chunk(out: &mut Vec<u8>, kind: &[u8], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let mut crc = flate2::Crc::new();
    crc.update(kind);
    crc.update(data);
    out.extend_from_slice(&crc.sum().to_be_bytes());
}

/// A PNG of packed rows (`rows`, whole bytes per row), every row filtered
/// with "None", deflated at level 6.
#[allow(clippy::too_many_arguments)]
fn plain_png(
    w: usize,
    h: usize,
    colour: u8,
    depth: u8,
    rows: &[u8],
    before: &[(Vec<u8>, Vec<u8>)],
    palette: Option<&[u8]>,
    trns: Option<&[u8]>,
    after: &[(Vec<u8>, Vec<u8>)],
) -> Vec<u8> {
    use std::io::Write;
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&(w as u32).to_be_bytes());
    ihdr.extend_from_slice(&(h as u32).to_be_bytes());
    ihdr.extend_from_slice(&[depth, colour, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    for (kind, data) in before {
        chunk(&mut out, kind, data);
    }
    if let Some(p) = palette {
        chunk(&mut out, b"PLTE", p);
    }
    if let Some(t) = trns {
        chunk(&mut out, b"tRNS", t);
    }
    for (kind, data) in after {
        chunk(&mut out, kind, data);
    }
    let stride = if h == 0 { 0 } else { rows.len() / h };
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::new(6));
    for row in rows.chunks(stride.max(1)) {
        z.write_all(&[0]).unwrap();
        z.write_all(row).unwrap();
    }
    let idat = z.finish().unwrap();
    chunk(&mut out, b"IDAT", &idat);
    chunk(&mut out, b"IEND", &[]);
    out
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::png_dec::decode_png;
    use tmi_pillow::image::Palette;

    #[test]
    fn modes_round_trip_through_the_decoder() {
        let (w, h) = (13usize, 5usize);
        let gray: Vec<u8> = (0..w * h).map(|i| (i * 37 % 256) as u8).collect();
        for (mode, bpp) in [(Mode::L, 1), (Mode::LA, 2), (Mode::RGB, 3), (Mode::RGBA, 4)] {
            let data: Vec<u8> = (0..w * h * bpp).map(|i| (i * 11 % 256) as u8).collect();
            let img = Image::new(mode, w, h, data.clone());
            let back = decode_png(&encode_png(&img, 0, &[], &[]).unwrap()).unwrap().image;
            assert_eq!((back.mode, back.data), (mode, data));
        }
        let ones: Vec<u8> = gray.iter().map(|&v| if v >= 128 { 255 } else { 0 }).collect();
        let back = decode_png(&encode_png(&Image::new(Mode::One, w, h, ones.clone()), 0, &[], &[]).unwrap()).unwrap().image;
        assert_eq!((back.mode, back.data), (Mode::One, ones));
        for n in [2usize, 3, 9, 200] {
            let idx: Vec<u8> = (0..w * h).map(|i| (i % n) as u8).collect();
            let mut img = Image::new(Mode::P, w, h, idx.clone());
            let rgb: Vec<u8> = (0..n * 3).map(|i| (i * 7) as u8).collect();
            img.palette = Some(Palette::from_rgb(&rgb));
            img.transparency = Some(Transparency::Int(1));
            let back = decode_png(&encode_png(&img, n, &[], &[]).unwrap()).unwrap().image;
            assert_eq!(back.data, idx, "{n}");
            assert_eq!(back.transparency, Some(Transparency::Int(1)));
            assert_eq!(&back.palette.unwrap().entries[..n], &img.palette.as_ref().unwrap().entries[..n]);
        }
    }
}
