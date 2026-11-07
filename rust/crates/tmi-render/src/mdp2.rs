//! The MDP2 compositor: `render_composite_rgba` of the Python reader
//! (`tmiref/mdp2.py`), byte for byte.
//!
//! Every visible, renderable leaf layer (not a folder, not a mask) is decoded
//! into its painted rectangle (`_render_layer_buffer`), its alpha scaled by
//! `alpha` (`a.point(lambda v: v * alpha // 255)`, clipped to a byte) and
//! trimmed, moved by its offset and clipped to the canvas. A clipped layer is
//! then masked by the alpha of its base, the nearest earlier sibling that
//! does not clip (`ImageChops.multiply`), or vanishes without one. Last, in
//! file order, each lands on the canvas: Pillow's `alpha_composite` for
//! normal (and pass-through) layers, the `blend_rgba` kernel at opacity 1.0
//! for the rest. Folders are not composited as groups: their children are
//! drawn straight onto the canvas.

use std::collections::{HashMap, HashSet};

use rayon::prelude::*;
use tmi_clip::mdp2::{decode_layer, IntKey, Layer, Mdp2, Strictness};
use tmi_pillow::chops::multiply;
use tmi_pillow::over_pixel;

use crate::blend::{blend_pixel, BlendMode};
use crate::buffer::{intersect, Buf, Rect};

/// `_MDP_BLEND_TO_TMI`, then tmi_accel's name lookup; anything else blends
/// as normal.
pub fn blend_mode(mode: &str) -> BlendMode {
    let name = match mode {
        "mul" => "multiply",
        "add" | "add2" | "ldodge" => "linear_dodge",
        "sub" => "subtract",
        "div" => "divide",
        "overlay" => "overlay",
        "screen" => "screen",
        "light" => "lighten",
        "dark" => "darken",
        "diff" => "difference",
        "exclusion" => "exclusion",
        "dodge" => "color_dodge",
        "burn" => "color_burn",
        "lburn" => "linear_burn",
        "softlight" => "soft_light",
        "hardlight" => "hard_light",
        "hue" => "hue",
        "saturation" => "saturation",
        "color" => "color",
        "luminosity" => "luminosity",
        "hardmix" => "hard_mix",
        _ => "normal",
    };
    BlendMode::from_name(name).unwrap_or(BlendMode::Normal)
}

enum Rendered {
    /// Drawn but empty (still a clipping base).
    Empty,
    Buf(Buf),
}

/// `_effectively_visible` over the layers being drawn (`by_id` of that list).
fn effectively_visible(layers: &[&Layer], by_id: &HashMap<&IntKey, usize>, i: usize) -> bool {
    let mut seen = HashSet::new();
    let mut cur = Some(i);
    while let Some(k) = cur {
        let l = layers[k];
        if !l.visible || !seen.insert(&l.id_key) {
            return false;
        }
        cur = by_id.get(&l.parent_key).copied();
    }
    true
}

/// `_render_layer_buffer` with the opacity, then the offset and the canvas.
fn render_layer(doc: &Mdp2, raw: &[u8], index: usize, canvas: Rect) -> Result<Rendered, String> {
    let layer = &doc.layers[index];
    let px = decode_layer(doc, raw, index, Strictness::Buffer)?;
    let Some(r) = px.rect else { return Ok(Rendered::Empty) };
    let mut buf = Buf { x: r.0 as i64, y: r.1 as i64, w: r.2 - r.0, h: r.3 - r.1, px: px.rgba };
    if layer.alpha < 255 {
        if layer.alpha < i32::MIN as i64 {
            return Err("Python int too large to convert to C long".into());
        }
        let lut: Vec<u8> = (0..256i64).map(|v| (v * layer.alpha).div_euclid(255).clamp(0, 255) as u8).collect();
        for p in buf.px.chunks_exact_mut(4) {
            p[3] = lut[p[3] as usize];
        }
        match buf.trim() {
            Some(b) => buf = b,
            None => return Ok(Rendered::Empty),
        }
    }
    // Python's ints do not overflow; an offset that far out misses the
    // canvas (at most 2^31 wide) whatever its exact value.
    const FAR: i64 = 1 << 40;
    match (buf.x.checked_add(layer.offset_x), buf.y.checked_add(layer.offset_y)) {
        (Some(x), Some(y)) if x.abs() < FAR && y.abs() < FAR => {
            buf.x = x;
            buf.y = y;
        }
        _ => return Ok(Rendered::Empty),
    }
    Ok(match buf.clip_to(canvas) {
        Some(b) => Rendered::Buf(b),
        None => Rendered::Empty,
    })
}

/// `_mask_buffer_by_base_alpha`.
fn mask_by_base(source: &Buf, base: &Buf) -> Rendered {
    let Some(o) = intersect(source.bounds(), base.bounds()) else { return Rendered::Empty };
    let mut crop = source.crop(o);
    let alpha = base.alpha(o);
    for (p, &a) in crop.px.chunks_exact_mut(4).zip(&alpha) {
        p[3] = multiply(p[3], a);
    }
    match crop.trim() {
        Some(b) => Rendered::Buf(b),
        None => Rendered::Empty,
    }
}

