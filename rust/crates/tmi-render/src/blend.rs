//! Blend modes of _blend_pixels (tmi/formats/clip.py), operation for
//! operation in numpy's float32 order.

use rayon::prelude::*;

const EPSILON: f32 = 1e-6;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlendMode {
    Normal,
    Multiply,
    Screen,
    Darken,
    Lighten,
    Difference,
    Subtract,
    LinearDodge,
    LinearBurn,
    ColorDodge,
    ColorBurn,
    Overlay,
    HardLight,
    SoftLight,
    Exclusion,
    Divide,
    DarkerColor,
    LighterColor,
    Hue,
    Saturation,
    Color,
    Luminosity,
    HardMix,
    LinearLight,
    VividLight,
    PinLight,
    GlowAdd,
    GlowDodge,
}

impl BlendMode {
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "normal" | "pass_through" => Some(Self::Normal),
            "multiply" => Some(Self::Multiply),
            "screen" => Some(Self::Screen),
            "darken" => Some(Self::Darken),
            "lighten" => Some(Self::Lighten),
            "difference" => Some(Self::Difference),
            "subtract" => Some(Self::Subtract),
            "linear_dodge" => Some(Self::LinearDodge),
            "linear_burn" => Some(Self::LinearBurn),
            "color_dodge" => Some(Self::ColorDodge),
            "color_burn" => Some(Self::ColorBurn),
            "overlay" => Some(Self::Overlay),
            "hard_light" => Some(Self::HardLight),
            "soft_light" => Some(Self::SoftLight),
            "exclusion" => Some(Self::Exclusion),
            "divide" => Some(Self::Divide),
            "darker_color" => Some(Self::DarkerColor),
            "lighter_color" => Some(Self::LighterColor),
            "hue" => Some(Self::Hue),
            "saturation" => Some(Self::Saturation),
            "color" => Some(Self::Color),
            "luminosity" => Some(Self::Luminosity),
            "hard_mix" => Some(Self::HardMix),
            "linear_light" => Some(Self::LinearLight),
            "vivid_light" => Some(Self::VividLight),
            "pin_light" => Some(Self::PinLight),
            "glow_add" => Some(Self::GlowAdd),
            "glow_dodge" => Some(Self::GlowDodge),
            _ => None,
        }
    }
}

/// The modes `BlendMode::from_name` knows, in the order tmi_accel reports them.
pub const SUPPORTED_BLEND_MODES: &[&str] = &[
    "normal",
    "pass_through",
    "multiply",
    "screen",
    "darken",
    "lighten",
    "difference",
    "subtract",
    "linear_dodge",
    "linear_burn",
    "color_dodge",
    "color_burn",
    "overlay",
    "hard_light",
    "soft_light",
    "exclusion",
    "divide",
    "darker_color",
    "lighter_color",
    "hue",
    "saturation",
    "color",
    "luminosity",
    "hard_mix",
    "linear_light",
    "vivid_light",
    "pin_light",
    "glow_add",
    "glow_dodge",
];

/// `blend_pixel` over whole rows: `out`, `target` and `source` are RGBA
/// images `width` pixels wide (the same length).
pub fn blend_rgba(out: &mut [u8], target: &[u8], source: &[u8], width: usize, mode: BlendMode, opacity: f32) {
    let row_stride = (width * 4).max(4);
    out.par_chunks_mut(row_stride)
        .zip(target.par_chunks(row_stride))
        .zip(source.par_chunks(row_stride))
        .for_each(|((out_row, target_row), source_row)| {
            for ((o, t), s) in out_row.chunks_exact_mut(4).zip(target_row.chunks_exact(4)).zip(source_row.chunks_exact(4)) {
                blend_pixel(o, t, s, mode, opacity);
            }
        });
}

