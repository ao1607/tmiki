//! `render_clip`: the whole CLIP render in one call (RUST_MIGRATION.md 1a).

use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyList};

use tmi_clip::ClipDoc;
use tmi_render::buffer::{intersect, size, Rect};
use tmi_render::clip::band::{render, Report, Target};
use tmi_render::clip::report::{Note, Warning};

fn declined<'py>(py: Python<'py>, reason: &str) -> PyResult<Bound<'py, PyDict>> {
    let d = PyDict::new(py);
    d.set_item("declined", reason)?;
    Ok(d)
}

fn rect<'py>(py: Python<'py>, r: Rect) -> PyResult<Bound<'py, PyList>> {
    PyList::new(py, r)
}

/// Render a CLIP file's layered composite (``raw``: the file's bytes) over
/// ``background`` (RGBA): the whole canvas, each of ``regions`` (windows
/// ``(x0, y0, x1, y1)`` inside it; ``decodable_all``: leaves decode only the
/// window's tiles), or, with ``folder`` (a MainId), that folder's children
/// alone. Returns ``{"images": [bytes], "decode_failures": [main_id],
/// "warnings": [(kind, key)], "trace": [record]}``, or ``{"declined":
/// reason}`` when the file uses something this renderer does not draw.
/// Warning kinds: "composite" (an unknown blend id), "filter" (an
/// unrendered correction filter id), "offscreen" (``(outer_id, reason)``:
/// an unreadable offscreen, left out), "tile-size" (``(bytes per tile,
/// pixels)``: an unknown tile encoding, drawn transparent).
/// ``decode_failures``: layers whose stored pixels did not decode whole.
/// The GIL is released for the whole render.
#[pyfunction]
#[pyo3(signature = (raw, background, *, regions=None, decodable_all=false, folder=None, trace=false))]
pub fn render_clip<'py>(
    py: Python<'py>,
    raw: &[u8],
    background: (u8, u8, u8, u8),
    regions: Option<Vec<(i64, i64, i64, i64)>>,
    decodable_all: bool,
    folder: Option<i64>,
    trace: bool,
) -> PyResult<Bound<'py, PyDict>> {
    let bg = [background.0, background.1, background.2, background.3];
    let doc = match py.allow_threads(|| ClipDoc::open(raw)) {
        Ok(d) => d,
        Err(e) => return declined(py, &format!("unreadable: {e}")),
    };
    let full: Rect = [0, 0, doc.width as i64, doc.height as i64];
    let mut targets: Vec<(Target, Rect)> = Vec::new();
    match (&regions, folder) {
        (Some(_), Some(_)) => return declined(py, "regions and folder together"),
        (None, None) => targets.push((Target::Canvas, full)),
        (None, Some(id)) => {
            let Some(i) = doc.nodes.iter().position(|n| n.main_id == id) else {
                return declined(py, "no such folder");
            };
            if !doc.nodes[i].folder {
                return declined(py, "not a folder");
            }
            targets.push((Target::Folder(i), full));
        }
        (Some(list), None) => {
            for &(x0, y0, x1, y1) in list {
                let Some(w) = intersect([x0, y0, x1, y1], full) else {
                    return declined(py, "region outside the canvas");
                };
                targets.push((Target::Region { window: w, decodable_all }, w));
            }
        }
    }
    let mut images = Vec::with_capacity(targets.len());
    let mut reports: Vec<Report> = Vec::with_capacity(targets.len());
    for (target, window) in &targets {
        let (w, h) = size(*window);
        let mut result = Ok(Report::default());
        let image = PyBytes::new_with(py, w * h * 4, |buf: &mut [u8]| {
            result = py.allow_threads(|| render(&doc, raw, bg, target, trace, buf));
            Ok(())
        })?;
        match result {
            Ok(r) => reports.push(r),
            Err(e) => return declined(py, &e),
        }
        images.push(image);
    }
    let out = PyDict::new(py);
    out.set_item("images", PyList::new(py, images)?)?;
    let mut failures: Vec<i64> = reports.iter().flat_map(|r| r.decode_failures.iter().copied()).collect();
    failures.sort_unstable();
    failures.dedup();
    out.set_item("decode_failures", PyList::new(py, failures)?)?;
    let warnings = PyList::empty(py);
    let records = PyList::empty(py);
    for r in &reports {
        for w in &r.warnings {
            match w {
                Warning::Composite(id) => warnings.append(("composite", *id))?,
                Warning::Filter(id) => warnings.append(("filter", *id as i64))?,
            }
        }
        for n in &r.notes {
            match n {
                Note::Offscreen { outer_id, reason } => warnings.append(("offscreen", (outer_id, reason)))?,
                Note::TileSize { bpt, pixels } => warnings.append(("tile-size", (*bpt, *pixels)))?,
            }
        }
        for t in &r.trace {
            let d = PyDict::new(py);
            d.set_item("phase", t.phase)?;
            d.set_item("band", rect(py, t.band)?)?;
            d.set_item("main_id", t.main_id)?;
            d.set_item("window", rect(py, t.window)?)?;
            match t.bounds {
                Some(b) => d.set_item("bounds", rect(py, b)?)?,
                None => d.set_item("bounds", py.None())?,
            }
            match &t.sha256 {
                Some(s) => d.set_item("sha256", s)?,
                None => d.set_item("sha256", py.None())?,
            }
            records.append(d)?;
        }
    }
    out.set_item("warnings", warnings)?;
    out.set_item("trace", records)?;
    Ok(out)
}
