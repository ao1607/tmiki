//! A Pillow image as the decoders hand it over: mode, size, the pixels as
//! `tobytes()` lays them out (except mode "1": one byte per pixel, 0 or
//! 255, as Pillow stores it), the palette of a "P" image and
//! `info["transparency"]`.

/// The modes tmiki meets (what Pillow's PNG and JPEG decoders produce, plus
/// the premultiplied and integer modes the conversions pass through).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Mode {
    /// "1": one byte per pixel here, 0 or 255.
    One,
    L,
    LA,
    La,
    P,
    RGB,
    RGBA,
    RGBa,
    CMYK,
    /// "I": i32, little-endian.
    I,
    /// "I;16": u16, little-endian.
    I16,
    /// "I;16B": u16, big-endian.
    I16B,
}

impl Mode {
    pub fn name(self) -> &'static str {
        match self {
            Mode::One => "1",
            Mode::L => "L",
            Mode::LA => "LA",
            Mode::La => "La",
            Mode::P => "P",
            Mode::RGB => "RGB",
            Mode::RGBA => "RGBA",
            Mode::RGBa => "RGBa",
            Mode::CMYK => "CMYK",
            Mode::I => "I",
            Mode::I16 => "I;16",
            Mode::I16B => "I;16B",
        }
    }

    pub fn from_name(s: &str) -> Option<Mode> {
        Some(match s {
            "1" => Mode::One,
            "L" => Mode::L,
            "LA" => Mode::LA,
            "La" => Mode::La,
            "P" => Mode::P,
            "RGB" => Mode::RGB,
            "RGBA" => Mode::RGBA,
            "RGBa" => Mode::RGBa,
            "CMYK" => Mode::CMYK,
            "I" => Mode::I,
            "I;16" | "I;16L" => Mode::I16,
            "I;16B" => Mode::I16B,
            _ => return None,
        })
    }

    /// Bytes per pixel in `Image::data`.
    pub fn bytes_per_pixel(self) -> usize {
        match self {
            Mode::One | Mode::L | Mode::P => 1,
            Mode::LA | Mode::La | Mode::I16 | Mode::I16B => 2,
            Mode::RGB => 3,
            Mode::RGBA | Mode::RGBa | Mode::CMYK | Mode::I => 4,
        }
    }

    /// Whether the mode is 8 bits per band (what resample / reduce /
    /// blend / chops take).
    pub fn is_u8_bands(self) -> bool {
        !matches!(self, Mode::I | Mode::I16 | Mode::I16B | Mode::P | Mode::One)
    }
}

/// `info["transparency"]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Transparency {
    /// An int: a palette index ("P"), or a grey value ("1", "L", "I;16").
    Int(i64),
    /// An (r, g, b) tuple ("RGB").
    Rgb(i64, i64, i64),
    /// One alpha per palette entry ("P").
    Bytes(Vec<u8>),
}

/// A Pillow palette: 256 RGBA entries (Pillow's `ImagingPalette`), the
/// unset ones (0, 0, 0, 255).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Palette {
    pub entries: Vec<[u8; 4]>,
}

impl Palette {
    /// `putpalette("RGB", data)`: the first `len / 3` entries from `rgb`.
    pub fn from_rgb(rgb: &[u8]) -> Palette {
        let mut entries = vec![[0, 0, 0, 255]; 256];
        for (e, c) in entries.iter_mut().zip(rgb.chunks_exact(3)) {
            *e = [c[0], c[1], c[2], 255];
        }
        Palette { entries }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Image {
    pub mode: Mode,
    pub width: usize,
    pub height: usize,
    pub data: Vec<u8>,
    pub palette: Option<Palette>,
    pub transparency: Option<Transparency>,
}

impl Image {
    pub fn new(mode: Mode, width: usize, height: usize, data: Vec<u8>) -> Image {
        assert_eq!(data.len(), width * height * mode.bytes_per_pixel(), "{} {}x{}", mode.name(), width, height);
        Image { mode, width, height, data, palette: None, transparency: None }
    }

    /// `tobytes()` (mode "1" packed 8 pixels to a byte, MSB first, rows
    /// padded to whole bytes).
    pub fn tobytes(&self) -> Vec<u8> {
        if self.mode != Mode::One {
            return self.data.clone();
        }
        let row = self.width.div_ceil(8);
        let mut out = vec![0u8; row * self.height];
        for y in 0..self.height {
            for x in 0..self.width {
                if self.data[y * self.width + x] != 0 {
                    out[y * row + x / 8] |= 0x80 >> (x % 8);
                }
            }
        }
        out
    }

    /// An image of mode "1" from `frombytes("1", ...)` data (bits, MSB
    /// first, rows padded to whole bytes).
    pub fn from_bits(width: usize, height: usize, bits: &[u8]) -> Image {
        let row = width.div_ceil(8);
        let mut data = vec![0u8; width * height];
        for y in 0..height {
            for x in 0..width {
                if bits[y * row + x / 8] & (0x80 >> (x % 8)) != 0 {
                    data[y * width + x] = 255;
                }
            }
        }
        Image::new(Mode::One, width, height, data)
    }
}