#[inline]
pub fn blend_pixel(out: &mut [u8], base: &[u8], src: &[u8], mode: BlendMode, opacity: f32) {
    // A fully transparent source pixel leaves the backdrop exactly as it was
    // (the maths below would truncate αb·Cb/αo a hair under Cb).
    if ((src[3] as f32) / 255.0) * opacity == 0.0 {
        out.copy_from_slice(base);
        return;
    }
    if matches!(mode, BlendMode::HardMix) {
        blend_pixel_hard_mix(out, base, src, opacity);
        return;
    }
    if matches!(mode, BlendMode::GlowAdd | BlendMode::GlowDodge) {
        blend_pixel_glow(out, base, src, matches!(mode, BlendMode::GlowDodge), opacity);
        return;
    }

    let cb = [
        (base[0] as f32) / 255.0,
        (base[1] as f32) / 255.0,
        (base[2] as f32) / 255.0,
    ];
    let cs = [
        (src[0] as f32) / 255.0,
        (src[1] as f32) / 255.0,
        (src[2] as f32) / 255.0,
    ];
    let alpha_b = (base[3] as f32) / 255.0;
    let alpha_s = ((src[3] as f32) / 255.0) * opacity;

    let bc = blend_rgb(mode, cb, cs).map(clip01);
    let mut co = [0.0f32; 3];
    let alpha_o = alpha_s + alpha_b * (1.0 - alpha_s);
    let safe_alpha_o = if alpha_o > 0.0 { alpha_o } else { 1.0 };

    for channel in 0..3 {
        let cs_eff = (1.0 - alpha_b) * cs[channel] + alpha_b * bc[channel];
        co[channel] = (alpha_s * cs_eff + (1.0 - alpha_s) * alpha_b * cb[channel]) / safe_alpha_o;
        if !(alpha_o > 0.0) {
            co[channel] = 0.0;
        }
    }

    out[0] = float_to_u8(co[0]);
    out[1] = float_to_u8(co[1]);
    out[2] = float_to_u8(co[2]);
    out[3] = float_to_u8(alpha_o);
}

