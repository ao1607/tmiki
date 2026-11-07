//! importers.py `import_clip_metadata`: the layer tree and per-layer
//! metadata as Python's sqlite3 reads them, every leaf's offscreen decoded
//! (tmi-clip) into a layer crop, and the composite: CSP's CanvasPreview, or
//! tmi-render's layered render (incremental from an earlier version when
//! one is given, `partial`).

use std::collections::{HashMap, HashSet};

use rusqlite::types::Value as Sv;
use rusqlite::{Connection, MAIN_DB};
use tmi_clip::container::{self, Chunk};
use tmi_clip::ClipDoc;
use tmi_store::json::{Int, Map, Value};
use tmi_store::oid::{sha256_hex, Sha256};

use crate::{decline, Decline, Options, Res, Rendered, Rgba, Warnings};

fn sq(e: rusqlite::Error) -> Decline {
    decline(format!("sqlite: {e}"))
}

fn int(v: i64) -> Value {
    Value::Int(Int::Small(v))
}

/// `x or 0` of an integer column; Decline for what Python would keep as
/// another type.
fn int_or0(v: &Sv) -> Res<i64> {
    match v {
        Sv::Null => Ok(0),
        Sv::Integer(i) => Ok(*i),
        Sv::Real(r) if *r == 0.0 => Ok(0),
        _ => Err(decline("CLIP column of an unexpected type")),
    }
}

/// A raw id column (`if not x` means absent).
fn key(v: &Sv) -> Res<Option<i64>> {
    match v {
        Sv::Null | Sv::Integer(0) => Ok(None),
        Sv::Integer(i) => Ok(Some(*i)),
        _ => Err(decline("CLIP id column of an unexpected type")),
    }
}

fn opt_text(v: &Sv) -> Res<Option<String>> {
    match v {
        Sv::Null => Ok(None),
        Sv::Text(s) => Ok(Some(s.clone())),
        _ => Err(decline("CLIP text column of an unexpected type")),
    }
}

fn opt_int(v: &Sv) -> Res<Option<i64>> {
    match v {
        Sv::Null => Ok(None),
        Sv::Integer(i) => Ok(Some(*i)),
        _ => Err(decline("CLIP column of an unexpected type")),
    }
}

/// A Python value a row gives, as JSON (`json.dumps` of int, float, str).
fn scalar(v: &Sv) -> Res<Value> {
    Ok(match v {
        Sv::Null => Value::Null,
        Sv::Integer(i) => int(*i),
        Sv::Real(r) => Value::Float(*r),
        Sv::Text(s) => Value::Str(s.clone()),
        Sv::Blob(_) => return Err(decline("bytes in CLIP metadata (Python's JSON fails)")),
    })
}

/// The `LayerNode` fields the importer reads.
pub struct Node {
    pub main_id: i64,
    pub name: Option<String>,
    pub layer_type: Option<i64>,
    pub folder: i64,
    pub visible: bool,
    pub opacity: i64,
    pub composite: i64,
    pub clip: i64,
    pub masking: i64,
    pub offset_x: i64,
    pub offset_y: i64,
    pub uuid: Option<String>,
    pub render_mipmap: Option<i64>,
    pub mask_mipmap: Option<i64>,
    pub render_outer_id: Option<String>,
    pub mask_outer_id: Option<String>,
    pub children: Vec<Node>,
}

impl Node {
    pub fn sid(&self) -> String {
        match &self.uuid {
            Some(u) if !u.is_empty() => u.clone(),
            _ => format!("clip:{}", self.main_id),
        }
    }
}

/// `layer_offscreen_info`: (offscreen id, Attribute, str(outer id)).
#[derive(Clone)]
pub struct Info {
    pub offscreen_id: i64,
    pub attribute: Vec<u8>,
    pub outer_id: String,
}

const OFFSCREEN_SQL: &str = "SELECT o.MainId, o.Attribute, CAST(o.BlockData AS TEXT) FROM Mipmap m \
     JOIN MipmapInfo mi ON mi.MainId = m.BaseMipmapInfo JOIN Offscreen o ON o.MainId = mi.Offscreen WHERE m.MainId = ?";

struct Db<'a> {
    con: &'a Connection,
    layer_columns: HashSet<String>,
}

