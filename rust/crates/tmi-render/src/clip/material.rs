//! 画像素材 (image material) layers (clip_style.py:172-369, stage 1c): CSP
//! saves no render for them, so the original (`ResizableOriginalMipmap`) is
//! resampled onto the canvas: `place_material` (premultiplied float32, the
//! float32 mip mean, `_bilinear`'s float32 x float64 products in float64,
//! stored back as float32) or, tiled, `_sample_tiled` (nearest, wrapping).
//! Every destination pixel is sampled from its own absolute coordinates, so
//! a window holds the whole canvas's bytes.

use std::sync::Arc;

use rayon::prelude::*;
use tmi_clip::attr::parse_offscreen_attribute;
use tmi_clip::model::{Col, Node};
use tmi_clip::tiles::{decode_offscreen_on, Offscreen};

use super::{memo, Ctx, Res};
use crate::buffer::{intersect, size, Buf, Layer, Rect};
use crate::style;

/// A material's original at its full offscreen size (`_full_offscreen`).
pub struct Original {
    pub w: usize,
    pub h: usize,
    pub px: Vec<u8>,
    pub profile: String,
}

/// Premultiplied float32 RGBA at a mip level, maybe with a 1-pixel border.
pub struct Plane {
    pub w: usize,
    pub h: usize,
    pub px: Vec<f32>,
}

/// `parse_resizable_image_info`: (tile, corners TL, TR, BL).
struct Placement {
    tile: bool,
    corners: [[f64; 2]; 3],
}

fn parse_placement(blob: &[u8]) -> Res<Placement> {
    if blob.len() < 96 {
        return Err("struct.error in ResizableImageInfo".into());
    }
    let u = |i: usize| u32::from_be_bytes(blob[i * 4..i * 4 + 4].try_into().unwrap());
    let tail = &blob[blob.len() - 64..];
    let d = |i: usize| f64::from_be_bytes(tail[i * 8..i * 8 + 8].try_into().unwrap());
    Ok(Placement { tile: u(4) != 0, corners: [[d(0), d(1)], [d(2), d(3)], [d(4), d(5)]] })
}

/// `np.clip(x, lo, hi)` (finite x).
#[inline]
fn clip(x: f64, lo: f64, hi: f64) -> f64 {
    let y = if x >= lo { x } else { lo };
    if y <= hi {
        y
    } else {
        hi
    }
}

const SAMPLE_EPS: f64 = 1e-4;

/// The largest image material original drawn when the canvas is smaller:
/// 2^27 pixels (11585 x 11585, a 100-megapixel photo with room to spare).
pub const MAX_ORIGINAL_PIXELS: u128 = 1 << 27;

/// `_mip_level`: the level whose scale stays above ½, and its plane.
fn mip_level(scale: f64, w: usize, h: usize) -> u32 {
    let (mut level, mut w, mut h) = (0u32, w, h);
    while level < 62 && scale * ((2u64 << level) as f64) <= 1.0 + 1e-9 && w.min(h) >= 2 {
        w /= 2;
        h /= 2;
        level += 1;
    }
    level
}

impl Original {
    /// `src.astype(np.float32)` premultiplied, halved `level` times, with a
    /// transparent 1-pixel border when `border`.
    pub fn plane(&self, level: u32, border: bool) -> Plane {
        let mut px: Vec<f32> = Vec::with_capacity(self.px.len());
        for p in self.px.chunks_exact(4) {
            let a = p[3] as f32;
            let k = a / 255.0f32;
            px.extend_from_slice(&[p[0] as f32 * k, p[1] as f32 * k, p[2] as f32 * k, a]);
        }
        let (mut w, mut h) = (self.w, self.h);
        for _ in 0..level {
            let (nw, nh) = (w / 2, h / 2);
            let mut next = vec![0f32; nw * nh * 4];
            for y in 0..nh {
                for x in 0..nw {
                    let at = |yy: usize, xx: usize| ((yy * w) + xx) * 4;
                    let (s00, s10, s01, s11) = (at(2 * y, 2 * x), at(2 * y + 1, 2 * x), at(2 * y, 2 * x + 1), at(2 * y + 1, 2 * x + 1));
                    for c in 0..4 {
                        next[(y * nw + x) * 4 + c] = (((px[s00 + c] + px[s10 + c]) + px[s01 + c]) + px[s11 + c]) * 0.25f32;
                    }
                }
            }
            px = next;
            (w, h) = (nw, nh);
        }
        if border {
            let (bw, bh) = (w + 2, h + 2);
            let mut padded = vec![0f32; bw * bh * 4];
            for y in 0..h {
                padded[((y + 1) * bw + 1) * 4..((y + 1) * bw + 1 + w) * 4].copy_from_slice(&px[y * w * 4..(y + 1) * w * 4]);
            }
            return Plane { w: bw, h: bh, px: padded };
        }
        Plane { w, h, px }
    }
}

