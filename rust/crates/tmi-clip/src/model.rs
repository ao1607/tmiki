//! `ClipDoc`: what the renderer needs from a CLIP file, read once when it is
//! opened (the layer tree of clip.py:110-145 / :602-694, the offscreen of
//! every mipmap a layer points at (:716-725), the per-layer styling columns of
//! clip_style.py:47-70 and the blobs of correction layers, fills and paper).
//! Nothing queries SQLite while rendering.
//!
//! Values keep Python's reading of them: `x or 0` makes NULL a 0, a raw
//! column stays an `Option`, truthiness is Python's. A value Python would
//! treat differently from an integer (a REAL where an INTEGER is expected,
//! text in a blob column) is kept as `Odd` and makes the renderer decline
//! wherever it is used.

use std::collections::{HashMap, HashSet};

use rusqlite::types::Value;
use rusqlite::{Connection, MAIN_DB};

use crate::container::{self, Chunk};

/// A column value as the renderer reads it.
#[derive(Clone, Debug, PartialEq)]
pub enum Col {
    Null,
    Int(i64),
    Blob(Vec<u8>),
    /// REAL / TEXT / a blob where a number was expected...: decline.
    Odd,
}

impl Col {
    fn from_value(v: Value) -> Col {
        match v {
            Value::Null => Col::Null,
            Value::Integer(i) => Col::Int(i),
            Value::Blob(b) => Col::Blob(b),
            _ => Col::Odd,
        }
    }

    /// Python truthiness.
    pub fn truthy(&self) -> Result<bool, String> {
        match self {
            Col::Null => Ok(false),
            Col::Int(i) => Ok(*i != 0),
            Col::Blob(b) => Ok(!b.is_empty()),
            Col::Odd => Err("unexpected column type".into()),
        }
    }

    /// `value or 0` as an integer.
    pub fn int_or0(&self) -> Result<i64, String> {
        match self {
            Col::Null => Ok(0),
            Col::Int(i) => Ok(*i),
            _ => Err("unexpected column type".into()),
        }
    }

    /// The raw value as an optional integer key (`if not x` means absent).
    pub fn key(&self) -> Result<Option<i64>, String> {
        match self {
            Col::Null => Ok(None),
            Col::Int(0) => Ok(None),
            Col::Int(i) => Ok(Some(*i)),
            _ => Err("unexpected column type".into()),
        }
    }

