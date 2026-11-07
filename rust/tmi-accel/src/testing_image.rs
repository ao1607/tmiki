//! `tmi_accel._test`, stage 2a: the Pillow-exact image kernels (resample,
//! reduce, thumbnail, convert, blend, chops, bbox, extrema) and the codecs
//! (PNG / JPEG decode, JPEG / WebP encode), for the differential tests in
//! tests/test_rust_image_kernels.py. Not used by tmi itself yet.

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyTuple};

use tmi_pillow::image::{Image, Mode, Palette, Transparency};
use tmi_pillow::resample::Filter;

fn err<E: std::fmt::Debug>(e: E) -> PyErr {
    PyValueError::new_err(format!("{e:?}"))
}

fn mode(name: &str) -> PyResult<Mode> {
    Mode::from_name(name).ok_or_else(|| PyValueError::new_err(format!("unknown mode {name}")))
}

fn filter(v: u32) -> PyResult<Filter> {
    Filter::from_pil(v).ok_or_else(|| PyValueError::new_err("unsupported filter"))
}

/// An image from `tobytes()` data ("1" as packed bits).
fn image(m: &str, data: &[u8], w: usize, h: usize) -> PyResult<Image> {
    let m = mode(m)?;
    if m == Mode::One {
        if data.len() != w.div_ceil(8) * h {
            return Err(PyValueError::new_err("bad size"));
        }
        return Ok(Image::from_bits(w, h, data));
    }
    if data.len() != w * h * m.bytes_per_pixel() {
        return Err(PyValueError::new_err("bad size"));
    }
    Ok(Image::new(m, w, h, data.to_vec()))
}

fn out(py: Python<'_>, img: &Image) -> (Py<PyBytes>, String, usize, usize) {
    (PyBytes::new(py, &img.tobytes()).unbind(), img.mode.name().to_string(), img.width, img.height)
}

/// `Image.resize(size, filter, box, reducing_gap)`: (bytes, mode, w, h).
#[pyfunction]
#[pyo3(signature = (mode, data, w, h, size, resample, r#box=None, reducing_gap=None))]
#[allow(clippy::too_many_arguments)]
fn pil_resize(
    py: Python<'_>,
    mode: &str,
    data: &[u8],
    w: usize,
    h: usize,
    size: (usize, usize),
    resample: u32,
    r#box: Option<(f64, f64, f64, f64)>,
    reducing_gap: Option<f64>,
) -> PyResult<(Py<PyBytes>, String, usize, usize)> {
    let img = image(mode, data, w, h)?;
    let f = filter(resample)?;
    let bx = r#box.map(|b| [b.0, b.1, b.2, b.3]);
    let r = py.allow_threads(|| tmi_pillow::resize::resize(&img, size, f, bx, reducing_gap)).map_err(err)?;
    Ok(out(py, &r))
}

/// `Image.reduce(factor, box)`: (bytes, mode, w, h).
#[pyfunction]
#[pyo3(signature = (mode, data, w, h, factor, r#box=None))]
fn pil_reduce(
    py: Python<'_>,
    mode: &str,
    data: &[u8],
    w: usize,
    h: usize,
    factor: (usize, usize),
    r#box: Option<(usize, usize, usize, usize)>,
) -> PyResult<(Py<PyBytes>, String, usize, usize)> {
    let img = image(mode, data, w, h)?;
    let bx = r#box.map(|b| [b.0, b.1, b.2, b.3]);
    let r = py.allow_threads(|| tmi_pillow::resize::reduce(&img, factor, bx)).map_err(err)?;
    Ok(out(py, &r))
}

/// `Image.thumbnail(size, filter, reducing_gap)` of a decoded image (no
/// draft): (bytes, mode, w, h).
#[pyfunction]
#[pyo3(signature = (mode, data, w, h, size, resample=3, reducing_gap=Some(2.0)))]
#[allow(clippy::too_many_arguments)]
fn pil_thumbnail(
    py: Python<'_>,
    mode: &str,
    data: &[u8],
    w: usize,
    h: usize,
    size: (f64, f64),
    resample: u32,
    reducing_gap: Option<f64>,
) -> PyResult<(Py<PyBytes>, String, usize, usize)> {
    let img = image(mode, data, w, h)?;
    let f = filter(resample)?;
    let r = py.allow_threads(|| tmi_pillow::resize::thumbnail(&img, size, f, reducing_gap)).map_err(err)?;
    Ok(out(py, &r))
}

