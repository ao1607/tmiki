//! `tmi_accel._test`: the renderer's building blocks exposed one by one, for
//! the differential tests against Pillow and numpy (tests/test_rust_kernels.py).
//! Not used by tmi itself.

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::PyBytes;

/// `ImageChops.multiply` of two equally long "L" planes.
#[pyfunction]
fn multiply(py: Python<'_>, a: &[u8], b: &[u8]) -> PyResult<Py<PyBytes>> {
    if a.len() != b.len() {
        return Err(PyValueError::new_err("planes differ in length"));
    }
    let mut out = a.to_vec();
    tmi_pillow::chops::multiply_into(&mut out, b);
    Ok(PyBytes::new(py, &out).unbind())
}

/// `dst.paste(src, None, mask)` for RGBA `dst` / `src` and an "L" `mask`.
#[pyfunction]
fn paste_mask_l(py: Python<'_>, dst: &[u8], src: &[u8], mask: &[u8]) -> PyResult<Py<PyBytes>> {
    if dst.len() != src.len() || dst.len() != mask.len() * 4 {
        return Err(PyValueError::new_err("sizes differ"));
    }
    let mut out = dst.to_vec();
    tmi_pillow::paste::paste_rgba_mask_l(&mut out, src, mask);
    Ok(PyBytes::new(py, &out).unbind())
}

/// `getchannel("A").getbbox()` of an RGBA image `width` pixels wide.
#[pyfunction]
fn alpha_bbox(rgba: &[u8], width: usize) -> Option<(usize, usize, usize, usize)> {
    tmi_pillow::alpha_bbox(rgba, width)
}

#[pyfunction]
fn interp(x: Vec<f64>, xp: Vec<f64>, fp: Vec<f64>) -> PyResult<Vec<f64>> {
    if xp.is_empty() || xp.len() != fp.len() {
        return Err(PyValueError::new_err("xp and fp must be non-empty and equally long"));
    }
    Ok(tmi_numpy::interp(&x, &xp, &fp))
}

#[pyfunction]
fn linspace(start: f64, stop: f64, num: usize) -> Vec<f64> {
    tmi_numpy::linspace(start, stop, num)
}

/// Python's `a % b` for each `a`.
#[pyfunction]
fn py_mod(a: Vec<f64>, b: f64) -> Vec<f64> {
    a.into_iter().map(|v| tmi_numpy::py_mod(v, b)).collect()
}

/// A FilterLayerInfo blob decoded: ("lut", [768 floats]), ("colour", None),
/// ("unsupported", id) or None.
#[pyfunction]
fn filter_lut(blob: &[u8]) -> (String, Option<Vec<f32>>, i64) {
    use tmi_render::filter::{decode, Decoded, Filter};
    match decode(blob) {
        Decoded::None => ("none".into(), None, 0),
        Decoded::Unsupported(k) => ("unsupported".into(), None, k as i64),
        Decoded::Filter(Filter::Lut(l)) => ("lut".into(), Some(l), 0),
        Decoded::Filter(_) => ("colour".into(), None, 0),
    }
}

/// A colour-mixing filter on each RGB triple of `rgb` (n * 3 bytes):
/// the clipped float32 results, flattened.
#[pyfunction]
fn filter_colours(blob: &[u8], rgb: &[u8]) -> Option<Vec<f32>> {
    use tmi_render::filter::{decode, Decoded};
    let Decoded::Filter(f) = decode(blob) else {
        return None;
    };
    Some(rgb.chunks_exact(3).flat_map(|c| f.colour([c[0], c[1], c[2]])).collect())
}

