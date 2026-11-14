//! The registry (PLUGINS.md 8.1): which reader takes a file. P1 has the
//! built-in raster reader only; `.tmiattributes` and plugin readers join the
//! lookup in P6.

use std::sync::{Arc, OnceLock};

use tmi_import::{decline, opaque, ImportedFile, Options, Res, Warnings};

use crate::builtin::RasterReader;
use crate::builtin_mdp2::Mdp2Reader;
use crate::pipeline::{self, ImportError};
use crate::reader::FormatReader;
use crate::types::Confidence;

pub struct Registry {
    readers: Vec<Arc<dyn FormatReader>>,
}

impl Registry {
    pub fn new() -> Registry {
        Registry { readers: Vec::new() }
    }

    /// The readers every tmiki has.
    pub fn builtin() -> &'static Registry {
        static REGISTRY: OnceLock<Registry> = OnceLock::new();
        REGISTRY.get_or_init(|| {
            let mut r = Registry::new();
            r.register(Arc::new(Mdp2Reader::new()));
            r.register(Arc::new(RasterReader::new()));
            r
        })
    }

    /// `.mdp` always goes through this reader, never through probing or
    /// `opaque()`: any failure declines (forwards to Python), as the old
    /// `tmi_import::mdp2::import` always did (never a silent opaque blob).
    pub fn mdp2() -> &'static Mdp2Reader {
        static READER: OnceLock<Mdp2Reader> = OnceLock::new();
        READER.get_or_init(Mdp2Reader::new)
    }

    /// `.clip` always goes through this reader, never through probing or
    /// `opaque()` (same rule as `mdp2()`, PLUGINS.md P3).
    pub fn clip() -> &'static crate::builtin_clip::ClipReader {
        static READER: OnceLock<crate::builtin_clip::ClipReader> = OnceLock::new();
        READER.get_or_init(crate::builtin_clip::ClipReader::new)
    }

    pub fn register(&mut self, reader: Arc<dyn FormatReader>) {
        self.readers.push(reader);
    }

    /// The reader that is surest about the file; among equals, the one
    /// registered first. None when every reader says `No`.
    pub fn select(&self, name: &str, data: &[u8]) -> Option<&dyn FormatReader> {
        let mut best: Option<(&dyn FormatReader, Confidence)> = None;
        for r in &self.readers {
            let head = &data[..data.len().min(r.info().probe_bytes as usize)];
            let c = r.probe(name, head, data.len() as u64);
            if c != Confidence::No && best.is_none_or(|(_, b)| c > b) {
                best = Some((r.as_ref(), c));
            }
        }
        best.map(|(r, _)| r)
    }
}

impl Default for Registry {
    fn default() -> Self {
        Registry::new()
    }
}

/// The import of a file that is neither CLIP nor MDP2: through the reader the
/// registry picks. A built-in reader that says `NotThisFormat` (P0e: tmiki's
/// decoders do not read it) leaves an opaque blob.
pub fn import_via_registry(registry: &Registry, name: &str, source: &[u8], digest: &str, opts: &Options, warns: &mut Warnings) -> Res<ImportedFile> {
    let Some(reader) = registry.select(name, source) else {
        return Ok(opaque(name, source, digest));
    };
    match pipeline::import(reader, name, source, digest, opts, warns) {
        Ok(f) => Ok(f),
        Err(ImportError::Decline(d)) => Err(d),
        Err(ImportError::Read(crate::types::ReadError::NotThisFormat(_))) if reader.builtin() => Ok(opaque(name, source, digest)),
        // the over-the-pixel-limit message is Python's to report
        Err(ImportError::Read(crate::types::ReadError::TooLarge(m))) => Err(decline(m)),
        Err(ImportError::Read(e)) => Err(decline(format!("{}: {e}", reader.info().id))),
    }
}