/// The size `thumbnail(size)` picks, or None when it does nothing.
#[pyfunction]
fn pil_thumbnail_size(w: usize, h: usize, size: (f64, f64)) -> Option<(usize, usize)> {
    tmi_pillow::resize::thumbnail_size(w, h, size)
}

fn transparency(t: Option<&Bound<'_, PyAny>>) -> PyResult<Option<Transparency>> {
    let Some(t) = t else { return Ok(None) };
    if t.is_none() {
        return Ok(None);
    }
    if let Ok(b) = t.downcast::<PyBytes>() {
        return Ok(Some(Transparency::Bytes(b.as_bytes().to_vec())));
    }
    if let Ok(tup) = t.downcast::<PyTuple>() {
        let v: (i64, i64, i64) = tup.extract()?;
        return Ok(Some(Transparency::Rgb(v.0, v.1, v.2)));
    }
    Ok(Some(Transparency::Int(t.extract()?)))
}

/// `Image.convert(to)` of an image with an optional RGB palette and
/// `info["transparency"]`: (bytes, mode, w, h).
#[pyfunction]
#[pyo3(signature = (mode, data, w, h, to, palette=None, transparency=None))]
#[allow(clippy::too_many_arguments)]
fn pil_convert(
    py: Python<'_>,
    mode: &str,
    data: &[u8],
    w: usize,
    h: usize,
    to: &str,
    palette: Option<&[u8]>,
    transparency: Option<&Bound<'_, PyAny>>,
) -> PyResult<(Py<PyBytes>, String, usize, usize)> {
    let mut img = image(mode, data, w, h)?;
    img.palette = palette.map(Palette::from_rgb);
    img.transparency = self::transparency(transparency)?;
    let to = self::mode(to)?;
    let r = py.allow_threads(|| tmi_pillow::convert::convert(&img, to)).map_err(err)?;
    Ok(out(py, &r))
}

/// `Image.blend(a, b, alpha)` of two equally long byte buffers.
#[pyfunction]
fn pil_blend(py: Python<'_>, a: &[u8], b: &[u8], alpha: f32) -> PyResult<Py<PyBytes>> {
    if a.len() != b.len() {
        return Err(PyValueError::new_err("sizes differ"));
    }
    let r = py.allow_threads(|| tmi_pillow::blend::blend(a, b, alpha));
    Ok(PyBytes::new(py, &r).unbind())
}

/// `ImageChops.<op>(a, b)` ("difference", "lighter", "darker",
/// "multiply") of two equally long byte buffers.
#[pyfunction]
fn pil_chop(py: Python<'_>, op: &str, a: &[u8], b: &[u8]) -> PyResult<Py<PyBytes>> {
    if a.len() != b.len() {
        return Err(PyValueError::new_err("sizes differ"));
    }
    let r = match op {
        "difference" => tmi_pillow::chops::difference(a, b),
        "lighter" => tmi_pillow::chops::lighter(a, b),
        "darker" => tmi_pillow::chops::darker(a, b),
        "multiply" => {
            let mut o = a.to_vec();
            tmi_pillow::chops::multiply_into(&mut o, b);
            o
        }
        _ => return Err(PyValueError::new_err("unknown op")),
    };
    Ok(PyBytes::new(py, &r).unbind())
}

/// `getbbox(alpha_only=False)` of an image `width` wide with `pixel_bytes`
/// per pixel.
#[pyfunction]
fn pil_bbox_any(data: &[u8], width: usize, pixel_bytes: usize) -> Option<(usize, usize, usize, usize)> {
    tmi_pillow::bbox::bbox_any(data, width, pixel_bytes)
}

/// `getextrema()` per band.
#[pyfunction]
fn pil_extrema(data: &[u8], bands: usize) -> Option<Vec<(u8, u8)>> {
    tmi_pillow::bbox::extrema(data, bands)
}

fn transparency_obj(py: Python<'_>, t: &Option<Transparency>) -> PyObject {
    match t {
        None => py.None(),
        Some(Transparency::Int(v)) => v.into_pyobject(py).unwrap().into_any().unbind(),
        Some(Transparency::Rgb(r, g, b)) => (*r, *g, *b).into_pyobject(py).unwrap().into_any().unbind(),
        Some(Transparency::Bytes(b)) => PyBytes::new(py, b).into_any().unbind(),
    }
}

