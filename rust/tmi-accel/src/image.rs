//! tmi's image operations without Pillow (RUST_MIGRATION.md 2b): the
//! decoders, resampling, compositing, diff and encoders of `tmi-pillow` and
//! `tmi-image`, on packed RGBA8 buffers (`tmi.raster.Rgba`). Each releases
//! the GIL while it works. Feature "image-v1".

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyTuple};

use tmi_pillow::image::{Image, Mode, Transparency};
use tmi_pillow::resample::Filter;

fn err<E: std::fmt::Debug>(e: E) -> PyErr {
    PyValueError::new_err(format!("{e:?}"))
}

fn check(data: &[u8], w: usize, h: usize) -> PyResult<()> {
    match w.checked_mul(h).and_then(|p| p.checked_mul(4)) {
        Some(n) if n == data.len() => Ok(()),
        _ => Err(PyValueError::new_err(format!("RGBA buffer of {} bytes is not {w}x{h}", data.len()))),
    }
}

fn rgba_image(data: &[u8], w: usize, h: usize) -> PyResult<Image> {
    check(data, w, h)?;
    Ok(Image::new(Mode::RGBA, w, h, data.to_vec()))
}

fn bytes(py: Python<'_>, v: &[u8]) -> Py<PyBytes> {
    PyBytes::new(py, v).unbind()
}

/// `Image.open(data).convert("RGBA")` of a PNG or JPEG: (rgba, w, h,
/// "png" / "jpeg"). ValueError when the data is something else, damaged, or
/// what tmi_image leaves to Pillow (an animated PNG, a JPEG libjpeg warns
/// about); the caller then decodes with Pillow.
#[pyfunction]
pub fn image_decode(py: Python<'_>, data: &[u8]) -> PyResult<(Py<PyBytes>, usize, usize, &'static str)> {
    let (img, fmt) = py.allow_threads(|| tmi_image::decode_rgba(data)).map_err(err)?;
    let f = fmt.name();
    Ok((bytes(py, &img.data), img.width, img.height, f))
}

/// tmi's rule for which files are images (PLUGINS.md P0e, `tmi_image::import`):
/// `(rgba, w, h, format, dpi, icc)` when tmi's own decoders read the file,
/// `None` when they do not (it is stored as an opaque blob). `format` is
/// Pillow's `Image.format.lower()` ("png", "jpeg", "gif", ...), `dpi` is
/// `float(info["dpi"][0])` or None, `icc` is `"icc_profile" in info`.
/// ValueError "too large: WxH" when a readable image has more than
/// `max_pixels` pixels (None or 0: no limit). Pillow is not consulted.
#[pyfunction]
#[pyo3(signature = (data, max_pixels=None))]
#[allow(clippy::type_complexity)]
pub fn image_import(
    py: Python<'_>,
    data: &[u8],
    max_pixels: Option<u64>,
) -> PyResult<Option<(Py<PyBytes>, usize, usize, &'static str, Option<f64>, bool)>> {
    use tmi_image::import::{open_for_import, Reject};
    match py.allow_threads(|| open_for_import(data, max_pixels)) {
        Ok(a) => Ok(Some((bytes(py, &a.image.data), a.image.width, a.image.height, a.format.name(), a.dpi, a.icc))),
        Err(Reject::Opaque(_)) => Ok(None),
        Err(Reject::TooLarge(w, h)) => Err(PyValueError::new_err(format!("too large: {w}x{h}"))),
    }
}

/// `getbbox(alpha_only=False)` of a `w`-wide RGBA buffer.
#[pyfunction]
pub fn rgba_bbox(py: Python<'_>, data: &[u8], w: usize) -> Option<(usize, usize, usize, usize)> {
    if w == 0 {
        return None;
    }
    py.allow_threads(|| tmi_pillow::bbox::bbox_any(data, w, 4))
}

/// `getbbox()` of an RGBA buffer: the box of the non-zero alphas.
#[pyfunction]
pub fn rgba_alpha_bbox(py: Python<'_>, data: &[u8], w: usize) -> Option<(usize, usize, usize, usize)> {
    if w == 0 {
        return None;
    }
    py.allow_threads(|| tmi_pillow::bbox::alpha_bbox(data, w))
}