/// `_bilinear` at one point: float64 (float32 samples times float64
/// weights), stored as float32.
#[inline]
fn bilinear(src: &Plane, u: f64, v: f64) -> [f32; 4] {
    let (w, h) = (src.w, src.h);
    let u = clip(u - SAMPLE_EPS, 0.0, (w - 1) as f64);
    let v = clip(v - SAMPLE_EPS, 0.0, (h - 1) as f64);
    let (i0, j0) = (u as i64, v as i64);
    // floor of a value in [0, 256): truncation (no libm call per pixel)
    let fu = (((u - i0 as f64) * 256.0) as i64) as f64 / 256.0;
    let fv = (((v - j0 as f64) * 256.0) as i64) as f64 / 256.0;
    let (i0, j0) = (i0 as usize, j0 as usize);
    let i1 = (i0 + 1).min(w - 1);
    let j1 = (j0 + 1).min(h - 1);
    let s = |j: usize, i: usize, c: usize| src.px[(j * w + i) * 4 + c] as f64;
    [0, 1, 2, 3].map(|c| {
        let top = s(j0, i0, c) * (1.0 - fu) + s(j0, i1, c) * fu;
        let bot = s(j1, i0, c) * (1.0 - fu) + s(j1, i1, c) * fu;
        (top * (1.0 - fv) + bot * fv) as f32
    })
}

/// `np.floor(x).astype(np.int64)` for |x| < 2^62, without a libm call.
#[inline]
fn floor_i64(x: f64) -> i64 {
    let t = x as i64;
    if (t as f64) > x {
        t - 1
    } else {
        t
    }
}

/// `np.round` of a point (half to even).
fn round2(p: [f64; 2]) -> [f64; 2] {
    p.map(f64::round_ties_even)
}

