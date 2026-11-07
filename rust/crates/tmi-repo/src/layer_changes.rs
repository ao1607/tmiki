//! repo.py `layer_changes` / `_layer_metadata_changed` and
//! tmi/layer_props.py: the readable per-layer lines of `status` ("changed
//! layer 影 (pixels, opacity: 100% -> 50%)", "changed pixels", "added
//! layer x", "deleted layer y"). Legacy records (no pixel digest, no
//! payload digests) are `Forward`: Python decodes stored blobs for them.

use crate::error::{RepoError, Result};
use crate::pyeq::{py_eq, py_ne};
use crate::repo::Repo;
use indexmap::IndexMap;
use tmi_import::{ImportedFile, ImportedLayer};
use tmi_store::json::{Map, Value};

fn fwd(why: impl Into<String>) -> RepoError {
    RepoError::forward(why)
}

pub const PARTIAL_STAGE_LAYER_FIELDS: [&str; 8] =
    ["name", "kind", "visible", "opacity", "blend_mode", "clip_to_below", "offset_x", "offset_y"];

pub const PARTIAL_STAGE_SOURCE_META_KEYS: [&str; 35] = [
    "type",
    "parent_id",
    "folder",
    "composite_id",
    "masking",
    "masking_type",
    "pixel_width",
    "pixel_height",
    "channel_profile",
    "render_payload_sha256",
    "mask_payload_sha256",
    "width",
    "height",
    "color",
    "lumi",
    "protect_alpha",
    "draft",
    "halftone_type",
    "halftone_line",
    "transform_offset_x",
    "transform_offset_y",
    "xml_attrs",
    "attributes",
    "text",
    "text_lines",
    "text_layer_attributes_sha256",
    "text_layer_add_attributes_v01_sha256",
    "text_layer_attributes_version",
    "text_layer_name_auto_change_enabled",
    "vector_normal_type",
    "vector_indices",
    "vector_outer_ids",
    "vector_normal_stroke_sha256",
    "vector_normal_fill_sha256",
    "vector_normal_balloon_sha256",
];

const PAYLOAD_KEYS: [&str; 2] = ["render_payload_sha256", "mask_payload_sha256"];
const META_LABELS: [(&str, &str); 5] =
    [("protect_alpha", "lock transparency"), ("draft", "draft"), ("color", "layer color"), ("text", "text"), ("text_lines", "text")];
const META_COVERED: [&str; 7] = ["parent_id", "composite_id", "pixel_width", "pixel_height", "channel_profile", "width", "height"];
const XML_COVERED: [&str; 16] = [
    "name", "alpha", "mode", "visible", "clipping", "ofsx", "ofsy", "parentId", "protectAlpha", "draft", "color", "id", "bin", "width",
    "height", "folderOpen",
];
const OTHER: &str = "other properties";
const RASTER_LAYER_ID: &str = "image:0";
const FOLDER_TOGGLE: &str = "folder open/closed";
const TOP_LEVEL: &str = "(top level)";

/// `(parent stable id, parent path)`.
pub type Parent = (Value, String);

/// `layer_props(...)`: the comparable properties.
pub struct Props {
    fields: Map,
    folder: Parent,
    meta: Map,
}

fn str_of(v: &Value) -> Result<String> {
    v.py_str().ok_or_else(|| fwd("str() of a container"))
}

/// `layer_props(layer, parent)` of a stored layer (a tree dict).
pub fn stored_props(layer: &Value, parent: &Parent) -> Result<Props> {
    let mut fields = Map::new();
    for k in PARTIAL_STAGE_LAYER_FIELDS {
        fields.insert(k.into(), layer.get(k).cloned().unwrap_or(Value::Null));
    }
    let meta = match layer.get("source_meta") {
        Some(Value::Object(m)) => m.clone(),
        Some(v) if v.truthy() => return Err(fwd("source_meta is not an object")),
        _ => Map::new(),
    };
    Ok(Props { fields, folder: parent.clone(), meta })
}

/// `layer_props(layer, parent)` of an `ImportedLayer`.
pub fn imported_props(layer: &ImportedLayer, parent: &Parent) -> Props {
    let mut fields = Map::new();
    for k in PARTIAL_STAGE_LAYER_FIELDS {
        fields.insert(k.into(), layer.field(k));
    }
    Props { fields, folder: parent.clone(), meta: layer.source_meta.clone() }
}

