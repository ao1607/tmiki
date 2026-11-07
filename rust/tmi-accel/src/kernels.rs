//! pyo3 shims over the kernels in the workspace crates: argument checks,
//! GIL release and bytes conversion only.

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyList, PyTuple};

use tmi_render::blend::{BlendMode, SUPPORTED_BLEND_MODES};
use tmi_render::composite::{Op, OP_BLEND, OP_CLEAR, OP_OVER};

#[pyfunction]
pub fn supported_blend_modes() -> Vec<&'static str> {
    SUPPORTED_BLEND_MODES.to_vec()
}

#[pyfunction]
#[pyo3(signature = (target_rgba, source_rgba, width, height, mode, opacity))]
pub fn blend_rgba(
    py: Python<'_>,
    target_rgba: &[u8],
    source_rgba: &[u8],
    width: usize,
    height: usize,
    mode: &str,
    opacity: f32,
) -> PyResult<Option<Py<PyBytes>>> {
    let Some(mode) = BlendMode::from_name(mode) else {
        return Ok(None);
    };
    let len = width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| PyValueError::new_err("image dimensions overflow"))?;
    if target_rgba.len() != len || source_rgba.len() != len {
        return Err(PyValueError::new_err("RGBA buffers do not match width and height"));
    }
    let mut out = vec![0u8; len];
    // Release the GIL: the importer composites on the main thread while the
    // layer pool's Python threads run.
    py.allow_threads(|| tmi_render::blend::blend_rgba(&mut out, target_rgba, source_rgba, width, mode, opacity));
    Ok(Some(PyBytes::new(py, &out).unbind()))
}

/// A CLIP offscreen's tiles decoded on all cores: ``(rgba, x, y, w, h,
/// unknown_sizes)``, the RGBA of the alpha bbox at (x, y) (``w == h == 0``
/// when nothing is painted) and the unknown tile encodings met as
/// ``(bytes per tile, pixels)`` (drawn transparent; tmi warns). Raises
/// ValueError when the offscreen is unreadable (feature "offscreen-v2";
/// older builds returned None for Python to decode instead).
#[pyfunction]
#[pyo3(signature = (raw, payload_off, payload_size, tile_cols, tile_rows, pixel_w, pixel_h))]
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
pub fn decode_clip_offscreen(
    py: Python<'_>,
    raw: &[u8],
    payload_off: usize,
    payload_size: usize,
    tile_cols: usize,
    tile_rows: usize,
    pixel_w: usize,
    pixel_h: usize,
) -> PyResult<(Py<PyBytes>, usize, usize, usize, usize, Vec<(usize, usize)>)> {
    let result = py.allow_threads(|| {
        tmi_clip::tiles::decode_offscreen(raw, payload_off, payload_size, tile_cols, tile_rows, pixel_w, pixel_h)
    });
    let d = result.map_err(PyValueError::new_err)?;
    Ok((PyBytes::new(py, &d.rgba).unbind(), d.x, d.y, d.w, d.h, d.unknown_sizes))
}

#[pyfunction]
#[pyo3(signature = (rgba, width, height, text=None))]
pub fn encode_png_rgba(
    py: Python<'_>,
    rgba: &[u8],
    width: u32,
    height: u32,
    text: Option<(String, String)>,
) -> PyResult<Py<PyBytes>> {
    tmi_codec::png_enc::check_args(rgba, width, height, text.as_ref()).map_err(PyValueError::new_err)?;
    let out = py
        .allow_threads(|| tmi_codec::png_enc::encode(rgba, width, height, text.as_ref()))
        .map_err(|exc| PyValueError::new_err(format!("png encode failed: {exc}")))?;
    Ok(PyBytes::new(py, &out).unbind())
}

