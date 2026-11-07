//! clip_partial.py: re-render only the canvas rectangles whose composite
//! can have changed since an earlier version of the file, pasted into that
//! version's composite. The result is the whole-canvas render by
//! construction, so every doubt here falls back to rendering the whole
//! canvas (it only costs time); `Err` only where Python would print a
//! warning or fail.

use std::collections::{HashMap, HashSet};

use rusqlite::types::Value as Sv;
use rusqlite::Connection;
use tmi_clip::attr::parse_offscreen_attribute;
use tmi_clip::container::{self, Chunk};
use tmi_clip::model::{Col, Node};
use tmi_clip::ClipDoc;
use tmi_render::clip::band::{render, Report, Target};

use crate::clip::{open_db, render_full};
use crate::{decline, CompositeBase, Res, Rgba};

pub const MAX_DIRTY_FRACTION: f64 = 0.5;
pub const MAX_RECTS: usize = 16;
pub const TILE: i64 = 256;

type Rect = [i64; 4];

/// `TMI_PARTIAL_LOG=<file>`: append "incremental" or "full" for every
/// layered composite that had a base (which path was taken; diagnosis).
pub fn log_path(incremental: bool) {
    if let Some(path) = std::env::var_os("TMI_PARTIAL_LOG").filter(|p| !p.is_empty()) {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(f, "{}", if incremental { "incremental" } else { "full" });
        }
    }
}

/// The base composite's `tmi-composite` text names this renderer version.
pub fn is_layered_base(png: &[u8]) -> bool {
    let Some(text) = png_text(png, "tmi-composite") else { return false };
    match tmi_store::json::loads(&text) {
        Ok(v) => v.get("renderer").and_then(|r| r.as_str()) == Some(&*format!("layered/v{}", crate::cache::RENDER_CACHE_VERSION)),
        Err(_) => false,
    }
}

/// `png_text(data, key)`: a tEXt value before the image data (latin-1).
pub fn png_text(data: &[u8], key: &str) -> Option<String> {
    if !data.starts_with(crate::header::PNG_SIGNATURE) {
        return None;
    }
    let mut want = key.as_bytes().to_vec();
    want.push(0);
    let mut p = 8usize;
    while p + 8 <= data.len() {
        let length = u32::from_be_bytes(data[p..p + 4].try_into().unwrap()) as usize;
        let kind = &data[p + 4..p + 8];
        if kind == b"IDAT" || kind == b"IEND" {
            return None;
        }
        let end = (p + 8).saturating_add(length).min(data.len());
        let body = &data[p + 8..end];
        if kind == b"tEXt" && body.starts_with(&want) {
            return Some(body[want.len()..].iter().map(|&b| b as char).collect());
        }
        p = p.saturating_add(12).saturating_add(length);
    }
    None
}

fn same_rows(a: &Connection, b: &Connection, sql: &str, cut: Option<usize>) -> Option<bool> {
    let read = |c: &Connection| -> Option<Vec<Vec<Sv>>> {
        let mut stmt = c.prepare(sql).ok()?;
        let n = stmt.column_count();
        let mut rows = stmt.query([]).ok()?;
        let mut out = Vec::new();
        while let Some(r) = rows.next().ok()? {
            let mut row: Vec<Sv> = (0..n).map(|i| r.get::<_, Sv>(i)).collect::<Result<_, _>>().ok()?;
            if let Some(k) = cut {
                if let Some(Sv::Blob(attr)) = row.get(k) {
                    let name: Vec<u8> = "BlockSize".encode_utf16().flat_map(|u| u.to_be_bytes()).collect();
                    if let Some(at) = attr.windows(name.len()).position(|w| w == name.as_slice()) {
                        if at >= 4 {
                            let end = (at + name.len() + 12).min(attr.len());
                            row[k] = Sv::Blob(attr[..end].to_vec());
                        }
                    }
                }
            }
            out.push(row);
        }
        Some(out)
    };
    Some(read(a)? == read(b)?)
}