/// `_style_colours` / the single-channel fill of `apply_layer_style` on
/// each RGB triple: (RGB bytes, alpha factors, 255 = unchanged).
#[pyfunction]
#[pyo3(signature = (rgb, ct, black, white, main=None, sub=None, profile=None))]
#[allow(clippy::too_many_arguments)]
fn style_colours(
    rgb: &[u8],
    ct: i64,
    black: bool,
    white: bool,
    main: Option<(u8, u8, u8)>,
    sub: Option<(u8, u8, u8)>,
    profile: Option<String>,
) -> (Vec<u8>, Vec<u32>) {
    use tmi_render::style::{style_one, LayerColour};
    let lc = main.map(|m| LayerColour { main: [m.0, m.1, m.2], sub: sub.map(|s| [s.0, s.1, s.2]).unwrap_or([255; 3]) });
    let mut out = Vec::with_capacity(rgb.len());
    let mut drop = Vec::with_capacity(rgb.len() / 3);
    for c in rgb.chunks_exact(3) {
        let (o, d) = style_one([c[0], c[1], c[2]], ct, black, white, lc, profile.as_deref());
        out.extend_from_slice(&o);
        drop.push(d);
    }
    (out, drop)
}

/// An "L" image `width` x `height` with `polygons` (point lists) then
/// `ellipses` (x0, y0, x1, y1) filled with 255 (`ImageDraw`), as bytes; with
/// `region`, only that rectangle of it.
#[pyfunction]
#[pyo3(signature = (width, height, polygons, ellipses, region=None))]
fn draw_l(
    py: Python<'_>,
    width: i32,
    height: i32,
    polygons: Vec<Vec<(f64, f64)>>,
    ellipses: Vec<(f64, f64, f64, f64)>,
    region: Option<(i32, i32, i32, i32)>,
) -> PyResult<Py<PyBytes>> {
    use tmi_pillow::draw::{coord_ok, ellipse, polygon, LCanvas};
    if polygons.iter().flatten().any(|&(x, y)| !coord_ok(x) || !coord_ok(y)) {
        return Err(PyValueError::new_err("coordinate out of range"));
    }
    let r = region.map(|r| [r.0, r.1, r.2, r.3]).unwrap_or([0, 0, width, height]);
    let mut c = LCanvas::new(width, height, r);
    for p in &polygons {
        polygon(&mut c, p);
    }
    for &(x0, y0, x1, y1) in &ellipses {
        if x1 < x0 || y1 < y0 {
            return Err(PyValueError::new_err("ellipse box is not ordered"));
        }
        ellipse(&mut c, [x0, y0, x1, y1]);
    }
    Ok(PyBytes::new(py, &c.px).unbind())
}

/// `Image.reduce(n)` of an "L" image with sides multiple of n.
#[pyfunction]
fn reduce_l(py: Python<'_>, data: &[u8], width: usize, height: usize, n: usize) -> PyResult<Py<PyBytes>> {
    if n == 0 || width % n != 0 || height % n != 0 || data.len() != width * height {
        return Err(PyValueError::new_err("bad size"));
    }
    Ok(PyBytes::new(py, &tmi_pillow::reduce::reduce(data, width, height, n)).unbind())
}

/// CPython's `math.hypot(x, y)` for each pair.
#[pyfunction]
fn py_hypot(x: Vec<f64>, y: Vec<f64>) -> Vec<f64> {
    x.iter().zip(&y).map(|(&a, &b)| tmi_numpy::py_hypot(a, b)).collect()
}

/// libm's `hypot` (what `np.hypot` calls) for each pair.
#[pyfunction]
fn libm_hypot(x: Vec<f64>, y: Vec<f64>) -> Vec<f64> {
    x.iter().zip(&y).map(|(&a, &b)| a.hypot(b)).collect()
}

