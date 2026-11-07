//! What [`crate::export_plugin!`] expands to on `wasm32`: wit-bindgen's
//! export traits, and the step from the SDK's write plan to the WIT's.

use crate::bindings::exports::tmi::format::writer as w;
use crate::bindings::tmi::format::{edit, source};
use crate::*;

pub use crate::bindings::exports::tmi::format::reader::Guest as ReaderGuest;
pub use crate::bindings::exports::tmi::format::writer::Guest as WriterGuest;
pub type GuestSource = source::SourceFile;
pub type GuestPixels = edit::Pixels;
pub type GuestWritePlan = w::WritePlan;

pub fn source(src: &GuestSource) -> SourceFile<'_> {
    SourceFile(crate::sys::Source(src))
}

pub fn write_layers<P: Plugin>(src: &GuestSource, edits: Vec<LayerEdit>, pixels: Vec<GuestPixels>) -> Result<GuestWritePlan, WriteError> {
    let pixels = pixels.into_iter().map(|p| crate::edit::Pixels(crate::sys::Pixels(p))).collect();
    let plan = P::write_layers(&source(src), edits, pixels)?;
    Ok(w::WritePlan {
        pieces: plan
            .pieces
            .into_iter()
            .map(|p| match p {
                Piece::Source(s) => w::Piece::Source(s),
                Piece::Bytes(b) => w::Piece::Bytes(b),
                Piece::Blob(b) => w::Piece::Blob(b.0 .0),
            })
            .collect(),
        warnings: plan.warnings,
    })
}