pub fn open_db(sqlite: &[u8]) -> Res<Connection> {
    let mut con = Connection::open_in_memory().map_err(sq)?;
    con.deserialize_read_exact(MAIN_DB, sqlite, sqlite.len(), true).map_err(sq)?;
    Ok(con)
}

/// The column names of `PRAGMA table_info(table)`, in order.
pub fn table_columns_ordered(con: &Connection, table: &str) -> Vec<String> {
    let Ok(mut stmt) = con.prepare(&format!("PRAGMA table_info({table})")) else {
        return Vec::new();
    };
    let Ok(rows) = stmt.query_map([], |r| r.get::<_, Sv>(1)) else {
        return Vec::new();
    };
    rows.filter_map(|r| match r {
        Ok(Sv::Text(s)) => Some(s),
        _ => None,
    })
    .collect()
}

pub fn table_columns(con: &Connection, table: &str) -> HashSet<String> {
    let Ok(mut stmt) = con.prepare(&format!("PRAGMA table_info({table})")) else {
        return HashSet::new();
    };
    let Ok(rows) = stmt.query_map([], |r| r.get::<_, Sv>(1)) else {
        return HashSet::new();
    };
    rows.filter_map(|r| match r {
        Ok(Sv::Text(s)) => Some(s),
        _ => None,
    })
    .collect()
}