/// CLIP STUDIO PAINT's compositing (`_blend_pixels(csp=True)` in
/// tmiref/clip.py), fitted to its full-size PNG export (research
/// clip-format.md §15.7): the layer opacity (0..256) is folded into the
/// source alpha in 8 bit, `a' = (a * opacity) >> 8`; the blended colour B is
/// rounded to 8 bit (not for multiply); the result is rounded instead of
/// truncated. 加算(発光) rounds too; 覆い焼き(発光) keeps truncating.
#[inline]
pub fn blend_pixel_clip(out: &mut [u8], base: &[u8], src: &[u8], mode: BlendMode, opacity: f32) {
    let op256 = (opacity * 256.0).round() as u32;
    let a8 = (src[3] as u32 * op256) >> 8;
    // multiply's [ref > s] correction applies even where a' rounds to 0
    // (seen over an opaque backdrop only)
    if a8 == 0 && !(matches!(mode, BlendMode::Multiply) && src[3] > 0 && base[3] == 255) {
        out.copy_from_slice(base);
        return;
    }
    let alpha_s = a8 as f32 / 255.0;
    if matches!(mode, BlendMode::HardMix) {
        blend_pixel_hard_mix_clip(out, base, src, alpha_s, a8);
        return;
    }
    if matches!(mode, BlendMode::GlowAdd | BlendMode::GlowDodge) {
        let dodge = matches!(mode, BlendMode::GlowDodge);
        let alpha_b = (base[3] as f32) / 255.0;
        let alpha_o = alpha_s + alpha_b * (1.0 - alpha_s);
        let safe_alpha_o = if alpha_o > 0.0 { alpha_o } else { 1.0 };
        for channel in 0..3 {
            let cb = (base[channel] as f32) / 255.0;
            let x = ((src[channel] as f32) / 255.0) * alpha_s;
            let g = if dodge {
                if x >= 1.0 {
                    1.0
                } else {
                    1.0f32.min(cb / (1.0 - x).max(EPSILON))
                }
            } else {
                1.0f32.min(cb + x)
            };
            let co = if alpha_o > 0.0 {
                (alpha_b * g + (1.0 - alpha_b) * x) / safe_alpha_o
            } else {
                0.0
            };
            out[channel] = if dodge { float_to_u8(co) } else { float_to_u8_round(co) };
        }
        out[3] = float_to_u8(alpha_o);
        return;
    }

    let cb = [
        (base[0] as f32) / 255.0,
        (base[1] as f32) / 255.0,
        (base[2] as f32) / 255.0,
    ];
    let cs = [
        (src[0] as f32) / 255.0,
        (src[1] as f32) / 255.0,
        (src[2] as f32) / 255.0,
    ];
    let alpha_b = (base[3] as f32) / 255.0;
    let mut bc = blend_rgb(mode, cb, cs).map(clip01);
    if matches!(mode, BlendMode::Overlay | BlendMode::HardLight | BlendMode::Divide) {
        for c in 0..3 {
            let (b, s) = (base[c] as i64, src[c] as i64);
            let v = match mode {
                BlendMode::Overlay => overlay_int(b, s),
                BlendMode::HardLight => overlay_int(s, b),
                _ => divide_int(b, s),
            };
            bc[c] = ((v as f64) / 255.0) as f32;
        }
    }
    if !matches!(mode, BlendMode::Multiply) {
        bc = bc.map(|v| (v * 255.0 + 0.5).floor() / 255.0);
    }
    let alpha_o = alpha_s + alpha_b * (1.0 - alpha_s);
    let safe_alpha_o = if alpha_o > 0.0 { alpha_o } else { 1.0 };
    for channel in 0..3 {
        let cs_eff = (1.0 - alpha_b) * cs[channel] + alpha_b * bc[channel];
        let mut co = (alpha_s * cs_eff + (1.0 - alpha_s) * alpha_b * cb[channel]) / safe_alpha_o;
        if !(alpha_o > 0.0) {
            co = 0.0;
        }
        out[channel] = float_to_u8_round(co);
    }
    if base[3] == 255 && !matches!(mode, BlendMode::Multiply) {
        // over an opaque backdrop CSP adds round((B - b) * a' / 255) to b,
        // halves away from zero: exactly, in integers
        for c in 0..3 {
            let b = base[c] as i64;
            let bi = ((bc[c] as f64) * 255.0 + 0.5).floor() as i64;
            let num = (bi - b) * a8 as i64;
            let r = b + num.signum() * ((2 * num.abs() + 255) / 510);
            out[c] = r.clamp(0, 255) as u8;
        }
    }
    if matches!(mode, BlendMode::Multiply) {
        // CSP's multiply: the unrounded product mixed by a' and rounded once
        // (exactly, in integers, over an opaque backdrop), then one lower
        // wherever that lands above the source byte
        for c in 0..3 {
            let s = src[c] as i64;
            let mut r = out[c] as i64;
            if base[3] == 255 {
                let (b, a1) = (base[c] as i64, a8 as i64);
                r = (b * (255 - a1) * 255 + b * s * a1 + 32512) / 65025;
            }
            if r > s {
                r -= 1;
            }
            out[c] = r.clamp(0, 255) as u8;
        }
    }
    out[3] = float_to_u8(alpha_o);
}

#[inline]
fn blend_pixel_hard_mix_clip(out: &mut [u8], base: &[u8], src: &[u8], alpha_s: f32, a8: u32) {
    let cb = [
        (base[0] as f32) / 255.0,
        (base[1] as f32) / 255.0,
        (base[2] as f32) / 255.0,
    ];
    let cs = [
        (src[0] as f32) / 255.0,
        (src[1] as f32) / 255.0,
        (src[2] as f32) / 255.0,
    ];
    let alpha_b = (base[3] as f32) / 255.0;
    let alpha_o = alpha_s + alpha_b * (1.0 - alpha_s);
    let safe_alpha_o = if alpha_o > 0.0 { alpha_o } else { 1.0 };
    for channel in 0..3 {
        let bc = if cb[channel] + cs[channel] < 1.0 {
            0.0f64
        } else {
            1.0f64
        };
        let cs_eff = (1.0f64 - alpha_b as f64) * cs[channel] as f64 + alpha_b as f64 * bc;
        let term2 = (1.0 - alpha_s) * alpha_b * cb[channel];
        let mut co = (alpha_s as f64 * cs_eff + term2 as f64) / safe_alpha_o as f64;
        if !(alpha_o > 0.0) {
            co = 0.0;
        }
        let scaled = (co * 255.0 + 0.5).floor();
        out[channel] = if scaled < 0.0 { 0 } else if scaled > 255.0 { 255 } else { scaled as u8 };
        if base[3] == 255 {
            // as in blend_pixel_clip: b + round((B - b) * a' / 255), halves away from zero
            let b = base[channel] as i64;
            let num = ((bc * 255.0) as i64 - b) * a8 as i64;
            let r = b + num.signum() * ((2 * num.abs() + 255) / 510);
            out[channel] = r.clamp(0, 255) as u8;
        }
    }
    out[3] = float64_to_u8(alpha_o as f64);
}