fn f<'a>(p: &'a Props, k: &str) -> &'a Value {
    p.fields.get(k).unwrap_or(&Value::Null)
}

/// `format_opacity(value)`.
fn format_opacity(v: &Value) -> Result<String> {
    match v {
        Value::Bool(_) => str_of(v),
        Value::Null => Ok(String::new()),
        Value::Int(tmi_store::json::Int::Small(i)) => Ok(format!("{}%", (*i as f64 * 100.0).round_ties_even() as i64)),
        Value::Float(x) => {
            if !x.is_finite() {
                return Err(fwd("round() of a non-finite opacity"));
            }
            Ok(format!("{}%", (x * 100.0).round_ties_even() as i64))
        }
        _ => str_of(v),
    }
}

fn as_number(v: &Value) -> Option<f64> {
    match v {
        Value::Int(tmi_store::json::Int::Small(i)) => Some(*i as f64),
        Value::Float(x) => Some(*x),
        _ => None,
    }
}

/// `opacity_texts(old, new)`.
fn opacity_texts(old: &Value, new: &Value) -> Result<(String, String)> {
    let (a, b) = (format_opacity(old)?, format_opacity(new)?);
    if a == b {
        if let (Some(x), Some(y)) = (as_number(old), as_number(new)) {
            return Ok((format!("{:.1}%", x * 100.0), format!("{:.1}%", y * 100.0)));
        }
    }
    Ok((a, b))
}

fn on_off(v: &Value) -> &'static str {
    if v.is_null() {
        "-"
    } else if v.truthy() {
        "on"
    } else {
        "off"
    }
}

fn yes_no(v: &Value) -> &'static str {
    if v.is_null() {
        "-"
    } else if v.truthy() {
        "yes"
    } else {
        "no"
    }
}

/// `_text(value)`.
fn text_of(v: &Value) -> Result<String> {
    let text = match v {
        Value::Array(items) => items.iter().map(str_of).collect::<Result<Vec<_>>>()?.join("\n"),
        Value::Null => String::new(),
        other => str_of(other)?,
    };
    let text = text.replace("\r\n", "\n").replace('\n', " / ");
    let chars: Vec<char> = text.chars().collect();
    let text = if chars.len() > 40 { chars[..39].iter().collect::<String>() + "…" } else { text };
    Ok(format!("\"{text}\""))
}

fn clip_folder_toggle(before: Option<&Value>, after: Option<&Value>) -> bool {
    match (before, after) {
        (Some(Value::Int(tmi_store::json::Int::Small(a))), Some(Value::Int(tmi_store::json::Int::Small(b)))) => {
            a & 1 == 1 && b & 1 == 1 && a != b
        }
        _ => false,
    }
}

fn folder_toggled(old: &Map, new: &Map) -> bool {
    if let (Some(a), Some(b)) = (old.get("folder_open"), new.get("folder_open")) {
        return a.truthy() != b.truthy();
    }
    clip_folder_toggle(old.get("folder"), new.get("folder"))
}

fn other_meta_changed(old: &Map, new: &Map) -> bool {
    for key in PARTIAL_STAGE_SOURCE_META_KEYS {
        if META_LABELS.iter().any(|(k, _)| *k == key) || META_COVERED.contains(&key) {
            continue;
        }
        let (Some(before), Some(after)) = (old.get(key), new.get(key)) else { continue };
        if key == "folder" && clip_folder_toggle(Some(before), Some(after)) {
            continue;
        }
        if key == "xml_attrs" {
            if let (Value::Object(a), Value::Object(b)) = (before, after) {
                let keep = |m: &Map| -> Value {
                    Value::Object(m.iter().filter(|(k, _)| !XML_COVERED.contains(&k.as_str())).map(|(k, v)| (k.clone(), v.clone())).collect())
                };
                if py_ne(&keep(a), &keep(b)) {
                    return true;
                }
                continue;
            }
        }
        if py_ne(before, after) {
            return true;
        }
    }
    false
}