impl Db<'_> {
    fn row(&self, sql: &str, id: i64, n: usize) -> Res<Option<Vec<Sv>>> {
        let mut stmt = self.con.prepare_cached(sql).map_err(sq)?;
        let mut rows = stmt.query([id]).map_err(sq)?;
        match rows.next().map_err(sq)? {
            None => Ok(None),
            Some(r) => Ok(Some((0..n).map(|i| r.get::<_, Sv>(i)).collect::<Result<_, _>>().map_err(sq)?)),
        }
    }

    /// `_outer_id_for_mipmap`.
    fn outer_id(&self, mipmap: Option<i64>) -> Res<Option<String>> {
        let Some(mm) = mipmap else { return Ok(None) };
        match self.row(OFFSCREEN_SQL, mm, 3)? {
            None => Ok(None),
            Some(r) => opt_text(&r[2]),
        }
    }

    /// `layer_offscreen_info(clip, mipmap)`.
    pub fn offscreen_info(&self, mipmap: Option<i64>) -> Res<Option<Info>> {
        let Some(mm) = mipmap else { return Ok(None) };
        let Some(r) = self.row(OFFSCREEN_SQL, mm, 3)? else { return Ok(None) };
        let Sv::Integer(offscreen_id) = r[0] else { return Err(decline("Offscreen.MainId is not an integer")) };
        let Sv::Blob(attribute) = &r[1] else { return Err(decline("Offscreen.Attribute is not a blob")) };
        let outer_id = match &r[2] {
            Sv::Null => "None".to_string(),
            Sv::Text(s) => s.clone(),
            _ => return Err(decline("BlockData did not cast to text")),
        };
        Ok(Some(Info { offscreen_id, attribute: attribute.clone(), outer_id }))
    }

    /// `load_layer(con, main_id)`.
    fn load(&self, main_id: i64) -> Res<Option<(Node, Option<i64>, Option<i64>)>> {
        let Some(r) = self.row(
            "SELECT MainId, LayerName, LayerType, LayerFolder, LayerVisibility, LayerOpacity, LayerComposite, \
             LayerClip, LayerMasking, LayerOffsetX, LayerOffsetY, LayerUuid, LayerRenderMipmap, LayerLayerMaskMipmap, \
             LayerNextIndex, LayerFirstChildIndex FROM Layer WHERE MainId = ?",
            main_id,
            16,
        )?
        else {
            return Ok(None);
        };
        let Sv::Integer(mid) = r[0] else { return Err(decline("Layer.MainId is not an integer")) };
        let vis = int_or0(&r[4])?;
        let render_mipmap = key(&r[12])?;
        let mask_mipmap = key(&r[13])?;
        let node = Node {
            main_id: mid,
            name: opt_text(&r[1])?,
            layer_type: opt_int(&r[2])?,
            folder: int_or0(&r[3])?,
            visible: vis & 1 != 0,
            opacity: int_or0(&r[5])?,
            composite: int_or0(&r[6])?,
            clip: int_or0(&r[7])?,
            masking: int_or0(&r[8])?,
            offset_x: int_or0(&r[9])?,
            offset_y: int_or0(&r[10])?,
            uuid: opt_text(&r[11])?,
            render_mipmap,
            mask_mipmap,
            render_outer_id: self.outer_id(render_mipmap)?,
            mask_outer_id: self.outer_id(mask_mipmap)?,
            children: Vec::new(),
        };
        Ok(Some((node, key(&r[14])?, key(&r[15])?)))
    }

    fn children(&self, first: Option<i64>, seen: &mut HashSet<i64>, depth: usize) -> Res<Vec<Node>> {
        // The one place the nesting is bounded (see `tmi_clip::model`): every
        // walk over the finished tree is shallower than this.
        if depth > tmi_clip::model::MAX_LAYER_DEPTH {
            return Err(decline(format!("CLIP layer tree is nested over {} folders deep", tmi_clip::model::MAX_LAYER_DEPTH)));
        }
        let mut out = Vec::new();
        let mut cur = first;
        while let Some(id) = cur {
            let Some((mut node, next, first_child)) = self.load(id)? else { break };
            if !seen.insert(node.main_id) {
                return Err(decline("CLIP layer tree has a cycle"));
            }
            if first_child.is_some() {
                node.children = self.children(first_child, seen, depth + 1)?;
            }
            out.push(node);
            cur = next;
        }
        Ok(out)
    }

    /// `_clip_extra_layer_meta(clip, node)`.
    fn extra_meta(&self, main_id: i64) -> Res<Map> {
        const WANTED: [&str; 12] = [
            "TextLayerString",
            "TextLayerAttributes",
            "TextLayerAddAttributesV01",
            "TextLayerAttributesVersion",
            "TextLayerNameAutoChangeEnabled",
            "VectorNormalType",
            "VectorNormalStrokeIndex",
            "VectorNormalFillIndex",
            "VectorNormalBalloonIndex",
            "VectorNormalStroke",
            "VectorNormalFill",
            "VectorNormalBalloon",
        ];
        let selected: Vec<&str> = WANTED.iter().copied().filter(|c| self.layer_columns.contains(*c)).collect();
        let mut meta = Map::new();
        if selected.is_empty() {
            return Ok(meta);
        }
        let sql = format!("SELECT {} FROM Layer WHERE MainId = ?", selected.join(", "));
        let Some(row) = self.row(&sql, main_id, selected.len())? else { return Ok(meta) };
        let values: HashMap<&str, &Sv> = selected.iter().copied().zip(row.iter()).collect();
        let get = |k: &str| values.get(k).copied().unwrap_or(&Sv::Null);
        let blob = |v: &Sv| -> Res<Vec<u8>> {
            match v {
                Sv::Blob(b) => Ok(b.clone()),
                _ => Err(decline("CLIP blob column of an unexpected type")),
            }
        };
        match get("TextLayerString") {
            Sv::Null => {}
            Sv::Text(s) => {
                meta.insert("text".into(), Value::Str(s.clone()));
            }
            Sv::Blob(b) => {
                let s = String::from_utf8(b.clone()).map_err(|_| decline("CLIP text is not UTF-8"))?;
                meta.insert("text".into(), Value::Str(s));
            }
            _ => return Err(decline("CLIP text column of an unexpected type")),
        }
        for (col, snake) in [
            ("TextLayerAttributes", "text_layer_attributes"),
            ("TextLayerAddAttributesV01", "text_layer_add_attributes_v01"),
        ] {
            let v = get(col);
            if !matches!(v, Sv::Null) {
                let b = blob(v)?;
                meta.insert(format!("{snake}_sha256"), Value::Str(sha256_hex(&b)));
                meta.insert(format!("{snake}_bytes"), int(b.len() as i64));
            }
        }
        for (col, snake) in [
            ("TextLayerAttributesVersion", "text_layer_attributes_version"),
            ("TextLayerNameAutoChangeEnabled", "text_layer_name_auto_change_enabled"),
            ("VectorNormalType", "vector_normal_type"),
        ] {
            let v = get(col);
            if !matches!(v, Sv::Null) {
                meta.insert(snake.into(), scalar(v)?);
            }
        }
        let mut indices = Map::new();
        for (col, snake) in [
            ("VectorNormalStrokeIndex", "vector_normal_stroke_index"),
            ("VectorNormalFillIndex", "vector_normal_fill_index"),
            ("VectorNormalBalloonIndex", "vector_normal_balloon_index"),
        ] {
            match get(col) {
                Sv::Null | Sv::Integer(0) => {}
                Sv::Integer(i) => {
                    indices.insert(snake.into(), int(*i));
                }
                Sv::Real(r) if *r == 0.0 => {}
                Sv::Text(s) if s.is_empty() => {}
                Sv::Blob(b) if b.is_empty() => {}
                _ => return Err(decline("CLIP vector index of an unexpected type")),
            }
        }
        if !indices.is_empty() {
            let ids: Vec<i64> = indices.values().filter_map(Value::as_i64).collect();
            meta.insert("vector_indices".into(), Value::Object(indices));
            meta.insert("vector_outer_ids".into(), Value::Array(self.vector_outer_ids(&ids)?));
        }
        for (col, snake) in [
            ("VectorNormalStroke", "vector_normal_stroke"),
            ("VectorNormalFill", "vector_normal_fill"),
            ("VectorNormalBalloon", "vector_normal_balloon"),
        ] {
            let v = get(col);
            if !matches!(v, Sv::Null) {
                meta.insert(format!("{snake}_sha256"), Value::Str(sha256_hex(&blob(v)?)));
            }
        }
        Ok(meta)
    }

    /// `_clip_vector_outer_ids(clip, indices)`.
    fn vector_outer_ids(&self, indices: &[i64]) -> Res<Vec<Value>> {
        let cols = table_columns(self.con, "VectorObjectList");
        if !cols.contains("MainId") || !cols.contains("VectorData") {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for &index in indices {
            let Some(r) = self.row("SELECT VectorData FROM VectorObjectList WHERE MainId = ?", index, 1)? else {
                continue;
            };
            let raw = match &r[0] {
                Sv::Null => continue,
                Sv::Blob(b) => b.clone(),
                _ => return Err(decline("VectorData of an unexpected type")),
            };
            out.push(Value::Str(if raw.is_ascii() { String::from_utf8(raw).unwrap() } else { sha256_hex(&raw) }));
        }
        Ok(out)
    }
}