/// The smallest alpha (`getchannel("A").getextrema()[0]`), 255 for an
/// empty buffer.
#[pyfunction]
pub fn rgba_alpha_min(py: Python<'_>, data: &[u8]) -> u8 {
    py.allow_threads(|| data.chunks_exact(4).map(|p| p[3]).min().unwrap_or(255))
}

/// `Image.crop(box)` (transparent black outside the image).
#[pyfunction]
pub fn rgba_crop(py: Python<'_>, data: &[u8], w: usize, h: usize, r#box: (i64, i64, i64, i64)) -> PyResult<Py<PyBytes>> {
    check(data, w, h)?;
    let (x0, y0, x1, y1) = r#box;
    if x1 < x0 || y1 < y0 {
        return Err(PyValueError::new_err("bad crop box"));
    }
    let out = py.allow_threads(|| tmi_pillow::rgba::crop(data, w, h, x0, y0, x1, y1));
    Ok(bytes(py, &out))
}

/// `image.resize(size, LANCZOS, box=box)` of RGBA (no reducing_gap).
#[pyfunction]
#[pyo3(signature = (data, w, h, size, r#box=None))]
pub fn rgba_resize(
    py: Python<'_>,
    data: &[u8],
    w: usize,
    h: usize,
    size: (usize, usize),
    r#box: Option<(f64, f64, f64, f64)>,
) -> PyResult<Py<PyBytes>> {
    let img = rgba_image(data, w, h)?;
    let bx = r#box.map(|b| [b.0, b.1, b.2, b.3]);
    let out = py.allow_threads(|| tmi_pillow::resize::resize(&img, size, Filter::Lanczos, bx, None)).map_err(err)?;
    Ok(bytes(py, &out.data))
}

/// `rgba_resize` of the `w` x `h` image that is transparent black but for
/// `crop` (`cw` x `ch` RGBA, empty for none) placed at (`x`, `y`) (what of
/// it lies outside the image is left out), without building that image:
/// only the painted pixels are resampled (`resize_sparse`; the same bytes).
/// Feature "image-sparse-v1".
#[pyfunction]
#[pyo3(signature = (crop, cw, ch, x, y, w, h, size, r#box=None))]
#[allow(clippy::too_many_arguments)]
pub fn rgba_resize_sparse(
    py: Python<'_>,
    crop: &[u8],
    cw: usize,
    ch: usize,
    x: i64,
    y: i64,
    w: usize,
    h: usize,
    size: (usize, usize),
    r#box: Option<(f64, f64, f64, f64)>,
) -> PyResult<Py<PyBytes>> {
    check(crop, cw, ch)?;
    let win = if cw == 0 || ch == 0 { None } else { tmi_pillow::resample::Window::placed(crop, cw, ch, 4, x, y, w, h) };
    let bx = r#box.map(|b| [b.0, b.1, b.2, b.3]);
    let out = py
        .allow_threads(|| tmi_pillow::resize::resize_sparse(Mode::RGBA, w, h, win, size, Filter::Lanczos, bx))
        .map_err(err)?;
    Ok(bytes(py, &out.data))
}

/// tmi's thumbnail (`image_ops.make_thumbnail`): `thumbnail((size, size),
/// LANCZOS)` (reducing_gap 2.0, which RGBA never uses) laid with
/// `alpha_composite` in the middle of a transparent `size` x `size` canvas.
#[pyfunction]
pub fn rgba_thumbnail(py: Python<'_>, data: &[u8], w: usize, h: usize, size: usize) -> PyResult<Py<PyBytes>> {
    let img = rgba_image(data, w, h)?;
    let out = py.allow_threads(|| -> Result<Vec<u8>, String> {
        let t = tmi_pillow::resize::thumbnail(&img, (size as f64, size as f64), Filter::Lanczos, Some(2.0)).map_err(|e| format!("{e:?}"))?;
        let mut canvas = vec![0u8; size * size * 4];
        let x = (size as i64 - t.width as i64).div_euclid(2);
        let y = (size as i64 - t.height as i64).div_euclid(2);
        tmi_pillow::rgba::over(&mut canvas, size, size, &t.data, t.width, t.height, x, y);
        Ok(canvas)
    });
    Ok(bytes(py, &out.map_err(PyValueError::new_err)?))
}