/// Blend `src` onto the canvas (`_blend_buffer_onto_surface` at opacity 1).
fn blend_onto(canvas: &mut [u8], cw: usize, src: &Buf, mode: BlendMode) {
    let stride = cw * 4;
    let x0 = src.x as usize;
    let rows = &mut canvas[src.y as usize * stride..(src.y as usize + src.h) * stride];
    rows.par_chunks_mut(stride).enumerate().for_each(|(y, row)| {
        let dst = &mut row[x0 * 4..(x0 + src.w) * 4];
        let s = &src.px[y * src.w * 4..(y + 1) * src.w * 4];
        if matches!(mode, BlendMode::Normal) {
            for (d, p) in dst.chunks_exact_mut(4).zip(s.chunks_exact(4)) {
                over_pixel(d, p);
            }
        } else {
            for (d, p) in dst.chunks_exact_mut(4).zip(s.chunks_exact(4)) {
                let base = [d[0], d[1], d[2], d[3]];
                blend_pixel(d, &base, p, mode, 1.0);
            }
        }
    });
}

/// The canvas size as `Image.new` takes it.
pub fn canvas_size(doc: &Mdp2) -> Result<(usize, usize), String> {
    let (w, h) = (doc.canvas_width, doc.canvas_height);
    if w < 0 || h < 0 {
        return Err("Width and height must be >= 0".into());
    }
    if w > i32::MAX as i64 || h > i32::MAX as i64 {
        return Err("Python int too large to convert to C int".into());
    }
    (w as usize).checked_mul(h as usize).and_then(|p| p.checked_mul(4)).ok_or("image too large")?;
    Ok((w as usize, h as usize))
}

/// `render_composite_rgba(mdp, background)` of the layers `subset` (indices
/// into `doc.layers`, in order; all of them when None): the canvas RGBA.
pub fn render(doc: &Mdp2, raw: &[u8], background: [u8; 4], subset: Option<&[usize]>) -> Result<Vec<u8>, String> {
    let (cw, ch) = canvas_size(doc)?;
    let mut out = vec![0u8; cw * ch * 4];
    render_into(doc, raw, background, subset, &mut out)?;
    Ok(out)
}

/// `render` into `out` (the canvas' RGBA, `canvas_size`).
pub fn render_into(doc: &Mdp2, raw: &[u8], background: [u8; 4], subset: Option<&[usize]>, out: &mut [u8]) -> Result<(), String> {
    let (cw, ch) = canvas_size(doc)?;
    assert_eq!(out.len(), cw * ch * 4);
    let all: Vec<usize>;
    let order: &[usize] = match subset {
        Some(s) => s,
        None => {
            all = (0..doc.layers.len()).collect();
            &all
        }
    };
    let layers: Vec<&Layer> = order.iter().map(|&i| &doc.layers[i]).collect();
    let mut by_id = HashMap::new();
    for (k, l) in layers.iter().enumerate() {
        by_id.insert(&l.id_key, k);
    }
    let canvas: Rect = [0, 0, cw as i64, ch as i64];
    let drawn: Vec<bool> = (0..layers.len())
        .map(|k| {
            let l = layers[k];
            effectively_visible(&layers, &by_id, k) && l.children.is_empty() && !l.masking && l.renderable
        })
        .collect();
    let decoded: Vec<Result<Option<Rendered>, String>> = (0..layers.len())
        .into_par_iter()
        .map(|k| if drawn[k] { render_layer(doc, raw, order[k], canvas).map(Some) } else { Ok(None) })
        .collect();
    let mut rendered: Vec<Option<Rendered>> = Vec::with_capacity(layers.len());
    for d in decoded {
        rendered.push(d?);
    }
    for k in 0..layers.len() {
        if rendered[k].is_none() || !layers[k].clipping {
            continue;
        }
        let parent = &layers[k].parent_key;
        let base = (0..k).rev().find(|&j| &layers[j].parent_key == parent && !layers[j].clipping);
        let masked = match base.and_then(|j| rendered[j].as_ref()) {
            Some(Rendered::Buf(b)) => match &rendered[k] {
                Some(Rendered::Buf(s)) => mask_by_base(s, b),
                _ => Rendered::Empty,
            },
            _ => Rendered::Empty,
        };
        rendered[k] = Some(masked);
    }
    out.par_chunks_mut(4).for_each(|p| p.copy_from_slice(&background));
    for (k, r) in rendered.iter().enumerate() {
        if let Some(Rendered::Buf(b)) = r {
            blend_onto(out, cw, b, blend_mode(&layers[k].mode));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_map_like_python() {
        assert!(matches!(blend_mode("normal"), BlendMode::Normal));
        assert!(matches!(blend_mode("through"), BlendMode::Normal));
        assert!(matches!(blend_mode("inverse"), BlendMode::Normal));
        assert!(matches!(blend_mode("mul"), BlendMode::Multiply));
        assert!(matches!(blend_mode("add2"), BlendMode::LinearDodge));
        assert!(matches!(blend_mode("hardmix"), BlendMode::HardMix));
    }
}
