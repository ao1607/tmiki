//! A leaf layer's pixels: the stored render offscreen (`_render_stored_buffer`,
//! clip.py:2200-2233) with 表現色 / レイヤーカラー applied
//! (`_render_layer_buffer`, :2060-2097), and correction-layer filters.

use tmi_clip::attr::{parse_offscreen_attribute, OffscreenAttribute};
use tmi_clip::container;
use tmi_clip::model::{Node, OffscreenInfo};
use tmi_clip::tiles::{decode_offscreen, decode_offscreen_on, offscreen_oversize, Offscreen};
use tmi_clip::window::{subset_payload, tile_index, windows_decode_like_whole};

use super::{memo, Ctx, Res, RESTART_ONE_PASS};
use crate::buffer::{Buf, Layer, Rect};
use crate::filter::{self, Decoded};
use crate::style;

impl Ctx<'_> {
    /// `_render_stored_buffer` over `window` of a `full` canvas. `Layer::None`
    /// when the layer stores no pixels, or when they are unreadable: then,
    /// as in Python, the offscreen is warned about and the layer goes on as
    /// one without stored pixels; a whole decode also records the layer
    /// (`decode_failures`). A band of a banded render instead asks for the
    /// render to start over in one pass (`RESTART_ONE_PASS`), which decodes
    /// every layer whole (`_render_banded`).
    pub fn stored_buffer(&self, node: &Node, full: Rect, window: Rect) -> Res<Layer> {
        let dx = node.offset_x + node.render_offset_x;
        let dy = node.offset_y + node.render_offset_y;
        let Some(info) = self.doc.offscreen(node.render_mipmap) else {
            return Ok(Layer::None);
        };
        let Some(exta) = self.doc.exta(info) else {
            return Ok(Layer::None);
        };
        let windowed = window != full && self.windowed_decode;
        let unreadable = |reason: String| -> Res<Layer> {
            if windowed && self.validate_whole {
                return Err(RESTART_ONE_PASS.into());
            }
            self.note_unreadable(info, reason);
            if !windowed {
                self.failures.lock().unwrap().push(node.main_id);
            }
            Ok(Layer::None)
        };
        let attr = match parse_offscreen_attribute(&info.attribute) {
            Ok(a) => a,
            Err(e) => return unreadable(format!("Offscreen.Attribute: {e}")),
        };
        if let Some(why) = offscreen_oversize(attr.pixel_width, attr.pixel_height, self.doc.width as usize, self.doc.height as usize) {
            return unreadable(why);
        }
        let decoded = if windowed {
            if self.validate_whole {
                let mm = node.render_mipmap.unwrap();
                memo(&self.validated, &mm, || self.validate(info, &attr, dx, dy)).map_err(|_| RESTART_ONE_PASS.to_string())?;
            }
            let payload = container::payload(self.raw, exta);
            let Some(index) = tile_index(payload) else {
                return unreadable("malformed tile records".into());
            };
            let bx = [window[0] - dx, window[1] - dy, window[2] - dx, window[3] - dy];
            let Some(subset) = subset_payload(&index, attr.tile_cols, bx, false) else {
                return Ok(Layer::Empty);
            };
            decode_offscreen(&subset, 0, subset.len(), attr.tile_cols, attr.tile_rows, attr.pixel_width, attr.pixel_height)
        } else {
            decode_offscreen(self.raw, exta.payload_off, exta.payload_size, attr.tile_cols, attr.tile_rows, attr.pixel_width, attr.pixel_height)
        };
        let Offscreen { rgba: px, x, y, w, h, unknown_sizes } = match decoded {
            Ok(d) => d,
            Err(e) => return unreadable(e),
        };
        self.note_tile_sizes(&unknown_sizes);
        if w == 0 || h == 0 {
            return Ok(Layer::Empty);
        }
        let shifted = Buf { x: x as i64 + dx, y: y as i64 + dy, w, h, px };
        let bounds = shifted.bounds();
        Ok(match shifted.clip_to(window) {
            None => Layer::Empty,
            Some(b) if b.bounds() == bounds => Layer::Buf(b), // already trimmed
            Some(b) => Layer::from_trimmed(b.trim()),
        })
    }

    /// Whether the bands of a banded render give this layer's whole decode:
    /// its windows decode like the whole (`windows_decode_like_whole`), and
    /// the tiles off the canvas, which no band decodes, decode
    /// (`_stored_pixels_outside_canvas_decode`). Err: render in one pass.
    fn validate(&self, info: &OffscreenInfo, attr: &OffscreenAttribute, dx: i64, dy: i64) -> Res<()> {
        let exta = self.doc.exta(info).ok_or("no data block")?;
        let payload = container::payload(self.raw, exta);
        let (w, h) = (self.doc.width as i64, self.doc.height as i64);
        if !windows_decode_like_whole(payload, attr.tile_cols) {
            return Err("windows of an offscreen do not decode like the whole".into());
        }
        let index = tile_index(payload).ok_or("malformed tile records")?;
        if let Some(outside) = subset_payload(&index, attr.tile_cols, [-dx, -dy, w - dx, h - dy], true) {
            // On this thread only (see `memo`).
            let d = decode_offscreen_on(&outside, 0, outside.len(), attr.tile_cols, attr.tile_rows, attr.pixel_width, attr.pixel_height, false)?;
            self.note_tile_sizes(&d.unknown_sizes);
        }
        Ok(())
    }

    /// `_render_layer_buffer`: stored pixels with the layer's style applied,
    /// or an image material's placed original. A layer of vector strokes is
    /// declined.
    pub fn layer_buffer(&self, node: &Node, full: Rect, window: Rect) -> Res<Layer> {
        let stored = self.stored_buffer(node, full, window)?;
        let st = self.doc.style(node.main_id);
        match stored {
            Layer::None => {
                if st.get("ResizableOriginalMipmap").truthy()? {
                    return self.material_buffer(node, window);
                }
                // A vector layer: re-stroked, its colours as colour data.
                match self.vector_pixels(node, full, window)? {
                    None => Ok(Layer::None),
                    Some(Layer::Buf(b)) if style::needs_profile(&st)? => {
                        Ok(Layer::Buf(style::apply_layer_style(&b, &st, Some("color-alpha-bgra"))?.unwrap_or(b)))
                    }
                    Some(layer) => Ok(layer),
                }
            }
            Layer::Empty => Ok(Layer::Empty),
            Layer::Buf(b) => {
                if !style::needs_profile(&st)? {
                    return Ok(Layer::Buf(b));
                }
                let profile = self
                    .doc
                    .offscreen(node.render_mipmap)
                    .and_then(|info| parse_offscreen_attribute(&info.attribute).ok())
                    .map(|a| a.channel_profile);
                Ok(match style::apply_layer_style(&b, &st, profile.as_deref())? {
                    Some(nb) => Layer::Buf(nb),
                    None => Layer::Buf(b),
                })
            }
        }
    }

    /// `_decode_filter_layer` (memoised: a tone curve's LUT is not free).
    pub fn filter_of(&self, node: &Node) -> Res<Decoded> {
        if node.layer_type != Some(4098) {
            return Ok(Decoded::None);
        }
        memo(&self.filters, &node.main_id, || {
            let Some(col) = self.doc.extra(node.main_id).filter_info else {
                return Ok(Decoded::None);
            };
            match col.blob()? {
                None => Ok(Decoded::None),
                Some(b) => Ok(filter::decode(b)),
            }
        })
    }
}