/// `Image.blend(a, b, alpha)` of two RGBA buffers of one size.
#[pyfunction]
pub fn rgba_blend(py: Python<'_>, a: &[u8], b: &[u8], alpha: f32) -> PyResult<Py<PyBytes>> {
    if a.len() != b.len() {
        return Err(PyValueError::new_err("sizes differ"));
    }
    let out = py.allow_threads(|| tmi_pillow::blend::blend(a, b, alpha));
    Ok(bytes(py, &out))
}

/// The visible difference box of two RGBA buffers of one size (tmi's
/// `diff_bbox`), or None.
#[pyfunction]
pub fn rgba_diff_bbox(py: Python<'_>, a: &[u8], b: &[u8], w: usize) -> PyResult<Option<(usize, usize, usize, usize)>> {
    if a.len() != b.len() || w == 0 || a.len() % (4 * w) != 0 {
        return Err(PyValueError::new_err("RGBA buffers of different sizes"));
    }
    Ok(py.allow_threads(|| tmi_pillow::rgba::visible_difference_bbox(a, b, w)))
}

/// A `w` x `h` RGBA canvas (`base`, or filled with `fill`) with `ops`
/// applied in order: `(kind, rgba, sw, sh, x, y)`, kind 0 =
/// `alpha_composite(src, (x, y))`, 1 = `paste(src, (x, y))`.
#[pyfunction]
#[pyo3(signature = (w, h, ops, fill=(0, 0, 0, 0), base=None))]
pub fn rgba_compose<'py>(
    py: Python<'py>,
    w: usize,
    h: usize,
    ops: Vec<Bound<'py, PyTuple>>,
    fill: (u8, u8, u8, u8),
    base: Option<&[u8]>,
) -> PyResult<Py<PyBytes>> {
    let mut parsed = Vec::with_capacity(ops.len());
    let mut holders = Vec::with_capacity(ops.len());
    for op in &ops {
        let (kind, src, sw, sh, x, y): (u8, Bound<'py, PyBytes>, usize, usize, i64, i64) = op.extract()?;
        check(src.as_bytes(), sw, sh)?;
        if kind > 1 {
            return Err(PyValueError::new_err("unknown op"));
        }
        parsed.push((kind, sw, sh, x, y));
        holders.push(src);
    }
    if let Some(b) = base {
        check(b, w, h)?;
    }
    let srcs: Vec<&[u8]> = holders.iter().map(|b| b.as_bytes()).collect();
    let out = py.allow_threads(|| {
        let mut canvas = match base {
            Some(b) => b.to_vec(),
            None => [fill.0, fill.1, fill.2, fill.3].repeat(w * h),
        };
        for (&(kind, sw, sh, x, y), src) in parsed.iter().zip(&srcs) {
            if kind == 0 {
                tmi_pillow::rgba::over(&mut canvas, w, h, src, sw, sh, x, y);
            } else {
                tmi_pillow::rgba::paste(&mut canvas, w, h, src, sw, sh, x, y);
            }
        }
        canvas
    });
    Ok(bytes(py, &out))
}

/// A copy of the RGBA buffer with `ImageDraw.rectangle(box, outline=ink,
/// width=width)` drawn on it.
#[pyfunction]
pub fn rgba_outline(
    py: Python<'_>,
    data: &[u8],
    w: usize,
    h: usize,
    r#box: (i32, i32, i32, i32),
    ink: (u8, u8, u8, u8),
    width: i32,
) -> PyResult<Py<PyBytes>> {
    check(data, w, h)?;
    let (x0, y0, x1, y1) = r#box;
    if x1 < x0 || y1 < y0 {
        return Err(PyValueError::new_err("bad rectangle"));
    }
    let mut out = data.to_vec();
    tmi_pillow::draw::rectangle_outline_rgba(&mut out, w, h, x0, y0, x1, y1, [ink.0, ink.1, ink.2, ink.3], width);
    Ok(bytes(py, &out))
}

/// Lossy WebP of RGBA as Pillow saves it (`quality=`, `method=`, alpha kept).
#[pyfunction]
pub fn rgba_webp(py: Python<'_>, data: &[u8], w: usize, h: usize, quality: f32, method: i32) -> PyResult<Py<PyBytes>> {
    check(data, w, h)?;
    let opts = tmi_image::webp_enc::WebpOptions { lossless: false, quality, alpha_quality: 100.0, method, exact: false };
    let out = py.allow_threads(|| tmi_image::webp_enc::encode_webp(data, w, h, 4, &opts)).map_err(PyValueError::new_err)?;
    Ok(bytes(py, &out))
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

fn source<'a>(mode: Option<&str>, palette: Option<&'a [u8]>, trns: Option<Transparency>) -> PyResult<Option<tmi_image::writeback::Source<'a>>> {
    let Some(mode) = mode else { return Ok(None) };
    let mode = Mode::from_name(mode).ok_or_else(|| PyValueError::new_err(format!("unknown mode {mode}")))?;
    Ok(Some(tmi_image::writeback::Source { mode, palette, transparency: trns }))
}

