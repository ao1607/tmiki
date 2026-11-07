//! render_cache.py `RenderCache`: composite + thumbnail PNGs of a layered
//! file, keyed by its source digest under `.tmi/cache/render/v<N>/`.

use std::path::{Path, PathBuf};

/// render_cache.py `RENDER_CACHE_VERSION`.
pub const RENDER_CACHE_VERSION: u32 = 14;

pub struct RenderCache {
    pub root: PathBuf,
}

impl RenderCache {
    pub fn new(root: impl Into<PathBuf>) -> RenderCache {
        RenderCache { root: root.into() }
    }

    fn paths(&self, digest: &str) -> (PathBuf, PathBuf) {
        let split = digest.char_indices().nth(2).map_or(digest.len(), |(i, _)| i);
        let base = self.root.join(format!("v{RENDER_CACHE_VERSION}")).join(&digest[..split]);
        let stem = &digest[split..];
        (base.join(format!("{stem}.composite.png")), base.join(format!("{stem}.thumb.png")))
    }

    pub fn get(&self, digest: &str) -> Option<(Vec<u8>, Vec<u8>)> {
        let (c, t) = self.paths(digest);
        let composite = std::fs::read(c).ok()?;
        let thumb = std::fs::read(t).ok()?;
        Some((composite, thumb))
    }

    /// Best effort: a failure never breaks the render.
    pub fn put(&self, digest: &str, composite: &[u8], thumb: &[u8]) {
        if composite.is_empty() || thumb.is_empty() {
            return;
        }
        let (c, t) = self.paths(digest);
        if let Some(parent) = c.parent() {
            if std::fs::create_dir_all(parent).is_err() {
                return;
            }
        }
        if write_atomic(&c, composite).is_ok() {
            let _ = write_atomic(&t, thumb);
        }
    }
}

fn write_atomic(path: &Path, data: &[u8]) -> Result<(), String> {
    tmi_store::store::write_file_atomic(path, data).map_err(|e| e.to_string())
}