impl Ctx<'_> {
    /// `_material_original` of mipmap `mm`, once per render.
    fn material_original(&self, mm: i64) -> Res<Option<Arc<Original>>> {
        memo(&self.materials, &mm, || {
            if !self.doc.offscreens.contains_key(&mm) {
                return Err(format!("material mipmap {mm} was not read"));
            }
            let Some(info) = self.doc.offscreen(Some(mm)) else {
                return Ok(None);
            };
            let Some(exta) = self.doc.exta(info) else {
                return Ok(None);
            };
            // (`_full_offscreen` reads the attribute outside the decode's
            // try: Python drops the whole layer, warning about the layer.)
            let attr = parse_offscreen_attribute(&info.attribute).map_err(|e| format!("unreadable material attribute: {e}"))?;
            // The original is held whole (4 bytes a pixel, then 16 in each
            // plane), however little of it is painted: not the 64 canvases
            // a layer's offscreen may be, but the canvas or MAX_ORIGINAL_PIXELS.
            let pixels = attr.pixel_width as u128 * attr.pixel_height as u128;
            if pixels > (self.doc.width as u128 * self.doc.height as u128).max(MAX_ORIGINAL_PIXELS) {
                self.note_unreadable(info, format!("a material original of {}x{} pixels", attr.pixel_width, attr.pixel_height));
                return Ok(None);
            }
            if let Some(why) = tmi_clip::tiles::offscreen_oversize(attr.pixel_width, attr.pixel_height, self.doc.width as usize, self.doc.height as usize) {
                self.note_unreadable(info, why);
                return Ok(None);
            }
            // On this thread only (see `memo`). Unreadable tiles: warned
            // about, and the material has no original (no pixels).
            let decoded = decode_offscreen_on(
                self.raw,
                exta.payload_off,
                exta.payload_size,
                attr.tile_cols,
                attr.tile_rows,
                attr.pixel_width,
                attr.pixel_height,
                false,
            );
            let Offscreen { rgba: px, x, y, w, h, unknown_sizes } = match decoded {
                Ok(d) => d,
                Err(e) => {
                    self.note_unreadable(info, e);
                    return Ok(None);
                }
            };
            self.note_tile_sizes(&unknown_sizes);
            if w == 0 || h == 0 {
                return Ok(None);
            }
            let (fw, fh) = (attr.pixel_width, attr.pixel_height);
            let mut full = Buf::filled([0, 0, fw as i64, fh as i64], [0, 0, 0, 0]);
            full.paste(&Buf { x: x as i64, y: y as i64, w, h, px });
            Ok(Some(Arc::new(Original { w: fw, h: fh, px: full.px, profile: attr.channel_profile })))
        })
    }

    fn material_plane(&self, mm: i64, src: &Original, level: u32, border: bool) -> Arc<Plane> {
        memo(&self.planes, &(mm, level, border), || Arc::new(src.plane(level, border)))
    }

    /// `render_image_material` over `window` followed by the rest of
    /// `_render_layer_buffer` (window clip, 表現色 / レイヤーカラー).
    pub fn material_buffer(&self, node: &Node, window: Rect) -> Res<Layer> {
        let st = self.doc.style(node.main_id);
        let blob = st.get("ResizableImageInfo");
        let mm = st.get("ResizableOriginalMipmap");
        if !blob.truthy()? || !mm.truthy()? {
            return Ok(Layer::None);
        }
        let Col::Int(mm) = *mm else {
            return Err(format!("layer {}: material mipmap is not an integer", node.main_id));
        };
        let Some(src) = self.material_original(mm)? else {
            return Ok(Layer::None);
        };
        let place = parse_placement(blob.blob()?.unwrap_or_default()).map_err(|e| format!("layer {}: {e}", node.main_id))?;
        let off = [node.offset_x as f64, node.offset_y as f64];
        let corners = place.corners.map(|p| [p[0] + off[0], p[1] + off[1]]);
        if !corners.iter().flatten().all(|v| v.is_finite()) {
            return Err(format!("layer {}: material corners are not finite", node.main_id));
        }
        let mono = style::material_mono(&st);
        let mut buf = if place.tile {
            sample_tiled(&src, corners, window).map_err(|e| format!("layer {}: {e}", node.main_id))?
        } else {
            place_material(&src, corners, window, mono.is_some(), &|level, border| self.material_plane(mm, &src, level, border))
                .map_err(|e| format!("layer {}: {e}", node.main_id))?
        };
        if let (Some(m), Some(b)) = (&mono, buf.as_mut()) {
            style::binarise_material(b, m);
        }
        let Some(buf) = buf.and_then(Buf::trim) else {
            return Ok(Layer::Empty);
        };
        if !style::needs_profile(&st)? {
            return Ok(Layer::Buf(buf));
        }
        Ok(match style::apply_layer_style(&buf, &st, Some(&src.profile))? {
            Some(b) => Layer::Buf(b),
            None => Layer::Buf(buf),
        })
    }
}

