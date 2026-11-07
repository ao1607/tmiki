//! The generic compositor `generic-v1` (PLUGINS.md 2.2 and P4): what the
//! host draws for a format that names `composite::render(generic-v1)`.
//! It is the MDP2 compositor (`mdp2.rs`) made format-neutral: its input is
//! a layer tree (the WIT `structure`'s layers: pre-order, siblings bottom
//! to top) with each layer's decoded pixels, not an MDP2 document.
//!
//! The definition (fixed for the name `generic-v1`; every step is in 8-bit
//! RGBA, straight alpha):
//!
//! * The canvas starts as the background colour (white for a composite).
//!   The root layers are drawn onto it bottom to top.
//! * A layer with children is a group. A group whose mode is
//!   `pass-through` has no picture of its own: its children are drawn
//!   onto a copy of the backdrop the group itself would be drawn onto, in
//!   their order, at the group's place, giving R. Its opacity fades that
//!   back toward the backdrop B (as Photoshop does; user decision
//!   2026-10-17), mixed premultiplied: with `o8` as below, `wb = Ba * (255 -
//!   o8)` and `wr = Ra * o8`, alpha becomes `(wb + wr) // 255` and each colour
//!   `(Bc * wb + Rc * wr) // (wb + wr)` (`(Bc * (255 - o8) + Rc * o8) // 255`
//!   when `wb + wr = 0`; `fade_pixel`). Monotone in `o8`; `o8 = 255` gives R,
//!   `o8 = 0` gives B; over an opaque backdrop (R opaque too) it is the
//!   straight `(B * (255 - o8) + R * o8) // 255`. Its mode and
//!   clipping flag have no effect, and it is never a clipping base; layers
//!   clipped inside it are cut by their bases there, before the fade. Any
//!   other group is
//!   isolated: its children are drawn onto a transparent picture, which is
//!   then drawn like a layer's pixels (opacity, clipping, mode). The pixels
//!   of a layer that has children are not drawn.
//! * A layer without children is drawn from its pixels, placed at
//!   `(x + offset-x, y + offset-y)` and cut to the canvas. `mask`, `stencil`
//!   and `adjustment` layers are never drawn.
//! * A hidden layer is not drawn, and neither is anything inside a hidden
//!   group.
//! * Opacity: `o8 = round(opacity * 255)` (half away from zero); every
//!   alpha becomes `a * o8 // 255`.
//! * Clipping: a layer with `clip-to-below` is cut by its base, the nearest
//!   earlier sibling (same parent) without the flag, whether or not that
//!   one is drawn: its alpha becomes `a * b // 255` (`ImageChops.multiply`)
//!   with `b` the base's alpha after the base's own opacity (0 outside the
//!   base). Without a base, or when the base is not drawn (hidden, a mask,
//!   a pass-through group, no pixels), the layer is not drawn.
//! * Modes: `normal` (and `pass-through` / `other` on a layer without
//!   children) is Pillow's `alpha_composite` (`tmi_pillow::over_pixel`);
//!   every other mode is `blend::blend_pixel` at opacity 1.0: the W3C
//!   compositing formula `Co = (1 - ab) Cs + ab B(Cb, Cs)` mixed over the
//!   backdrop by `as`, in float32 on bytes / 255, truncated back to bytes.
//!   `glow-add` and `glow-dodge` are `linear-dodge` and `color-dodge`
//!   (CLIP STUDIO PAINT's own glow formulas are not part of generic-v1).
//!   The separable and non-separable B(Cb, Cs) are listed in PLUGINS.md P4.
//!
//! On every MDP2 file whose own compositor fits this definition (see
//! PLUGINS.md P4 for what does not), the result equals `mdp2::render` byte
//! for byte; `tmi-format`'s tests check it.

use std::borrow::Cow;

use rayon::prelude::*;
use tmi_pillow::chops::multiply;
use tmi_pillow::over_pixel;

use crate::blend::{blend_pixel, BlendMode};
use crate::buffer::{intersect, union, Rect};

/// The `renderer` a generic-v1 composite's `tmi-composite` text names.
pub const RENDERER: &str = "generic/v1";

/// Deeper layer trees are refused (the compositor recurses per level).
pub const MAX_DEPTH: usize = 512;

/// How a layer lands on its backdrop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Pillow's `alpha_composite`.
    Normal,
    /// A group without a picture of its own (a layer without children: normal).
    PassThrough,
    /// `blend_pixel` with this mode at opacity 1.0.
    Blend(BlendMode),
}

