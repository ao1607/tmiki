//! A record as one canonical JSON value, the shape `tmi-conform
//! import-dump` writes (blobs, the composite and the thumbnail by SHA-256):
//! what the tests and `tmi-conform plugin-import-dump` print and compare.

use tmi_import::{ImportedFile, ImportedLayer};
use tmi_store::json::{self, Map, Value};
use tmi_store::oid::sha256_hex;

fn layer_value(l: &ImportedLayer) -> Value {
    let mut m = Map::new();
    for key in [
        "layer_id", "stable_source_id", "name", "kind", "visible", "opacity", "blend_mode", "clip_to_below", "offset_x", "offset_y",
        "mask_ref", "pixel_bbox", "pixel_sha256", "source_meta",
    ] {
        m.insert(key.into(), l.field(key));
    }
    m.insert("blob".into(), l.blob_bytes.as_ref().map_or(Value::Null, |b| Value::Str(sha256_hex(b))));
    m.insert("children".into(), Value::Array(l.children.iter().map(layer_value).collect()));
    Value::Object(m)
}

pub fn value(f: &ImportedFile) -> Value {
    let sha = |b: &[u8]| if b.is_empty() { Value::Null } else { Value::Str(sha256_hex(b)) };
    let mut m = Map::new();
    m.insert("source_format".into(), Value::Str(f.source_format.clone()));
    m.insert("source_digest".into(), Value::Str(f.source_digest.clone()));
    m.insert("canvas_meta".into(), Value::Object(f.canvas_meta.clone()));
    m.insert("source_meta".into(), Value::Object(f.source_meta.clone()));
    m.insert("composite".into(), sha(&f.composite_png));
    m.insert("thumbnail".into(), sha(&f.thumbnail_png));
    m.insert("layers".into(), Value::Array(f.layers.iter().map(layer_value).collect()));
    if let Some(i) = &f.importer {
        m.insert("importer".into(), Value::Object(i.clone()));
    }
    Value::Object(m)
}

pub fn text(f: &ImportedFile) -> String {
    json::dumps(&value(f), &json::DumpOptions::CANONICAL)
}