/// `_same_database(a, b)`.
fn same_database(a: &Connection, b: &Connection) -> bool {
    let schema = "SELECT type, name, sql FROM sqlite_master ORDER BY type, name";
    if same_rows(a, b, schema, None) != Some(true) {
        return false;
    }
    let tables: Vec<(String, String)> = {
        let Ok(mut stmt) = a.prepare(schema) else { return false };
        let Ok(rows) = stmt.query_map([], |r| Ok((r.get::<_, Sv>(0)?, r.get::<_, Sv>(1)?))) else { return false };
        let mut out = Vec::new();
        for r in rows {
            match r {
                Ok((Sv::Text(t), Sv::Text(n))) => out.push((t, n)),
                _ => return false,
            }
        }
        out
    };
    for (kind, name) in tables {
        if kind != "table" || name == "CanvasPreview" || name == "sqlite_sequence" {
            continue;
        }
        if name == "ExternalChunk" {
            if same_rows(a, b, "SELECT ExternalID FROM ExternalChunk ORDER BY ExternalID", None) != Some(true) {
                return false;
            }
            continue;
        }
        let cut = if name == "Offscreen" {
            let cols = crate::clip::table_columns_ordered(a, "\"Offscreen\"");
            cols.iter().position(|c| c == "Attribute")
        } else {
            None
        };
        let sql = if name == "LayerThumbnail" {
            // `_same_database`: the write-back marks the edited layer's
            // thumbnail stale (its *NeedRefresh* columns); no composite reads them.
            let keep: Vec<String> = crate::clip::table_columns_ordered(a, "\"LayerThumbnail\"")
                .into_iter()
                .filter(|c| !c.contains("NeedRefresh"))
                .map(|c| format!("\"{}\"", c.replace('"', "\"\"")))
                .collect();
            format!("SELECT {} FROM \"LayerThumbnail\" ORDER BY rowid", keep.join(", "))
        } else {
            format!("SELECT * FROM \"{name}\" ORDER BY rowid")
        };
        if same_rows(a, b, &sql, cut) != Some(true) {
            return false;
        }
    }
    true
}

/// Exta ids in container order (dict insertion order of `index_extas`).
fn exta_order(raw: &[u8]) -> Option<Vec<String>> {
    let chunks = container::parse_container(raw).ok()?;
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for c in chunks.iter().filter(|c| &c.cid == b"Exta") {
        let a = (c.payload_off + 8).min(raw.len());
        let b = (c.payload_off + 48).min(raw.len()).max(a);
        let id = std::str::from_utf8(&raw[a..b]).ok()?.to_string();
        if seen.insert(id.clone()) {
            out.push(id);
        }
    }
    Some(out)
}

fn chunk<'a>(doc: &'a ClipDoc, id: &str) -> Option<&'a Chunk> {
    doc.extas.get(id.as_bytes())
}

/// `_offscreen_tile_index` records: block -> record bytes (in file order).
fn records(raw: &[u8], c: &Chunk) -> Option<HashMap<u32, Vec<Vec<u8>>>> {
    let payload = container::payload(raw, c);
    if payload.get(8..16) != Some(&b"extrnlid"[..]) {
        return None;
    }
    let inner_size = u64::from_be_bytes(payload.get(48..56)?.try_into().ok()?) as usize;
    let inner = &payload[56.min(payload.len())..56usize.saturating_add(inner_size).min(payload.len())];
    let begin: Vec<u8> = "BlockDataBeginChunk".encode_utf16().flat_map(|u| u.to_be_bytes()).collect();
    let mut out: HashMap<u32, Vec<Vec<u8>>> = HashMap::new();
    let mut p = 0usize;
    while p + 8 <= inner.len() {
        let rec_size = u32::from_be_bytes(inner[p..p + 4].try_into().unwrap()) as usize;
        let name_len = u32::from_be_bytes(inner[p + 4..p + 8].try_into().unwrap()) as usize;
        if name_len * 2 + 8 > rec_size || p + rec_size > inner.len() {
            break;
        }
        if rec_size >= 50 && inner.get(p + 8..p + 8 + 2 * name_len) == Some(begin.as_slice()) {
            let block = u32::from_be_bytes(inner[p + 46..p + 50].try_into().unwrap());
            out.entry(block).or_default().push(inner[p..p + rec_size].to_vec());
        }
        p += rec_size;
    }
    Some(out)
}

fn outer_key(doc: &ClipDoc, mipmap: Option<i64>) -> Option<String> {
    let info = doc.offscreen(mipmap)?;
    Some(match &info.outer_id {
        None => "None".to_string(),
        Some(b) => String::from_utf8_lossy(b).into_owned(),
    })
}

struct Use {
    kind: &'static str,
    node: usize,
    margin: i64,
}

/// `_offscreen_uses(clip)`; None where Python's `_edge_margin` raises.
fn offscreen_uses(doc: &ClipDoc) -> Option<HashMap<String, Vec<Use>>> {
    let mut uses: HashMap<String, Vec<Use>> = HashMap::new();
    fn walk(doc: &ClipDoc, nodes: &[usize], inherited: i64, uses: &mut HashMap<String, Vec<Use>>) -> Option<()> {
        for &i in nodes {
            let n: &Node = &doc.nodes[i];
            let style = doc.style(n.main_id);
            let edge = tmi_render::clip::edge::parse_edge_effect(&style, n.main_id).ok()?;
            let margin = inherited + edge.map_or(0, |e| e.margin());
            let mut add = |kind: &'static str, mm: Option<i64>| {
                if let Some(key) = outer_key(doc, mm) {
                    uses.entry(key).or_default().push(Use { kind, node: i, margin });
                }
            };
            add(if n.folder { "folder-render" } else { "render" }, n.render_mipmap);
            add("mask", n.mask_mipmap);
            if let Col::Int(m) = style.get("ResizableOriginalMipmap") {
                if *m != 0 {
                    add("material", Some(*m));
                }
            }
            walk(doc, &n.children, margin, uses)?;
        }
        Some(())
    }
    walk(doc, &doc.top, 0, &mut uses)?;
    Some(uses)
}