/// The PNG that replaces a user's PNG (issue #21): the RGBA pixels in the
/// original's mode where they fit it (`src_mode`, its palette entries and
/// `info["transparency"]`), with the chunks `before` the palette and
/// `after` the transparency (lists of (type, data)). The original's ICC
/// profile `icc` goes first as iCCP (and then no sRGB is written) unless
/// the written mode left its colour space. Returns (png, mode).
#[pyfunction]
#[pyo3(signature = (data, w, h, src_mode=None, src_palette=None, src_transparency=None, icc=None, before=Vec::new(), after=Vec::new()))]
#[allow(clippy::too_many_arguments)]
pub fn writeback_png(
    py: Python<'_>,
    data: &[u8],
    w: usize,
    h: usize,
    src_mode: Option<&str>,
    src_palette: Option<&[u8]>,
    src_transparency: Option<&Bound<'_, PyAny>>,
    icc: Option<&[u8]>,
    before: Vec<(Vec<u8>, Vec<u8>)>,
    after: Vec<(Vec<u8>, Vec<u8>)>,
) -> PyResult<(Py<PyBytes>, &'static str)> {
    let img = rgba_image(data, w, h)?;
    let src = source(src_mode, src_palette, transparency(src_transparency)?)?;
    let (png, mode) = py
        .allow_threads(|| -> Result<(Vec<u8>, Mode), String> {
            let (out, n) = tmi_image::writeback::fit_mode(&img, src.as_ref(), false)?;
            let mut chunks = before;
            if let (Some(icc), Some(s)) = (icc, src.as_ref()) {
                if tmi_image::writeback::keeps_icc(icc, out.mode, s.mode) {
                    chunks.retain(|(kind, _)| kind.as_slice() != b"sRGB");
                    chunks.insert(0, (b"iCCP".to_vec(), tmi_image::writeback::iccp_chunk(icc)));
                }
            }
            Ok((tmi_image::png_enc::encode_png(&out, n, &chunks, &after)?, out.mode))
        })
        .map_err(PyValueError::new_err)?;
    Ok((bytes(py, &png), mode.name()))
}

/// The JPEG that replaces a user's JPEG: "L" when the original was grey and
/// the pixels are, else RGB, encoded as Pillow's `save(..., "JPEG", ...)`
/// with these parameters: the markers after the header are `exif` (APP1),
/// `xmp` (APP1), the ICC profile `icc` (APP2 segments; left out when the
/// written mode left its colour space) and `comment`. `adobe_rgb`: the
/// original stored RGB without YCbCr (`keep_rgb` when written as RGB).
/// Returns (jpeg, mode).
#[pyfunction]
#[pyo3(signature = (data, w, h, src_mode=None, quality=-1, qtables=None, subsampling=-1, progressive=false, adobe_rgb=false,
                    dpi=(0, 0), exif=None, xmp=None, icc=None, comment=None))]
