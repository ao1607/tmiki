//! Whole renders: one window (`_render_window`), a whole canvas in bands
//! (`_render_banded`, `_band_layout`; clip.py:1552-1705) and a folder alone
//! (`native.render_folder`).
//!
//! A band is an exact window of the canvas, so the stitched bands are the
//! one-pass render; the band layout only decides the peak memory, and (as
//! in Python) the `band` of the trace records.

use rayon::prelude::*;
use tmi_clip::ClipDoc;

use super::report::{BandState, Note, TraceRecord, Warning};
use super::{Ctx, Res, RESTART_ONE_PASS};
use crate::buffer::{intersect, Rect, Surface};

const TILE: usize = 256;
pub const BAND_MIN_PIXELS: usize = 4 << 20;
pub const BAND_TARGET_PIXELS: usize = 1 << 21;
pub const BAND_WORKERS: usize = 8;

/// `_band_layout`.
pub fn band_layout(width: usize, height: usize, workers: usize) -> Vec<Rect> {
    let mut rows = (BAND_TARGET_PIXELS / width.max(1)).max(1);
    rows = rows.min(height.div_ceil(2 * workers));
    rows = (rows / TILE * TILE).max(TILE);
    (0..height)
        .step_by(rows)
        .map(|y| [0, y as i64, width as i64, (height.min(y + rows)) as i64])
        .collect()
}

/// Python's worker count for the layout: `min(_BAND_WORKERS, os.cpu_count())`.
pub fn band_workers() -> usize {
    BAND_WORKERS.min(std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1))
}

#[derive(Default)]
pub struct Report {
    pub warnings: Vec<Warning>,
    pub trace: Vec<TraceRecord>,
    /// Unreadable offscreens and unknown tile sizes, in the order met.
    pub notes: Vec<Note>,
    /// MainIds whose stored pixels did not decode whole (sorted, unique).
    pub decode_failures: Vec<i64>,
}

impl Report {
    fn new(st: BandState, ctx: Ctx<'_>) -> Report {
        let mut decode_failures = ctx.failures.into_inner().unwrap();
        decode_failures.sort_unstable();
        decode_failures.dedup();
        Report { warnings: st.warnings, trace: st.trace.unwrap_or_default(), notes: ctx.notes.into_inner().unwrap(), decode_failures }
    }
}

/// What to render.
pub enum Target {
    /// The whole canvas (`render_layered_composite_rgba(clip)`).
    Canvas,
    /// A window of it; `decodable_all`: leaves decode only its tiles.
    Region { window: Rect, decodable_all: bool },
    /// A folder's children on a transparent canvas (`native.render_folder`).
    Folder(usize),
}

/// Render `target` of `doc` into `out` (the window's RGBA bytes).
pub fn render(doc: &ClipDoc, raw: &[u8], background: [u8; 4], target: &Target, trace: bool, out: &mut [u8]) -> Res<Report> {
    let full: Rect = [0, 0, doc.width as i64, doc.height as i64];
    let (window, layers, decodable) = match target {
        Target::Canvas => (full, &doc.top, false),
        Target::Region { window, decodable_all } => {
            let w = intersect(*window, full).ok_or("region outside the canvas")?;
            (w, &doc.top, *decodable_all)
        }
        Target::Folder(i) => (full, &doc.nodes[*i].children, false),
    };
    let (ww, wh) = crate::buffer::size(window);
    if out.len() != ww * wh * 4 {
        return Err("output buffer has the wrong size".into());
    }
    let whole = window == full && !matches!(target, Target::Folder(_));
    if whole && doc.width * doc.height >= BAND_MIN_PIXELS {
        let bands = band_layout(doc.width, doc.height, band_workers());
        if bands.len() >= 2 {
            match render_bands(doc, raw, background, &bands, trace, out) {
                // A layer's stored pixels do not decode (or would not in
                // windows): Python then renders in one pass, as below.
                Err(e) if e == RESTART_ONE_PASS => {}
                r => return r,
            }
        }
    }
    let ctx = Ctx::new(doc, raw, decodable && !whole, false);
    let mut st = BandState::new(window, trace);
    let mut surface = Surface::fixed(window, full, background);
    ctx.composite_group(&mut st, &mut surface, layers)?;
    out.copy_from_slice(&surface.img.unwrap().px);
    Ok(Report::new(st, ctx))
}

/// The bands of a whole-canvas render; `Err(RESTART_ONE_PASS)` when a band
/// asks for a one-pass render instead (its records are dropped then).
fn render_bands(doc: &ClipDoc, raw: &[u8], background: [u8; 4], bands: &[Rect], trace: bool, out: &mut [u8]) -> Res<Report> {
    let full: Rect = [0, 0, doc.width as i64, doc.height as i64];
    let ctx = Ctx::new(doc, raw, true, true);
    ctx.warm_vector_plans();
    let stride = doc.width * 4;
    let rows = (bands[0][3] - bands[0][1]) as usize;
    // At most `band_workers()` bands at a time (Python's pool size): a band
    // waiting inside its own parallel steps would otherwise start further
    // bands on the waiting thread, each holding its buffers.
    let mut results: Vec<Res<BandState>> = Vec::with_capacity(bands.len());
    let wave = band_workers().max(1);
    for (chunks, bands) in out.chunks_mut(stride * rows * wave).zip(bands.chunks(wave)) {
        let part: Vec<Res<BandState>> = chunks
            .par_chunks_mut(stride * rows)
            .zip(bands.par_iter())
            .map(|(chunk, &band)| {
                let mut st = BandState::new(band, trace);
                let mut surface = Surface::fixed(band, full, background);
                ctx.composite_group(&mut st, &mut surface, &doc.top)?;
                chunk.copy_from_slice(&surface.img.unwrap().px);
                Ok(st)
            })
            .collect();
        let failed = part.iter().any(Result::is_err);
        results.extend(part);
        if failed {
            break;
        }
    }
    if results.iter().any(|r| matches!(r, Err(e) if e == RESTART_ONE_PASS)) {
        return Err(RESTART_ONE_PASS.into());
    }
    let mut first = None;
    let mut trace_records = Vec::new();
    for (k, r) in results.into_iter().enumerate() {
        let mut st = r?;
        trace_records.extend(st.trace.take().unwrap_or_default());
        if k == 0 {
            first = Some(st);
        }
    }
    let mut report = Report::new(first.expect("at least one band"), ctx);
    report.trace = trace_records;
    Ok(report)
}
