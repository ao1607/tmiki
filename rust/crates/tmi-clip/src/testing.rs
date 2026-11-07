//! Hostile CLIP files for tests, made from a real one (`example/tama.clip`):
//! the container is taken apart, the SQLite database edited and the file
//! emitted again. Not used by tmi itself; public so the importer's and the
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