/// clip_vector's `_draw_stroke(xy, r, bounds, blur)` (blur: EDGE_BLUR or
/// 0) then its opacity table: (bytes, x0, y0, x1, y1) or None.
#[pyfunction]
fn vector_stroke(
    py: Python<'_>,
    xy: Vec<(f64, f64)>,
    r: Vec<f64>,
    bounds: [i64; 4],
    blur: bool,
    opacity: f64,
) -> PyResult<Option<(Py<PyBytes>, i64, i64, i64, i64)>> {
    if xy.len() != r.len() || xy.iter().any(|&(x, y)| !tmi_pillow::draw::coord_ok(x) || !tmi_pillow::draw::coord_ok(y)) {
        return Err(PyValueError::new_err("bad stroke"));
    }
    let xy: Vec<[f64; 2]> = xy.into_iter().map(|(x, y)| [x, y]).collect();
    Ok(tmi_render::clip::vector::draw_stroke_coverage(&xy, &r, bounds, blur, opacity)
        .map(|(px, b)| (PyBytes::new(py, &px).unbind(), b[0], b[1], b[2], b[3])))
}

/// clip_frame's `_rasterize(polys, bounds)`: float32 coverage, flattened.
#[pyfunction]
fn frame_rasterize(polys: Vec<Vec<(f64, f64)>>, bounds: [i64; 4]) -> Vec<f32> {
    tmi_render::clip::frame::rasterize_all(&polys, bounds)
}

/// `np.add.reduceat(a, indices)` (strictly increasing indices).
#[pyfunction]
fn add_reduceat(a: Vec<f64>, indices: Vec<usize>) -> Vec<f64> {
    tmi_numpy::sum::add_reduceat(&a, &indices)
}

/// One C-runtime function over `x` (`y` the second argument of pow /
/// hypot): "cos", "sin", "pow", "cbrt", "hypot", "exp", "sqrt".
#[pyfunction]
#[pyo3(signature = (name, x, y=None))]
fn libm(name: &str, x: Vec<f64>, y: Option<Vec<f64>>) -> PyResult<Vec<f64>> {
    let two = |f: fn(f64, f64) -> f64| -> PyResult<Vec<f64>> {
        let y = y.as_ref().ok_or_else(|| PyValueError::new_err("needs y"))?;
        if y.len() != x.len() {
            return Err(PyValueError::new_err("x and y differ in length"));
        }
        Ok(x.iter().zip(y).map(|(&a, &b)| f(a, b)).collect())
    };
    let one = |f: fn(f64) -> f64| -> PyResult<Vec<f64>> { Ok(x.iter().map(|&a| f(a)).collect()) };
    match name {
        "cos" => one(f64::cos),
        "sin" => one(f64::sin),
        "cbrt" => one(f64::cbrt),
        "exp" => one(f64::exp),
        "sqrt" => one(f64::sqrt),
        "pow" => two(f64::powf),
        "hypot" => two(f64::hypot),
        _ => Err(PyValueError::new_err("unknown function")),
    }
}

/// `x @ m.T` of an (n, 3) array (flattened) and a 3 x 3 matrix (row major).
#[pyfunction]
fn matmul3(x: Vec<f64>, m: [[f64; 3]; 3]) -> Vec<f64> {
    x.chunks_exact(3).flat_map(|r| tmi_numpy::matmul3([r[0], r[1], r[2]], &m)).collect()
}

/// Whether `matmul3` reproduces numpy's matmul on this CPU (else the
/// renderer declines the gradient modes that need it).
#[pyfunction]
fn fma_kernels() -> bool {
    tmi_numpy::linalg::fma_kernels()
}

/// `np.linalg.inv` of a 2 x 2 matrix, or None where it is declined.
#[pyfunction]
fn inv2(m: [[f64; 2]; 2]) -> Option<[[f64; 2]; 2]> {
    tmi_numpy::inv2(m)
}

/// clip_gradient's matrices as Rust holds them (LMS, LMS_INV, OK_M1, OK_M2,
/// OK_M1_INV, OK_M2_INV).
#[pyfunction]
fn gradient_matrices() -> Vec<[[f64; 3]; 3]> {
    tmi_render::clip::gradient::matrices().to_vec()
}

