//! MDP2 shims (stage 2c): parse, layer decode, composite and write-back.
//! Each call parses ``raw`` (the file's bytes) itself: the XML is a few KiB
//! and the PAC records are a header walk, so no state crosses the boundary.

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyList};

use tmi_clip::mdp2::{image_size, parse, plan_layer, Mdp2, Strictness};

fn open(py: Python<'_>, raw: &[u8]) -> PyResult<Mdp2> {
    py.allow_threads(|| parse(raw)).map_err(PyValueError::new_err)
}

fn layer_index(doc: &Mdp2, index: usize) -> PyResult<usize> {
    if index >= doc.layers.len() {
        return Err(PyValueError::new_err("MDP2 layer index out of range"));
    }
    Ok(index)
}

/// ``parse_mdp2``: ``{"reserved", "xml_size", "pack_size", "canvas_width",
/// "canvas_height", "dpi", "blobs": [(name, file_off, record_size, flag,
/// payload_size, aux_size, payload_off)], "layers": [dict]}``; each layer
/// dict holds the ``Mdp2Layer`` fields plus ``children`` (indices) and
/// ``renderable``. ValueError where the Python reader raised.
#[pyfunction]
pub fn parse_mdp2<'py>(py: Python<'py>, raw: &[u8]) -> PyResult<Bound<'py, PyDict>> {
    let doc = open(py, raw)?;
    let out = PyDict::new(py);
    out.set_item("reserved", doc.reserved)?;
    out.set_item("xml_size", doc.xml_size)?;
    out.set_item("pack_size", doc.pack_size)?;
    out.set_item("canvas_width", doc.canvas_width)?;
    out.set_item("canvas_height", doc.canvas_height)?;
    out.set_item("dpi", doc.dpi)?;
    let int = py.import("builtins")?.getattr("int")?;
    for (field, text) in &doc.big {
        out.set_item(*field, int.call1((text,))?)?;
    }
    let blobs = PyList::empty(py);
    for b in &doc.blobs {
        blobs.append((b.name.as_str(), b.file_off, b.record_size, b.flag, b.payload_size, b.aux_size, b.payload_off))?;
    }
    out.set_item("blobs", blobs)?;
    let layers = PyList::empty(py);
    for l in &doc.layers {
        let d = PyDict::new(py);
        d.set_item("name", &l.name)?;
        d.set_item("width", l.width)?;
        d.set_item("height", l.height)?;
        d.set_item("mode", &l.mode)?;
        d.set_item("alpha", l.alpha)?;
        d.set_item("layer_id", l.layer_id)?;
        d.set_item("parent_id", l.parent_id)?;
        d.set_item("bin_name", &l.bin_name)?;
        d.set_item("type", &l.kind)?;
        d.set_item("visible", l.visible)?;
        d.set_item("offset_x", l.offset_x)?;
        d.set_item("offset_y", l.offset_y)?;
        d.set_item("clipping", l.clipping)?;
        d.set_item("protect_alpha", l.protect_alpha)?;
        d.set_item("folder_open", l.folder_open)?;
        d.set_item("bin_type", &l.bin_type)?;
        d.set_item("color", &l.color)?;
        d.set_item("masking", l.masking)?;
        d.set_item("masking_type", l.masking_type)?;
        d.set_item("draft", l.draft)?;
        d.set_item("lumi", l.lumi)?;
        d.set_item("transform_offset_x", l.transform_offset_x)?;
        d.set_item("transform_offset_y", l.transform_offset_y)?;
        d.set_item("halftone_type", &l.halftone_type)?;
        d.set_item("halftone_line", l.halftone_line)?;
        d.set_item("text_lines", &l.text_lines)?;
        let attrs = PyDict::new(py);
        for (k, v) in &l.attributes {
            attrs.set_item(k, v)?;
        }
        d.set_item("attributes", attrs)?;
        d.set_item("children", &l.children)?;
        d.set_item("renderable", l.renderable)?;
        // Integers past i64, as Python's exact big ints.
        for (field, text) in &l.big {
            d.set_item(*field, int.call1((text,))?)?;
        }
        layers.append(d)?;
    }
    out.set_item("layers", layers)?;
    Ok(out)
}

