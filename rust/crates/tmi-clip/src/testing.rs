//! Hostile CLIP files for tests, made from a real one (`example/tama.clip`):
//! the container is taken apart, the SQLite database edited and the file
//! emitted again. Not used by tmiki itself; public so the importer's and the
//! format layer's tests build the same files.

use rusqlite::{Connection, MAIN_DB};

use crate::container;
use crate::write::emit_with_sqli;

type Chunks = Vec<([u8; 4], Vec<u8>)>;

/// `raw` re-emitted with its `Exta` chunks dropped (or kept), `extra` payloads
/// added as `Exta` chunks, and the database edited by `edit`.
fn rebuild(raw: &[u8], keep_exta: bool, extra: Vec<Vec<u8>>, edit: impl FnOnce(&Connection) -> rusqlite::Result<()>) -> Result<Vec<u8>, String> {
    let sql = |e: rusqlite::Error| format!("sqlite3: {e}");
    let table = container::parse_container(raw)?;
    let first = u64::from_be_bytes(raw[16..24].try_into().unwrap());
    let mut con = Connection::open_in_memory().map_err(sql)?;
    con.deserialize_read_exact(MAIN_DB, container::sqlite_payload(raw, &table)?, table.iter().find(|c| &c.cid == b"SQLi").unwrap().payload_size, false)
        .map_err(sql)?;
    edit(&con).map_err(sql)?;
    let sqli = con.serialize(MAIN_DB).map_err(sql)?.to_vec();

    let mut chunks: Chunks = Vec::new();
    for c in &table {
        if &c.cid == b"Exta" && !keep_exta {
            continue;
        }
        chunks.push((c.cid, container::payload(raw, c).to_vec()));
    }
    let at = chunks.iter().position(|c| &c.0 == b"SQLi").unwrap();
    for (i, p) in extra.into_iter().enumerate() {
        chunks.insert(at + i, (*b"Exta", p));
    }
    Ok(emit_with_sqli(first, &chunks, sqli))
}

/// `raw` with `n` folders nested in one another under the root (each the only
/// child of the one above; the innermost holds the layer `MainId = 4`). The
/// folders are copies of the folder `MainId = 6`; the tama.clip layout.
pub fn nested_folders(raw: &[u8], n: usize) -> Result<Vec<u8>, String> {
    rebuild(raw, false, Vec::new(), |con| {
        let cols: Vec<String> = con.prepare("PRAGMA table_info(Layer)")?.query_map([], |r| r.get::<_, String>(1))?.collect::<Result<_, _>>()?;
        let exprs: Vec<String> = cols
            .iter()
            .map(|c| match c.as_str() {
                "MainId" | "_PW_ID" => "100000 + n.i".to_string(),
                "LayerFirstChildIndex" => format!("CASE WHEN n.i + 1 < {n} THEN 100000 + n.i + 1 ELSE 4 END"),
                "LayerNextIndex" => "0".to_string(),
                "LayerUuid" => "printf('%08x-0000-4000-8000-000000000000', n.i)".to_string(),
                "LayerRenderMipmap" | "LayerLayerMaskMipmap" => "NULL".to_string(),
                other => format!("Layer.{other}"),
            })
            .collect();
        con.execute_batch(&format!(
            "WITH RECURSIVE n(i) AS (SELECT 0 UNION ALL SELECT i + 1 FROM n WHERE i + 1 < {n}) \
             INSERT INTO Layer ({}) SELECT {} FROM n, Layer WHERE Layer.MainId = 6; \
             UPDATE Layer SET LayerFirstChildIndex = 100000 WHERE MainId = 2;",
            cols.join(", "),
            exprs.join(", ")
        ))
    })
}

