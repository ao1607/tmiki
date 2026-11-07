//! Layers CSP saves without pixels that 1a draws: the paper layer
//! (`_render_paper_layer`, clip.py:2428-2447) and fills
//! (`_parse_gradation_fill` / `_render_gradation_fill`, :2355-2425; a real
//! gradient is drawn by `gradient`, stage 1b).

use tmi_clip::model::{Col, Node};

use std::sync::Arc;

use super::{gradient, memo, Ctx, Res};
use crate::buffer::{Buf, Rect};
use crate::style;

fn section(blob: &[u8], name: &str) -> Option<usize> {
    let mut key = (name.len() as u32).to_be_bytes().to_vec();
    key.extend(name.encode_utf16().flat_map(|u| u.to_be_bytes()));
    blob.windows(key.len()).position(|w| w == key.as_slice()).map(|i| i + key.len())
}

/// `struct.unpack(">nI", blob[a:a + 4n])`: exactly that many bytes or Err.
fn u32s(blob: &[u8], a: usize, n: usize) -> Res<Vec<u32>> {
    let b = blob.get(a..a + 4 * n).ok_or("struct.error in GradationFillInfo")?;
    Ok(b.chunks_exact(4).map(|c| u32::from_be_bytes(c.try_into().unwrap())).collect())
}

pub struct GradationFill {
    /// (position, RGBA), sorted by position.
    pub stops: Vec<(f64, [u8; 4])>,
    pub points: Option<[f64; 4]>,
    pub solid: Option<[u8; 4]>,
    pub mode: u32,
    pub level: u32,
}

pub fn parse_gradation_fill(blob: &[u8]) -> Res<GradationFill> {
    let p = section(blob, "GradationData").ok_or("GradationFillInfo without GradationData")?;
    let h = u32s(blob, p + 4, 3)?;
    let (header_size, stop_size, count) = (h[0] as usize, h[1] as usize, h[2] as usize);
    // 24-byte headers add the 混色モード and 輝度の補正 level.
    let (mut mode, mut level) = (0, 3);
    if header_size >= 24 {
        let ml = u32s(blob, p + 20, 2)?;
        (mode, level) = (ml[0], ml[1]);
    }
    if count > 1 << 16 {
        return Err("too many gradient stops".into());
    }
    let mut stops = Vec::with_capacity(count);
    for i in 0..count {
        let s = p + 4 + header_size + i * stop_size;
        let v = u32s(blob, s, 6)?;
        let pos = (v[5].min(0x8000) as f64) / 0x8000 as f64;
        stops.push((pos, [(v[0] >> 24) as u8, (v[1] >> 24) as u8, (v[2] >> 24) as u8, (v[3] >> 24) as u8]));
    }
    stops.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
    let mut points = None;
    if let Some(q) = section(blob, "GradationSetting") {
        let b = blob.get(q + 12..q + 68).ok_or("struct.error in GradationSetting")?;
        let d: Vec<f64> = b.chunks_exact(8).map(|c| f64::from_be_bytes(c.try_into().unwrap())).collect();
        points = Some([d[3], d[4], d[5], d[6]]);
    }
    let mut solid = None;
    if let Some(a) = section(blob, "GradationSettingAdd0001") {
        let v = u32s(blob, a + 4, 4)?;
        if v[0] == 1 {
            solid = Some([(v[1] >> 24) as u8, (v[2] >> 24) as u8, (v[3] >> 24) as u8, 255]);
        }
    }
    Ok(GradationFill { stops, points, solid, mode, level })
}

/// `_u32_to_u8` of `value or 0` (a negative value is declined).
fn colour(v: &Col) -> Res<u8> {
    style::u32_to_u8(v)
}

impl Ctx<'_> {
    /// `_synthesize_layer` over `window`: Some(buffer) or None (no pixels).
    pub fn synthesize(&self, node: &Node, window: Rect) -> Res<Option<Buf>> {
        let extra = self.doc.extra(node.main_id);
        if node.layer_type == Some(1584) {
            let Some([r, g, b, enable]) = &extra.paper else {
                return Ok(None);
            };
            if !enable.truthy()? {
                return Ok(None);
            }
            return Ok(Some(Buf::filled(window, [colour(r)?, colour(g)?, colour(b)?, 255])));
        }
        let Some(col) = &extra.gradation_info else {
            return Ok(None);
        };
        let blob = match col.blob()? {
            Some(b) if !b.is_empty() => b,
            _ => return Ok(None),
        };
        let fill = parse_gradation_fill(blob).map_err(|e| format!("layer {}: {e}", node.main_id))?;
        let image = if let Some(s) = fill.solid {
            Buf::filled(window, s)
        } else {
            let Some(points) = fill.points else {
                return Ok(None);
            };
            if fill.stops.is_empty() {
                return Ok(None);
            }
            let (dx, dy) = (points[2] - points[0], points[3] - points[1]);
            if !points.iter().all(|v| v.is_finite()) || dx.abs() >= 1e100 || dy.abs() >= 1e100 {
                // `(x1 - x0) ** 2` overflows (Python warns and drops the layer)
                return Err(format!("layer {}: gradient points out of range", node.main_id));
            }
            if dx * dx + dy * dy <= 0.0 {
                Buf::filled(window, fill.stops.last().unwrap().1)
            } else {
                let canvas = (self.doc.width, self.doc.height);
                // The colour table once per render (every band needs it).
                let lut = memo(&self.luts, &node.main_id, || {
                    gradient::gradient_lut(&fill.stops, fill.mode, fill.level).map(Arc::new)
                })
                .map_err(|e| format!("layer {}: {e}", node.main_id))?;
                gradient::render_with_lut(&lut, points, canvas, window).map_err(|e| format!("layer {}: {e}", node.main_id))?
            }
        };
        let st = self.doc.style(node.main_id);
        Ok(Some(style::apply_layer_style(&image, &st, None)?.unwrap_or(image)))
    }
}
