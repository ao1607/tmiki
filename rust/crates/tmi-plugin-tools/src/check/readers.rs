//! The imports `tmiki plugin test` runs and compares, and the ways it calls a
//! plugin out of the usual order (PLUGINS.md 7.1 item 2).

use std::collections::HashSet;

use tmi_format::pipeline::{self, ImportError, Importer};
use tmi_format::reader::{FormatReader, ReadCx, Source};
use tmi_format::types::*;
use tmi_import::{ImportedFile, LayerBlobs, Limits, Options, Warnings};
use tmi_plugin_host::Plugin;

use crate::cbor::{self, V};

/// The limits of the checks: what tmiki allows by default (Pillow's floor).
pub const LIMITS: Limits = Limits { layered: tmi_import::limit::FLOOR, raster: tmi_import::limit::FLOOR };

/// `add`'s import: the composite and every layer blob.
pub fn options(blobs: &LayerBlobs, limits: Limits) -> Options<'_> {
    Options {
        render_composite: true,
        cache: None,
        reuse_pixels: None,
        reuse_decoded: None,
        layer_blobs: blobs,
        composite_base: None,
        limits,
        verify_partial: false,
        progress: None,
    }
}

/// One import as `add` makes it: the record and the warnings.
pub struct Imported {
    pub file: ImportedFile,
    pub warnings: Vec<String>,
}

impl Imported {
    /// The record as a value (blobs, the composite and the thumbnail by
    /// SHA-256: `tmi-conform import-dump`'s shape), with the warnings.
    pub fn value(&self) -> V {
        cbor::map([("record", cbor::from_json(&tmi_plugin_host::dump::value(&self.file))), ("warnings", V::Array(self.warnings.iter().map(|w| V::text(w.clone())).collect()))])
    }
}

pub fn import(reader: &dyn FormatReader, importer: &Importer, name: &str, data: &[u8], limits: Limits) -> Result<Imported, String> {
    let all = LayerBlobs::All;
    let digest = tmi_store::oid::sha256_hex(data);
    let mut warns = Warnings::default();
    match pipeline::import_plugin(reader, importer, name, data, &digest, &options(&all, limits), &mut warns) {
        Ok(file) => Ok(Imported { file, warnings: warns.lines }),
        Err(ImportError::Read(e)) => Err(e.to_string()),
        Err(ImportError::Decline(d)) => Err(d.0),
    }
}

/// The structure with every deferred layer's recipe read (in index order).
pub fn resolved_structure(reader: &dyn FormatReader, name: &str, data: &[u8]) -> Result<Structure, String> {
    let cx = ReadCx::new(None);
    let src = Source::new(name, data);
    let mut s = reader.read_structure(&src, &cx).map_err(|e| e.to_string())?;
    for i in 0..s.layers.len() {
        if let LayerPixels::Deferred(token) = s.layers[i].pixels {
            let b = reader.read_layer(&src, &s.context, token, &cx).map_err(|e| format!("read-layer({token}): {e}"))?;
            s.layers[i].pixels = LayerPixels::Ready(b);
        }
    }
    Ok(s)
}

/// A reader whose `read-structure` reads every deferred layer itself, the
/// last one first, one after the other, and hands them on as `ready`: the
/// pipeline's parallel order replaced by the reverse one.
pub struct Reversed<'a> {
    pub inner: &'a dyn FormatReader,
}

impl FormatReader for Reversed<'_> {
    fn info(&self) -> &PluginInfo {
        self.inner.info()
    }

    fn probe(&self, name: &str, head: &[u8], size: u64) -> Confidence {
        self.inner.probe(name, head, size)
    }

    fn read_structure(&self, src: &Source, cx: &ReadCx) -> Result<Structure, ReadError> {
        let mut s = self.inner.read_structure(src, cx)?;
        for i in (0..s.layers.len()).rev() {
            if let LayerPixels::Deferred(token) = s.layers[i].pixels {
                s.layers[i].pixels = LayerPixels::Ready(self.inner.read_layer(src, &s.context, token, cx)?);
            }
        }
        Ok(s)
    }

    fn read_layer(&self, src: &Source, context: &[u8], token: u32, cx: &ReadCx) -> Result<Bitmap, ReadError> {
        self.inner.read_layer(src, context, token, cx)
    }

    fn decode_chunk(&self, codec_id: u32, input: &[u8], out_len: u32) -> Result<Vec<u8>, ReadError> {
        self.inner.decode_chunk(codec_id, input, out_len)
    }
}

/// Whether the structure defers any layer.
pub fn defers(s: &Structure) -> bool {
    s.layers.iter().any(|l| matches!(l.pixels, LayerPixels::Deferred(_)))
}

/// One instance for everything: `read-structure` twice, then every
/// deferred layer, in that one instance (a plugin that keeps state between
/// calls answers differently here than with a fresh instance per call).
/// The structure of the second call, deferred layers read.
pub fn one_instance(plugin: &Plugin, name: &str, data: &[u8]) -> Result<(Structure, Structure), String> {
    use wasmtime::component::Resource;
    let r = plugin.call(Some((name, data)), |store, ex, src| {
        let rep = src.expect("a source").rep();
        let handle = || Resource::new_own(rep);
        let first = ex.reader.call_read_structure(&mut *store, handle())?;
        let second = ex.reader.call_read_structure(&mut *store, handle())?;
        let (first, second) = match (first, second) {
            (Ok(a), Ok(b)) => (tmi_plugin_host::convert::structure(a), tmi_plugin_host::convert::structure(b)),
            (Err(e), _) | (_, Err(e)) => return Ok(Err(tmi_plugin_host::convert::read_error(e).to_string())),
        };
        let mut resolved = second.clone();
        for l in resolved.layers.iter_mut() {
            if let LayerPixels::Deferred(token) = l.pixels {
                match ex.reader.call_read_layer(&mut *store, handle(), &second.context, token)? {
                    Ok(b) => l.pixels = LayerPixels::Ready(tmi_plugin_host::convert::bitmap(b)),
                    Err(e) => return Ok(Err(format!("read-layer({token}): {}", tmi_plugin_host::convert::read_error(e)))),
                }
            }
        }
        Ok(Ok((first, resolved)))
    });
    match r {
        Ok(inner) => inner,
        Err(e) => Err(e.to_string()),
    }
}

/// The layers a test may write: writable, with pixels (stable ids).
pub fn writable(f: &ImportedFile) -> Vec<(String, String)> {
    fn walk(layers: &[tmi_import::ImportedLayer], out: &mut Vec<(String, String)>) {
        for l in layers {
            let w = l.source_meta.get("write_capability").and_then(tmi_store::json::Value::as_str) == Some("raster-pixels");
            if w && l.pixel_sha256.is_some() {
                out.push((l.stable_source_id.clone(), l.name.clone()));
            }
            walk(&l.children, out);
        }
    }
    let mut out = Vec::new();
    walk(&f.layers, &mut out);
    out
}

/// Stable ids appearing more than once (the host refuses them; said here
/// with the ids).
pub fn duplicate_ids(s: &Structure) -> Vec<String> {
    let mut seen = HashSet::new();
    s.layers.iter().filter(|l| !seen.insert(l.stable_id.as_str())).map(|l| l.stable_id.clone()).collect()
}
