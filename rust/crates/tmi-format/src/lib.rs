//! tmi's format interface (PLUGINS.md, stage P1).
//!
//! - [`types`]: the Rust types of `wit/tmi-format.wit` (version 0.1.0), one
//!   for one. A reader describes a file as a `Structure`: canvas, layers,
//!   and for each layer's pixels a *recipe* ("describe, don't decode").
//! - [`recipe`]: the native decoder of recipes (codecs, channel groups, tint,
//!   tiles, overlap, cropping to the bitmap) into RGBA8, which the pipeline
//!   cuts to a `tmi_import::crop::LayerCrop`.
//! - [`reader`]: the `FormatReader` / `FormatWriter` traits, what a read
//!   shares with the host.
//! - [`builtin`]: the built-in readers (P1: the raster formats).
//! - [`registry`], [`pipeline`]: which reader takes a file, and one reader's
//!   file into `tmi_import::ImportedFile`.
//!
//! [`import_file`] is the front door `tmi_import::import_file` was: MDP2
//! (P2) and CLIP (P3) go through `pipeline::import` with their own
//! `FormatReader` (`builtin_mdp2`, `builtin_clip`), everything else through
//! the registry.

pub mod builtin;
pub mod builtin_clip;
pub mod builtin_mdp2;
pub mod compose;
pub mod pipeline;
pub mod reader;
pub mod recipe;
pub mod recipe_key;
pub mod registry;
pub mod safe_text;
pub mod types;
pub mod validate;
pub mod write;

pub use tmi_import::{path_stem, path_suffix};

use tmi_import::{decline, ImportedFile, Options, Res, Warnings};

/// `import_file(path, ...)` of a file named `name` (its last path
/// component) holding `source` with sha256 `digest`.
pub fn import_file(name: &str, source: &[u8], digest: &str, opts: &Options, warns: &mut Warnings) -> Res<ImportedFile> {
    let suffix = path_suffix(name).to_lowercase();
    match suffix.as_str() {
        // Always this reader (never probing, never a silent opaque blob):
        // any failure declines, as `tmi_import::mdp2::import` always did.
        ".mdp" => pipeline::import(registry::Registry::mdp2(), name, source, digest, opts, warns).map_err(|e| match e {
            pipeline::ImportError::Decline(d) => d,
            pipeline::ImportError::Read(e) => decline(format!("mdp2: {e}")),
        }),
        ".clip" => pipeline::import(registry::Registry::clip(), name, source, digest, opts, warns).map_err(|e| match e {
            pipeline::ImportError::Decline(d) => d,
            pipeline::ImportError::Read(e) => decline(format!("clip: {e}")),
        }),
        // Every other extension, and a .png / .jpg whatever it holds: an image
        // when tmi's decoders read it, else an opaque blob (PLUGINS.md P0e).
        _ => {
            let imported = registry::import_via_registry(registry::Registry::builtin(), name, source, digest, opts, warns)?;
            if imported.source_format == "binary" {
                if let Some(note) = unreadable_image_note(name, source) {
                    warns.lines.push(format!("tmiki: warning: could not read {name} as {note}; tracking as opaque binary"));
                }
            }
            Ok(imported)
        }
    }
}

const IMAGE_SUFFIXES: [(&str, &str); 12] = [
    (".png", "PNG"),
    (".jpg", "JPEG"),
    (".jpeg", "JPEG"),
    (".jpe", "JPEG"),
    (".jfif", "JPEG"),
    (".gif", "GIF"),
    (".bmp", "BMP"),
    (".dib", "BMP"),
    (".tif", "TIFF"),
    (".tiff", "TIFF"),
    (".webp", "WebP"),
    (".psd", "PSD"),
];

fn image_signature(suffix: &str, data: &[u8]) -> Option<&'static str> {
    if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("PNG")
    } else if data.starts_with(b"\xff\xd8\xff") {
        Some("JPEG")
    } else if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
        Some("GIF")
    } else if [&b"II*\x00"[..], b"MM\x00*", b"II+\x00", b"MM\x00+"].iter().any(|s| data.starts_with(s)) {
        Some("TIFF")
    } else if data.starts_with(b"RIFF") && data.get(8..12) == Some(&b"WEBP"[..]) {
        Some("WebP")
    } else if data.starts_with(b"8BPS") {
        Some("PSD")
    } else if data.starts_with(b"BM") && (suffix == ".bmp" || suffix == ".dib") {
        Some("BMP")
    } else {
        None
    }
}

/// importers.py `unreadable_image_note(name, data)`: for a file tmi did not
/// read as an image, what to tell the user when it looks like one (an image
/// extension, or an image signature). Such a file is kept as an opaque blob;
/// the warning lets the user see that the visual diff is gone (issue #37).
pub fn unreadable_image_note(name: &str, data: &[u8]) -> Option<String> {
    let suffix = path_suffix(name).to_lowercase();
    let (fmt, why) = match image_signature(&suffix, data) {
        Some(fmt) => (
            fmt,
            "the data is damaged, incomplete or of a kind tmi cannot read; was the file still being saved or downloaded?".to_string(),
        ),
        None => {
            let fmt = IMAGE_SUFFIXES.iter().find(|(s, _)| *s == suffix)?.1;
            (fmt, if data.is_empty() { "the file is empty".to_string() } else { format!("it does not contain {fmt} data") })
        }
    };
    Some(format!("a {fmt} image ({why})"))
}

#[cfg(test)]
mod compose_tests;
#[cfg(test)]
mod pipeline_tests;
#[cfg(test)]
mod wit_tests;
#[cfg(test)]
mod hostile_tests;