    /// A BLOB column (`bytes(row[0])`), None for NULL.
    pub fn blob(&self) -> Result<Option<&[u8]>, String> {
        match self {
            Col::Null => Ok(None),
            Col::Blob(b) => Ok(Some(b)),
            _ => Err("unexpected column type".into()),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Node {
    pub main_id: i64,
    /// LayerType as stored (compared with 4098 / 1584 only).
    pub layer_type: Option<i64>,
    pub folder: bool,
    pub visible: bool,
    pub mask_enabled: bool,
    pub opacity: i64,
    pub composite: i64,
    pub clip: bool,
    pub offset_x: i64,
    pub offset_y: i64,
    pub render_mipmap: Option<i64>,
    pub mask_mipmap: Option<i64>,
    pub render_offset_x: i64,
    pub render_offset_y: i64,
    pub mask_offset_x: i64,
    pub mask_offset_y: i64,
    pub comic_frame: bool,
    pub children: Vec<usize>,
}

/// How deep folders may nest. CSP's own files are a handful of levels deep;
/// the layer tree is built recursively and walked recursively (render,
/// import, write-back), so this is checked once, when the tree is built.
pub const MAX_LAYER_DEPTH: usize = 256;

/// `layer_offscreen_info`: (offscreen id, Attribute, outer id) of a mipmap.
#[derive(Clone, Debug)]
pub struct OffscreenInfo {
    pub offscreen_id: i64,
    pub attribute: Vec<u8>,
    /// The Exta key; None when CLIP stored no data block (NULL BlockData).
    pub outer_id: Option<Vec<u8>>,
}

/// The styling columns of one layer (`clip_style.layer_style`); a missing
/// column is `Col::Null`, as the dict's `.get` gives None.
#[derive(Clone, Debug, Default)]
pub struct Style {
    pub cols: HashMap<&'static str, Col>,
}

impl Style {
    pub fn get(&self, name: &str) -> &Col {
        self.cols.get(name).unwrap_or(&Col::Null)
    }
}

pub const STYLE_COLUMNS: &[&str] = &[
    "LayerType",
    "LayerFolder",
    "LayerColorTypeIndex",
    "LayerColorTypeBlackChecked",
    "LayerColorTypeWhiteChecked",
    "DrawColorEnable",
    "DrawColorMainRed",
    "DrawColorMainGreen",
    "DrawColorMainBlue",
    "DrawColorSubRed",
    "DrawColorSubGreen",
    "DrawColorSubBlue",
    "ResizableImageInfo",
    "ResizableOriginalMipmap",
    "LayerEffectAttached",
    "LayerEffectInfo",
    // `material_mono_conversion` (an image material drawn in モノクロ)
    "UsePreviewColorType",
    "PreviewColorTypeIndex",
    "PreviewColorTypeBlackChecked",
    "PreviewColorTypeWhiteChecked",
    "PreviewColorTypeThresholdImage",
    "PreviewColorTypeThresholdAlpha",
];

const OPTIONAL_LAYER_COLUMNS: &[&str] = &[
    "LayerRenderOffscrOffsetX",
    "LayerRenderOffscrOffsetY",
    "LayerMaskOffsetX",
    "LayerMaskOffsetY",
    "LayerMaskOffscrOffsetX",
    "LayerMaskOffscrOffsetY",
    "ComicFrameLineMipmap",
];

/// Per-layer columns read with `_safe_layer_blob` and friends. `None` when
/// the column does not exist (sqlite3.Error in Python).
#[derive(Clone, Debug, Default)]
pub struct Extra {
    pub filter_info: Option<Col>,
    pub gradation_info: Option<Col>,
    pub vector_index: Option<Col>,
    /// (DrawColorMainRed, Green, Blue, DrawColorEnable) for the paper layer,
    /// None when a column is missing.
    pub paper: Option<[Col; 4]>,
}

pub struct ClipDoc {
    pub width: usize,
    pub height: usize,
    pub resolution: f64,
    pub nodes: Vec<Node>,
    /// Indices of the root folder's children.
    pub top: Vec<usize>,
    pub extas: HashMap<Vec<u8>, Chunk>,
    /// mipmap id -> offscreen (None: no row).
    pub offscreens: HashMap<i64, Option<OffscreenInfo>>,
    pub styles: HashMap<i64, Style>,
    pub extra: HashMap<i64, Extra>,
    /// Strokes, brushes and comic-frame outlines (stage 1d).
    pub vectors: crate::vector::VectorTables,
}

fn err(e: rusqlite::Error) -> String {
    format!("sqlite: {e}")
}

/// `_canvas_to_pixels` (Python `round`: ties to even).
fn canvas_to_pixels(width: f64, height: f64, dpi: f64, unit: i64) -> (i64, i64) {
    let factor = match unit {
        0 => 1.0,
        1 => dpi / 2.54,
        2 => dpi / 25.4,
        3 => dpi,
        4 => dpi / 72.0,
        _ => 1.0,
    };
    let r = |v: f64| (v * factor).round_ties_even() as i64;
    (r(width).max(1), r(height).max(1))
}

fn number(v: &Value) -> Result<f64, String> {
    match v {
        Value::Integer(i) => Ok(*i as f64),
        Value::Real(r) => Ok(*r),
        _ => Err("Canvas value is not a number".into()),
    }
}

impl ClipDoc {
    pub fn open(raw: &[u8]) -> Result<ClipDoc, String> {
        let chunks = container::parse_container(raw)?;
        if !chunks.iter().any(|c| &c.cid == b"Head") {
            return Err("no Head chunk".into());
        }
        let extas = container::index_extas(raw, &chunks)?;
        let sqlite = container::sqlite_payload(raw, &chunks)?;
        let mut con = Connection::open_in_memory().map_err(err)?;
        con.deserialize_read_exact(MAIN_DB, sqlite, sqlite.len(), true).map_err(err)?;

        // Canvas
        let row: Vec<Value> = con
            .query_row(
                "SELECT CanvasUnit, CanvasWidth, CanvasHeight, CanvasResolution, CanvasRootFolder FROM Canvas",
                [],
                |r| (0..5).map(|i| r.get::<_, Value>(i)).collect(),
            )
            .map_err(err)?;
        let unit = match &row[0] {
            Value::Null => 0,
            Value::Integer(i) => *i,
            _ => return Err("CanvasUnit is not an integer".into()),
        };
        let (w, h) = canvas_to_pixels(number(&row[1])?, number(&row[2])?, number(&row[3])?, unit);
        let resolution = number(&row[3])?;
        let root_id = match &row[4] {
            Value::Integer(i) => *i,
            _ => return Err("CanvasRootFolder is not an integer".into()),
        };

        let columns: HashSet<String> = {
            let mut stmt = con.prepare("PRAGMA table_info(Layer)").map_err(err)?;
            let names = stmt.query_map([], |r| r.get::<_, String>(1)).map_err(err)?;
            names.collect::<Result<_, _>>().map_err(err)?
        };

        let mut doc = ClipDoc {
            width: w as usize,
            height: h as usize,
            resolution,
            nodes: Vec::new(),
            top: Vec::new(),
            extas,
            offscreens: HashMap::new(),
            styles: HashMap::new(),
            extra: HashMap::new(),
            vectors: Default::default(),
        };
        let loader = Loader { con: &con, columns: &columns };
        let root = loader.load(root_id)?.ok_or_else(|| format!("no Layer row for root {root_id}"))?;
        let mut seen = HashSet::new();
        seen.insert(root.main_id);
        let first = root.first_child;
        doc.top = if first.is_some() { loader.children(first, &mut doc.nodes, &mut seen, 1)? } else { Vec::new() };

        // Offscreens of every mipmap the tree points at.
        let mut stmt = con
            .prepare(
                "SELECT o.MainId, o.Attribute, CAST(o.BlockData AS TEXT) FROM Mipmap m \
                 JOIN MipmapInfo mi ON mi.MainId = m.BaseMipmapInfo \
                 JOIN Offscreen o ON o.MainId = mi.Offscreen WHERE m.MainId = ?",
            )
            .map_err(err)?;
        let mut mipmaps: Vec<i64> = doc.nodes.iter().flat_map(|n| [n.render_mipmap, n.mask_mipmap]).flatten().collect();
        mipmaps.extend(material_mipmaps(&con, &columns)?);
        for mm in mipmaps {
            if doc.offscreens.contains_key(&mm) {
                continue;
            }
            let mut rows = stmt.query([mm]).map_err(err)?;
            let info = match rows.next().map_err(err)? {
                None => None,
                Some(r) => {
                    let id: Value = r.get(0).map_err(err)?;
                    let attr: Value = r.get(1).map_err(err)?;
                    let outer: Value = r.get(2).map_err(err)?;
                    let Value::Integer(offscreen_id) = id else {
                        return Err("Offscreen.MainId is not an integer".into());
                    };
                    // bytes(attr): a BLOB (TEXT / NULL would differ or raise)
                    let Value::Blob(attribute) = attr else {
                        return Err("Offscreen.Attribute is not a blob".into());
                    };
                    let outer_id = match outer {
                        Value::Null => None, // str(None) == "None": never an Exta key
                        Value::Text(t) => Some(t.into_bytes()),
                        _ => return Err("BlockData did not cast to text".into()),
                    };
                    Some(OffscreenInfo { offscreen_id, attribute, outer_id })
                }
            };
            doc.offscreens.insert(mm, info);
        }
        drop(stmt);

        // Styles: every layer row at once (later rows win, as in the dict).
        let present: Vec<&'static str> = STYLE_COLUMNS.iter().copied().filter(|c| columns.contains(*c)).collect();
        if !present.is_empty() {
            let sql = format!("SELECT MainId, {} FROM Layer", present.join(", "));
            let mut stmt = con.prepare(&sql).map_err(err)?;
            let mut rows = stmt.query([]).map_err(err)?;
            while let Some(r) = rows.next().map_err(err)? {
                let Value::Integer(id) = r.get::<_, Value>(0).map_err(err)? else {
                    continue; // never matched by an integer MainId
                };
                let mut style = Style::default();
                for (k, name) in present.iter().enumerate() {
                    style.cols.insert(name, Col::from_value(r.get::<_, Value>(k + 1).map_err(err)?));
                }
                doc.styles.insert(id, style);
            }
        }

        // Per-layer blobs (first row per MainId, as fetchone).
        let one = |col: &str, id: i64| -> Result<Option<Col>, String> {
            if !columns.contains(col) {
                return Ok(None);
            }
            let mut stmt = con.prepare_cached(&format!("SELECT {col} FROM Layer WHERE MainId = ?")).map_err(err)?;
            let mut rows = stmt.query([id]).map_err(err)?;
            Ok(Some(match rows.next().map_err(err)? {
                None => Col::Null,
                Some(r) => Col::from_value(r.get::<_, Value>(0).map_err(err)?),
            }))
        };
        let paper_cols = ["DrawColorMainRed", "DrawColorMainGreen", "DrawColorMainBlue", "DrawColorEnable"];
        let ids: Vec<i64> = doc.nodes.iter().map(|n| n.main_id).collect();
        for id in ids {
            let mut extra = Extra {
                filter_info: one("FilterLayerInfo", id)?,
                gradation_info: one("GradationFillInfo", id)?,
                vector_index: one("VectorNormalStrokeIndex", id)?,
                paper: None,
            };
            if paper_cols.iter().all(|c| columns.contains(*c)) {
                let mut stmt = con
                    .prepare_cached(
                        "SELECT DrawColorMainRed, DrawColorMainGreen, DrawColorMainBlue, DrawColorEnable FROM Layer WHERE MainId = ?",
                    )
                    .map_err(err)?;
                let mut rows = stmt.query([id]).map_err(err)?;
                if let Some(r) = rows.next().map_err(err)? {
                    let g = |i| r.get::<_, Value>(i).map(Col::from_value).map_err(err);
                    extra.paper = Some([g(0)?, g(1)?, g(2)?, g(3)?]);
                } else {
                    extra.paper = Some([Col::Null, Col::Null, Col::Null, Col::Null]);
                }
            }
            doc.extra.insert(id, extra);
        }
        let index: HashMap<i64, Option<Col>> = doc.extra.iter().map(|(k, e)| (*k, e.vector_index.clone())).collect();
        doc.vectors = crate::vector::load(&con, raw, &doc.extas, &doc.nodes, &index);
        Ok(doc)
    }

    pub fn style(&self, main_id: i64) -> Style {
        self.styles.get(&main_id).cloned().unwrap_or_default()
    }

    pub fn offscreen(&self, mipmap: Option<i64>) -> Option<&OffscreenInfo> {
        self.offscreens.get(&mipmap?)?.as_ref()
    }

    /// The Exta chunk of an offscreen that has one (`info[2] in clip.extas`).
    pub fn exta(&self, info: &OffscreenInfo) -> Option<&Chunk> {
        self.extas.get(info.outer_id.as_ref()?)
    }

    pub fn extra(&self, main_id: i64) -> Extra {
        self.extra.get(&main_id).cloned().unwrap_or_default()
    }
}

/// Every integer `ResizableOriginalMipmap` (image materials' originals,
/// stage 1c): their offscreens are read with the tree's.
fn material_mipmaps(con: &Connection, columns: &HashSet<String>) -> Result<Vec<i64>, String> {
    if !columns.contains("ResizableOriginalMipmap") {
        return Ok(Vec::new());
    }
    let mut stmt = con.prepare("SELECT ResizableOriginalMipmap FROM Layer").map_err(err)?;
    let mut rows = stmt.query([]).map_err(err)?;
    let mut out = Vec::new();
    while let Some(r) = rows.next().map_err(err)? {
        if let Value::Integer(i) = r.get::<_, Value>(0).map_err(err)? {
            if i != 0 {
                out.push(i);
            }
        }
    }
    Ok(out)
}

struct Loaded {
    main_id: i64,
    node: Node,
    first_child: Option<i64>,
    next: Option<i64>,
}

struct Loader<'a> {
    con: &'a Connection,
    columns: &'a HashSet<String>,
}

impl Loader<'_> {
    fn load(&self, main_id: i64) -> Result<Option<Loaded>, String> {
        let mut stmt = self
            .con
            .prepare_cached(
                "SELECT MainId, LayerType, LayerFolder, LayerVisibility, LayerOpacity, LayerComposite, \
                 LayerClip, LayerOffsetX, LayerOffsetY, LayerRenderMipmap, LayerLayerMaskMipmap, \
                 LayerNextIndex, LayerFirstChildIndex FROM Layer WHERE MainId = ?",
            )
            .map_err(err)?;
        let mut rows = stmt.query([main_id]).map_err(err)?;
        let Some(r) = rows.next().map_err(err)? else {
            return Ok(None);
        };
        let c = |i: usize| r.get::<_, Value>(i).map(Col::from_value).map_err(err);
        let Col::Int(mid) = c(0)? else {
            return Err("Layer.MainId is not an integer".into());
        };
        let layer_type = match c(1)? {
            Col::Null => None,
            Col::Int(i) => Some(i),
            _ => return Err("LayerType is not an integer".into()),
        };
        let vis = c(3)?.int_or0()?;
        let mut node = Node {
            main_id: mid,
            layer_type,
            folder: c(2)?.int_or0()? != 0,
            visible: vis & 1 != 0,
            mask_enabled: vis & 2 != 0,
            opacity: c(4)?.int_or0()?,
            composite: c(5)?.int_or0()?,
            clip: c(6)?.int_or0()? != 0,
            offset_x: c(7)?.int_or0()?,
            offset_y: c(8)?.int_or0()?,
            render_mipmap: c(9)?.key()?,
            mask_mipmap: c(10)?.key()?,
            render_offset_x: 0,
            render_offset_y: 0,
            mask_offset_x: 0,
            mask_offset_y: 0,
            comic_frame: false,
            children: Vec::new(),
        };
        let next = c(11)?.key()?;
        let first_child = c(12)?.key()?;
        drop(rows);
        let wanted: Vec<&str> = OPTIONAL_LAYER_COLUMNS.iter().copied().filter(|n| self.columns.contains(*n)).collect();
        if !wanted.is_empty() {
            let mut stmt = self
                .con
                .prepare_cached(&format!("SELECT {} FROM Layer WHERE MainId = ?", wanted.join(", ")))
                .map_err(err)?;
            let mut rows = stmt.query([main_id]).map_err(err)?;
            if let Some(r) = rows.next().map_err(err)? {
                let mut get = HashMap::new();
                for (k, name) in wanted.iter().enumerate() {
                    get.insert(*name, Col::from_value(r.get::<_, Value>(k).map_err(err)?));
                }
                let v = |name: &str| get.get(name).cloned().unwrap_or(Col::Null);
                node.render_offset_x = v("LayerRenderOffscrOffsetX").int_or0()?;
                node.render_offset_y = v("LayerRenderOffscrOffsetY").int_or0()?;
                node.mask_offset_x = v("LayerMaskOffsetX").int_or0()? + v("LayerMaskOffscrOffsetX").int_or0()?;
                node.mask_offset_y = v("LayerMaskOffsetY").int_or0()? + v("LayerMaskOffscrOffsetY").int_or0()?;
                node.comic_frame = v("ComicFrameLineMipmap").truthy()?;
            }
        }
        Ok(Some(Loaded { main_id: mid, node, first_child, next }))
    }

    /// `_children_of`, appending the nodes depth first; returns their indices.
    fn children(&self, first: Option<i64>, nodes: &mut Vec<Node>, seen: &mut HashSet<i64>, depth: usize) -> Result<Vec<usize>, String> {
        // The one place the nesting is bounded: every recursive walk over the
        // finished tree (render, import, write-back) is shallower than this.
        if depth > MAX_LAYER_DEPTH {
            return Err(format!("layer tree is nested over {MAX_LAYER_DEPTH} folders deep"));
        }
        let mut out = Vec::new();
        let mut cur = first;
        while let Some(id) = cur {
            let Some(loaded) = self.load(id)? else {
                break;
            };
            if !seen.insert(loaded.main_id) || seen.len() > 1_000_000 {
                return Err("layer tree has a cycle".into()); // Python would loop forever
            }
            let idx = nodes.len();
            nodes.push(loaded.node);
            if loaded.first_child.is_some() {
                let kids = self.children(loaded.first_child, nodes, seen, depth + 1)?;
                nodes[idx].children = kids;
            }
            out.push(idx);
            cur = loaded.next;
        }
        Ok(out)
    }
}
