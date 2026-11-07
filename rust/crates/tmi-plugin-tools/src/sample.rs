//! The made-up "tmi sample" format the templates read (their SPEC.md
//! describes it): files for `tests/corpus/`, made with tmi's own PNG
//! encoder so that every build of tmi writes the same bytes.

pub const MAGIC: &[u8; 8] = b"TMISMPL1";

pub struct Layer {
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub visible: bool,
    pub opacity: u8,
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

impl Layer {
    /// A `width` x `height` layer filled with `rgba`.
    pub fn filled(name: &str, x: i32, y: i32, width: usize, height: usize, rgba: [u8; 4]) -> Layer {
        Layer { name: name.into(), x, y, visible: true, opacity: 255, width, height, rgba: rgba.repeat(width * height) }
    }
}

/// A sample file of `layers` (bottom to top) on a `width` x `height` canvas.
pub fn file(width: u32, height: u32, layers: &[Layer]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&width.to_le_bytes());
    out.extend_from_slice(&height.to_le_bytes());
    out.extend_from_slice(&(layers.len() as u32).to_le_bytes());
    for l in layers {
        let png = tmi_codec::png_enc::encode(&l.rgba, l.width as u32, l.height as u32, None).expect("encode a png");
        out.extend_from_slice(&(l.name.len() as u16).to_le_bytes());
        out.extend_from_slice(l.name.as_bytes());
        out.extend_from_slice(&l.x.to_le_bytes());
        out.extend_from_slice(&l.y.to_le_bytes());
        out.push(u8::from(l.visible));
        out.push(l.opacity);
        out.extend_from_slice(&(png.len() as u32).to_le_bytes());
        out.extend_from_slice(&png);
    }
    out
}

/// The corpus file `new` writes: an opaque background, a half-opaque
/// layer with a gradient at an offset, and a hidden layer.
pub fn corpus_file() -> Vec<u8> {
    let background = Layer::filled("background", 0, 0, 24, 16, [235, 235, 228, 255]);
    let mut mark = Layer::filled("mark", 4, 3, 10, 6, [0, 0, 0, 0]);
    for y in 0..6 {
        for x in 0..10 {
            let p = (y * 10 + x) * 4;
            mark.rgba[p..p + 4].copy_from_slice(&[200, (x * 25) as u8, (y * 40) as u8, 255]);
        }
    }
    mark.opacity = 128;
    let mut hidden = Layer::filled("hidden", 14, 8, 6, 6, [30, 60, 220, 255]);
    hidden.visible = false;
    file(24, 16, &[background, mark, hidden])
}
