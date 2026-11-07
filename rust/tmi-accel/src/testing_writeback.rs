//! `tmi_accel._test` entries for stage 2c's write-back parts (zlib).

use pyo3::prelude::*;
use pyo3::types::PyBytes;

/// `zlib.compress(data, level)` through the pinned stock zlib.
#[pyfunction]
#[pyo3(signature = (data, level=-1))]
fn zlib_compress(py: Python<'_>, data: &[u8], level: i32) -> Py<PyBytes> {
    let out = py.allow_threads(|| tmi_clip::zlib::compress(data, level));
    PyBytes::new(py, &out).unbind()
}

/// The zlib version the write-back bytes come from.
#[pyfunction]
fn zlib_version() -> String {
    tmi_clip::zlib::version()
}

/// `build_tile_record(block, tile)` of a 256x256 RGBA tile already on
/// transparent (alpha 0 is (0, 0, 0, 0)).
#[pyfunction]
fn clip_tile_record(py: Python<'_>, block: usize, tile: &[u8]) -> PyResult<Py<PyBytes>> {
    if tile.len() != 256 * 256 * 4 {
        return Err(pyo3::exceptions::PyValueError::new_err("a tile is 256x256 RGBA"));
    }
    let out = py.allow_threads(|| tmi_clip::write::build_tile_record(block, tile)).map_err(pyo3::exceptions::PyValueError::new_err)?;
    Ok(PyBytes::new(py, &out).unbind())
}

pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(clip_tile_record, m)?)?;
    m.add_function(wrap_pyfunction!(zlib_compress, m)?)?;
    m.add_function(wrap_pyfunction!(zlib_version, m)?)?;
    Ok(())
}