/// `gradient_mix(stops, t, mode, level)`, flattened (n * 4).
#[pyfunction]
fn gradient_mix(stops: Vec<(f64, (u8, u8, u8, u8))>, t: Vec<f64>, mode: u32, level: u32) -> Vec<f64> {
    let stops: Vec<(f64, [u8; 4])> = stops.into_iter().map(|(p, c)| (p, [c.0, c.1, c.2, c.3])).collect();
    tmi_render::clip::gradient::gradient_mix(&stops, &t, mode, level).into_iter().flatten().collect()
}

/// `render_linear_gradient(stops, points, canvas, mode, level, region)`:
/// RGBA bytes, or None where declined.
#[pyfunction]
#[allow(clippy::too_many_arguments)]
fn linear_gradient(
    py: Python<'_>,
    stops: Vec<(f64, (u8, u8, u8, u8))>,
    points: [f64; 4],
    canvas: (usize, usize),
    mode: u32,
    level: u32,
    region: [i64; 4],
) -> Option<Py<PyBytes>> {
    let stops: Vec<(f64, [u8; 4])> = stops.into_iter().map(|(p, c)| (p, [c.0, c.1, c.2, c.3])).collect();
    let buf = tmi_render::clip::gradient::render_linear(&stops, points, canvas, mode, level, region).ok()?;
    Some(PyBytes::new(py, &buf.px).unbind())
}

/// `get_screen(tone, dpi).index(x0, y0, w, h)` (shape "circle" / "square" /
/// "line"), flattened, or None where declined; plus N and n.
#[pyfunction]
#[allow(clippy::too_many_arguments)]
#[pyo3(signature = (lpi, angle, shape, dpi, x0, y0, w, h, unk16=0))]
fn tone_index(lpi: f64, angle: f64, shape: &str, dpi: f64, x0: i64, y0: i64, w: i64, h: i64, unk16: u32) -> Option<(Vec<i64>, i64)> {
    use tmi_render::clip::tone::{Screen, Shape, Tone};
    let shape = match shape {
        "square" => Shape::Square,
        "line" => Shape::Line,
        _ => Shape::Circle,
    };
    let screen = Screen::new(&Tone { lpi, angle, shape, u16: unk16 }, dpi).ok()?;
    let mut out = Vec::with_capacity((w * h) as usize);
    for y in y0..y0 + h {
        for x in x0..x0 + w {
            out.push(screen.index(x, y)?);
        }
    }
    Some((out, screen.levels))
}

/// `apply_tone(image, x, y, tone, dpi)` on RGBA bytes `width` wide.
#[pyfunction]
#[allow(clippy::too_many_arguments)]
#[pyo3(signature = (rgba, width, x, y, lpi, angle, shape, dpi, unk16=0))]
fn apply_tone(py: Python<'_>, rgba: &[u8], width: usize, x: i64, y: i64, lpi: f64, angle: f64, shape: &str, dpi: f64, unk16: u32) -> Option<Py<PyBytes>> {
    use tmi_render::buffer::Buf;
    use tmi_render::clip::tone::{Shape, Tone};
    let shape = match shape {
        "square" => Shape::Square,
        "line" => Shape::Line,
        _ => Shape::Circle,
    };
    let buf = Buf { x, y, w: width, h: rgba.len() / 4 / width.max(1), px: rgba.to_vec() };
    let out = tmi_render::clip::tone::apply_tone(&buf, &Tone { lpi, angle, shape, u16: unk16 }, dpi).ok()?;
    Some(PyBytes::new(py, &out.px).unbind())
}

/// `apply_edge(buf, width, rgb, canvas_bounds)` of an RGBA buffer at
/// (x, y): (bytes, x, y, w, h), or None where declined.
#[pyfunction]
#[allow(clippy::too_many_arguments)]
fn apply_edge(
    py: Python<'_>,
    rgba: &[u8],
    width: usize,
    x: i64,
    y: i64,
    edge_width: f64,
    rgb: (u8, u8, u8),
    canvas: [i64; 4],
) -> Option<(Py<PyBytes>, i64, i64, usize, usize)> {
    use tmi_render::buffer::Buf;
    use tmi_render::clip::edge::Edge;
    let buf = Buf { x, y, w: width, h: rgba.len() / 4 / width.max(1), px: rgba.to_vec() };
    let out = tmi_render::clip::edge::apply_edge(buf, &Edge { width: edge_width, rgb: [rgb.0, rgb.1, rgb.2] }, canvas).ok()?;
    Some((PyBytes::new(py, &out.px).unbind(), out.x, out.y, out.w, out.h))
}

