//! 表現色 / レイヤーカラー (`clip_style.apply_layer_style`, clip_style.py:73-167),
//! in numpy's float32 order: the luma is `r*0.299 + g*0.587 + b*0.114` with
//! the weights as float32, the mix `main*(1-t) + sub*t`, then `rint`.

use tmi_clip::model::{Col, Style};

use crate::buffer::Buf;

/// `_u32_to_u8` of a style column (`value or 0`).
pub fn u32_to_u8(v: &Col) -> Result<u8, String> {
    let v = v.int_or0()? as i128;
    let x = (v * 255 + 0x7FFF_FFFF).div_euclid(0xFFFF_FFFF).min(255);
    if x < 0 {
        return Err("negative colour value".into());
    }
    Ok(x as u8)
}

#[derive(Clone, Copy, Debug)]
pub struct LayerColour {
    pub main: [u8; 3],
    pub sub: [u8; 3],
}

/// `layer_colour`: (main, sub) when レイヤーカラー is on (not on the paper).
pub fn layer_colour(style: &Style) -> Result<Option<LayerColour>, String> {
    if !style.get("DrawColorEnable").truthy()? || *style.get("LayerType") == Col::Int(1584) {
        return Ok(None);
    }
    if let Col::Odd = style.get("LayerType") {
        return Err("LayerType is not an integer".into());
    }
    let main = [
        u32_to_u8(style.get("DrawColorMainRed"))?,
        u32_to_u8(style.get("DrawColorMainGreen"))?,
        u32_to_u8(style.get("DrawColorMainBlue"))?,
    ];
    let sub = if *style.get("DrawColorSubRed") == Col::Null {
        [255, 255, 255]
    } else {
        [
            u32_to_u8(style.get("DrawColorSubRed"))?,
            u32_to_u8(style.get("DrawColorSubGreen"))?,
            u32_to_u8(style.get("DrawColorSubBlue"))?,
        ]
    };
    Ok(Some(LayerColour { main, sub }))
}

pub fn colour_type(style: &Style) -> Result<i64, String> {
    style.get("LayerColorTypeIndex").int_or0()
}

/// `needs_profile`.
pub fn needs_profile(style: &Style) -> Result<bool, String> {
    Ok(layer_colour(style)?.is_some() || matches!(colour_type(style)?, 1 | 2))
}

#[inline]
fn luma(r: f32, g: f32, b: f32) -> f32 {
    r * 0.299f32 + g * 0.587f32 + b * 0.114f32
}

#[inline]
fn mix(lc: &LayerColour, rgb: [f32; 3]) -> [f32; 3] {
    let t = luma(rgb[0], rgb[1], rgb[2]) / 255.0f32;
    let k = 1.0f32 - t;
    [0, 1, 2].map(|c| lc.main[c] as f32 * k + lc.sub[c] as f32 * t)
}

/// `material_mono_conversion`: the モノクロ conversion an image material is
/// drawn with (`UsePreviewColorType` with `PreviewColorTypeIndex` = 2), or
/// None (Python's `except`: any value it cannot read).
pub struct MaterialMono {
    pub black: bool,
    pub white: bool,
    pub threshold_image: u8,
    pub threshold_alpha: u8,
}

pub fn material_mono(style: &Style) -> Option<MaterialMono> {
    let used = style.get("UsePreviewColorType").truthy().ok()?;
    if !used || *style.get("PreviewColorTypeIndex") != Col::Int(MONO) {
        return None;
    }
    let thr = |name: &str| match style.get(name) {
        Col::Int(_) => u32_to_u8(style.get(name)).ok(),
        _ => None,
    };
    Some(MaterialMono {
        black: style.get("PreviewColorTypeBlackChecked").truthy().ok()?,
        white: style.get("PreviewColorTypeWhiteChecked").truthy().ok()?,
        threshold_image: thr("PreviewColorTypeThresholdImage")?,
        threshold_alpha: thr("PreviewColorTypeThresholdAlpha")?,
    })
}

/// `_binarise_material`: alpha cut to 0 / 255 at the alpha threshold, the
/// colour to black / white at the image threshold (an unticked colour dropped).
pub fn binarise_material(buf: &mut Buf, m: &MaterialMono) {
    let edge = (m.threshold_image as f64 - 0.5) as f32;
    for p in buf.px.chunks_exact_mut(4) {
        let keep = p[3] >= m.threshold_alpha;
        let white = luma(p[0] as f32, p[1] as f32, p[2] as f32) >= edge;
        let drop = if white { !m.white } else { !m.black };
        p[3] = if keep && !drop { 255 } else { 0 };
        let c = if white { 255 } else { 0 };
        p[0] = c;
        p[1] = c;
        p[2] = c;
    }
}

