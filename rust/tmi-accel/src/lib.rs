//! `tmi_accel`: the Python extension module. Thin pyo3 shims over the
//! workspace crates (tmi-pillow, tmi-numpy, tmi-clip, tmi-render, tmi-codec).

use pyo3::prelude::*;

mod clip_io;
mod image;
mod kernels;
mod mdp2;
mod render;
mod testing;
mod testing_image;
mod testing_writeback;

/// Stack size of the rayon workers. Several Python threads calling into the
/// kernels at once (decode prefetch, banded composites) each inject a job; a
/// worker waiting inside one job's join steals the others' jobs onto its own
/// stack, so the nesting grows with the number of callers. The 2 MiB default
/// overflowed decoding lolita's 704-tile layers from 16 threads. Windows only
/// reserves the address space.
const WORKER_STACK: usize = 32 << 20;

/// What this build supports beyond the original functions; Python checks it
/// before relying on newer behaviour (an older wheel has no FEATURES).
/// "tmil-v1": `encode_layer_blob` / `decode_layer_blob` (layer blob format).
/// "render-clip-v1": `render_clip` (the CLIP compositor, RUST_MIGRATION.md 1a).
/// "render-clip-v2": `render_clip` warns about and leaves out unreadable
/// offscreens (with "offscreen" / "tile-size" warnings and decode_failures)
/// instead of declining the file (1e).
/// "image-v1": the image operations of `image.rs` (decode, resize,
/// thumbnail, diff, compose, WebP, PNG / JPEG write-back; 2b).
/// "offscreen-v2": `decode_clip_offscreen` decodes every payload (warning
/// about unknown tile sizes) or raises ValueError; it never returns None.
/// "mdp2-v1": the MDP2 reader, layer decode and compositor (`parse_mdp2`,
/// `mdp2_layer_pixels`, `render_mdp2`, `mdp2_write_layer`; stage 2c).
/// `decode_mdp2_layer` is gone.
/// "clip-io-v1": the CLIP CanvasPreview decode and LANCZOS resize
/// (`decode_canvas_preview`, `resize_canvas_preview`) and layer write-back
/// (`clip_write_layer`: stock zlib 1.3.1, SQLite 3.50.4), and `same_bytes`
/// (stage 2c).
/// "image-sparse-v1": `rgba_resize_sparse` (a layer's crop resampled as the
/// whole transparent layer, without building it).
/// "image-import-v1": `image_import`, tmi's own rule for which files are
/// images (PLUGINS.md P0e).
const FEATURES: &[&str] = &[
    "image-import-v1",
    "concurrent-calls",
    "composite-ops",
    "tmil-v1",
    "render-clip-v1",
    "render-clip-v2",
    "offscreen-v2",
    "image-v1",
    "mdp2-v1",
    "clip-io-v1",
    "image-sparse-v1",
];

#[pymodule]
fn tmi_accel(m: &Bound<'_, PyModule>) -> PyResult<()> {
    // Fails only when the global pool already exists (then it is kept).
    let _ = rayon::ThreadPoolBuilder::new().stack_size(WORKER_STACK).build_global();
    m.add_function(wrap_pyfunction!(kernels::blend_rgba, m)?)?;
    m.add_function(wrap_pyfunction!(kernels::supported_blend_modes, m)?)?;
    m.add_function(wrap_pyfunction!(kernels::decode_clip_offscreen, m)?)?;
    m.add_function(wrap_pyfunction!(kernels::encode_png_rgba, m)?)?;
    m.add_function(wrap_pyfunction!(kernels::regrade_rgba, m)?)?;
    m.add_function(wrap_pyfunction!(kernels::cdc_cuts, m)?)?;
    m.add_function(wrap_pyfunction!(mdp2::parse_mdp2, m)?)?;
    m.add_function(wrap_pyfunction!(mdp2::mdp2_layer_pixels, m)?)?;
    m.add_function(wrap_pyfunction!(mdp2::render_mdp2, m)?)?;
    m.add_function(wrap_pyfunction!(mdp2::mdp2_write_layer, m)?)?;
    m.add_function(wrap_pyfunction!(clip_io::decode_canvas_preview, m)?)?;
    m.add_function(wrap_pyfunction!(clip_io::resize_canvas_preview, m)?)?;
    m.add_function(wrap_pyfunction!(clip_io::clip_write_layer, m)?)?;
    m.add_function(wrap_pyfunction!(clip_io::same_bytes, m)?)?;
    m.add_function(wrap_pyfunction!(kernels::composite_ops, m)?)?;
    m.add_function(wrap_pyfunction!(kernels::encode_layer_blob, m)?)?;
    m.add_function(wrap_pyfunction!(kernels::decode_layer_blob, m)?)?;
    m.add_function(wrap_pyfunction!(render::render_clip, m)?)?;
    image::register(m)?;
    m.add("FEATURES", FEATURES.to_vec())?;
    m.add_submodule(&testing::module(m.py())?)?;
    Ok(())
}