/// `Image.open(png)`: (mode, w, h, tobytes(), palette as 768 RGB bytes or
/// None, info["transparency"] or None, animated).
#[pyfunction]
#[allow(clippy::type_complexity)]
fn png_open(py: Python<'_>, data: &[u8]) -> PyResult<(String, usize, usize, Py<PyBytes>, Option<Py<PyBytes>>, PyObject, bool)> {
    let d = py.allow_threads(|| tmi_image::png_dec::decode_png(data)).map_err(err)?;
    let img = d.image;
    let pal = img.palette.as_ref().map(|p| {
        let rgb: Vec<u8> = p.entries.iter().flat_map(|e| [e[0], e[1], e[2]]).collect();
        PyBytes::new(py, &rgb).unbind()
    });
    let t = transparency_obj(py, &img.transparency);
    Ok((img.mode.name().to_string(), img.width, img.height, PyBytes::new(py, &img.tobytes()).unbind(), pal, t, d.animated))
}

/// `Image.open(jpeg)`: (mode, w, h, tobytes()).
#[pyfunction]
fn jpeg_open(py: Python<'_>, data: &[u8]) -> PyResult<(String, usize, usize, Py<PyBytes>)> {
    let img = py.allow_threads(|| tmi_image::jpeg::decode_jpeg(data)).map_err(err)?;
    Ok((img.mode.name().to_string(), img.width, img.height, PyBytes::new(py, &img.data).unbind()))
}

/// `Image.open(png_or_jpeg).convert("RGBA")`: (bytes, w, h, "png" / "jpeg").
#[pyfunction]
fn decode_rgba(py: Python<'_>, data: &[u8]) -> PyResult<(Py<PyBytes>, usize, usize, String)> {
    let (img, fmt) = py.allow_threads(|| tmi_image::decode_rgba(data)).map_err(err)?;
    Ok((PyBytes::new(py, &img.data).unbind(), img.width, img.height, fmt.name().to_string()))
}

/// The opened image (`Image.open(data)` + `load()`) of the P0 decoders
/// (`tmi_image::open_any`): (mode, w, h, tobytes(), format, dpi, icc).
/// Errors are ValueErrors starting "declined:", "corrupt:", "too large:"
/// or "unknown:".
type RasterOut = (String, usize, usize, Py<PyBytes>, String, Option<(f64, f64)>, Option<Py<PyBytes>>);

fn raster_err(e: tmi_image::DecodeError) -> PyErr {
    use tmi_image::DecodeError as E;
    PyValueError::new_err(match e {
        E::Declined(s) => format!("declined: {s}"),
        E::Corrupt(f, s) => format!("corrupt: {} {s}", f.name()),
        E::TooLarge(w, h) => format!("too large: {w}x{h}"),
        E::UnknownFormat => "unknown: not a format tmi_image reads".to_string(),
        other => format!("corrupt: {other:?}"),
    })
}

fn raster_out(py: Python<'_>, o: tmi_image::Opened) -> RasterOut {
    (
        o.image.mode.name().to_string(),
        o.image.width,
        o.image.height,
        PyBytes::new(py, &o.image.tobytes()).unbind(),
        o.format.name().to_string(),
        o.info.dpi,
        o.info.icc_profile.as_deref().map(|b| PyBytes::new(py, b).unbind()),
    )
}

/// `Image.open(data)` + `load()` (P0 formats).
#[pyfunction]
#[pyo3(signature = (data, max_pixels=None))]
fn raster_open(py: Python<'_>, data: &[u8], max_pixels: Option<u64>) -> PyResult<RasterOut> {
    let o = py.allow_threads(|| tmi_image::open_any(data, max_pixels)).map_err(raster_err)?;
    Ok(raster_out(py, o))
}

/// `Image.open(data).convert("RGBA")` (P0 formats).
#[pyfunction]
#[pyo3(signature = (data, max_pixels=None))]
fn raster_decode(py: Python<'_>, data: &[u8], max_pixels: Option<u64>) -> PyResult<RasterOut> {
    let o = py.allow_threads(|| tmi_image::decode_rgba_any(data, max_pixels)).map_err(raster_err)?;
    Ok(raster_out(py, o))
}

/// `JpegImageFile.quantization` as a dict {id: [64 values, natural order]}.
#[pyfunction]
fn jpeg_quantization(data: &[u8]) -> std::collections::BTreeMap<u8, Vec<u16>> {
    tmi_image::jpeg::quantization(data).into_iter().map(|(i, t)| (i, t.to_vec())).collect()
}