#[inline]
fn float_to_u8_round(v: f32) -> u8 {
    let scaled = (v * 255.0 + 0.5).floor();
    if scaled < 0.0 {
        0
    } else if scaled > 255.0 {
        255
    } else {
        scaled as u8
    }
}

#[inline]
fn blend_pixel_hard_mix(out: &mut [u8], base: &[u8], src: &[u8], opacity: f32) {
    let cb = [
        (base[0] as f32) / 255.0,
        (base[1] as f32) / 255.0,
        (base[2] as f32) / 255.0,
    ];
    let cs = [
        (src[0] as f32) / 255.0,
        (src[1] as f32) / 255.0,
        (src[2] as f32) / 255.0,
    ];
    let alpha_b = (base[3] as f32) / 255.0;
    let alpha_s = ((src[3] as f32) / 255.0) * opacity;
    let alpha_o = alpha_s + alpha_b * (1.0 - alpha_s);
    let safe_alpha_o = if alpha_o > 0.0 { alpha_o } else { 1.0 };

    for channel in 0..3 {
        let bc = if cb[channel] + cs[channel] < 1.0 {
            0.0f64
        } else {
            1.0f64
        };
        let cs_eff = (1.0f64 - alpha_b as f64) * cs[channel] as f64 + alpha_b as f64 * bc;
        let term2 = (1.0 - alpha_s) * alpha_b * cb[channel];
        let mut co = (alpha_s as f64 * cs_eff + term2 as f64) / safe_alpha_o as f64;
        if !(alpha_o > 0.0) {
            co = 0.0;
        }
        out[channel] = float64_to_u8(co);
    }
    out[3] = float64_to_u8(alpha_o as f64);
}

/// CSP's 加算(発光) / 覆い焼き(発光): the source enters premultiplied by its
/// alpha and the result is not mixed back by alpha (mirrors the numpy branch
/// of `_blend_onto_at`, operation for operation):
///   x = Cs·αs,  G = min(1, Cb + x)  |  G = 1 if x >= 1 else min(1, Cb / max(ε, 1 − x))
///   Co = (αb·G + (1 − αb)·x) / αo
#[inline]
fn blend_pixel_glow(out: &mut [u8], base: &[u8], src: &[u8], dodge: bool, opacity: f32) {
    let alpha_b = (base[3] as f32) / 255.0;
    let alpha_s = ((src[3] as f32) / 255.0) * opacity;
    let alpha_o = alpha_s + alpha_b * (1.0 - alpha_s);
    let safe_alpha_o = if alpha_o > 0.0 { alpha_o } else { 1.0 };
    for channel in 0..3 {
        let cb = (base[channel] as f32) / 255.0;
        let x = ((src[channel] as f32) / 255.0) * alpha_s;
        let g = if dodge {
            if x >= 1.0 {
                1.0
            } else {
                1.0f32.min(cb / (1.0 - x).max(EPSILON))
            }
        } else {
            1.0f32.min(cb + x)
        };
        let co = if alpha_o > 0.0 {
            (alpha_b * g + (1.0 - alpha_b) * x) / safe_alpha_o
        } else {
            0.0
        };
        out[channel] = float_to_u8(co);
    }
    out[3] = float_to_u8(alpha_o);
}