/// `place_material(src, corners, window, mono=)`: None when off the window.
/// `plane(level, border)` gives the premultiplied original at a mip level.
/// `mono`: an axis-aligned placement stretches the mip level over the
/// whole-pixel rectangle floor(tl + ½) .. floor(br).
pub fn place_material(src: &Original, corners: [[f64; 2]; 3], window: Rect, mono: bool, plane: &dyn Fn(u32, bool) -> Arc<Plane>) -> Res<Option<Buf>> {
    let (sw, sh) = (src.w as f64, src.h as f64);
    let [mut tl, mut tr, mut bl] = corners;
    let mut ex = [(tr[0] - tl[0]) / sw, (tr[1] - tl[1]) / sw];
    let mut ey = [(bl[0] - tl[0]) / sh, (bl[1] - tl[1]) / sh];
    let axis = ex[1].abs() < 1e-6 && ey[0].abs() < 1e-6;
    let mut level_rect = None;
    if axis && mono {
        if !corners.iter().flatten().all(|v| v.abs() < 1e15) {
            return Err("material placement out of range".into());
        }
        let (x0, y0) = ((tl[0] + 0.5).floor(), (tl[1] + 0.5).floor());
        let (x1, y1) = (tr[0].floor(), bl[1].floor());
        if x1 <= x0 || y1 <= y0 {
            return Ok(None);
        }
        level_rect = Some([x0, y0, x1, y1]);
        (tl, tr, bl) = ([x0, y0], [x1, y0], [x0, y1]);
    } else if axis {
        (tl, tr, bl) = (round2(tl), round2(tr), round2(bl));
        ex = [(tr[0] - tl[0]) / sw, (tr[1] - tl[1]) / sw];
        ey = [(bl[0] - tl[0]) / sh, (bl[1] - tl[1]) / sh];
        if ex[0] == 0.0 || ey[1] == 0.0 {
            return Ok(None);
        }
    }
    let br = [tr[0] + bl[0] - tl[0], tr[1] + bl[1] - tl[1]];
    let pts = [tl, tr, bl, br];
    let lo = [0, 1].map(|c| pts.iter().map(|p| p[c]).fold(f64::INFINITY, f64::min));
    let hi = [0, 1].map(|c| pts.iter().map(|p| p[c]).fold(f64::NEG_INFINITY, f64::max));
    let (hx, hy) = (ex[0].hypot(ex[1]), ey[0].hypot(ey[1]));
    if !(hx.is_finite() && hy.is_finite() && lo.iter().chain(&hi).all(|v| v.abs() < 1e15)) {
        return Err("material placement out of range".into());
    }
    // Python's max(a, b) / min(a, b): the first unless the second wins.
    let pad = if axis { 0 } else { (0.5 * if hy > hx { hy } else { hx }).ceil() as i64 + 1 };
    let reach = [lo[0].floor() as i64 - pad, lo[1].floor() as i64 - pad, hi[0].ceil() as i64 + pad, hi[1].ceil() as i64 + pad];
    let Some(dst) = intersect(reach, window) else {
        return Ok(None);
    };
    let level = mip_level(if hy < hx { hy } else { hx }, src.w, src.h);
    let plane = plane(level, !axis);
    let f = (1u64 << level) as f64;
    if let Some([x0, y0, x1, y1]) = level_rect {
        // the level's floor(W / 2^k) x floor(H / 2^k) pixels span the rectangle
        ex = [(x1 - x0) / (plane.w as f64 * f), 0.0];
        ey = [0.0, (y1 - y0) / (plane.h as f64 * f)];
    }
    let mi = tmi_numpy::inv2([[ex[0] * f, ey[0] * f], [ex[1] * f, ey[1] * f]]).ok_or("singular material placement")?;
    let shift = if axis { 0.5 } else { -0.5 };
    let (w, h) = size(dst);
    let pxs: Vec<f64> = (dst[0]..dst[2]).map(|x| x as f64 + 0.5 - tl[0]).collect();
    let mut out = vec![0u8; w * h * 4];
    let bad = std::sync::atomic::AtomicBool::new(false);
    out.par_chunks_mut(w * 4).enumerate().for_each(|(row, o)| {
        let py = (dst[1] + row as i64) as f64 + 0.5 - tl[1];
        for (k, p) in o.chunks_exact_mut(4).enumerate() {
            let u = mi[0][0] * pxs[k] + mi[0][1] * py - shift;
            let v = mi[1][0] * pxs[k] + mi[1][1] * py - shift;
            if !(u.is_finite() && v.is_finite()) {
                bad.store(true, std::sync::atomic::Ordering::Relaxed);
                return;
            }
            let s = bilinear(&plane, u, v);
            let a = s[3];
            for c in 0..3 {
                let x = if a > 0.0 { s[c] * 255.0f32 / a.max(1e-6f32) } else { 0.0 };
                p[c] = tmi_numpy::round::rint_to_u8_fast(x);
            }
            p[3] = tmi_numpy::round::rint_to_u8_fast(a);
        }
    });
    if bad.into_inner() {
        return Err("material sample point is not finite".into());
    }
    Ok(Some(Buf { x: dst[0], y: dst[1], w, h, px: out }))
}