/// `_canvas_to_pixels` (Python `round`: ties to even).
fn canvas_to_pixels(width: f64, height: f64, dpi: f64, unit: i64) -> (i64, i64) {
    let factor = match unit {
        1 => dpi / 2.54,
        2 => dpi / 25.4,
        3 => dpi,
        4 => dpi / 72.0,
        _ => 1.0,
    };
    let r = |v: f64| (v * factor).round_ties_even() as i64;
    (r(width).max(1), r(height).max(1))
}

fn float_of(v: &Sv) -> Res<f64> {
    match v {
        Sv::Integer(i) => Ok(*i as f64),
        Sv::Real(r) => Ok(*r),
        _ => Err(decline("Canvas value is not a number")),
    }
}

/// What the importer reads from a CLIP file before decoding pixels.
pub struct ClipFile {
    pub chunk_count: usize,
    pub head_uuid: Vec<u8>,
    pub extas: HashMap<String, Chunk>,
    pub width: i64,
    pub height: i64,
    pub resolution: f64,
    pub root_children: Vec<Node>,
    /// `layer_offscreen_info` of every leaf, by MainId.
    pub leaf_infos: HashMap<i64, Option<Info>>,
    /// `_clip_extra_layer_meta` of every node, by MainId.
    pub extra: HashMap<i64, Map>,
    /// The CanvasPreview row (ImageWidth, ImageHeight, ImageData), if any.
    pub preview: Option<(Sv, Sv, Sv)>,
}

fn walk<'a>(nodes: &'a [Node], out: &mut Vec<&'a Node>) {
    for n in nodes {
        out.push(n);
        walk(&n.children, out);
    }
}