/// One `(label, old, new)` of `property_changes` (None: Python's None).
pub type PropChange = (String, Option<String>, Option<String>);

/// `format_change(label, before, after)`.
pub fn format_change(change: &PropChange) -> String {
    let (label, a, b) = change;
    if a.is_none() && b.is_none() {
        return format!("{label} changed");
    }
    let s = |v: &Option<String>| v.clone().unwrap_or_else(|| "None".into());
    format!("{label}: {} -> {}", s(a), s(b))
}

/// `property_changes(old, new, metadata_changed=...)` as line fragments
/// (`format_change`).
fn property_changes(old: &Props, new: &Props, metadata_changed: bool) -> Result<Vec<String>> {
    Ok(property_change_parts(old, new, metadata_changed)?.iter().map(format_change).collect())
}

/// `property_changes(old, new, metadata_changed=...)`.
pub fn property_change_parts(old: &Props, new: &Props, metadata_changed: bool) -> Result<Vec<PropChange>> {
    let mut out: Vec<PropChange> = Vec::new();
    let raw = |v: &Value| -> Result<Option<String>> { if v.is_null() { Ok(None) } else { str_of(v).map(Some) } };
    let change = |label: &str, a: String, b: String| (label.to_string(), Some(a), Some(b));
    if py_ne(f(old, "name"), f(new, "name")) {
        out.push(("name".into(), raw(f(old, "name"))?, raw(f(new, "name"))?));
    }
    if py_ne(f(old, "kind"), f(new, "kind")) {
        out.push(("kind".into(), raw(f(old, "kind"))?, raw(f(new, "kind"))?));
    }
    if py_ne(&old.folder.0, &new.folder.0) {
        let p = |s: &str| if s.is_empty() { TOP_LEVEL.to_string() } else { s.to_string() };
        out.push(change("folder", p(&old.folder.1), p(&new.folder.1)));
    }
    if py_ne(f(old, "visible"), f(new, "visible")) {
        out.push(change("visible", yes_no(f(old, "visible")).into(), yes_no(f(new, "visible")).into()));
    }
    if py_ne(f(old, "opacity"), f(new, "opacity")) {
        let (a, b) = opacity_texts(f(old, "opacity"), f(new, "opacity"))?;
        out.push(change("opacity", a, b));
    }
    if py_ne(f(old, "blend_mode"), f(new, "blend_mode")) {
        out.push(("blend mode".into(), raw(f(old, "blend_mode"))?, raw(f(new, "blend_mode"))?));
    }
    if f(old, "clip_to_below").truthy() != f(new, "clip_to_below").truthy() {
        out.push(change("clipping", on_off(f(old, "clip_to_below")).into(), on_off(f(new, "clip_to_below")).into()));
    }
    let off = |p: &Props, k: &str| -> Value {
        let v = f(p, k);
        if v.truthy() { v.clone() } else { Value::Int(tmi_store::json::Int::Small(0)) }
    };
    let (ox, oy, nx, ny) = (off(old, "offset_x"), off(old, "offset_y"), off(new, "offset_x"), off(new, "offset_y"));
    if py_ne(&ox, &nx) || py_ne(&oy, &ny) {
        out.push(change("offset", format!("({}, {})", str_of(&ox)?, str_of(&oy)?), format!("({}, {})", str_of(&nx)?, str_of(&ny)?)));
    }
    let mut shown: Vec<&str> = Vec::new();
    for (key, label) in META_LABELS {
        if !old.meta.contains_key(key) && !new.meta.contains_key(key) {
            continue;
        }
        let null = Value::Null;
        let before = old.meta.get(key).unwrap_or(&null);
        let after = new.meta.get(key).unwrap_or(&null);
        if py_eq(before, after) || shown.contains(&label) {
            continue;
        }
        shown.push(label);
        if key == "text" || key == "text_lines" {
            out.push(change(label, text_of(before)?, text_of(after)?));
        } else if matches!(before, Value::Bool(_)) || matches!(after, Value::Bool(_)) {
            out.push(change(label, on_off(before).into(), on_off(after).into()));
        } else {
            let s = |v: &Value| -> Result<String> { if v.is_null() { Ok("-".into()) } else { str_of(v) } };
            out.push(change(label, s(before)?, s(after)?));
        }
    }
    if folder_toggled(&old.meta, &new.meta) {
        out.push((FOLDER_TOGGLE.into(), None, None));
    }
    if other_meta_changed(&old.meta, &new.meta) || (metadata_changed && out.is_empty()) {
        out.push((OTHER.into(), None, None));
    }
    Ok(out)
}