/// `raw` whose layer `MainId = 4` has an offscreen that declares `pixel_w` x
/// `pixel_h` pixels, and an `Exta` of no tiles (the other layers lose
/// theirs).
pub fn declared_offscreen(raw: &[u8], pixel_w: u32, pixel_h: u32) -> Result<Vec<u8>, String> {
    let mut outer = Vec::new();
    let mut edited = None;
    let out = rebuild(raw, false, Vec::new(), |con| {
        let (mut attr, id): (Vec<u8>, String) = con.query_row(
            "SELECT o.Attribute, CAST(o.BlockData AS TEXT) FROM Mipmap m JOIN MipmapInfo mi ON mi.MainId = m.BaseMipmapInfo \
             JOIN Offscreen o ON o.MainId = mi.Offscreen WHERE m.MainId = 4",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let name_len = u32::from_be_bytes(attr[16..20].try_into().unwrap()) as usize;
        let p = 20 + name_len * 2;
        attr[p..p + 4].copy_from_slice(&pixel_w.to_be_bytes());
        attr[p + 4..p + 8].copy_from_slice(&pixel_h.to_be_bytes());
        con.execute(
            "UPDATE Offscreen SET Attribute = ? WHERE MainId = (SELECT o.MainId FROM Mipmap m JOIN MipmapInfo mi ON mi.MainId = m.BaseMipmapInfo \
             JOIN Offscreen o ON o.MainId = mi.Offscreen WHERE m.MainId = 4)",
            [&attr],
        )?;
        outer = id.into_bytes();
        edited = Some(());
        Ok(())
    })?;
    edited.ok_or("no offscreen")?;
    // Rebuilt again to add the Exta that carries the outer id found above.
    let mut payload = 0x28u64.to_be_bytes().to_vec();
    payload.extend_from_slice(&outer);
    payload.extend_from_slice(&0u64.to_be_bytes());
    rebuild(&out, true, vec![payload], |_| Ok(()))
}

/// `raw` whose CanvasPreview is `data`, declared `width` x `height`.
pub fn with_preview(raw: &[u8], width: i64, height: i64, data: &[u8]) -> Result<Vec<u8>, String> {
    rebuild(raw, false, Vec::new(), |con| {
        con.execute("UPDATE CanvasPreview SET ImageWidth = ?, ImageHeight = ?, ImageData = ?", rusqlite::params![width, height, data]).map(|_| ())
    })
}

/// The outer id (`extrnlid` + 32 hex digits) of test offscreen `n`.
pub fn outer_id(n: u32) -> String {
    format!("extrnlid{n:032X}")
}

/// An `Offscreen.Attribute` `parse_offscreen_attribute` reads: colour
/// (`color-alpha-bgra`), `pixel_w` x `pixel_h`, a `cols` x `rows` grid, no
/// init colour.
pub fn offscreen_attribute(pixel_w: u32, pixel_h: u32, cols: u32, rows: u32) -> Vec<u8> {
    let name = |s: &str| -> Vec<u8> {
        let mut out = (s.len() as u32).to_be_bytes().to_vec();
        out.extend(s.encode_utf16().flat_map(u16::to_be_bytes));
        out
    };
    let count = cols * rows;
    let mut a = vec![0u8; 12];
    a.extend_from_slice(&(34 + count * 4).to_be_bytes());
    a.extend(name("Parameter"));
    for v in [pixel_w, pixel_h, cols, rows, 33] {
        a.extend_from_slice(&v.to_be_bytes());
    }
    a.extend_from_slice(&[0; 60 + 16]);
    a.extend(name("InitColor"));
    a.extend_from_slice(&4u32.to_be_bytes());
    a.extend(name("BlockSize"));
    for v in [12, count, 4] {
        a.extend_from_slice(&v.to_be_bytes());
    }
    a.extend(std::iter::repeat_n(0u8, count as usize * 4));
    a
}

/// A `BlockDataBeginChunk` record with `zlib` (a zlib stream) as its data.
pub fn tile_record(block: u32, bpt: u32, tw: u32, th: u32, zlib: Option<&[u8]>) -> Vec<u8> {
    let mut body = Vec::new();
    for v in [block, bpt, tw, th, zlib.is_some() as u32] {
        body.extend_from_slice(&v.to_be_bytes());
    }
    if let Some(z) = zlib {
        body.extend_from_slice(&(4 + z.len() as u32).to_be_bytes());
        body.extend_from_slice(&(z.len() as u32).to_le_bytes());
        body.extend_from_slice(z);
    }
    body.extend_from_slice(&17u32.to_be_bytes());
    body.extend("BlockDataEndChunk".encode_utf16().flat_map(u16::to_be_bytes));
    let begin: Vec<u8> = "BlockDataBeginChunk".encode_utf16().flat_map(u16::to_be_bytes).collect();
    let mut out = ((8 + begin.len() + body.len()) as u32).to_be_bytes().to_vec();
    out.extend_from_slice(&19u32.to_be_bytes());
    out.extend(begin);
    out.extend(body);
    out
}

/// An Exta payload of `records` for `outer` (an [`outer_id`]).
pub fn exta_payload(outer: &str, records: &[Vec<u8>]) -> Vec<u8> {
    let inner: Vec<u8> = records.concat();
    let mut p = 0x28u64.to_be_bytes().to_vec();
    p.extend_from_slice(outer.as_bytes());
    p.extend_from_slice(&(inner.len() as u64).to_be_bytes());
    p.extend(inner);
    p
}

/// The SQL of a raster layer `id` (followed by `next`, 0 for none) whose
/// render offscreen (mipmap `id`) is `attribute` stored in the Exta `outer`.
pub fn raster_layer_sql(id: i64, next: i64, outer: &str, attribute: &[u8]) -> String {
    let hex: String = attribute.iter().map(|b| format!("{b:02X}")).collect();
    format!(
        "INSERT INTO Layer(MainId, LayerName, LayerFolder, LayerVisibility, LayerOpacity, LayerRenderMipmap, LayerNextIndex) \
         VALUES ({id}, 'layer {id}', 0, 1, 256, {id}, {next}); \
         INSERT INTO Mipmap VALUES ({id}, {id}); INSERT INTO MipmapInfo VALUES ({id}, {id}); \
         INSERT INTO Offscreen VALUES ({id}, X'{hex}', '{outer}');"
    )
}

/// A CLIP file made from nothing: a `width` x `height` canvas whose root
/// folder (`MainId` 1) has the layer `MainId` 2 as its first child, the
/// database edited by `sql` (layers, views, ...), and `extas` as Exta chunks.
pub fn minimal_clip(width: u32, height: u32, sql: &str, extas: Vec<Vec<u8>>) -> Result<Vec<u8>, String> {
    let e = |e: rusqlite::Error| format!("sqlite3: {e}");
    let con = Connection::open_in_memory().map_err(e)?;
    con.execute_batch(&format!(
        "CREATE TABLE Canvas(CanvasUnit, CanvasWidth, CanvasHeight, CanvasResolution, CanvasRootFolder);
         INSERT INTO Canvas VALUES (0, {width}, {height}, 72.0, 1);
         CREATE TABLE Layer(MainId INTEGER PRIMARY KEY, LayerName, LayerType, LayerFolder, LayerVisibility, LayerOpacity,
             LayerComposite, LayerClip, LayerMasking, LayerOffsetX, LayerOffsetY, LayerUuid, LayerRenderMipmap,
             LayerLayerMaskMipmap, LayerNextIndex, LayerFirstChildIndex);
         INSERT INTO Layer(MainId, LayerName, LayerFolder, LayerVisibility, LayerOpacity, LayerFirstChildIndex) VALUES (1, 'root', 1, 1, 256, 2);
         CREATE TABLE Mipmap(MainId INTEGER PRIMARY KEY, BaseMipmapInfo);
         CREATE TABLE MipmapInfo(MainId INTEGER PRIMARY KEY, Offscreen);
         CREATE TABLE Offscreen(MainId INTEGER PRIMARY KEY, Attribute, BlockData);
         CREATE TABLE RemovedExternal(ExternalID BLOB);
         CREATE TABLE ExternalChunk(ExternalID BLOB, Offset INTEGER);
         CREATE TRIGGER ExternalUpdateOfBlockDataOnOffscreen UPDATE OF BlockData ON Offscreen
            BEGIN INSERT INTO RemovedExternal(ExternalID) VALUES(OLD.BlockData); END;
         CREATE TRIGGER ExternalDeleteOfBlockDataOnOffscreen DELETE ON Offscreen
            BEGIN INSERT INTO RemovedExternal(ExternalID) VALUES(OLD.BlockData); END;
         {sql}"
    ))
    .map_err(e)?;
    let sqli = con.serialize(MAIN_DB).map_err(e)?.to_vec();
    let mut chunks: Chunks = vec![(*b"Head", vec![0; 40])];
    chunks.extend(extas.into_iter().map(|p| (*b"Exta", p)));
    chunks.push((*b"SQLi", Vec::new()));
    chunks.push((*b"Foot", Vec::new()));
    Ok(emit_with_sqli(24, &chunks, sqli))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ClipDoc;

    pub(crate) fn tama() -> Vec<u8> {
        std::fs::read(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../example/tama.clip")).expect("example/tama.clip")
    }

    #[test]
    fn deeply_nested_folders_are_refused_not_recursed_into() {
        let raw = tama();
        // A normal depth still opens.
        ClipDoc::open(&nested_folders(&raw, 200).unwrap()).expect("200 folders deep");
        // 200,000 folders overflowed the stack of the tree's loader.
        let e = ClipDoc::open(&nested_folders(&raw, 200_000).unwrap()).err().expect("refused");
        assert!(e.contains("nested over"), "{e}");
        let e = ClipDoc::open(&nested_folders(&raw, crate::model::MAX_LAYER_DEPTH + 1).unwrap()).err().expect("refused");
        assert!(e.contains("nested over"), "{e}");
    }

    #[test]
    fn the_untouched_example_still_opens() {
        ClipDoc::open(&tama()).expect("tama.clip");
    }
}