impl Mode {
    /// A WIT `blend-mode` by its record name (`types.rs` `BlendMode::name`).
    /// `other` and names generic-v1 does not know draw as `normal`.
    pub fn from_name(name: &str) -> Mode {
        match name {
            "normal" | "other" => Mode::Normal,
            "pass_through" => Mode::PassThrough,
            "glow_add" => Mode::Blend(BlendMode::LinearDodge),
            "glow_dodge" => Mode::Blend(BlendMode::ColorDodge),
            n => match BlendMode::from_name(n) {
                Some(BlendMode::Normal) | None => Mode::Normal,
                Some(m) => Mode::Blend(m),
            },
        }
    }
}

/// A layer's decoded pixels: the rectangle `(x, y, w, h)` of its bitmap
/// (bitmap coordinates, before the layer offset) and that rectangle's RGBA.
#[derive(Clone, Copy, Debug)]
pub struct Pixels<'a> {
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
    pub rgba: &'a [u8],
}

/// One layer of the tree, in the structure's order.
#[derive(Clone, Copy, Debug)]
pub struct Node<'a> {
    /// The parent's index, which must be smaller than this layer's.
    pub parent: Option<usize>,
    pub visible: bool,
    /// 0..1.
    pub opacity: f64,
    pub mode: Mode,
    pub clip: bool,
    /// False for `mask`, `stencil` and `adjustment` layers.
    pub drawable: bool,
    pub offset: (i64, i64),
    /// None: nothing painted (or no pixels at all).
    pub pixels: Option<Pixels<'a>>,
}

/// `o8 = round(opacity * 255)`.
pub fn opacity8(opacity: f64) -> u8 {
    if opacity.is_nan() {
        return 255;
    }
    (opacity.clamp(0.0, 1.0) * 255.0).round() as u8
}

/// A picture placed on the canvas.
struct Img<'a> {
    x: i64,
    y: i64,
    w: usize,
    h: usize,
    px: Cow<'a, [u8]>,
}

impl Img<'_> {
    fn bounds(&self) -> Rect {
        [self.x, self.y, self.x + self.w as i64, self.y + self.h as i64]
    }

    fn crop(&self, r: Rect) -> Img<'static> {
        let (w, h) = ((r[2] - r[0]) as usize, (r[3] - r[1]) as usize);
        let (sx, sy) = ((r[0] - self.x) as usize, (r[1] - self.y) as usize);
        let mut px = Vec::with_capacity(w * h * 4);
        for y in 0..h {
            let s = ((sy + y) * self.w + sx) * 4;
            px.extend_from_slice(&self.px[s..s + w * 4]);
        }
        Img { x: r[0], y: r[1], w, h, px: Cow::Owned(px) }
    }

    fn row(&self, y: i64, x0: i64, x1: i64) -> &[u8] {
        let s = (((y - self.y) as usize) * self.w + (x0 - self.x) as usize) * 4;
        &self.px[s..s + ((x1 - x0) as usize) * 4]
    }

    /// Every alpha through `a * o8 // 255`.
    fn fade(&mut self, o8: u8) {
        if o8 == 255 {
            return;
        }
        let lut: Vec<u8> = (0..256u32).map(|v| (v * o8 as u32 / 255) as u8).collect();
        self.px.to_mut().par_chunks_mut(4 * 1024).for_each(|c| {
            for p in c.chunks_exact_mut(4) {
                p[3] = lut[p[3] as usize];
            }
        });
    }
}

/// A sibling, prepared for drawing.
enum Prep<'a> {
    /// Not drawn (hidden, a mask, no base to clip to).
    None,
    /// Drawn, but nothing painted.
    Empty,
    Draw(Img<'a>, Mode),
    /// A pass-through group's children and the group's `o8` (1..=255).
    Through(Vec<Prep<'a>>, u8),
}

struct Tree<'n, 'a> {
    nodes: &'n [Node<'a>],
    /// `kids[i]`: the children of layer i; `kids[nodes.len()]`: the roots.
    kids: Vec<Vec<usize>>,
    canvas: Rect,
}

/// `src`'s alpha multiplied by `base`'s (0 outside it).
fn clip_to_base(src: &Img, base: &Img) -> Option<Img<'static>> {
    let o = intersect(src.bounds(), base.bounds())?;
    let mut out = src.crop(o);
    let w = out.w;
    out.px.to_mut().par_chunks_mut(w * 4).enumerate().for_each(|(dy, row)| {
        let b = base.row(o[1] + dy as i64, o[0], o[2]);
        for (p, q) in row.chunks_exact_mut(4).zip(b.chunks_exact(4)) {
            p[3] = multiply(p[3], q[3]);
        }
    });
    Some(out)
}

