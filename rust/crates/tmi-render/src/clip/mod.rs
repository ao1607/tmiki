//! The CLIP compositor (`render_layered_composite_rgba` and everything below
//! it in tmi/formats/clip.py), stage 1a of RUST_MIGRATION.md: raster layers,
//! folders, pass-through, clipping, layer masks, every blend mode,
//! correction layers, paper and solid fills, レイヤーカラー and 表現色, banding;
//! 1b: gradient fills, トーン, フチ; 1c: image materials; 1d: ベクター layers
//! and コマ枠 folders.
//!
//! The output is byte for byte the Python renderer's (now the reference
//! package `tmiref`). An offscreen it cannot read (stored pixels, a mask, an
//! image material's original) is warned about and left out, as Python
//! does (stage 1e). Whatever else this module does not reproduce, it
//! declines (`Err`): tmiki then reports that it cannot render the file.

pub mod band;
pub mod decode;
pub mod frame;
pub mod edge;
pub mod gradient;
pub mod group;
pub mod material;
pub mod tone;
pub mod mask;
pub mod report;
pub mod synth;
pub mod vector;

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Arc, Mutex, OnceLock};

use tmi_clip::model::{Node, OffscreenInfo};
use tmi_clip::ClipDoc;

use crate::buffer::Rect;
use crate::filter::Decoded;
use report::Note;

/// Why the Rust renderer will not draw a file.
pub type Res<T> = Result<T, String>;

/// The `Err` with which a band of a banded render asks for the whole render
/// to start over in one pass (a layer's stored pixels do not decode, or its
/// windows would not decode like the whole; `_render_banded` returns None).
pub const RESTART_ONE_PASS: &str = "restart in one pass";

type Memo<K, V> = Mutex<HashMap<K, Arc<OnceLock<V>>>>;

/// `f()` computed once per key, shared by every band (the `_RenderMemo`).
///
/// `f` must not wait on rayon work (no `par_iter`, no `join`): a worker
/// waiting there may steal another band, which can block on this same key
/// on the same thread, a deadlock.
fn memo<K: Eq + Hash + Clone, V: Clone>(map: &Memo<K, V>, key: &K, f: impl FnOnce() -> V) -> V {
    let slot = {
        let mut m = map.lock().unwrap();
        m.entry(key.clone()).or_insert_with(|| Arc::new(OnceLock::new())).clone()
    };
    slot.get_or_init(f).clone()
}

/// One render of one document.
pub struct Ctx<'a> {
    pub doc: &'a ClipDoc,
    pub raw: &'a [u8],
    /// Leaves decode only the tiles a window needs (`decodable_layers`).
    pub windowed_decode: bool,
    /// Banded whole-canvas render: every decoded offscreen must decode whole
    /// (or the one-pass render Python falls back to would drop it).
    pub validate_whole: bool,
    masks: Memo<Vec<u8>, Res<Option<Arc<mask::MaskPlane>>>>,
    validated: Memo<i64, Res<()>>,
    filters: Memo<i64, Res<Decoded>>,
    /// Vector layers' stroke plans (shared by the bands).
    vector_plans: Memo<i64, Res<Arc<vector::Plan>>>,
    materials: Memo<i64, Res<Option<Arc<material::Original>>>>,
    planes: Memo<(i64, u32, bool), Arc<material::Plane>>,
    luts: Memo<i64, Res<Arc<Vec<[f64; 4]>>>>,
    /// Unreadable offscreens and unknown tile sizes met (Python warns as it
    /// decodes; these are replayed after the render).
    pub notes: Mutex<Vec<Note>>,
    /// MainIds whose stored pixels did not decode whole (`decode_failures`).
    pub failures: Mutex<Vec<i64>>,
}

impl<'a> Ctx<'a> {
    pub fn new(doc: &'a ClipDoc, raw: &'a [u8], windowed_decode: bool, validate_whole: bool) -> Ctx<'a> {
        Ctx {
            doc,
            raw,
            windowed_decode,
            validate_whole,
            masks: Mutex::new(HashMap::new()),
            validated: Mutex::new(HashMap::new()),
            filters: Mutex::new(HashMap::new()),
            vector_plans: Mutex::new(HashMap::new()),
            materials: Mutex::new(HashMap::new()),
            planes: Mutex::new(HashMap::new()),
            luts: Mutex::new(HashMap::new()),
            notes: Mutex::new(Vec::new()),
            failures: Mutex::new(Vec::new()),
        }
    }

    /// `_warn_offscreen_failure`: the offscreen of `info` is unreadable.
    pub fn note_unreadable(&self, info: &OffscreenInfo, reason: String) {
        let outer_id = String::from_utf8_lossy(info.outer_id.as_deref().unwrap_or(b"None")).into_owned();
        self.notes.lock().unwrap().push(Note::Offscreen { outer_id, reason });
    }

    /// `_warn_unknown_tile_size` for each `(bytes per tile, pixels)`.
    pub fn note_tile_sizes(&self, sizes: &[(usize, usize)]) {
        if !sizes.is_empty() {
            let mut notes = self.notes.lock().unwrap();
            notes.extend(sizes.iter().map(|&(bpt, pixels)| Note::TileSize { bpt, pixels }));
        }
    }

    pub fn node(&self, i: usize) -> &'a Node {
        &self.doc.nodes[i]
    }

    pub fn full(&self) -> Rect {
        [0, 0, self.doc.width as i64, self.doc.height as i64]
    }
}

/// `(node.opacity or 0) / 256.0`, declining outside [0, 256] (Pillow would
/// clamp or wrap an "L" weight beyond 255; not reproduced).
pub fn opacity(node: &Node) -> Res<f64> {
    if !(0..=256).contains(&node.opacity) {
        return Err(format!("layer {} has opacity {}", node.main_id, node.opacity));
    }
    Ok(node.opacity as f64 / 256.0)
}

/// `_COMPOSITE_MAP` (None: unknown id, drawn as normal after a warning).
pub fn composite_name(id: i64) -> Option<&'static str> {
    Some(match id {
        0 => "normal",
        1 => "darken",
        2 => "multiply",
        3 => "color_burn",
        4 => "linear_burn",
        5 => "subtract",
        6 => "darker_color",
        7 => "lighten",
        8 => "screen",
        9 => "color_dodge",
        10 => "glow_dodge",
        11 => "linear_dodge",
        12 => "glow_add",
        13 => "lighter_color",
        14 => "overlay",
        15 => "soft_light",
        16 => "hard_light",
        17 => "vivid_light",
        18 => "linear_light",
        19 => "pin_light",
        20 => "hard_mix",
        21 => "difference",
        22 => "exclusion",
        23 => "hue",
        24 => "saturation",
        25 => "color",
        26 => "luminosity",
        30 => "pass_through",
        36 => "divide",
        _ => return None,
    })
}
