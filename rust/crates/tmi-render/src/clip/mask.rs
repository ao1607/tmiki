//! Layer masks (`_MaskPlane`, `_mask_init_alpha`, `_layer_mask_crop`,
//! `_mask_is_enabled`; clip.py:2559-2725).

use std::sync::Arc;

use tmi_clip::attr::{mask_init_alpha, parse_offscreen_attribute};
use tmi_clip::container;
use tmi_clip::model::Node;
use tmi_clip::window::{mask_tiles, MaskTile};

use super::{memo, Ctx, Res};
use crate::buffer::{intersect, size, Rect};

/// A decoded mask: `init` everywhere but the painted tiles (later tiles win).
pub struct MaskPlane {
    pub width: usize,
    pub height: usize,
    pub init: u8,
    pub tiles: Vec<MaskTile>,
}

/// `_mask_is_enabled`.
pub fn mask_is_enabled(node: &Node) -> bool {
    node.mask_mipmap.is_some() && !node.comic_frame && node.mask_enabled
}

impl Ctx<'_> {
    /// `_render_mask_attenuation` of the node's mask offscreen, once per
    /// render (keyed by its data block id, as `clip.mask_cache`). An
    /// unreadable mask is warned about and does not apply (None).
    fn mask_plane(&self, node: &Node) -> Res<Option<Arc<MaskPlane>>> {
        let Some(info) = self.doc.offscreen(node.mask_mipmap) else {
            return Ok(None);
        };
        let key = info.outer_id.clone().unwrap_or_else(|| b"None".to_vec());
        memo(&self.masks, &key, || {
            let attr = match parse_offscreen_attribute(&info.attribute) {
                Ok(a) => a,
                Err(e) => {
                    self.note_unreadable(info, format!("Offscreen.Attribute: {e}"));
                    return Ok(None);
                }
            };
            if let Some(why) = tmi_clip::tiles::offscreen_oversize(attr.pixel_width, attr.pixel_height, self.doc.width, self.doc.height) {
                self.note_unreadable(info, why);
                return Ok(None);
            }
            let init = mask_init_alpha(&attr);
            let tiles = match self.doc.exta(info) {
                None => Vec::new(),
                Some(exta) => match mask_tiles(container::payload(self.raw, exta), attr.tile_cols, attr.pixel_width, attr.pixel_height) {
                    Ok((tiles, unknown_sizes)) => {
                        self.note_tile_sizes(&unknown_sizes);
                        tiles
                    }
                    Err(e) => {
                        self.note_unreadable(info, e);
                        return Ok(None);
                    }
                },
            };
            Ok(Some(Arc::new(MaskPlane { width: attr.pixel_width, height: attr.pixel_height, init, tiles })))
        })
    }

    /// `_layer_mask_crop`: the node's mask over `crop_bounds ∩ canvas_bounds`
    /// as an "L" plane, None without an active mask.
    pub fn layer_mask_crop(&self, node: &Node, crop_bounds: Rect, canvas_bounds: Rect) -> Res<Option<(Rect, Vec<u8>)>> {
        if !mask_is_enabled(node) {
            return Ok(None);
        }
        let Some(crop) = intersect(crop_bounds, canvas_bounds) else {
            return Ok(None);
        };
        let Some(plane) = self.mask_plane(node)? else {
            return Ok(None);
        };
        let (cw, ch) = size(crop);
        let mut mask = vec![plane.init; cw * ch];
        let mx = node.offset_x + node.mask_offset_x;
        let my = node.offset_y + node.mask_offset_y;
        let source = [mx, my, mx + plane.width as i64, my + plane.height as i64];
        let Some(overlap) = intersect(crop, source) else {
            return Ok(Some((crop, mask)));
        };
        for t in &plane.tiles {
            let tile = [mx + t.x as i64, my + t.y as i64, mx + (t.x + t.w) as i64, my + (t.y + t.h) as i64];
            let Some(d) = intersect(tile, overlap) else {
                continue;
            };
            let n = (d[2] - d[0]) as usize;
            for y in d[1]..d[3] {
                let s = ((y - tile[1]) as usize) * t.w + (d[0] - tile[0]) as usize;
                let o = ((y - crop[1]) as usize) * cw + (d[0] - crop[0]) as usize;
                if t.alpha.is_empty() {
                    mask[o..o + n].fill(0); // a tile of an unknown size
                } else {
                    mask[o..o + n].copy_from_slice(&t.alpha[s..s + n]);
                }
            }
        }
        Ok(Some((crop, mask)))
    }
}