/// ``LayerCrop.from_full(render_layer_rgba(mdp, layer, apply_opacity=False))``
/// of layer ``index``: ``(width, height, rect, rgba)`` with ``rect`` the
/// painted rectangle ``(x0, y0, x1, y1)`` (None when nothing is painted) and
/// ``rgba`` its pixels. ValueError where Python raised.
#[pyfunction]
#[allow(clippy::type_complexity)]
pub fn mdp2_layer_pixels<'py>(
    py: Python<'py>,
    raw: &[u8],
    index: usize,
) -> PyResult<(usize, usize, Option<(usize, usize, usize, usize)>, Bound<'py, PyBytes>)> {
    let doc = open(py, raw)?;
    let index = layer_index(&doc, index)?;
    let layer = &doc.layers[index];
    let (w, h) = image_size(layer).map_err(PyValueError::new_err)?;
    if !layer.renderable {
        return Ok((w, h, None, PyBytes::new(py, b"")));
    }
    let plan = py.allow_threads(|| plan_layer(&doc, raw, index, Strictness::Image)).map_err(PyValueError::new_err)?;
    let Some(rect) = plan.rect else { return Ok((w, h, None, PyBytes::new(py, b""))) };
    // Decoded straight into the bytes object (zero-filled by PyO3).
    let len = plan.rgba_len(None).map_err(PyValueError::new_err)?;
    let out = PyBytes::new_with(py, len, |buf| {
        py.allow_threads(|| plan.fill(buf));
        Ok(())
    })?;
    Ok((w, h, Some(rect), out))
}

/// ``replace_layer_pixels`` after the selector and the PNG decode: layer
/// ``index``'s pixels replaced by ``rgba`` (``width`` x ``height``, at most
/// the layer's size). The new file's bytes, or None when the pixels equal
/// the layer's (the file stays as it is). ValueError where Python raised.
#[pyfunction]
pub fn mdp2_write_layer<'py>(
    py: Python<'py>,
    raw: &[u8],
    index: usize,
    rgba: &[u8],
    width: usize,
    height: usize,
) -> PyResult<Option<Bound<'py, PyBytes>>> {
    let doc = open(py, raw)?;
    let index = layer_index(&doc, index)?;
    let out = py
        .allow_threads(|| tmi_clip::mdp2_write::replace_layer_pixels(&doc, raw, index, rgba, width, height))
        .map_err(PyValueError::new_err)?;
    Ok(out.map(|b| PyBytes::new(py, &b)))
}

/// ``render_composite_rgba``: the canvas RGBA of the layers ``subset``
/// (indices, in order; all when None) over ``background``. ValueError where
/// Python raised.
#[pyfunction]
#[pyo3(signature = (raw, background, subset=None))]
pub fn render_mdp2<'py>(
    py: Python<'py>,
    raw: &[u8],
    background: (u8, u8, u8, u8),
    subset: Option<Vec<usize>>,
) -> PyResult<(usize, usize, Bound<'py, PyBytes>)> {
    let doc = open(py, raw)?;
    if let Some(s) = &subset {
        for &i in s {
            layer_index(&doc, i)?;
        }
    }
    let (w, h) = tmi_render::mdp2::canvas_size(&doc).map_err(PyValueError::new_err)?;
    let bg = [background.0, background.1, background.2, background.3];
    let mut result = Ok(());
    let out = PyBytes::new_with(py, w * h * 4, |buf: &mut [u8]| {
        result = py.allow_threads(|| tmi_render::mdp2::render_into(&doc, raw, bg, subset.as_deref(), buf));
        Ok(())
    })?;
    result.map_err(PyValueError::new_err)?;
    Ok((w, h, out))
}
