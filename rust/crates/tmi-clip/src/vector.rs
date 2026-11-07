//! The vector data a render regenerates (tmiref/clip_vector.py and
//! clip_frame.py): the strokes of ベクター layers, the brush fields that set
//! their width, and the outlines of コマ枠 folders. Read once when the file
//! is opened, with Python's queries and its reading of odd values: a query
//! or record Python would swallow (`except Exception`) gives no strokes /
//! no frame; a value Python would raise on outside its `try` (the brush and
//! its pressure graph) is kept as `Err`, and the renderer declines where it
//! needs it.

use std::collections::HashMap;

use rusqlite::types::Value;
use rusqlite::Connection;

use crate::container::{self, Chunk};
use crate::model::{Col, Node};

/// One stroke of a vector layer (`parse_strokes`).
#[derive(Clone, Debug)]
pub struct Stroke {
    pub flags: u32,
    pub brush: u32,
    /// 0 when not finite or not positive.
    pub size: f64,
    pub color: [u8; 3],
    pub opacity: f64,
    pub xy: Vec<[f64; 2]>,
    pub pflags: Vec<u32>,
    /// The seven f32 point fields, as f64.
    pub f: Vec<[f64; 7]>,
    /// The dab phase (+0x4c), when the point records are long enough.
    pub phase: Option<Vec<f64>>,
}

/// `_brush`'s width model of a brush.
#[derive(Clone, Debug)]
pub enum Pressure {
    /// "linear": the pressure itself.
    Linear,
    /// None: the size does not follow pressure.
    Constant,
    /// (minimum, graph (x, y) sorted by x, or None).
    Curve(f64, Option<(Vec<f64>, Vec<f64>)>),
}

#[derive(Clone, Debug)]
pub struct Brush {
    pub pressure: Pressure,
    pub thick: f64,
    pub composite: i64,
}

/// One frame border of a コマ枠 folder (`parse_frame_objects`), its points
/// already shifted by the layer offset (`frame_objects`).
#[derive(Clone, Debug)]
pub struct FrameObject {
    pub points: Vec<(f64, f64)>,
    pub half_width: f64,
    pub color: [u8; 3],
    pub opacity: f64,
}

#[derive(Clone, Debug, Default)]
pub struct VectorTables {
    /// Layers with a truthy `VectorNormalStrokeIndex`: their strokes
    /// (`layer_strokes`), Err where the index itself is odd.
    pub strokes: HashMap<i64, Result<Vec<Stroke>, String>>,
    /// Brushes the strokes use.
    pub brushes: HashMap<u32, Result<Brush, String>>,
    /// Comic-frame folders: their frame objects (`frame_objects`).
    pub frames: HashMap<i64, Result<Vec<FrameObject>, String>>,
}

fn be_u32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(b[at..at + 4].try_into().unwrap())
}

fn be_f64(b: &[u8], at: usize) -> f64 {
    f64::from_be_bytes(b[at..at + 8].try_into().unwrap())
}

fn be_f32(b: &[u8], at: usize) -> f64 {
    f32::from_be_bytes(b[at..at + 4].try_into().unwrap()) as f64
}

/// `min(255, (c * 255 + 0x7FFFFFFF) // 0xFFFFFFFF)`.
fn colour(c: u32) -> u8 {
    ((c as u64 * 255 + 0x7FFF_FFFF) / 0xFFFF_FFFF).min(255) as u8
}

/// `_vector_inner`: the inner stream of the Exta chunk a VectorData key
/// names, None where Python gets None or raises (inside its `try`).
pub fn vector_inner<'a>(raw: &'a [u8], extas: &HashMap<Vec<u8>, Chunk>, key: &[u8]) -> Option<&'a [u8]> {
    // A non-ASCII key decodes with U+FFFD and matches no Exta id.
    let chunk = extas.get(key)?;
    let payload = container::payload(raw, chunk);
    if payload.get(8..16) != Some(&b"extrnlid"[..]) {
        return None;
    }
    let size = u64::from_be_bytes(payload.get(48..56)?.try_into().unwrap());
    let start = 56.min(payload.len());
    let end = (56u64.saturating_add(size)).min(payload.len() as u64) as usize;
    Some(&payload[start..end.max(start)])
}