/// layer_props `walk_tree(repo, tree_ref)`: every stored layer with its
/// `(parent stable id, parent path)`.
pub fn walk_tree(repo: &Repo, tree_ref: &str, parent: Parent, out: &mut Vec<(Value, Parent)>, depth: usize) -> Result<()> {
    if depth > 200 {
        return Err(fwd("layer tree too deep"));
    }
    let tree = repo.store.read_json(tree_ref, Some("tree"))?;
    let Some(layers) = tree.get("layers") else { return Ok(()) };
    for layer in layers.as_array().ok_or_else(|| fwd("tree layers not a list"))? {
        out.push((layer.clone(), parent.clone()));
        match layer.get("subtree_ref") {
            Some(Value::Str(s)) if !s.is_empty() => {
                let name = match layer.get("name") {
                    Some(Value::Str(n)) => n.clone(),
                    _ => return Err(fwd("layer name is not a str")),
                };
                let path = if parent.1.is_empty() { name } else { format!("{}/{name}", parent.1) };
                let sid = layer.get("stable_source_id").cloned().unwrap_or(Value::Null);
                walk_tree(repo, s, (sid, path), out, depth + 1)?;
            }
            Some(v) if v.truthy() => return Err(fwd("subtree_ref is not a str")),
            _ => {}
        }
    }
    Ok(())
}

/// layer_props `walk_imported(layers)`.
pub fn walk_imported<'a>(layers: &'a [ImportedLayer], parent: Parent, out: &mut Vec<(&'a ImportedLayer, Parent)>) {
    for l in layers {
        out.push((l, parent.clone()));
        if !l.children.is_empty() {
            let path = if parent.1.is_empty() { l.name.clone() } else { format!("{}/{}", parent.1, l.name) };
            walk_imported(&l.children, (Value::Str(l.stable_source_id.clone()), path), out);
        }
    }
}