/// `open_clip_bytes` plus the importer's per-layer SQL, Python's reading.
pub fn read(raw: &[u8]) -> Res<ClipFile> {
    let chunks = container::parse_container(raw).map_err(decline)?;
    let head = chunks.iter().find(|c| &c.cid == b"Head").ok_or_else(|| decline("no Head chunk"))?;
    let head_payload = container::payload(raw, head);
    if head_payload.len() != 40 {
        return Err(decline("Head payload is not 40 bytes"));
    }
    let extas: HashMap<String, Chunk> = container::index_extas(raw, &chunks)
        .map_err(decline)?
        .into_iter()
        .map(|(k, v)| (String::from_utf8(k).unwrap(), v))
        .collect();
    let con = open_db(container::sqlite_payload(raw, &chunks).map_err(decline)?)?;
    let canvas: Vec<Sv> = con
        .query_row(
            "SELECT CanvasUnit, CanvasWidth, CanvasHeight, CanvasResolution, CanvasRootFolder FROM Canvas",
            [],
            |r| (0..5).map(|i| r.get::<_, Sv>(i)).collect(),
        )
        .map_err(sq)?;
    let unit = int_or0(&canvas[0])?;
    let resolution = float_of(&canvas[3])?;
    let (width, height) = canvas_to_pixels(float_of(&canvas[1])?, float_of(&canvas[2])?, resolution, unit);
    let Sv::Integer(root_id) = canvas[4] else { return Err(decline("CanvasRootFolder is not an integer")) };
    let db = Db { con: &con, layer_columns: table_columns(&con, "Layer") };
    let (root, _next, first_child) = db.load(root_id)?.ok_or_else(|| decline("no root layer"))?;
    let mut seen = HashSet::from([root.main_id]);
    let root_children = if first_child.is_some() { db.children(first_child, &mut seen, 1)? } else { Vec::new() };
    let mut all = Vec::new();
    walk(&root_children, &mut all);
    let mut leaf_infos = HashMap::new();
    let mut extra = HashMap::new();
    for n in &all {
        if n.folder == 0 {
            leaf_infos.insert(n.main_id, db.offscreen_info(n.render_mipmap)?);
        }
    }
    for n in &all {
        extra.insert(n.main_id, db.extra_meta(n.main_id)?);
    }
    let preview = match con.prepare("SELECT ImageWidth, ImageHeight, ImageData FROM CanvasPreview LIMIT 1") {
        Err(_) => None,
        Ok(mut stmt) => match stmt.query([]) {
            Err(_) => None,
            Ok(mut rows) => match rows.next() {
                Err(_) => None,
                Ok(None) => None,
                Ok(Some(r)) => {
                    let g = |i| r.get::<_, Sv>(i).map_err(sq);
                    Some((g(0)?, g(1)?, g(2)?))
                }
            },
        },
    };
    Ok(ClipFile {
        chunk_count: chunks.len(),
        head_uuid: head_payload[24..40].to_vec(),
        extas,
        width,
        height,
        resolution,
        root_children,
        leaf_infos,
        extra,
        preview,
    })
}

/// `_clip_exta_pieces`: the payload without its embedded external id.
pub fn exta_pieces<'a>(raw: &'a [u8], chunk: &Chunk) -> Vec<&'a [u8]> {
    let view = container::payload(raw, chunk);
    if view.len() >= 48 && &view[8..16] == b"extrnlid" {
        vec![&view[..8], &view[48..]]
    } else {
        vec![view]
    }
}

/// `_clip_exta_payload_sha256`: the sha256 of the exta payload an outer id
/// names (its `extrnlid` header stripped, like `exta_pieces`), or `None`
/// when there is no such id or chunk.
pub fn exta_payload_sha256(f: &ClipFile, raw: &[u8], outer_id: Option<&str>) -> Option<String> {
    let chunk = f.extas.get(outer_id.filter(|o| !o.is_empty())?)?;
    let mut h = Sha256::default();
    for p in exta_pieces(raw, chunk) {
        h.update(p);
    }
    Some(h.hexdigest())
}

/// The render's warnings, replayed as `_render` does.
pub fn replay(report: &tmi_render::clip::band::Report, warns: &mut Warnings) {
    use tmi_render::clip::report::{Note, Warning};
    for w in &report.warnings {
        match w {
            Warning::Composite(id) => warns.composite(*id),
            Warning::Filter(id) => warns.filter(*id),
        }
    }
    for n in &report.notes {
        match n {
            Note::Offscreen { outer_id, reason } => warns.offscreen(outer_id, reason),
            Note::TileSize { bpt, pixels } => warns.tile_size(*bpt, *pixels),
        }
    }
}