fn bounds_of(items: &[Prep]) -> Option<Rect> {
    let mut r: Option<Rect> = None;
    for it in items {
        let b = match it {
            Prep::Draw(img, _) => Some(img.bounds()),
            Prep::Through(sub, _) => bounds_of(sub),
            Prep::None | Prep::Empty => None,
        };
        if let Some(b) = b {
            r = Some(r.map_or(b, |a| union(a, b)));
        }
    }
    r
}

/// A drawing target: RGBA rows covering `rect`.
struct Target<'t> {
    rect: Rect,
    px: &'t mut [u8],
}

fn blend_onto(t: &mut Target, img: &Img, mode: Mode) {
    let Some(o) = intersect(t.rect, img.bounds()) else { return };
    let tw = (t.rect[2] - t.rect[0]) as usize;
    let stride = tw * 4;
    let (y0, y1) = ((o[1] - t.rect[1]) as usize, (o[3] - t.rect[1]) as usize);
    let x0 = (o[0] - t.rect[0]) as usize;
    let n = (o[2] - o[0]) as usize;
    t.px[y0 * stride..y1 * stride].par_chunks_mut(stride).enumerate().for_each(|(dy, row)| {
        let dst = &mut row[x0 * 4..(x0 + n) * 4];
        let src = img.row(o[1] + dy as i64, o[0], o[2]);
        match mode {
            Mode::Normal | Mode::PassThrough => {
                for (d, p) in dst.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
                    over_pixel(d, p);
                }
            }
            Mode::Blend(m) => {
                for (d, p) in dst.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
                    let base = [d[0], d[1], d[2], d[3]];
                    blend_pixel(d, &base, p, m, 1.0);
                }
            }
        }
    });
}

/// A pass-through group below full opacity: its children drawn onto a copy
/// of the backdrop (R), then every pixel `fade_pixel(B, R, o8)`.
fn draw_faded(t: &mut Target, sub: &[Prep], o8: u8) {
    let Some(r) = bounds_of(sub).and_then(|b| intersect(b, t.rect)) else { return };
    let stride = ((t.rect[2] - t.rect[0]) as usize) * 4;
    let (y0, y1) = ((r[1] - t.rect[1]) as usize, (r[3] - t.rect[1]) as usize);
    let (x0, n) = ((r[0] - t.rect[0]) as usize * 4, (r[2] - r[0]) as usize * 4);
    let mut copy = Vec::with_capacity((y1 - y0) * n);
    for y in y0..y1 {
        copy.extend_from_slice(&t.px[y * stride + x0..y * stride + x0 + n]);
    }
    draw(&mut Target { rect: r, px: &mut copy }, sub);
    t.px[y0 * stride..y1 * stride].par_chunks_mut(stride).zip(copy.par_chunks(n)).for_each(|(row, res)| {
        for (b, r) in row[x0..x0 + n].chunks_exact_mut(4).zip(res.chunks_exact(4)) {
            fade_pixel(b, r, o8);
        }
    });
}

/// `b` faded toward `r` by `o8`, mixed premultiplied (PLUGINS.md P4):
/// with `wb = Ba * (255 - o8)` and `wr = Ra * o8`, alpha is
/// `(wb + wr) // 255` and each colour `(Bc * wb + Rc * wr) // (wb + wr)`:
/// the premultiplied colours lerped by the same weights and divided by the
/// unrounded alpha, truncating as Pillow's `rgbA2rgba` does (`255 * v // a`),
/// but on the exact sums, not on 8-bit premultiplied values (which would lose
/// `o8 = 255`). When `wb + wr = 0` the colour is `(Bc * (255 - o8) + Rc * o8) // 255`.
pub fn fade_pixel(b: &mut [u8], r: &[u8], o8: u8) {
    let (o, p) = (o8 as u32, 255 - o8 as u32);
    let (wb, wr) = (b[3] as u32 * p, r[3] as u32 * o);
    let s = wb + wr;
    for c in 0..3 {
        b[c] = if s == 0 { (b[c] as u32 * p + r[c] as u32 * o) / 255 } else { (b[c] as u32 * wb + r[c] as u32 * wr) / s } as u8;
    }
    b[3] = (s / 255) as u8;
}