/// `_sample_tiled` over `window` (nearest, wrapping around the original).
pub fn sample_tiled(src: &Original, corners: [[f64; 2]; 3], window: Rect) -> Res<Option<Buf>> {
    let (sw, sh) = (src.w as f64, src.h as f64);
    let [tl, tr, bl] = corners;
    let ex = [(tr[0] - tl[0]) / sw, (tr[1] - tl[1]) / sw];
    let ey = [(bl[0] - tl[0]) / sh, (bl[1] - tl[1]) / sh];
    let mi = tmi_numpy::inv2([[ex[0], ey[0]], [ex[1], ey[1]]]).ok_or("singular material placement")?;
    let (w, h) = size(window);
    let xs: Vec<f64> = (window[0]..window[2]).map(|x| x as f64 - tl[0]).collect();
    let (iw, ih) = (src.w as i64, src.h as i64);
    let mut out = vec![0u8; w * h * 4];
    let bad = std::sync::atomic::AtomicBool::new(false);
    const LIMIT: f64 = (1u64 << 62) as f64;
    out.par_chunks_mut(w * 4).enumerate().for_each(|(row, o)| {
        let y = (window[1] + row as i64) as f64 - tl[1];
        for (k, p) in o.chunks_exact_mut(4).enumerate() {
            let u = mi[0][0] * xs[k] + mi[0][1] * y;
            let v = mi[1][0] * xs[k] + mi[1][1] * y;
            if !(u.abs() < LIMIT && v.abs() < LIMIT) {
                bad.store(true, std::sync::atomic::Ordering::Relaxed);
                return;
            }
            let ui = floor_i64(u).rem_euclid(iw) as usize;
            let vi = floor_i64(v).rem_euclid(ih) as usize;
            let s = (vi * src.w + ui) * 4;
            p.copy_from_slice(&src.px[s..s + 4]);
        }
    });
    if bad.into_inner() {
        return Err("material sample point out of range".into());
    }
    Ok(Some(Buf { x: window[0], y: window[1], w, h, px: out }))
}

#[cfg(test)]
mod tests {
    use tmi_clip::testing::{exta_payload, minimal_clip, offscreen_attribute, outer_id, tile_record};

    use crate::clip::band::{render, Target};
    use crate::clip::report::Note;

    /// An image material whose original declares 20000 x 20000 (1.6 GB held
    /// whole, 6.4 GB per plane) on a 13000 x 13000 canvas, with one painted
    /// 4 x 4 tile: within the 64 canvases a layer's offscreen may be, but
    /// left out (and warned about) before the original is allocated.
    #[test]
    fn a_material_original_declaring_gigapixels_is_left_out_before_allocating() {
        let outer = outer_id(5);
        let z = tmi_clip::zlib::compress(&[255u8; 16], -1);
        let exta = exta_payload(&outer, &[tile_record(0, 16, 4, 4, Some(&z))]);
        let hex: String = offscreen_attribute(20000, 20000, 79, 79).iter().map(|b| format!("{b:02X}")).collect();
        let sql = format!(
            "ALTER TABLE Layer ADD COLUMN ResizableOriginalMipmap; ALTER TABLE Layer ADD COLUMN ResizableImageInfo;
             INSERT INTO Layer(MainId, LayerName, LayerFolder, LayerVisibility, LayerOpacity, LayerNextIndex, ResizableOriginalMipmap, ResizableImageInfo)
                 VALUES (2, 'material', 0, 1, 256, 0, 5, X'01');
             INSERT INTO Mipmap VALUES (5, 5); INSERT INTO MipmapInfo VALUES (5, 5);
             INSERT INTO Offscreen VALUES (5, X'{hex}', '{outer}');"
        );
        let raw = minimal_clip(13000, 13000, &sql, vec![exta]).unwrap();
        let doc = tmi_clip::ClipDoc::open(&raw).unwrap();
        let mut out = vec![0u8; 4];
        let report = render(&doc, &raw, [255; 4], &Target::Region { window: [0, 0, 1, 1], decodable_all: false }, false, &mut out).unwrap();
        assert!(
            report.notes.iter().any(|n| matches!(n, Note::Offscreen { reason, .. } if reason.contains("material original"))),
            "{:?}",
            report.notes
        );
    }
}