/// `render_full(clip)`: the whole-canvas layered render, whether a later
/// version can build on it (every layer decoded), and the report.
pub fn render_full(doc: &ClipDoc, raw: &[u8]) -> Res<(Rgba, bool, tmi_render::clip::band::Report)> {
    use tmi_render::clip::band::{render, Target};
    let mut out = vec![0u8; doc.width * doc.height * 4];
    let report = render(doc, raw, [255, 255, 255, 255], &Target::Canvas, false, &mut out)
        .map_err(|e| decline(format!("the CLIP renderer cannot draw this file ({e}): Python tracks it as opaque")))?;
    let is_base = report.decode_failures.is_empty();
    Ok((Rgba { width: doc.width, height: doc.height, data: out }, is_base, report))
}

pub fn composite_text() -> (String, String) {
    (
        "tmi-composite".to_string(),
        format!("{{\"renderer\":\"layered/v{}\"}}", crate::cache::RENDER_CACHE_VERSION),
    )
}

/// `canvas_preview(clip, canvas_size=True)` and `render_composite_with_png`'s
/// first step: Some((image, png to store as is)) when the CanvasPreview
/// serves.
pub fn preview(f: &ClipFile) -> Res<Option<(Rgba, Option<Vec<u8>>)>> {
    let Some((w, h, data)) = &f.preview else { return Ok(None) };
    let data = match data {
        Sv::Null => return Ok(None),
        Sv::Blob(b) => b,
        _ => return Err(decline("CanvasPreview.ImageData is not a blob")),
    };
    let canvas = (f.width, f.height);
    let declared = (int_or0(w)?, int_or0(h)?);
    // CSP writes a preview no larger than the canvas. A bigger one is a
    // decompression bomb (a 4 MB PNG can hold gigabytes of pixels): its
    // header says so before anything is allocated, and the layers draw the
    // composite instead. (`declared` is a column of the file, not trusted.)
    let canvas_pixels = (canvas.0.max(1) as u128) * (canvas.1.max(1) as u128);
    if let Ok(Some(h)) = tmi_image::header::header(data) {
        if (h.size.0 as u128) * (h.size.1 as u128) > canvas_pixels {
            return Ok(None);
        }
    }
    if (declared.0.max(0) as u128) * (declared.1.max(0) as u128) > canvas_pixels {
        return Ok(None);
    }
    let decode = || -> Res<(usize, usize, Vec<u8>)> {
        let (img, _) = tmi_image::decode_rgba(data).map_err(|e| decline(format!("CanvasPreview for Pillow ({e:?})")))?;
        Ok((img.width, img.height, img.data))
    };
    if data.starts_with(crate::header::PNG_SIGNATURE)
        && data.len() >= 24
        && &data[12..16] == b"IHDR"
        && (u32::from_be_bytes(data[16..20].try_into().unwrap()) as i64, u32::from_be_bytes(data[20..24].try_into().unwrap()) as i64)
            == canvas
        && (declared == canvas || declared == (0, 0))
    {
        let (dw, dh, rgba) = decode()?;
        if (dw as i64, dh as i64) == canvas {
            return Ok(Some((Rgba { width: dw, height: dh, data: rgba }, Some(data.clone()))));
        }
    }
    let (dw, dh, rgba) = decode()?;
    let expected = (if declared.0 != 0 { declared.0 } else { dw as i64 }, if declared.1 != 0 { declared.1 } else { dh as i64 });
    let mut image = Rgba { width: dw, height: dh, data: rgba };
    if (dw as i64, dh as i64) != expected || expected != canvas {
        use tmi_pillow::image::{Image, Mode};
        use tmi_pillow::resample::Filter;
        let mut steps = vec![expected];
        if canvas != expected {
            steps.push(canvas);
        }
        let mut img = Image::new(Mode::RGBA, image.width, image.height, image.data);
        for (sw, sh) in steps {
            if (sw, sh) == (img.width as i64, img.height as i64) {
                continue;
            }
            if sw <= 0 || sh <= 0 {
                return Err(decline("CanvasPreview resize to a non-positive size: Python raises"));
            }
            img = tmi_pillow::resize::resize(&img, (sw as usize, sh as usize), Filter::Lanczos, None, None)
                .map_err(|e| decline(format!("CanvasPreview resize: {e:?}")))?;
        }
        image = Rgba { width: img.width, height: img.height, data: img.data };
    }
    Ok(Some((image, None)))
}