fn draw(t: &mut Target, items: &[Prep]) {
    for it in items {
        match it {
            Prep::Draw(img, mode) => blend_onto(t, img, *mode),
            Prep::Through(sub, 255) => draw(t, sub),
            Prep::Through(sub, o8) => draw_faded(t, sub, *o8),
            Prep::None | Prep::Empty => {}
        }
    }
}

impl<'a> Tree<'_, 'a> {
    fn children(&self, slot: usize) -> Vec<Prep<'a>> {
        let ks = &self.kids[slot];
        let mut items: Vec<Prep<'a>> = ks.par_iter().map(|&k| self.prepare(k)).collect();
        for pos in 0..ks.len() {
            if !self.nodes[ks[pos]].clip || matches!(items[pos], Prep::None | Prep::Through(..)) {
                continue;
            }
            let base = (0..pos).rev().find(|&j| !self.nodes[ks[j]].clip);
            let masked = match (base.map(|j| &items[j]), &items[pos]) {
                (Some(Prep::Draw(b, _)), Prep::Draw(s, mode)) => match clip_to_base(s, b) {
                    Some(img) => Prep::Draw(img, *mode),
                    None => Prep::Empty,
                },
                (Some(Prep::Draw(..)), _) => Prep::Empty,
                _ => Prep::None,
            };
            items[pos] = masked;
        }
        items
    }

    fn prepare(&self, k: usize) -> Prep<'a> {
        let n = &self.nodes[k];
        if !n.visible {
            return Prep::None;
        }
        if !self.kids[k].is_empty() {
            let items = self.children(k);
            if n.mode == Mode::PassThrough {
                // at opacity 0 the fade gives the backdrop back, so nothing is drawn
                return match opacity8(n.opacity) {
                    0 => Prep::Empty,
                    o8 => Prep::Through(items, o8),
                };
            }
            let Some(r) = bounds_of(&items) else { return Prep::Empty };
            let (w, h) = ((r[2] - r[0]) as usize, (r[3] - r[1]) as usize);
            let mut px = vec![0u8; w * h * 4];
            draw(&mut Target { rect: r, px: &mut px }, &items);
            let mut img = Img { x: r[0], y: r[1], w, h, px: Cow::Owned(px) };
            img.fade(opacity8(n.opacity));
            return Prep::Draw(img, n.mode);
        }
        if !n.drawable {
            return Prep::None;
        }
        let Some(p) = n.pixels else { return Prep::Empty };
        if p.w == 0 || p.h == 0 {
            return Prep::Empty;
        }
        // An offset this far out misses any canvas (at most 2^31 wide).
        const FAR: i128 = 1 << 40;
        let (x, y) = (p.x as i128 + n.offset.0 as i128, p.y as i128 + n.offset.1 as i128);
        if x.abs() >= FAR || y.abs() >= FAR {
            return Prep::Empty;
        }
        let img = Img { x: x as i64, y: y as i64, w: p.w, h: p.h, px: Cow::Borrowed(p.rgba) };
        let Some(c) = intersect(img.bounds(), self.canvas) else { return Prep::Empty };
        let o8 = opacity8(n.opacity);
        if o8 == 0 {
            return Prep::Empty;
        }
        let mut img = if c == img.bounds() { img } else { img.crop(c) };
        img.fade(o8);
        Prep::Draw(img, n.mode)
    }
}