/// `parse_strokes`.
pub fn parse_strokes(inner: &[u8]) -> Vec<Stroke> {
    let mut out = Vec::new();
    let n = inner.len() as u64;
    let mut p = 0usize;
    while p as u64 + 24 <= n {
        let hsize = be_u32(inner, p) as u64;
        let psize = be_u32(inner, p + 8) as u64;
        let count = be_u32(inner, p + 16) as u64;
        let flags = be_u32(inner, p + 20);
        if hsize < 0x58 || psize < 0x40 || count == 0 || p as u64 + hsize + count * psize > n {
            break;
        }
        let (hs, ps) = (hsize as usize, psize as usize);
        let color = [colour(be_u32(inner, p + 0x28)), colour(be_u32(inner, p + 0x2c)), colour(be_u32(inner, p + 0x30))];
        let op = be_f64(inner, p + 0x40);
        let brush = be_u32(inner, p + hs - 16);
        let size = be_f64(inner, p + hs - 12);
        let base = p + hs;
        let mut s = Stroke {
            flags,
            brush,
            size: if size.is_finite() && size > 0.0 { size } else { 0.0 },
            color,
            opacity: if op.is_finite() { op.min(1.0).max(0.0) } else { 1.0 },
            xy: Vec::with_capacity(count as usize),
            pflags: Vec::with_capacity(count as usize),
            f: Vec::with_capacity(count as usize),
            phase: if ps >= 80 { Some(Vec::with_capacity(count as usize)) } else { None },
        };
        for k in 0..count as usize {
            let q = base + k * ps;
            s.xy.push([be_f64(inner, q), be_f64(inner, q + 8)]);
            s.pflags.push(be_u32(inner, q + 32));
            let mut f = [0.0; 7];
            for (i, v) in f.iter_mut().enumerate() {
                *v = be_f32(inner, q + 36 + 4 * i);
            }
            s.f.push(f);
            if let Some(ph) = s.phase.as_mut() {
                ph.push(be_f32(inner, q + 76));
            }
        }
        out.push(s);
        p = base + count as usize * ps;
    }
    out
}

/// `parse_frame_objects` (without the stored miters, which go unused).
pub fn parse_frame_objects(inner: &[u8]) -> Vec<FrameObject> {
    let mut out = Vec::new();
    let n = inner.len() as u64;
    let mut p = 0usize;
    while p as u64 + 0x60 <= n {
        let hsize = be_u32(inner, p) as u64;
        let psize = be_u32(inner, p + 8) as u64;
        let count = be_u32(inner, p + 16) as u64;
        if hsize < 0x60 || psize < 0x4c || count == 0 || p as u64 + hsize + count * psize > n {
            break;
        }
        let (hs, ps) = (hsize as usize, psize as usize);
        let color = [colour(be_u32(inner, p + 0x28)), colour(be_u32(inner, p + 0x2c)), colour(be_u32(inner, p + 0x30))];
        let op = be_f64(inner, p + 0x40);
        let width = be_f64(inner, p + hs - 12);
        let points = (0..count as usize)
            .map(|i| {
                let q = p + hs + i * ps;
                (be_f64(inner, q), be_f64(inner, q + 8))
            })
            .collect();
        // max(0.0, min(1.0, op)): NaN gives 1.0 (min keeps its first argument).
        let opacity = if op.is_nan() { 1.0 } else { op.min(1.0).max(0.0) };
        out.push(FrameObject { points, half_width: width, color, opacity });
        p += hs + count as usize * ps;
    }
    out
}

/// The first row of `sql` with one parameter, Err when the query fails
/// (a missing table or column: Python's `except`).
fn first_row(con: &Connection, sql: &str, id: i64, cols: usize) -> Result<Option<Vec<Value>>, ()> {
    let mut stmt = con.prepare_cached(sql).map_err(|_| ())?;
    let mut rows = stmt.query([id]).map_err(|_| ())?;
    match rows.next().map_err(|_| ())? {
        None => Ok(None),
        Some(r) => (0..cols).map(|i| r.get::<_, Value>(i).map_err(|_| ())).collect::<Result<Vec<_>, _>>().map(Some),
    }
}

/// The VectorData blob of a vector object list, as the key bytes Python
/// would look up, or None where it finds nothing (missing row / table,
/// NULL, a value `bytes()` rejects or that names no chunk).
fn vector_data(con: &Connection, index: i64) -> Option<Vec<u8>> {
    let row = first_row(con, "SELECT VectorData FROM VectorObjectList WHERE MainId = ?", index, 1).ok()??;
    match &row[0] {
        Value::Blob(b) => Some(b.clone()),
        // bytes(int) is that many zero bytes: no Exta id; text / real raise.
        _ => None,
    }
}

/// The truthy integer an index column holds, None when falsy, Err for a
/// value Python would treat in ways not reproduced (REAL, TEXT, BLOB).
fn index_of(col: &Col) -> Result<Option<i64>, String> {
    match col {
        Col::Null | Col::Int(0) => Ok(None),
        Col::Int(i) => Ok(Some(*i)),
        Col::Blob(b) if b.is_empty() => Ok(None),
        _ => Err("odd vector index".into()),
    }
}

fn number(v: &Value) -> Result<f64, String> {
    match v {
        Value::Integer(i) => Ok(*i as f64),
        Value::Real(r) => Ok(*r),
        _ => Err("odd ThicknessBase".into()),
    }
}