#[pyfunction]
#[pyo3(signature = (rgba, width, height, lut, opacity, mask=None, base_alpha=None))]
#[allow(clippy::too_many_arguments)]
pub fn regrade_rgba(
    py: Python<'_>,
    rgba: &[u8],
    width: usize,
    height: usize,
    lut: Vec<f32>,
    opacity: f32,
    mask: Option<&[u8]>,
    base_alpha: Option<&[u8]>,
) -> PyResult<Py<PyBytes>> {
    let pixels = width * height;
    if rgba.len() != pixels * 4 {
        return Err(PyValueError::new_err("rgba length does not match width*height*4"));
    }
    if lut.len() != 3 * 256 {
        return Err(PyValueError::new_err("lut must hold 3x256 entries"));
    }
    for plane in [mask, base_alpha].into_iter().flatten() {
        if plane.len() != pixels {
            return Err(PyValueError::new_err("mask/base_alpha length does not match width*height"));
        }
    }
    // Written straight into the returned bytes object (no second copy).
    let out = PyBytes::new_with(py, rgba.len(), |out: &mut [u8]| {
        py.allow_threads(|| tmi_render::adjust::regrade(out, rgba, width, &lut, opacity, mask, base_alpha));
        Ok(())
    })?;
    Ok(out.unbind())
}

#[pyfunction]
#[pyo3(signature = (data, start, end, gear, piece_min, piece_max, bits, window))]
#[allow(clippy::too_many_arguments)]
pub fn cdc_cuts(
    py: Python<'_>,
    data: &[u8],
    start: usize,
    end: usize,
    gear: Vec<u64>,
    piece_min: usize,
    piece_max: usize,
    bits: u32,
    window: usize,
) -> PyResult<Vec<usize>> {
    py.allow_threads(|| tmi_codec::cdc::cdc_cuts(data, start, end, &gear, piece_min, piece_max, bits, window))
        .map_err(PyValueError::new_err)
}

fn parse_ops<'py>(
    ops: &Bound<'py, PyList>,
    tw: usize,
    th: usize,
    holders: &mut Vec<Bound<'py, PyBytes>>,
) -> PyResult<Vec<(u8, usize, [usize; 8], BlendMode, f32)>> {
    // (kind, holder index, [src_w, src_h, sx, sy, w, h, dx, dy], mode, opacity)
    let mut out = Vec::with_capacity(ops.len());
    for item in ops.iter() {
        let t = item.downcast::<PyTuple>()?;
        if t.len() != 12 {
            return Err(PyValueError::new_err("an op is a 12-tuple"));
        }
        let kind: u8 = t.get_item(0)?.extract()?;
        let mut g = [0usize; 8];
        for (k, v) in g.iter_mut().enumerate() {
            *v = t.get_item(2 + k)?.extract()?;
        }
        let [src_w, src_h, sx, sy, w, h, dx, dy] = g;
        if dx.checked_add(w).map_or(true, |e| e > tw) || dy.checked_add(h).map_or(true, |e| e > th) {
            return Err(PyValueError::new_err("op rectangle outside the target"));
        }
        let mut mode = BlendMode::Normal;
        let mut opacity = 1.0f32;
        let mut holder = usize::MAX;
        match kind {
            OP_OVER | OP_BLEND => {
                let src = t.get_item(1)?.downcast_into::<PyBytes>()?;
                let need = src_w.checked_mul(src_h).and_then(|p| p.checked_mul(4));
                if need != Some(src.as_bytes().len()) {
                    return Err(PyValueError::new_err("source bytes do not match its size"));
                }
                if sx.checked_add(w).map_or(true, |e| e > src_w) || sy.checked_add(h).map_or(true, |e| e > src_h) {
                    return Err(PyValueError::new_err("op rectangle outside the source"));
                }
                if kind == OP_BLEND {
                    let name: String = t.get_item(10)?.extract()?;
                    mode = BlendMode::from_name(&name)
                        .ok_or_else(|| PyValueError::new_err(format!("unsupported blend mode {name}")))?;
                    opacity = t.get_item(11)?.extract()?;
                }
                holder = holders.len();
                holders.push(src);
            }
            OP_CLEAR => {}
            _ => return Err(PyValueError::new_err("unknown op kind")),
        }
        out.push((kind, holder, g, mode, opacity));
    }
    Ok(out)
}