/// `_decodes(clip, node)`: the node's render offscreen decodes whole.
/// Err where Python warns about it.
fn decodes(doc: &ClipDoc, raw: &[u8], mipmap: Option<i64>) -> Res<bool> {
    let Some(info) = doc.offscreen(mipmap) else { return Ok(false) };
    let Some(c) = doc.exta(info) else { return Ok(false) };
    let attr = parse_offscreen_attribute(&info.attribute).map_err(|e| decline(format!("unreadable offscreen ({e}): Python warns")))?;
    if let Some(why) = tmi_clip::tiles::offscreen_oversize(attr.pixel_width, attr.pixel_height, doc.width, doc.height) {
        return Err(decline(format!("unreadable offscreen ({why}): Python warns")));
    }
    tmi_clip::tiles::decode_offscreen(raw, c.payload_off, c.payload_size, attr.tile_cols, attr.tile_rows, attr.pixel_width, attr.pixel_height)
        .map_err(|e| decline(format!("unreadable offscreen ({e}): Python warns")))?;
    Ok(true)
}

fn intersect(a: Rect, b: Rect) -> Option<Rect> {
    let r = [a[0].max(b[0]), a[1].max(b[1]), a[2].min(b[2]), a[3].min(b[3])];
    (r[0] < r[2] && r[1] < r[3]).then_some(r)
}

fn union(a: Rect, b: Rect) -> Rect {
    [a[0].min(b[0]), a[1].min(b[1]), a[2].max(b[2]), a[3].max(b[3])]
}

/// `_merge_rects`.
fn merge_rects(rects: Vec<Rect>) -> Vec<Rect> {
    let mut out = rects;
    let mut merged = true;
    while merged {
        merged = false;
        let mut result: Vec<Rect> = Vec::new();
        for r in out {
            let mut hit = false;
            for s in result.iter_mut() {
                if r[0] <= s[2] && s[0] <= r[2] && r[1] <= s[3] && s[1] <= r[3] {
                    *s = union(r, *s);
                    merged = true;
                    hit = true;
                    break;
                }
            }
            if !hit {
                result.push(r);
            }
        }
        out = result;
    }
    if out.len() > MAX_RECTS {
        let mut b = out[0];
        for r in &out[1..] {
            b = union(b, *r);
        }
        out = vec![b];
    }
    out
}