/// `image.save(buf, "JPEG", quality=, qtables=, subsampling=, optimize=,
/// progressive=)` of an "L" / "RGB" / "CMYK" image; `sampling`,
/// `restart` and `arithmetic` go beyond Pillow, to write test files.
#[pyfunction]
#[pyo3(signature = (mode, data, w, h, quality=-1, qtables=None, subsampling=-1, optimize=false, progressive=false,
                    sampling=None, restart=0, arithmetic=false))]
#[allow(clippy::too_many_arguments)]
fn jpeg_encode(
    py: Python<'_>,
    mode: &str,
    data: &[u8],
    w: usize,
    h: usize,
    quality: i32,
    qtables: Option<Vec<Vec<u16>>>,
    subsampling: i32,
    optimize: bool,
    progressive: bool,
    sampling: Option<Vec<(i32, i32)>>,
    restart: u32,
    arithmetic: bool,
) -> PyResult<Py<PyBytes>> {
    let img = image(mode, data, w, h)?;
    let qtables = match qtables {
        None => None,
        Some(ts) => Some(
            ts.into_iter()
                .map(|t| <[u16; 64]>::try_from(t.as_slice()).map_err(|_| PyValueError::new_err("table must have 64 values")))
                .collect::<PyResult<Vec<_>>>()?,
        ),
    };
    let opts = tmi_image::jpeg::JpegOptions {
        quality,
        qtables,
        subsampling,
        optimize,
        progressive,
        sampling_factors: sampling,
        restart_interval: restart,
        arithmetic,
        ..Default::default()
    };
    let out = py.allow_threads(|| tmi_image::jpeg::encode_jpeg(&img, &opts)).map_err(err)?;
    Ok(PyBytes::new(py, &out).unbind())
}

/// `image.save(buf, "WEBP", quality=, method=, alpha_quality=, lossless=,
/// exact=)` of RGBA (bands 4) or RGB (3) bytes.
#[pyfunction]
#[pyo3(signature = (data, w, h, bands=4, quality=80.0, method=4, alpha_quality=100.0, lossless=false, exact=false))]
#[allow(clippy::too_many_arguments)]
fn webp_encode(
    py: Python<'_>,
    data: &[u8],
    w: usize,
    h: usize,
    bands: usize,
    quality: f32,
    method: i32,
    alpha_quality: f32,
    lossless: bool,
    exact: bool,
) -> PyResult<Py<PyBytes>> {
    let opts = tmi_image::webp_enc::WebpOptions { lossless, quality, alpha_quality, method, exact };
    let out = py.allow_threads(|| tmi_image::webp_enc::encode_webp(data, w, h, bands, &opts)).map_err(PyValueError::new_err)?;
    Ok(PyBytes::new(py, &out).unbind())
}

/// libwebp's encoder version (0xMMmmrr).
#[pyfunction]
fn webp_version() -> i32 {
    tmi_image::webp_enc::encoder_version()
}

pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(png_open, m)?)?;
    m.add_function(wrap_pyfunction!(jpeg_open, m)?)?;
    m.add_function(wrap_pyfunction!(decode_rgba, m)?)?;
    m.add_function(wrap_pyfunction!(raster_open, m)?)?;
    m.add_function(wrap_pyfunction!(raster_decode, m)?)?;
    m.add_function(wrap_pyfunction!(jpeg_quantization, m)?)?;
    m.add_function(wrap_pyfunction!(jpeg_encode, m)?)?;
    m.add_function(wrap_pyfunction!(webp_encode, m)?)?;
    m.add_function(wrap_pyfunction!(webp_version, m)?)?;
    m.add_function(wrap_pyfunction!(pil_resize, m)?)?;
    m.add_function(wrap_pyfunction!(pil_reduce, m)?)?;
    m.add_function(wrap_pyfunction!(pil_thumbnail, m)?)?;
    m.add_function(wrap_pyfunction!(pil_thumbnail_size, m)?)?;
    m.add_function(wrap_pyfunction!(pil_convert, m)?)?;
    m.add_function(wrap_pyfunction!(pil_blend, m)?)?;
    m.add_function(wrap_pyfunction!(pil_chop, m)?)?;
    m.add_function(wrap_pyfunction!(pil_bbox_any, m)?)?;
    m.add_function(wrap_pyfunction!(pil_extrema, m)?)?;
    Ok(())
}