/// LayerColorTypeIndex (`clip_style.GRAY` / `MONO`): 0 colour, 1 グレー, 2 モノクロ.
pub const GRAY: i64 = 1;
pub const MONO: i64 = 2;
/// `clip_style.MONO_ALPHA_THRESHOLD`: モノクロ draws without antialiasing.
pub const MONO_ALPHA_THRESHOLD: u8 = 128;

/// The colour steps of `_style_colours` for one pixel: (RGB, alpha factor,
/// 255 = unchanged).
fn style_pixel(rgb: [u8; 3], ct: i64, black: bool, white: bool, lc: Option<&LayerColour>) -> ([u8; 3], u32) {
    let mut v = rgb.map(|c| c as f32);
    let mut keep = 255u32;
    if ct == GRAY {
        let l = luma(v[0], v[1], v[2]);
        if black && !white {
            v = [0.0; 3];
            keep = tmi_numpy::rint_to_u8(255.0f32 - l) as u32;
        } else if white && !black {
            v = [255.0; 3];
            keep = tmi_numpy::rint_to_u8(l) as u32;
        } else {
            v = [l, l, l];
        }
    } else if ct == MONO {
        let is_white = luma(v[0], v[1], v[2]) >= 127.5f32;
        let x = if is_white { 255.0 } else { 0.0 };
        v = [x, x, x];
        let drop = (is_white && !white) || (!is_white && !black);
        keep = if drop { 0 } else { 255 };
    }
    if let Some(lc) = lc {
        v = mix(lc, v);
    }
    (v.map(tmi_numpy::rint_to_u8), keep)
}

#[inline]
fn scale_alpha(a: u8, keep: u32) -> u8 {
    ((a as u32 * keep + 127) / 255) as u8
}

/// `apply_layer_style`: None when nothing applies (the image is unchanged).
pub fn apply_layer_style(buf: &Buf, style: &Style, profile: Option<&str>) -> Result<Option<Buf>, String> {
    let ct = colour_type(style)?;
    let black = style.get("LayerColorTypeBlackChecked").truthy()?;
    let white = style.get("LayerColorTypeWhiteChecked").truthy()?;
    let lc = layer_colour(style)?;
    let single = profile == Some("single-channel");
    if lc.is_none() && !matches!(ct, GRAY | MONO) {
        return Ok(None);
    }
    let mut out = buf.clone();
    if ct == MONO && !single {
        for p in out.px.chunks_exact_mut(4) {
            p[3] = if p[3] >= MONO_ALPHA_THRESHOLD { 255 } else { 0 };
        }
    }
    if matches!(ct, 1 | 2) && single {
        let x = if black || !white { 0.0f32 } else { 255.0f32 };
        let mut v = [x, x, x];
        if let Some(lc) = &lc {
            v = mix(lc, v);
        }
        let rgb = v.map(tmi_numpy::rint_to_u8);
        for p in out.px.chunks_exact_mut(4) {
            p[..3].copy_from_slice(&rgb);
        }
        return Ok(Some(out));
    }
    // A pure function of the colour: cache it (paintings hold few colours).
    let mut cache: std::collections::HashMap<[u8; 3], ([u8; 3], u32)> = std::collections::HashMap::new();
    for p in out.px.chunks_exact_mut(4) {
        let key = [p[0], p[1], p[2]];
        let (rgb, keep) = *cache
            .entry(key)
            .or_insert_with(|| style_pixel(key, ct, black, white, lc.as_ref()));
        p[..3].copy_from_slice(&rgb);
        if keep != 255 {
            p[3] = scale_alpha(p[3], keep);
        }
    }
    Ok(Some(out))
}

/// One pixel through `apply_layer_style` (for the differential tests).
pub fn style_one(rgb: [u8; 3], ct: i64, black: bool, white: bool, lc: Option<LayerColour>, profile: Option<&str>) -> ([u8; 3], u32) {
    let single = profile == Some("single-channel");
    if matches!(ct, 1 | 2) && single {
        let x = if black || !white { 0.0f32 } else { 255.0f32 };
        let mut v = [x, x, x];
        if let Some(lc) = &lc {
            v = mix(lc, v);
        }
        return (v.map(tmi_numpy::rint_to_u8), 255);
    }
    style_pixel(rgb, ct, black, white, lc.as_ref())
}