/// `_graph`.
fn graph(con: &Connection, gid: u32) -> Result<Option<(Vec<f64>, Vec<f64>)>, String> {
    if gid == 0 {
        return Ok(None);
    }
    let Ok(Some(row)) = first_row(con, "SELECT ControlNumber, ControlPoints FROM BrushEffectorGraphData WHERE MainId = ?", gid as i64, 2) else {
        return Ok(None);
    };
    let n = match &row[0] {
        Value::Null | Value::Integer(0) => return Ok(None),
        Value::Integer(n) => *n,
        _ => return Err("odd ControlNumber".into()),
    };
    let blob = match &row[1] {
        Value::Null => return Ok(None),
        Value::Blob(b) => b,
        _ => return Err("odd ControlPoints".into()),
    };
    if n < 0 || (n as u64).saturating_mul(16) > blob.len() as u64 {
        return Err("unreadable brush graph".into()); // struct.error in Python
    }
    let n = n as usize;
    let mut pts: Vec<(f64, f64)> = (0..n).map(|i| (be_f64(blob, 16 * i), be_f64(blob, 16 * i + 8))).collect();
    if pts.iter().any(|(x, y)| !x.is_finite() || !y.is_finite()) {
        return Err("non-finite brush graph".into());
    }
    pts.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap()); // stable, as argsort(kind="stable")
    Ok(Some(pts.into_iter().unzip()))
}

/// `_brush`.
fn brush(con: &Connection, id: u32) -> Result<Brush, String> {
    let mut b = Brush { pressure: Pressure::Linear, thick: 1.0, composite: 0 };
    let row = match first_row(con, "SELECT SizeEffector, ThicknessBase, CompositeMode FROM BrushStyle WHERE MainId = ?", id as i64, 3) {
        Ok(Some(r)) => r,
        _ => return Ok(b),
    };
    b.thick = match &row[1] {
        Value::Null => 1.0,
        v => number(v)?,
    };
    b.composite = match &row[2] {
        Value::Null => 0,
        Value::Integer(i) => *i,
        _ => return Err("odd CompositeMode".into()),
    };
    match &row[0] {
        Value::Null => {}
        Value::Blob(e) if e.len() < 4 => {}
        Value::Blob(e) => {
            let flags = be_u32(e, 0);
            if flags & 0x10 != 0 && e.len() >= 12 {
                let mn = be_f32(e, 4);
                let gid = be_u32(e, 8);
                b.pressure = Pressure::Curve(if mn.is_finite() { mn } else { 0.0 }, graph(con, gid)?);
            } else {
                b.pressure = Pressure::Constant;
            }
        }
        _ => return Err("odd SizeEffector".into()),
    }
    Ok(b)
}

/// The vector data of every node that may need it.
pub fn load(con: &Connection, raw: &[u8], extas: &HashMap<Vec<u8>, Chunk>, nodes: &[Node], stroke_index: &HashMap<i64, Option<Col>>) -> VectorTables {
    let mut t = VectorTables::default();
    let has_balloon = con.prepare("SELECT VectorNormalBalloonIndex FROM Layer LIMIT 0").is_ok();
    for node in nodes {
        if node.folder {
            if !node.comic_frame {
                continue;
            }
            let objs = (|| -> Result<Vec<FrameObject>, String> {
                if !has_balloon {
                    return Ok(Vec::new());
                }
                let Ok(Some(row)) = first_row(con, "SELECT VectorNormalBalloonIndex FROM Layer WHERE MainId = ?", node.main_id, 1) else {
                    return Ok(Vec::new());
                };
                let col = match &row[0] {
                    Value::Null => Col::Null,
                    Value::Integer(i) => Col::Int(*i),
                    Value::Blob(b) => Col::Blob(b.clone()),
                    _ => Col::Odd,
                };
                let Some(index) = index_of(&col)? else {
                    return Ok(Vec::new());
                };
                let mut objs = vector_data(con, index)
                    .and_then(|key| vector_inner(raw, extas, &key))
                    .filter(|inner| !inner.is_empty())
                    .map(parse_frame_objects)
                    .unwrap_or_default();
                let (ox, oy) = (node.offset_x, node.offset_y);
                if ox != 0 || oy != 0 {
                    for o in &mut objs {
                        for p in &mut o.points {
                            *p = (p.0 + ox as f64, p.1 + oy as f64);
                        }
                    }
                }
                Ok(objs)
            })();
            t.frames.insert(node.main_id, objs);
            continue;
        }
        let Some(Some(col)) = stroke_index.get(&node.main_id) else {
            continue;
        };
        let index = match index_of(col) {
            Ok(None) => continue, // no strokes: the layer is drawn as one without vector data
            Ok(Some(i)) => i,
            Err(e) => {
                t.strokes.insert(node.main_id, Err(e));
                continue;
            }
        };
        let list = vector_data(con, index)
            .and_then(|key| vector_inner(raw, extas, &key))
            .filter(|inner| !inner.is_empty())
            .map(parse_strokes)
            .unwrap_or_default();
        for s in &list {
            t.brushes.entry(s.brush).or_insert_with(|| brush(con, s.brush));
        }
        t.strokes.insert(node.main_id, Ok(list));
    }
    t
}