#[inline]
fn blend_rgb(mode: BlendMode, cb: [f32; 3], cs: [f32; 3]) -> [f32; 3] {
    match mode {
        BlendMode::Normal => cs,
        BlendMode::Multiply => map_channels(cb, cs, |b, s| b * s),
        BlendMode::Screen => map_channels(cb, cs, |b, s| b + s - b * s),
        BlendMode::Darken => map_channels(cb, cs, |b, s| b.min(s)),
        BlendMode::Lighten => map_channels(cb, cs, |b, s| b.max(s)),
        BlendMode::Difference => map_channels(cb, cs, |b, s| (b - s).abs()),
        BlendMode::Subtract => map_channels(cb, cs, |b, s| clip01(b - s)),
        BlendMode::LinearDodge => map_channels(cb, cs, |b, s| clip01(b + s)),
        BlendMode::LinearBurn => map_channels(cb, cs, |b, s| clip01(b + s - 1.0)),
        BlendMode::ColorDodge => map_channels(cb, cs, color_dodge_channel),
        BlendMode::ColorBurn => map_channels(cb, cs, color_burn_channel),
        BlendMode::Overlay => map_channels(cb, cs, overlay_channel),
        BlendMode::HardLight => map_channels(cb, cs, |b, s| overlay_channel(s, b)),
        BlendMode::SoftLight => map_channels(cb, cs, soft_light_channel),
        BlendMode::Exclusion => map_channels(cb, cs, |b, s| b + s - 2.0 * b * s),
        BlendMode::Divide => map_channels(cb, cs, divide_channel),
        BlendMode::DarkerColor => {
            if luma(cb) <= luma(cs) {
                cb
            } else {
                cs
            }
        }
        BlendMode::LighterColor => {
            if luma(cb) >= luma(cs) {
                cb
            } else {
                cs
            }
        }
        BlendMode::Hue => set_lum(set_sat(cs, saturation(cb)), luma(cb)),
        BlendMode::Saturation => set_lum(set_sat(cb, saturation(cs)), luma(cb)),
        BlendMode::Color => set_lum(cs, luma(cb)),
        BlendMode::Luminosity => set_lum(cb, luma(cs)),
        BlendMode::HardMix => map_channels(cb, cs, |b, s| if b + s < 1.0 { 0.0 } else { 1.0 }),
        BlendMode::LinearLight => map_channels(cb, cs, |b, s| clip01(b + 2.0 * s - 1.0)),
        BlendMode::VividLight => map_channels(cb, cs, |b, s| {
            let burn = color_burn_channel(b, 2.0 * s);
            let dodge = color_dodge_channel(b, 2.0 * (s - 0.5));
            if s < 0.5 {
                burn
            } else {
                dodge
            }
        }),
        BlendMode::PinLight => map_channels(cb, cs, |b, s| {
            if s < 0.5 {
                b.min(2.0 * s)
            } else {
                b.max(2.0 * (s - 0.5))
            }
        }),
        // Handled per pixel in blend_pixel_glow (alpha enters the formula).
        BlendMode::GlowAdd | BlendMode::GlowDodge => cs,
    }
}

#[inline]
fn map_channels<F>(cb: [f32; 3], cs: [f32; 3], mut f: F) -> [f32; 3]
where
    F: FnMut(f32, f32) -> f32,
{
    [f(cb[0], cs[0]), f(cb[1], cs[1]), f(cb[2], cs[2])]
}

#[inline]
fn color_dodge_channel(cb: f32, cs: f32) -> f32 {
    let out = if cs >= 1.0 {
        1.0
    } else {
        1.0f32.min(cb / (1.0 - cs).max(EPSILON))
    };
    if cb <= 0.0 {
        0.0
    } else {
        out
    }
}

#[inline]
fn color_burn_channel(cb: f32, cs: f32) -> f32 {
    let out = if cs <= 0.0 {
        0.0
    } else {
        1.0 - 1.0f32.min((1.0 - cb) / cs.max(EPSILON))
    };
    if cb >= 1.0 {
        1.0
    } else {
        out
    }
}

#[inline]
fn overlay_channel(cb: f32, cs: f32) -> f32 {
    if cb < 0.5 {
        2.0 * cb * cs
    } else {
        1.0 - 2.0 * (1.0 - cb) * (1.0 - cs)
    }
}

