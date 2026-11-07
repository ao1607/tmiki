//! `tmi-conform import-dump <files>`: tmi-import's result for each file as
//! one JSON line, for tools/import_parity.py to compare with Python's
//! `import_file` (blobs, the composite and the thumbnail by sha256).
//!
//! Each file is imported twice: as `add` does (composite, every blob) and
//! as `status` does (no composite, no blobs, the first import's decodes
//! reused for every layer whose pixel source is unchanged).

use std::collections::{HashMap, HashSet};
use tmi_format::import_file;
use tmi_import::{ImportedFile, ImportedLayer, LayerBlobs, Limits, Options, PriorDecode, Warnings};
use tmi_store::json::{self, Map, Value};
use tmi_store::oid::sha256_hex;

fn layer_value(l: &ImportedLayer) -> Value {
    let mut m = Map::new();
    for key in [
        "layer_id", "stable_source_id", "name", "kind", "visible", "opacity", "blend_mode", "clip_to_below", "offset_x",
        "offset_y", "mask_ref", "pixel_bbox", "pixel_sha256", "source_meta",
    ] {
        m.insert(key.into(), l.field(key));
    }
    m.insert("blob".into(), l.blob_bytes.as_ref().map_or(Value::Null, |b| Value::Str(sha256_hex(b))));
    m.insert("children".into(), Value::Array(l.children.iter().map(layer_value).collect()));
    Value::Object(m)
}

fn sha_or_null(b: &[u8]) -> Value {
    if b.is_empty() {
        Value::Null
    } else {
        Value::Str(sha256_hex(b))
    }
}

fn file_value(f: &ImportedFile) -> Value {
    let mut m = Map::new();
    m.insert("source_format".into(), Value::Str(f.source_format.clone()));
    m.insert("source_digest".into(), Value::Str(f.source_digest.clone()));
    m.insert("canvas_meta".into(), Value::Object(f.canvas_meta.clone()));
    m.insert("source_meta".into(), Value::Object(f.source_meta.clone()));
    m.insert("composite".into(), sha_or_null(&f.composite_png));
    m.insert("thumbnail".into(), sha_or_null(&f.thumbnail_png));
    m.insert("layers".into(), Value::Array(f.layers.iter().map(layer_value).collect()));
    Value::Object(m)
}

fn priors(layers: &[ImportedLayer], out: &mut HashMap<String, PriorDecode>) {
    for l in layers {
        if let (Some(sha), Some(src)) = (&l.pixel_sha256, l.source_meta.get("pixel_source")) {
            if src.truthy() {
                out.insert(
                    l.stable_source_id.clone(),
                    PriorDecode { pixel_source: Some(src.clone()), pixel_sha256: sha.clone(), pixel_bbox: l.pixel_bbox.clone() },
                );
            }
        }
        priors(&l.children, out);
    }
}

pub fn run(files: &[String]) {
    let all = LayerBlobs::All;
    let none = LayerBlobs::Only(HashSet::new());
    for path in files {
        let data = std::fs::read(path).unwrap();
        let digest = sha256_hex(&data);
        let name = std::path::Path::new(path).file_name().unwrap().to_string_lossy().into_owned();
        let mut out = Map::new();
        out.insert("file".into(), Value::Str(path.clone()));
        let mut warns = Warnings::default();
        let opts = Options {
            render_composite: true,
            cache: None,
            reuse_pixels: None,
            reuse_decoded: None,
            layer_blobs: &all,
            composite_base: None,
            // the limit Python's import resolves (an image over it declines:
            // Python reports the error)
            limits: tmi_import::limit::resolve(None, None).unwrap_or(Limits::NONE),
            verify_partial: false,
            progress: None,
        };
        match import_file(&name, &data, &digest, &opts, &mut warns) {
            Err(d) => {
                out.insert("declined".into(), Value::Str(d.0));
            }
            Ok(f) => {
                out.insert("add".into(), file_value(&f));
                out.insert("warnings".into(), Value::Array(warns.lines.iter().cloned().map(Value::Str).collect()));
                let mut prior = HashMap::new();
                priors(&f.layers, &mut prior);
                let mut warns2 = Warnings::default();
                let opts2 = Options {
                    render_composite: false,
                    reuse_decoded: if prior.is_empty() { None } else { Some(&prior) },
                    layer_blobs: &none,
                    ..opts
                };
                match import_file(&name, &data, &digest, &opts2, &mut warns2) {
                    Err(d) => {
                        out.insert("status_declined".into(), Value::Str(d.0));
                    }
                    Ok(s) => {
                        out.insert("status".into(), file_value(&s));
                    }
                }
            }
        }
        println!("{}", json::dumps(&Value::Object(out), &json::DumpOptions::CANONICAL));
    }
}