/// `_layer_metadata_changed(old_entry, old_layer, new_layer)`.
fn metadata_changed(old_entry: &Value, old: &Value, new: &ImportedLayer) -> Result<bool> {
    for key in PARTIAL_STAGE_LAYER_FIELDS {
        if py_ne(&old.get(key).cloned().unwrap_or(Value::Null), &new.field(key)) {
            return Ok(true);
        }
    }
    let old_meta = match old.get("source_meta") {
        Some(Value::Object(m)) => m.clone(),
        Some(v) if v.truthy() => return Err(fwd("source_meta is not an object")),
        _ => Map::new(),
    };
    let new_meta = &new.source_meta;
    let null = Value::Null;
    if old_entry.get("source_format").and_then(Value::as_str) != Some("clip") {
        return Ok(py_ne(old_meta.get("semantic_digest").unwrap_or(&null), new_meta.get("semantic_digest").unwrap_or(&null)));
    }
    for key in PARTIAL_STAGE_SOURCE_META_KEYS {
        if !old_meta.contains_key(key) && !new_meta.contains_key(key) {
            continue;
        }
        let old_value = match old_meta.get(key) {
            Some(v) => v.clone(),
            None => {
                if PAYLOAD_KEYS.contains(&key) {
                    // _clip_legacy_payload_digest: reads the old source blob
                    return Err(fwd("legacy CLIP record without payload digests"));
                }
                if (key == "width" || key == "height") && old.get("blob_ref").is_some_and(Value::truthy) {
                    return Err(fwd("legacy record: Python decodes the stored blob"));
                }
                Value::Null
            }
        };
        let new_value = new_meta.get(key).unwrap_or(&null);
        if py_ne(&old_value, new_value) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn sort_key_name(v: &Value) -> Result<String> {
    match v.get("name") {
        None => Ok(String::new()),
        Some(Value::Str(s)) => Ok(s.clone()),
        Some(_) => Err(fwd("layer name is not a str")),
    }
}

/// `layer_changes(old_entry, imported)`.
pub fn layer_changes(repo: &Repo, old_entry: &Value, imported: &ImportedFile) -> Result<Vec<String>> {
    let tree_ref = match old_entry.get("tree_ref") {
        Some(Value::Str(s)) => s.clone(),
        _ => return Err(fwd("entry without a tree_ref")),
    };
    let mut old_list = Vec::new();
    walk_tree(repo, &tree_ref, (Value::Null, String::new()), &mut old_list, 0)?;
    let mut old_layers: IndexMap<String, (Value, Parent)> = IndexMap::new();
    for (layer, parent) in old_list {
        let sid = match layer.get("stable_source_id") {
            Some(Value::Str(s)) => s.clone(),
            _ => return Err(fwd("stable_source_id is not a str")),
        };
        old_layers.insert(sid, (layer, parent));
    }
    let mut new_list = Vec::new();
    walk_imported(&imported.layers, (Value::Null, String::new()), &mut new_list);
    let mut new_layers: IndexMap<String, (&ImportedLayer, Parent)> = IndexMap::new();
    for (layer, parent) in new_list {
        new_layers.insert(layer.stable_source_id.clone(), (layer, parent));
    }
    let mut sorted_new: Vec<(&String, &(&ImportedLayer, Parent))> = new_layers.iter().collect();
    sorted_new.sort_by(|a, b| a.1 .0.name.cmp(&b.1 .0.name));
    let mut changes = Vec::new();
    for (sid, (layer, new_parent)) in sorted_new {
        let Some((old, old_parent)) = old_layers.get(sid) else {
            changes.push(format!("added layer {}", layer.name));
            continue;
        };
        let old_pixel = old.get("pixel_sha256").cloned().unwrap_or(Value::Null);
        if old_pixel.is_null() && old.get("blob_ref").is_some_and(Value::truthy) {
            return Err(fwd("legacy layer without a pixel digest: Python decodes its blob"));
        }
        let pixels_changed = match (&old_pixel, &layer.pixel_sha256) {
            (p, Some(n)) if !p.is_null() && !n.is_null() => py_ne(p, n),
            _ => {
                let old_blob = old.get("blob_ref").cloned().unwrap_or(Value::Null);
                let new_blob = layer.blob_bytes.as_ref().map_or(Value::Null, |b| Value::Str(tmi_store::object_id("blob", b)));
                py_ne(&old_blob, &new_blob)
            }
        };
        let prop_changed = ["visible", "opacity", "blend_mode", "clip_to_below", "offset_x", "offset_y"]
            .iter()
            .any(|k| py_ne(&old.get(*k).cloned().unwrap_or(Value::Null), &layer.field(k)));
        let meta_changed = metadata_changed(old_entry, old, layer)?;
        let props = property_changes(
            &stored_props(old, old_parent)?,
            &imported_props(layer, new_parent),
            (prop_changed || meta_changed) && !pixels_changed,
        )?;
        if pixels_changed || !props.is_empty() {
            let mut items: Vec<String> = Vec::new();
            if pixels_changed && !props.is_empty() {
                items.push("pixels".into());
            }
            items.extend(props.iter().cloned());
            let detail = if items.is_empty() { String::new() } else { format!(" ({})", items.join(", ")) };
            if sid == RASTER_LAYER_ID && props.is_empty() {
                changes.push("changed pixels".to_string());
            } else {
                changes.push(format!("changed layer {}{detail}", layer.name));
            }
        }
    }
    let mut sorted_old: Vec<(&String, &(Value, Parent))> = old_layers.iter().collect();
    let mut keys = Vec::new();
    for (sid, (old, _)) in &sorted_old {
        keys.push(((*sid).clone(), sort_key_name(old)?));
    }
    let key_of: std::collections::HashMap<String, String> = keys.into_iter().collect();
    sorted_old.sort_by(|a, b| key_of[a.0].cmp(&key_of[b.0]));
    for (sid, (old, _)) in sorted_old {
        if !new_layers.contains_key(sid) {
            let name = match old.get("name") {
                Some(v) => str_of(v)?,
                None => sid.clone(),
            };
            changes.push(format!("deleted layer {name}"));
        }
    }
    Ok(changes)
}