/// Apply `ops` in order to a `width` x `height` RGBA surface (`target`, or
/// transparent black when None) and return the result.
///
/// Each op is `(kind, source, src_w, src_h, sx, sy, w, h, dx, dy, mode,
/// opacity)`: the `w` x `h` rectangle at (`sx`, `sy`) of the RGBA `source`
/// (`src_w` x `src_h`) lands at (`dx`, `dy`). `kind` 0 = Pillow
/// alpha_composite (mode/opacity unused), 1 = blend in `mode` at `opacity`,
/// 2 = clear the transparent pixels of the rectangle (`source` None).
#[pyfunction]
#[pyo3(signature = (target, width, height, ops))]
pub fn composite_ops<'py>(
    py: Python<'py>,
    target: Option<&[u8]>,
    width: usize,
    height: usize,
    ops: &Bound<'py, PyList>,
) -> PyResult<Py<PyBytes>> {
    let len = width
        .checked_mul(height)
        .and_then(|p| p.checked_mul(4))
        .ok_or_else(|| PyValueError::new_err("image dimensions overflow"))?;
    if let Some(t) = target {
        if t.len() != len {
            return Err(PyValueError::new_err("target bytes do not match width and height"));
        }
    }
    let mut holders = Vec::new();
    let parsed = parse_ops(ops, width, height, &mut holders)?;
    let slices: Vec<&[u8]> = holders.iter().map(|b| b.as_bytes()).collect();
    let ops: Vec<Op<'_>> = parsed
        .into_iter()
        .map(|(kind, holder, g, mode, opacity)| Op {
            kind,
            src: if holder == usize::MAX { &[] } else { slices[holder] },
            src_w: g[0],
            sx: g[2],
            sy: g[3],
            w: g[4],
            h: g[5],
            dx: g[6],
            dy: g[7],
            mode,
            opacity,
        })
        .collect();
    let out = PyBytes::new_with(py, len, |buf: &mut [u8]| {
        py.allow_threads(|| {
            match target {
                Some(t) => buf.copy_from_slice(t),
                None => buf.fill(0),
            }
            tmi_render::composite::apply_ops(buf, width, height, &ops);
        });
        Ok(())
    })?;
    Ok(out.unbind())
}

/// TMIL blob of a `width` x `height` RGBA rectangle at (`x0`, `y0`) in a layer
/// of `layer_width` x `layer_height` (an empty rectangle: an all-zero layer).
#[pyfunction]
#[pyo3(signature = (rgba, width, height, layer_width, layer_height, x0=0, y0=0))]
pub fn encode_layer_blob(
    py: Python<'_>,
    rgba: &[u8],
    width: u32,
    height: u32,
    layer_width: u32,
    layer_height: u32,
    x0: u32,
    y0: u32,
) -> PyResult<Py<PyBytes>> {
    let out = py
        .allow_threads(|| tmi_codec::layer_blob::encode(rgba, width, height, layer_width, layer_height, x0, y0))
        .map_err(PyValueError::new_err)?;
    Ok(PyBytes::new(py, &out).unbind())
}

/// The rectangle's RGBA rows of a TMIL blob (empty for an all-zero layer).
/// ValueError for a corrupt or truncated blob.
#[pyfunction]
pub fn decode_layer_blob(py: Python<'_>, data: &[u8]) -> PyResult<Py<PyBytes>> {
    use tmi_codec::layer_blob::{decode_into, parse};
    let header = parse(data).map_err(|e| PyValueError::new_err(format!("TMIL layer blob: {e}")))?;
    let mut result: Result<(), String> = Ok(());
    let out = PyBytes::new_with(py, 4 * header.w * header.h, |dst: &mut [u8]| {
        result = py.allow_threads(|| decode_into(data, &header, dst));
        Ok(())
    })?;
    result.map_err(|e| PyValueError::new_err(format!("TMIL layer blob: {e}")))?;
    Ok(out.unbind())
}