/// CSP's overlay in 8 bit: the bright half halves the rounded screen
/// product, so it is always odd. Hard light is this with b and s swapped.
#[inline]
fn overlay_int(b: i64, s: i64) -> i64 {
    if b < 128 {
        (2 * b * s + 127) / 255
    } else {
        255 - 2 * (((255 - b) * (255 - s) + 127) / 255)
    }
}

/// CSP's divide in 8 bit: round(255·b / s), 255 where s = 0, but 0 where b = 0.
#[inline]
fn divide_int(b: i64, s: i64) -> i64 {
    if b == 0 {
        0
    } else if s == 0 {
        255
    } else {
        ((510 * b + s) / (2 * s)).min(255)
    }
}

#[inline]
fn soft_light_channel(cb: f32, cs: f32) -> f32 {
    let d = if cb <= 0.25 {
        ((16.0 * cb - 12.0) * cb + 4.0) * cb
    } else {
        clip01(cb).sqrt()
    };
    if cs <= 0.5 {
        cb - (1.0 - 2.0 * cs) * cb * (1.0 - cb)
    } else {
        cb + (2.0 * cs - 1.0) * (d - cb)
    }
}

#[inline]
fn divide_channel(cb: f32, cs: f32) -> f32 {
    if cs <= 0.0 {
        1.0
    } else {
        clip01(cb / cs.max(EPSILON))
    }
}

#[inline]
fn luma(rgb: [f32; 3]) -> f32 {
    0.2126 * rgb[0] + 0.7152 * rgb[1] + 0.0722 * rgb[2]
}

#[inline]
fn saturation(rgb: [f32; 3]) -> f32 {
    rgb[0].max(rgb[1]).max(rgb[2]) - rgb[0].min(rgb[1]).min(rgb[2])
}

#[inline]
fn set_lum(c: [f32; 3], target_lum: f32) -> [f32; 3] {
    let current_lum = luma(c);
    let delta = target_lum - current_lum;
    let mut out = [c[0] + delta, c[1] + delta, c[2] + delta];
    let n = out[0].min(out[1]).min(out[2]);
    let x = out[0].max(out[1]).max(out[2]);

    if n < 0.0 {
        let denom = (target_lum - n).max(EPSILON);
        out = [
            target_lum + (out[0] - target_lum) * target_lum / denom,
            target_lum + (out[1] - target_lum) * target_lum / denom,
            target_lum + (out[2] - target_lum) * target_lum / denom,
        ];
    }
    if x > 1.0 {
        let denom = (x - target_lum).max(EPSILON);
        out = [
            target_lum + (out[0] - target_lum) * (1.0 - target_lum) / denom,
            target_lum + (out[1] - target_lum) * (1.0 - target_lum) / denom,
            target_lum + (out[2] - target_lum) * (1.0 - target_lum) / denom,
        ];
    }

    [clip01(out[0]), clip01(out[1]), clip01(out[2])]
}

#[inline]
fn set_sat(c: [f32; 3], target_sat: f32) -> [f32; 3] {
    let mn = c[0].min(c[1]).min(c[2]);
    let mx = c[0].max(c[1]).max(c[2]);
    let span = (mx - mn).max(EPSILON);
    let scale = if mx > mn { target_sat / span } else { 0.0 };
    [
        (c[0] - mn) * scale,
        (c[1] - mn) * scale,
        (c[2] - mn) * scale,
    ]
}

#[inline]
fn clip01(v: f32) -> f32 {
    if v < 0.0 {
        0.0
    } else if v > 1.0 {
        1.0
    } else {
        v
    }
}

#[inline]
fn float_to_u8(v: f32) -> u8 {
    let scaled = v * 255.0;
    let clipped = if scaled < 0.0 {
        0.0
    } else if scaled > 255.0 {
        255.0
    } else {
        scaled
    };
    clipped as u8
}

#[inline]
fn float64_to_u8(v: f64) -> u8 {
    let scaled = v * 255.0;
    let clipped = if scaled < 0.0 {
        0.0
    } else if scaled > 255.0 {
        255.0
    } else {
        scaled
    };
    clipped as u8
}