/// The composite of `nodes` on a `width` x `height` canvas filled with
/// `background`: its RGBA rows.
pub fn render(nodes: &[Node], width: usize, height: usize, background: [u8; 4]) -> Result<Vec<u8>, String> {
    let len = width.checked_mul(height).and_then(|p| p.checked_mul(4)).ok_or("image too large")?;
    let mut kids = vec![Vec::new(); nodes.len() + 1];
    let mut depth = vec![0usize; nodes.len()];
    for (i, n) in nodes.iter().enumerate() {
        match n.parent {
            Some(p) if p >= i => return Err(format!("layer {i}: its parent {p} is not before it")),
            Some(p) => {
                depth[i] = depth[p] + 1;
                if depth[i] > MAX_DEPTH {
                    return Err(format!("the layer tree is deeper than {MAX_DEPTH}"));
                }
                kids[p].push(i);
            }
            None => kids[nodes.len()].push(i),
        }
        if let Some(p) = &n.pixels {
            if p.w.checked_mul(p.h).and_then(|a| a.checked_mul(4)) != Some(p.rgba.len()) {
                return Err(format!("layer {i}: {} bytes for {}x{} pixels", p.rgba.len(), p.w, p.h));
            }
        }
    }
    let tree = Tree { nodes, kids, canvas: [0, 0, width as i64, height as i64] };
    let mut out = vec![0u8; len];
    out.par_chunks_mut(4 * 1024).for_each(|c| {
        for p in c.chunks_exact_mut(4) {
            p.copy_from_slice(&background);
        }
    });
    let roots = tree.children(nodes.len());
    if width > 0 && height > 0 {
        draw(&mut Target { rect: tree.canvas, px: &mut out }, &roots);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lum(c: [f64; 3]) -> f64 {
        0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
    }

    /// B(Cb, Cs) of the modes MDP2 does not use, straight from their
    /// definitions (PLUGINS.md P4), in f64.
    fn reference(mode: &str, cb: [f64; 3], cs: [f64; 3]) -> [f64; 3] {
        let burn = |b: f64, s: f64| if b >= 1.0 { 1.0 } else if s <= 0.0 { 0.0 } else { 1.0 - ((1.0 - b) / s).min(1.0) };
        let dodge = |b: f64, s: f64| if b <= 0.0 { 0.0 } else if s >= 1.0 { 1.0 } else { (b / (1.0 - s)).min(1.0) };
        let sep = |f: &dyn Fn(f64, f64) -> f64| [f(cb[0], cs[0]), f(cb[1], cs[1]), f(cb[2], cs[2])];
        match mode {
            "linear_light" => sep(&|b, s| (b + 2.0 * s - 1.0).clamp(0.0, 1.0)),
            "vivid_light" => sep(&|b, s| if s < 0.5 { burn(b, 2.0 * s) } else { dodge(b, 2.0 * s - 1.0) }),
            "pin_light" => sep(&|b, s| if s < 0.5 { b.min(2.0 * s) } else { b.max(2.0 * s - 1.0) }),
            "darker_color" => if lum(cb) <= lum(cs) { cb } else { cs },
            "lighter_color" => if lum(cb) >= lum(cs) { cb } else { cs },
            "glow_add" | "linear_dodge" => sep(&|b, s| (b + s).min(1.0)),
            "glow_dodge" | "color_dodge" => sep(&dodge),
            _ => unreachable!(),
        }
    }

    #[test]
    fn the_modes_mdp2_does_not_use_follow_their_definitions() {
        // every (backdrop, source) pair of a coarse grid, the source at
        // several alphas, one pixel each, over an opaque backdrop
        let vals: Vec<u8> = (0..=255).step_by(15).collect();
        let mut back = Vec::new();
        let mut src = Vec::new();
        for &b in &vals {
            for &s in &vals {
                for &a in &[255u8, 200, 77, 1] {
                    back.extend_from_slice(&[b, 255 - b, b / 2, 255]);
                    src.extend_from_slice(&[s, s / 3, 255 - s, a]);
                }
            }
        }
        let n = back.len() / 4;
        for mode in ["linear_light", "vivid_light", "pin_light", "darker_color", "lighter_color", "glow_add", "glow_dodge"] {
            let nodes = [
                Node { parent: None, visible: true, opacity: 1.0, mode: Mode::Normal, clip: false, drawable: true, offset: (0, 0), pixels: Some(Pixels { x: 0, y: 0, w: n, h: 1, rgba: &back }) },
                Node { parent: None, visible: true, opacity: 1.0, mode: Mode::from_name(mode), clip: false, drawable: true, offset: (0, 0), pixels: Some(Pixels { x: 0, y: 0, w: n, h: 1, rgba: &src }) },
            ];
            let out = render(&nodes, n, 1, [0, 0, 0, 255]).unwrap();
            for i in 0..n {
                let (b, s, o) = (&back[i * 4..i * 4 + 4], &src[i * 4..i * 4 + 4], &out[i * 4..i * 4 + 4]);
                let f = |p: &[u8]| [p[0] as f64 / 255.0, p[1] as f64 / 255.0, p[2] as f64 / 255.0];
                let (cb, cs) = (f(b), f(s));
                let bl = reference(mode, cb, cs);
                let a = s[3] as f64 / 255.0;
                for c in 0..3 {
                    let want = ((1.0 - a) * cb[c] + a * bl[c]) * 255.0;
                    // truncated from float32: at most one below, never above
                    assert!(o[c] as f64 <= want + 1e-6 && o[c] as f64 > want - 1.0 - 1e-6, "{mode}: backdrop {b:?} source {s:?}: {o:?}, want {want} in channel {c}");
                }
                assert_eq!(o[3], 255);
            }
        }
    }
}