/// `dirty_rects(old, new)`: Ok(None) to render everything.
fn dirty_rects(old: &ClipDoc, old_raw: &[u8], new: &ClipDoc, new_raw: &[u8]) -> Res<Option<Vec<Rect>>> {
    let canvas: Rect = [0, 0, new.width as i64, new.height as i64];
    if (old.width, old.height) != (new.width, new.height) {
        return Ok(None);
    }
    let dbs = (|| -> Option<(Connection, Connection)> {
        let a = open_db(container::sqlite_payload(old_raw, &container::parse_container(old_raw).ok()?).ok()?).ok()?;
        let b = open_db(container::sqlite_payload(new_raw, &container::parse_container(new_raw).ok()?).ok()?).ok()?;
        Some((a, b))
    })();
    let Some((a, b)) = dbs else { return Ok(None) };
    if !same_database(&a, &b) {
        return Ok(None);
    }
    let old_ids: HashSet<&Vec<u8>> = old.extas.keys().collect();
    let new_ids: HashSet<&Vec<u8>> = new.extas.keys().collect();
    if old_ids != new_ids {
        return Ok(None);
    }
    let Some(order) = exta_order(new_raw) else { return Ok(None) };
    let mut changed = Vec::new();
    for id in &order {
        let (Some(x), Some(y)) = (chunk(old, id), chunk(new, id)) else { return Ok(None) };
        if x.payload_size != y.payload_size {
            changed.push(id.clone());
            continue;
        }
        let (Some(px), Some(py)) = (
            old_raw.get(x.payload_off..x.payload_off.saturating_add(x.payload_size)),
            new_raw.get(y.payload_off..y.payload_off.saturating_add(y.payload_size)),
        ) else {
            return Ok(None); // ValueError: the caller renders in full
        };
        if px != py {
            changed.push(id.clone());
        }
    }
    if changed.is_empty() {
        return Ok(Some(Vec::new()));
    }
    let Some(uses) = offscreen_uses(new) else { return Ok(None) };
    let mut rects = Vec::new();
    for outer_id in &changed {
        let Some(users) = uses.get(outer_id) else { return Ok(None) };
        if users.is_empty() || users.iter().any(|u| u.kind != "render") {
            return Ok(None);
        }
        // _changed_cells
        let mipmap = new.nodes[users[0].node].render_mipmap;
        let Some(info) = new.offscreen(mipmap) else { return Ok(None) };
        if info.outer_id.as_deref() != Some(outer_id.as_bytes()) {
            return Ok(None);
        }
        let Ok(attr) = parse_offscreen_attribute(&info.attribute) else { return Ok(None) };
        let cols = attr.tile_cols.max(1) as i64;
        let (Some(ra), Some(rb)) = (records(old_raw, chunk(old, outer_id).unwrap()), records(new_raw, chunk(new, outer_id).unwrap())) else {
            return Ok(None);
        };
        let blocks: HashSet<u32> = ra.keys().chain(rb.keys()).copied().collect();
        let mut cells = Vec::new();
        for block in blocks {
            if ra.get(&block) != rb.get(&block) {
                let (x, y) = ((block as i64 % cols) * TILE, (block as i64 / cols) * TILE);
                cells.push([x, y, x + TILE, y + TILE]);
            }
        }
        if cells.is_empty() {
            continue;
        }
        for u in users {
            let node = &new.nodes[u.node];
            if !decodes(new, new_raw, node.render_mipmap)? || !decodes(old, old_raw, node.render_mipmap)? {
                return Ok(None);
            }
            let dx = node.offset_x + node.render_offset_x;
            let dy = node.offset_y + node.render_offset_y;
            for c in &cells {
                let cell = [c[0] + dx - u.margin, c[1] + dy - u.margin, c[2] + dx + u.margin, c[3] + dy + u.margin];
                if let Some(hit) = intersect(cell, canvas) {
                    rects.push(hit);
                }
            }
        }
    }
    let rects = merge_rects(rects);
    let area: i64 = rects.iter().map(|r| (r[2] - r[0]) * (r[3] - r[1])).sum();
    if area as f64 > MAX_DIRTY_FRACTION * (canvas[2] * canvas[3]) as f64 {
        return Ok(None);
    }
    Ok(Some(rects))
}

/// `render_incremental(clip, base)`: Ok(None) when the whole canvas must
/// be rendered instead.
pub fn render_incremental(doc: &ClipDoc, raw: &[u8], base: &dyn CompositeBase, verify: bool) -> Res<Option<(Rgba, Report)>> {
    let Ok(png) = base.load_composite_png() else { return Ok(None) };
    if png.is_empty() || !is_layered_base(&png) {
        return Ok(None);
    }
    let Ok(old_raw) = base.load_source() else { return Ok(None) };
    let Ok(old) = ClipDoc::open(&old_raw) else { return Ok(None) };
    let (rects, decoded) = rayon::join(
        || dirty_rects(&old, &old_raw, doc, raw),
        || -> Option<Rgba> {
            let h = crate::header::png_header(&png, false).ok()?;
            if h.mode != Some("RGBA") {
                return None;
            }
            let (img, fmt) = tmi_image::decode_rgba(&png).ok()?;
            (fmt == tmi_image::Format::Png).then_some(Rgba { width: img.width, height: img.height, data: img.data })
        },
    );
    let Some(rects) = rects? else { return Ok(None) };
    let decodable_all = !doc.nodes.is_empty();
    let mut report = Report::default();
    let mut patches = Vec::new();
    for r in &rects {
        let (w, h) = ((r[2] - r[0]) as usize, (r[3] - r[1]) as usize);
        let mut buf = vec![0u8; w * h * 4];
        let rep = render(doc, raw, [255, 255, 255, 255], &Target::Region { window: *r, decodable_all }, false, &mut buf)
            .map_err(|e| decline(format!("the CLIP renderer cannot draw this file ({e}): Python tracks it as opaque")))?;
        report.warnings.extend(rep.warnings);
        report.notes.extend(rep.notes);
        patches.push((*r, w, h, buf));
    }
    let Some(mut image) = decoded else { return Ok(None) };
    if (image.width, image.height) != (doc.width, doc.height) {
        return Ok(None);
    }
    for (r, w, h, buf) in &patches {
        tmi_pillow::rgba::paste(&mut image.data, image.width, image.height, buf, *w, *h, r[0], r[1]);
    }
    if verify {
        let (full, _, _) = render_full(doc, raw)?;
        if full.data != image.data {
            return Err(decline("TMI_VERIFY_PARTIAL: the incremental composite differs (Python reports it)"));
        }
    }
    Ok(Some((image, report)))
}
