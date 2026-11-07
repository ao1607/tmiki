//! CLIP shims of stage 2c: the CanvasPreview decode and resize, and the
//! layer write-back.

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::PyBytes;

use tmi_pillow::image::{Image, Mode};
use tmi_pillow::resample::Filter;
use tmi_pillow::resize::resize;

/// ``replace_layer_pixels`` after the layer lookup, the attribute check and
/// the PNG decode: ``rgba`` (``width`` x ``height``) replaces the pixels of
/// offscreen ``offscreen_id`` (``attr_blob``, ``outer_id``) in the CLIP file
/// ``raw``. Returns ``(new_bytes, decode_failure, unknown_sizes)``:
/// ``new_bytes`` None when the file stays as it is; ``decode_failure`` why
/// the current pixels did not decode (Python warned and wrote every block
/// anew); ``unknown_sizes`` the unknown tile encodings met (Python warned).
/// ValueError where Python raised.
#[pyfunction]
#[pyo3(signature = (raw, offscreen_id, attr_blob, outer_id, rgba, width, height, invalidate_preview_on_noop=false))]
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
pub fn clip_write_layer<'py>(
    py: Python<'py>,
    raw: &[u8],
    offscreen_id: i64,
    attr_blob: &[u8],
    outer_id: &str,
    rgba: &[u8],
    width: usize,
    height: usize,
    invalidate_preview_on_noop: bool,
) -> PyResult<(Option<Bound<'py, PyBytes>>, Option<String>, Vec<(usize, usize)>)> {
    let out = py
        .allow_threads(|| {
            tmi_clip::write::replace_layer_pixels(raw, offscreen_id, attr_blob, outer_id, rgba, width, height, invalidate_preview_on_noop)
        })
        .map_err(PyValueError::new_err)?;
    Ok((out.bytes.map(|b| PyBytes::new(py, &b)), out.decode_failure, out.unknown_sizes))
}

/// Whether ``a[a_off:a_off + size] == b[b_off:b_off + size]`` (both in
/// range), without copying either slice (clip_partial's payload check).
#[pyfunction]
pub fn same_bytes(py: Python<'_>, a: &[u8], a_off: usize, b: &[u8], b_off: usize, size: usize) -> PyResult<bool> {
    let (Some(x), Some(y)) = (
        a_off.checked_add(size).and_then(|e| a.get(a_off..e)),
        b_off.checked_add(size).and_then(|e| b.get(b_off..e)),
    ) else {
        return Err(PyValueError::new_err("range outside the bytes"));
    };
    Ok(py.allow_threads(|| x == y))
}

/// ``Image.open(data).convert("RGBA")`` of a CanvasPreview (PNG or JPEG, as
/// tmi_image decodes them to Pillow's pixels): ``(width, height, rgba)``,
/// or None for anything tmi_image does not decode itself (other formats,
/// APNG, damaged data): the caller then asks Pillow, which decides.
#[pyfunction]
pub fn decode_canvas_preview<'py>(py: Python<'py>, data: &[u8]) -> Option<(usize, usize, Bound<'py, PyBytes>)> {
    let image = py.allow_threads(|| tmi_image::decode_rgba(data)).ok()?.0;
    Some((image.width, image.height, PyBytes::new(py, &image.data)))
}

/// ``image.resize(size, LANCZOS)`` of an RGBA image, once to ``size`` and,
/// when ``then`` differs from it, again to ``then`` (the CanvasPreview at
/// its declared size, then at the canvas size): ``(width, height, rgba)``.
#[pyfunction]
#[pyo3(signature = (rgba, width, height, size, then=None))]
pub fn resize_canvas_preview<'py>(
    py: Python<'py>,
    rgba: &[u8],
    width: usize,
    height: usize,
    size: (i64, i64),
    then: Option<(i64, i64)>,
) -> PyResult<(usize, usize, Bound<'py, PyBytes>)> {
    if width.checked_mul(height).and_then(|p| p.checked_mul(4)) != Some(rgba.len()) {
        return Err(PyValueError::new_err("RGBA bytes do not match the size"));
    }
    let mut steps = vec![size];
    if let Some(t) = then.filter(|&t| t != size) {
        steps.push(t);
    }
    let result = py.allow_threads(|| {
        let mut image = Image::new(Mode::RGBA, width, height, rgba.to_vec());
        for (w, h) in steps {
            if (w, h) == (image.width as i64, image.height as i64) {
                continue;
            }
            if w <= 0 || h <= 0 {
                return Err("height and width must be > 0".to_string());
            }
            image = resize(&image, (w as usize, h as usize), Filter::Lanczos, None, None).map_err(|e| format!("{e:?}"))?;
        }
        Ok(image)
    });
    let image = result.map_err(PyValueError::new_err)?;
    Ok((image.width, image.height, PyBytes::new(py, &image.data)))
}
