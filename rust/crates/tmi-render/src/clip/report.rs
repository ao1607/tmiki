//! What a render reports besides pixels: the warnings Python would print
//! (replayed through its `_warn_*` functions, which keep the text and the
//! de-duplication) and the `TMI_RENDER_TRACE` records (render_trace.py).

use sha2::{Digest, Sha256};

use crate::buffer::{Buf, Rect};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Warning {
    /// `_warn_unknown_composite(id)`
    Composite(i64),
    /// `_warn_unsupported_filter(id)`
    Filter(u32),
}

/// What decoding offscreens noticed, render-wide (not per band).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Note {
    /// `_warn_offscreen_failure(outer_id, ...)`: an unreadable offscreen,
    /// left out of the render.
    Offscreen { outer_id: String, reason: String },
    /// `_warn_unknown_tile_size(bpt, pixels)`: a tile drawn transparent.
    TileSize { bpt: usize, pixels: usize },
}

#[derive(Clone, Debug)]
pub struct TraceRecord {
    pub phase: &'static str,
    pub band: Rect,
    pub main_id: i64,
    pub window: Rect,
    pub bounds: Option<Rect>,
    /// sha256 hex, "empty" for an empty buffer, None for an empty surface.
    pub sha256: Option<String>,
}

/// `render_trace.digest`: sha256 over "x0,y0,x1,y1:" + RGBA bytes.
pub fn digest(buf: &Buf) -> String {
    let b = buf.bounds();
    let mut h = Sha256::new();
    h.update(format!("{},{},{},{}:", b[0], b[1], b[2], b[3]).as_bytes());
    h.update(&buf.px);
    h.finalize().iter().map(|x| format!("{x:02x}")).collect()
}

/// Per band (or window): its warnings and trace, and the group nesting.
pub struct BandState {
    pub band: Rect,
    pub depth: usize,
    pub warnings: Vec<Warning>,
    pub trace: Option<Vec<TraceRecord>>,
}

impl BandState {
    pub fn new(band: Rect, trace: bool) -> BandState {
        BandState { band, depth: 0, warnings: Vec::new(), trace: if trace { Some(Vec::new()) } else { None } }
    }

    /// A record of this band; `empty_text` is the hash field without pixels.
    pub fn make_record(&self, phase: &'static str, main_id: i64, window: Rect, buf: Option<&Buf>, empty_text: Option<&str>) -> TraceRecord {
        TraceRecord {
            phase,
            band: self.band,
            main_id,
            window,
            bounds: buf.map(Buf::bounds),
            sha256: match buf {
                Some(b) => Some(digest(b)),
                None => empty_text.map(str::to_string),
            },
        }
    }
}
