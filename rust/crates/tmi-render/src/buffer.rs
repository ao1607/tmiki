//! RGBA buffers with an origin (`_LayerBuffer`, clip.py:199-222, 319-376).

use tmi_pillow::bbox::alpha_bbox;

/// (x0, y0, x1, y1) in canvas pixels.
pub type Rect = [i64; 4];

pub fn intersect(a: Rect, b: Rect) -> Option<Rect> {
    let r = [a[0].max(b[0]), a[1].max(b[1]), a[2].min(b[2]), a[3].min(b[3])];
    if r[0] >= r[2] || r[1] >= r[3] {
        None
    } else {
        Some(r)
    }
}

pub fn union(a: Rect, b: Rect) -> Rect {
    [a[0].min(b[0]), a[1].min(b[1]), a[2].max(b[2]), a[3].max(b[3])]
}

pub fn size(r: Rect) -> (usize, usize) {
    ((r[2] - r[0]) as usize, (r[3] - r[1]) as usize)
}

/// A straight-alpha RGBA image placed at (x, y).
#[derive(Clone, Debug)]
pub struct Buf {
    pub x: i64,
    pub y: i64,
    pub w: usize,
    pub h: usize,
    pub px: Vec<u8>,
}

impl Buf {
    pub fn filled(r: Rect, rgba: [u8; 4]) -> Buf {
        let (w, h) = size(r);
        let mut px = vec![0u8; w * h * 4];
        if rgba != [0, 0, 0, 0] {
            for p in px.chunks_exact_mut(4) {
                p.copy_from_slice(&rgba);
            }
        }
        Buf { x: r[0], y: r[1], w, h, px }
    }

    pub fn bounds(&self) -> Rect {
        [self.x, self.y, self.x + self.w as i64, self.y + self.h as i64]
    }

    /// The pixels of `r` (canvas coordinates, inside `bounds`).
    pub fn crop(&self, r: Rect) -> Buf {
        let (w, h) = size(r);
        let (sx, sy) = ((r[0] - self.x) as usize, (r[1] - self.y) as usize);
        let mut px = Vec::with_capacity(w * h * 4);
        for y in 0..h {
            let s = ((sy + y) * self.w + sx) * 4;
            px.extend_from_slice(&self.px[s..s + w * 4]);
        }
        Buf { x: r[0], y: r[1], w, h, px }
    }

    /// One row segment `[x0, x1)` of row `y` (canvas coordinates).
    #[inline]
    pub fn row(&self, y: i64, x0: i64, x1: i64) -> &[u8] {
        let s = (((y - self.y) as usize) * self.w + (x0 - self.x) as usize) * 4;
        &self.px[s..s + ((x1 - x0) as usize) * 4]
    }

    #[inline]
    pub fn row_mut(&mut self, y: i64, x0: i64, x1: i64) -> &mut [u8] {
        let s = (((y - self.y) as usize) * self.w + (x0 - self.x) as usize) * 4;
        &mut self.px[s..s + ((x1 - x0) as usize) * 4]
    }

    /// `_clip_buffer_to_bounds`: None outside `r`; the buffer itself when it
    /// lies inside.
    pub fn clip_to(self, r: Rect) -> Option<Buf> {
        let c = intersect(self.bounds(), r)?;
        if c == self.bounds() {
            Some(self)
        } else {
            Some(self.crop(c))
        }
    }

    /// `_trim_buffer`: cropped to its alpha bbox, None when transparent.
    pub fn trim(self) -> Option<Buf> {
        let (x0, y0, x1, y1) = alpha_bbox(&self.px, self.w)?;
        if (x0, y0, x1, y1) == (0, 0, self.w, self.h) {
            return Some(self);
        }
        let r = [self.x + x0 as i64, self.y + y0 as i64, self.x + x1 as i64, self.y + y1 as i64];
        Some(self.crop(r))
    }

    /// `alpha_composite` of `self` onto a transparent `r`-sized canvas
    /// (`r` holds the overlap): pixels with alpha 0 become (0, 0, 0, 0).
    pub fn onto_transparent(&self, r: Rect) -> Buf {
        let mut out = Buf::filled(r, [0, 0, 0, 0]);
        if let Some(o) = intersect(self.bounds(), r) {
            for y in o[1]..o[3] {
                let src = self.row(y, o[0], o[2]);
                let dst = out.row_mut(y, o[0], o[2]);
                for (d, s) in dst.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
                    if s[3] != 0 {
                        d.copy_from_slice(s);
                    }
                }
            }
        }
        out
    }

    /// Copy `src`'s pixels into `self` where they overlap (`Image.paste`).
    pub fn paste(&mut self, src: &Buf) {
        if let Some(o) = intersect(self.bounds(), src.bounds()) {
            for y in o[1]..o[3] {
                let n = ((o[2] - o[0]) * 4) as usize;
                let s = (((y - src.y) as usize) * src.w + (o[0] - src.x) as usize) * 4;
                let d = (((y - self.y) as usize) * self.w + (o[0] - self.x) as usize) * 4;
                self.px[d..d + n].copy_from_slice(&src.px[s..s + n]);
            }
        }
    }

    /// The alpha plane of `r` (inside `bounds`).
    pub fn alpha(&self, r: Rect) -> Vec<u8> {
        let (w, h) = size(r);
        let mut out = Vec::with_capacity(w * h);
        for y in r[1]..r[3] {
            out.extend(self.row(y, r[0], r[2]).chunks_exact(4).map(|p| p[3]));
        }
        out
    }
}

/// A rendered sibling: `None` (nothing to draw), `Empty` (drawn, but fully
/// transparent: still a clipping base) or pixels.
#[derive(Clone, Debug)]
pub enum Layer {
    None,
    Empty,
    Buf(Buf),
}

impl Layer {
    pub fn from_trimmed(b: Option<Buf>) -> Layer {
        match b {
            Some(b) => Layer::Buf(b),
            None => Layer::Empty,
        }
    }
}

/// A compositing target (`_CompositeSurface`): `canvas` is the window drawn,
/// `full` the document canvas; a fixed surface always holds its whole window.
pub struct Surface {
    pub canvas: Rect,
    pub full: Rect,
    pub img: Option<Buf>,
    pub fixed: bool,
}

impl Surface {
    pub fn fixed(canvas: Rect, full: Rect, background: [u8; 4]) -> Surface {
        Surface { canvas, full, img: Some(Buf::filled(canvas, background)), fixed: true }
    }

    pub fn empty(canvas: Rect, full: Rect) -> Surface {
        Surface { canvas, full, img: None, fixed: false }
    }

    pub fn bounds(&self) -> Option<Rect> {
        self.img.as_ref().map(Buf::bounds)
    }

    pub fn trim(&mut self) {
        if self.fixed {
            return;
        }
        if let Some(img) = self.img.take() {
            self.img = img.trim();
        }
    }
}