/// The composite a CLIP file renders when imported (PLUGINS.md P3's
/// `NativeComposite`): the CanvasPreview when it serves (`preview`), else
/// the incremental or whole-canvas layered render, with the report either
/// one leaves for `replay` to turn into warnings. This is `import`'s old
/// composite closure, pulled out so `tmi_format::builtin_clip` can call it
/// too (the pixel decode now goes through `pipeline::import`, not here).
pub fn composite(f: &ClipFile, raw: &[u8], opts: &Options) -> Res<(Rendered, Option<tmi_render::clip::band::Report>)> {
    if let Some((image, png)) = preview(f)? {
        return Ok((Rendered { image, png, text: None, encoded_in_render: false }, None));
    }
    let doc =
        ClipDoc::open(raw).map_err(|e| decline(format!("the CLIP renderer cannot read this file ({e}): Python tracks it as opaque")))?;
    if let Some(base) = opts.composite_base {
        let incremental = crate::partial::render_incremental(&doc, raw, base, opts.verify_partial)?;
        crate::partial::log_path(incremental.is_some());
        if let Some((image, r)) = incremental {
            return Ok((Rendered { image, png: None, text: Some(composite_text()), encoded_in_render: true }, Some(r)));
        }
    }
    let (image, is_base, r) = render_full(&doc, raw)?;
    Ok((Rendered { image, png: None, text: is_base.then(composite_text), encoded_in_render: true }, Some(r)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tama() -> Vec<u8> {
        std::fs::read(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../example/tama.clip")).expect("example/tama.clip")
    }

    /// A gray 8-bit PNG of `side` x `side` zero pixels: a few hundred KB
    /// that inflate to side^2 bytes.
    fn png_bomb(side: u32) -> Vec<u8> {
        fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
            let mut crc = flate2::Crc::new();
            crc.update(kind);
            crc.update(data);
            out.extend_from_slice(&(data.len() as u32).to_be_bytes());
            out.extend_from_slice(kind);
            out.extend_from_slice(data);
            out.extend_from_slice(&crc.sum().to_be_bytes());
        }
        let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
        let row = vec![0u8; side as usize + 1];
        for _ in 0..side {
            z.write_all(&row).unwrap();
        }
        let idat = z.finish().unwrap();
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&side.to_be_bytes());
        ihdr.extend_from_slice(&side.to_be_bytes());
        ihdr.extend_from_slice(&[8, 0, 0, 0, 0]);
        let mut png = crate::header::PNG_SIGNATURE.to_vec();
        chunk(&mut png, b"IHDR", &ihdr);
        chunk(&mut png, b"IDAT", &idat);
        chunk(&mut png, b"IEND", &[]);
        png
    }

    #[test]
    fn deeply_nested_folders_are_a_decline_not_a_stack_overflow() {
        let raw = tama();
        read(&tmi_clip::testing::nested_folders(&raw, 200).unwrap()).ok().expect("200 folders deep");
        let hostile = tmi_clip::testing::nested_folders(&raw, 200_000).unwrap();
        let e = read(&hostile).err().expect("refused");
        assert!(format!("{e:?}").contains("nested over"), "{e:?}");
    }

    #[test]
    fn a_canvas_preview_larger_than_the_canvas_is_not_decoded() {
        let raw = tama();
        let f = read(&raw).ok().expect("tama.clip");
        let (cw, ch) = (f.width, f.height);
        // 30000 x 30000 gray = 900 MP (3.6 GB as RGBA) from a few megabytes.
        let bomb = png_bomb(30000);
        assert!(bomb.len() < 16_000_000, "{}", bomb.len());
        let hostile = tmi_clip::testing::with_preview(&raw, cw, ch, &bomb).unwrap();
        let f = read(&hostile).ok().expect("read");
        assert!(matches!(preview(&f), Ok(None)), "the preview bigger than its canvas is left out");
        // Declared larger than the canvas, whatever the data.
        let hostile = tmi_clip::testing::with_preview(&raw, 1 << 20, 1 << 20, b"junk").unwrap();
        let f = read(&hostile).ok().expect("read");
        assert!(matches!(preview(&f), Ok(None)));
        // The file's own preview (smaller than the canvas) still serves.
        let f = read(&raw).ok().expect("read");
        assert!(matches!(preview(&f), Ok(Some(_))));
    }
}