#[allow(clippy::too_many_arguments)]
pub fn writeback_jpeg(
    py: Python<'_>,
    data: &[u8],
    w: usize,
    h: usize,
    src_mode: Option<&str>,
    quality: i32,
    qtables: Option<Vec<Vec<u16>>>,
    subsampling: i32,
    progressive: bool,
    adobe_rgb: bool,
    dpi: (i32, i32),
    exif: Option<&[u8]>,
    xmp: Option<&[u8]>,
    icc: Option<&[u8]>,
    comment: Option<&[u8]>,
) -> PyResult<(Py<PyBytes>, &'static str)> {
    let img = rgba_image(data, w, h)?;
    let src = source(src_mode, None, None)?;
    let qtables = match qtables {
        None => None,
        Some(ts) => Some(
            ts.into_iter()
                .map(|t| <[u16; 64]>::try_from(t.as_slice()).map_err(|_| PyValueError::new_err("Invalid quantization table")))
                .collect::<PyResult<Vec<_>>>()?,
        ),
    };
    let (out, mode) = py
        .allow_threads(|| -> Result<(Vec<u8>, Mode), String> {
            let (out, _) = tmi_image::writeback::fit_mode(&img, src.as_ref(), true)?;
            let mut markers: Vec<(u8, Vec<u8>)> = Vec::new();
            if let Some(e) = exif.filter(|e| !e.is_empty()) {
                markers.push((0xE1, e.to_vec()));
            }
            if let Some(x) = xmp.filter(|x| !x.is_empty()) {
                markers.push((0xE1, [&b"http://ns.adobe.com/xap/1.0/\x00"[..], x].concat()));
            }
            if let (Some(icc), Some(s)) = (icc.filter(|i| !i.is_empty()), src.as_ref()) {
                if tmi_image::writeback::keeps_icc(icc, out.mode, s.mode) {
                    markers.extend(tmi_image::writeback::icc_segments(icc).into_iter().map(|seg| (0xE2, seg)));
                }
            }
            if let Some(c) = comment.filter(|c| !c.is_empty()) {
                markers.push((0xFE, c.to_vec()));
            }
            let opts = tmi_image::jpeg::JpegOptions {
                quality,
                qtables,
                subsampling,
                progressive,
                keep_rgb: adobe_rgb && out.mode == Mode::RGB,
                dpi,
                markers,
                ..Default::default()
            };
            Ok((tmi_image::jpeg::encode_jpeg(&out, &opts).map_err(|e| format!("{e:?}"))?, out.mode))
        })
        .map_err(PyValueError::new_err)?;
    Ok((bytes(py, &out), mode.name()))
}

/// (precision, components, sampling factors) of a JPEG's frame, and its
/// quantisation tables by id in natural order; None for no frame header.
#[pyfunction]
#[allow(clippy::type_complexity)]
pub fn jpeg_tables(data: &[u8]) -> (Option<(u8, u8, Vec<(u8, u8)>)>, Vec<(u8, Vec<u16>)>) {
    let frame = tmi_image::jpeg::frame_info(data);
    let tables = tmi_image::jpeg::quantization(data).into_iter().map(|(i, t)| (i, t.to_vec())).collect();
    (frame, tables)
}

pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(image_decode, m)?)?;
    m.add_function(wrap_pyfunction!(image_import, m)?)?;
    m.add_function(wrap_pyfunction!(rgba_bbox, m)?)?;
    m.add_function(wrap_pyfunction!(rgba_alpha_bbox, m)?)?;
    m.add_function(wrap_pyfunction!(rgba_alpha_min, m)?)?;
    m.add_function(wrap_pyfunction!(rgba_crop, m)?)?;
    m.add_function(wrap_pyfunction!(rgba_resize, m)?)?;
    m.add_function(wrap_pyfunction!(rgba_resize_sparse, m)?)?;
    m.add_function(wrap_pyfunction!(rgba_thumbnail, m)?)?;
    m.add_function(wrap_pyfunction!(rgba_blend, m)?)?;
    m.add_function(wrap_pyfunction!(rgba_diff_bbox, m)?)?;
    m.add_function(wrap_pyfunction!(rgba_compose, m)?)?;
    m.add_function(wrap_pyfunction!(rgba_outline, m)?)?;
    m.add_function(wrap_pyfunction!(rgba_webp, m)?)?;
    m.add_function(wrap_pyfunction!(writeback_png, m)?)?;
    m.add_function(wrap_pyfunction!(writeback_jpeg, m)?)?;
    m.add_function(wrap_pyfunction!(jpeg_tables, m)?)?;
    Ok(())
}