/// `place_material` (`tile` false) or `_sample_tiled` (true) of an RGBA
/// original `width` wide: (bytes, x, y, w, h), "none" when off the window
/// (place_material's None), or None where declined.
#[pyfunction]
fn material(
    py: Python<'_>,
    rgba: &[u8],
    width: usize,
    corners: [[f64; 2]; 3],
    window: [i64; 4],
    tile: bool,
) -> PyResult<Option<(Py<PyBytes>, i64, i64, usize, usize)>> {
    use std::sync::Arc;
    use tmi_render::clip::material::{place_material, sample_tiled, Original};
    let src = Original { w: width, h: rgba.len() / 4 / width.max(1), px: rgba.to_vec(), profile: String::new() };
    let got = if tile { sample_tiled(&src, corners, window) } else { place_material(&src, corners, window, false, &|l, b| Arc::new(src.plane(l, b))) };
    match got {
        Err(_) => Ok(None),
        Ok(None) => Err(PyValueError::new_err("none")),
        Ok(Some(b)) => Ok(Some((PyBytes::new(py, &b.px).unbind(), b.x, b.y, b.w, b.h))),
    }
}

pub fn module(py: Python<'_>) -> PyResult<Bound<'_, PyModule>> {
    let m = PyModule::new(py, "_test")?;
    m.add_function(wrap_pyfunction!(draw_l, &m)?)?;
    m.add_function(wrap_pyfunction!(reduce_l, &m)?)?;
    m.add_function(wrap_pyfunction!(py_hypot, &m)?)?;
    m.add_function(wrap_pyfunction!(libm_hypot, &m)?)?;
    m.add_function(wrap_pyfunction!(add_reduceat, &m)?)?;
    m.add_function(wrap_pyfunction!(vector_stroke, &m)?)?;
    m.add_function(wrap_pyfunction!(frame_rasterize, &m)?)?;
    m.add_function(wrap_pyfunction!(multiply, &m)?)?;
    m.add_function(wrap_pyfunction!(paste_mask_l, &m)?)?;
    m.add_function(wrap_pyfunction!(alpha_bbox, &m)?)?;
    m.add_function(wrap_pyfunction!(interp, &m)?)?;
    m.add_function(wrap_pyfunction!(linspace, &m)?)?;
    m.add_function(wrap_pyfunction!(py_mod, &m)?)?;
    m.add_function(wrap_pyfunction!(filter_lut, &m)?)?;
    m.add_function(wrap_pyfunction!(filter_colours, &m)?)?;
    m.add_function(wrap_pyfunction!(style_colours, &m)?)?;
    m.add_function(wrap_pyfunction!(libm, &m)?)?;
    m.add_function(wrap_pyfunction!(matmul3, &m)?)?;
    m.add_function(wrap_pyfunction!(fma_kernels, &m)?)?;
    m.add_function(wrap_pyfunction!(inv2, &m)?)?;
    m.add_function(wrap_pyfunction!(gradient_matrices, &m)?)?;
    m.add_function(wrap_pyfunction!(gradient_mix, &m)?)?;
    m.add_function(wrap_pyfunction!(linear_gradient, &m)?)?;
    m.add_function(wrap_pyfunction!(tone_index, &m)?)?;
    m.add_function(wrap_pyfunction!(apply_tone, &m)?)?;
    m.add_function(wrap_pyfunction!(apply_edge, &m)?)?;
    m.add_function(wrap_pyfunction!(material, &m)?)?;
    crate::testing_image::register(&m)?;
    crate::testing_writeback::register(&m)?;
    Ok(m)
}
