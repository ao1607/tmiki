//! CLIP layer write-back: `replace_layer_pixels` of the Python reader
//! (`tmiref/clip_io.py`) after the layer is found and the PNG decoded, byte
//! for byte.
//!
//! The new pixels (padded by `alpha_composite` onto a transparent
//! offscreen-sized image) are compared with the offscreen's; when they
//! differ, every 256px block is written again (`build_tile_record`: an alpha
//! plane and a BGRx plane, `zlib.compress` at level 6) unless its pixels did
//! not change, in which case CSP's record is kept byte for byte. The
//! rewritten Exta ends with CSP's two trailing tables (`trailing_tables`:
//! BlockStatus, the source's value or 1, and BlockCheckSum, the adler32 of
//! every stored tile blob). The Offscreen.Attribute's BlockSize table is
//! updated, the layer's LayerThumbnail marked stale (`mark_thumbnail_stale`:
//! every `*NeedRefresh*` column set to 1, so CSP redraws the thumbnail instead
//! of trusting the old one), ExternalChunk rewritten with the new offsets and
//! the CanvasPreview's ImageData cleared, through
//! SQLite 3.50.4 (the version CPython 3.13 carries; the database file
//! records the version that wrote it) doing what Python's sqlite3 module
//! does: `backup` into a fresh in-memory database, two write transactions,
//! `serialize`.

use rayon::prelude::*;
use rusqlite::backup::Backup;
use rusqlite::{params, Connection, MAIN_DB};
use tmi_pillow::over_pixel;

use crate::attr::parse_offscreen_attribute;
use crate::container::{self, Chunk};
use crate::tiles::decode_offscreen;
use crate::zlib;

pub const TILE: usize = 256;
const BEGIN_NAME: &str = "BlockDataBeginChunk";
const END_NAME: &str = "BlockDataEndChunk";

fn utf16be(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(|u| u.to_be_bytes()).collect()
}

fn be32(v: usize) -> Result<[u8; 4], String> {
    u32::try_from(v).map(u32::to_be_bytes).map_err(|_| "'I' format requires 0 <= number <= 4294967295".to_string())
}

/// `build_tile_record(block_index, tile)` of a 256x256 RGBA tile that
/// `alpha_composite` already put on transparent (alpha 0 is (0, 0, 0, 0)).
pub fn build_tile_record(block_index: usize, tile: &[u8]) -> Result<Vec<u8>, String> {
    let n = TILE * TILE;
    debug_assert_eq!(tile.len(), n * 4);
    let mut raw = vec![0u8; n * 5];
    let (alpha, bgrx) = raw.split_at_mut(n);
    let mut any_alpha = false;
    for (i, p) in tile.chunks_exact(4).enumerate() {
        alpha[i] = p[3];
        any_alpha |= p[3] != 0;
        bgrx[i * 4] = p[2];
        bgrx[i * 4 + 1] = p[1];
        bgrx[i * 4 + 2] = p[0];
    }
    let mut body = Vec::new();
    for v in [block_index, n * 5, TILE, TILE, any_alpha as usize] {
        body.extend_from_slice(&be32(v)?);
    }
    if any_alpha {
        let compressed = zlib::compress(&raw, -1);
        let blob_len = 4 + compressed.len();
        body.extend_from_slice(&be32(blob_len)?);
        body.extend_from_slice(&(compressed.len() as u32).to_le_bytes());
        body.extend_from_slice(&compressed);
    }
    let end = utf16be(END_NAME);
    body.extend_from_slice(&be32(END_NAME.len())?);
    body.extend_from_slice(&end);
    let name = utf16be(BEGIN_NAME);
    let mut out = Vec::with_capacity(8 + name.len() + body.len());
    out.extend_from_slice(&be32(8 + name.len() + body.len())?);
    out.extend_from_slice(&be32(BEGIN_NAME.len())?);
    out.extend_from_slice(&name);
    out.extend_from_slice(&body);
    Ok(out)
}

/// `_exta_records_by_block`: the BlockDataBeginChunk records of an Exta
/// payload by block index (a later one wins). Err where Python raises.
fn records_by_block(payload: &[u8]) -> Result<std::collections::HashMap<u32, &[u8]>, String> {
    let mut out = std::collections::HashMap::new();
    if payload.get(8..16) != Some(b"extrnlid".as_slice()) {
        return Ok(out);
    }
    let size = payload.get(48..56).ok_or("unpack requires a buffer of 8 bytes")?;
    let size = u64::from_be_bytes(size.try_into().unwrap());
    let end = 56usize.saturating_add(usize::try_from(size).unwrap_or(usize::MAX)).min(payload.len());
    let inner = &payload[56.min(payload.len())..end.max(56.min(payload.len()))];
    let begin = utf16be(BEGIN_NAME);
    let mut p = 0usize;
    while p + 8 <= inner.len() {
        let rec_size = u32::from_be_bytes(inner[p..p + 4].try_into().unwrap()) as usize;
        let name_len = u32::from_be_bytes(inner[p + 4..p + 8].try_into().unwrap()) as usize;
        if name_len * 2 + 8 > rec_size || p + rec_size > inner.len() {
            break;
        }
        if inner[p + 8..p + 8 + 2 * name_len] == begin[..] {
            let idx = inner.get(p + 46..p + 50).ok_or("unpack requires a buffer of 4 bytes")?;
            out.insert(u32::from_be_bytes(idx.try_into().unwrap()), &inner[p..p + rec_size]);
        }
        p += rec_size;
    }
    Ok(out)
}

/// `_exta_block_status`: the first entry of the BlockStatus table an Exta
/// payload ends with (CSP's files all have one; every entry of a table is the
/// same), 1 when there is none.
fn source_block_status(payload: &[u8]) -> u32 {
    let be = |b: &[u8], at: usize| b.get(at..at + 4).map(|w| u32::from_be_bytes(w.try_into().unwrap()) as usize);
    if payload.get(8..16) != Some(b"extrnlid".as_slice()) {
        return 1;
    }
    let Some(size) = payload.get(48..56).map(|s| u64::from_be_bytes(s.try_into().unwrap())) else { return 1 };
    let end = 56usize.saturating_add(usize::try_from(size).unwrap_or(usize::MAX)).min(payload.len());
    let inner = &payload[56.min(payload.len())..end.max(56.min(payload.len()))];
    // The tile records first (the walk of `records_by_block`) ...
    let mut p = 0usize;
    while let (Some(rec_size), Some(name_len)) = (be(inner, p), be(inner, p + 4)) {
        if name_len * 2 + 8 > rec_size || p + rec_size > inner.len() {
            break;
        }
        p += rec_size;
    }
    // ... then the tables: u32 name length, UTF-16BE name, u32 12, u32 count,
    // u32 4, count x u32.
    let status = utf16be("BlockStatus");
    while let Some(name_len) = be(inner, p) {
        let name_end = p + 4 + name_len.saturating_mul(2);
        let (Some(count), Some(entry)) = (be(inner, name_end + 4), be(inner, name_end + 12)) else { return 1 };
        if inner.get(p + 4..name_end) == Some(status.as_slice()) {
            return if count > 0 { entry as u32 } else { 1 };
        }
        p = name_end + 12 + count.saturating_mul(4);
    }
    1
}

/// `_record_checksum`: the adler32 of the blob a tile record stores (the
/// `data_size` bytes at body +24: the little-endian length and the zlib
/// stream), 0 for an empty tile.
fn record_checksum(rec: &[u8]) -> u32 {
    let be = |at: usize| rec.get(at..at + 4).map(|w| u32::from_be_bytes(w.try_into().unwrap()) as usize);
    let Some(name_len) = be(4) else { return 0 };
    let body = 8 + 2 * name_len;
    if be(body + 16).unwrap_or(0) == 0 {
        return 0;
    }
    let size = be(body + 20).unwrap_or(0);
    let blob = rec.get(body + 24..).unwrap_or(&[]);
    let blob = &blob[..size.min(blob.len())];
    // SAFETY: zlib's adler32 over a valid slice (blobs are far below 4 GiB).
    unsafe { libz_sys::adler32(1, blob.as_ptr(), blob.len() as libz_sys::uInt) as u32 }
}

/// `_trailing_tables`: BlockStatus (every entry `status`) and BlockCheckSum
/// (one adler32 per record), after the tile records of an Exta.
fn trailing_tables(status: u32, checksums: &[u32]) -> Result<Vec<u8>, String> {
    let n = be32(checksums.len())?;
    let mut out = Vec::new();
    for (name, entries) in [("BlockStatus", vec![status; checksums.len()]), ("BlockCheckSum", checksums.to_vec())] {
        out.extend_from_slice(&be32(name.len())?);
        out.extend_from_slice(&utf16be(name));
        out.extend_from_slice(&12u32.to_be_bytes());
        out.extend_from_slice(&n);
        out.extend_from_slice(&4u32.to_be_bytes());
        for e in entries {
            out.extend_from_slice(&e.to_be_bytes());
        }
    }
    Ok(out)
}

/// `_mark_thumbnail_stale`: every `*NeedRefresh*` column of the LayerThumbnail
/// rows of the layers whose render offscreen is `offscreen_id` set to 1.
/// Nothing happens when the table, the columns or the layer are missing.
fn mark_thumbnail_stale(con: &Connection, offscreen_id: i64) -> Result<(), String> {
    let columns: Vec<String> = {
        let Ok(mut stmt) = con.prepare("PRAGMA table_info(LayerThumbnail)") else { return Ok(()) };
        let Ok(rows) = stmt.query_map([], |r| r.get::<_, String>(1)) else { return Ok(()) };
        rows.filter_map(Result::ok).filter(|c| c.contains("NeedRefresh")).collect()
    };
    if columns.is_empty() {
        return Ok(());
    }
    let sets: Vec<String> = columns.iter().map(|c| format!("\"{}\" = 1", c.replace('"', "\"\""))).collect();
    // A database without Layer / Mipmap / MipmapInfo fails to prepare: skipped.
    let _ = con.execute(
        &format!(
            "UPDATE LayerThumbnail SET {} WHERE MainId IN (SELECT l.LayerRenderThumbnail FROM Layer l \
             JOIN Mipmap m ON m.MainId = l.LayerRenderMipmap \
             JOIN MipmapInfo mi ON mi.MainId = m.BaseMipmapInfo WHERE mi.Offscreen = ?)",
            sets.join(", ")
        ),
        params![offscreen_id],
    );
    Ok(())
}
/// `image.crop((x0, y0, x0 + 256, y0 + 256))` of a `w`-wide RGBA image
/// (zeros past its edges).
fn crop_block(image: &[u8], w: usize, h: usize, x0: usize, y0: usize) -> Vec<u8> {
    let mut out = vec![0u8; TILE * TILE * 4];
    let cw = TILE.min(w.saturating_sub(x0));
    for y in 0..TILE.min(h.saturating_sub(y0)) {
        let s = ((y0 + y) * w + x0) * 4;
        out[y * TILE * 4..y * TILE * 4 + cw * 4].copy_from_slice(&image[s..s + cw * 4]);
    }
    out
}

/// `replace_block_sizes`.
pub fn replace_block_sizes(attr_blob: &[u8], record_sizes: &[usize]) -> Result<Vec<u8>, String> {
    let name = utf16be("BlockSize");
    let at = attr_blob.windows(name.len()).position(|w| w == name.as_slice());
    let at = match at {
        Some(a) if a >= 4 => a,
        _ => return Err("CLIP Offscreen.Attribute missing BlockSize".into()),
    };
    let block_off = at - 4;
    let name_len = u32::from_be_bytes(attr_blob[block_off..block_off + 4].try_into().unwrap()) as usize;
    if name_len != "BlockSize".len() {
        return Err("bad CLIP BlockSize name length".into());
    }
    let size_off = block_off + 4 + name_len * 2;
    let table = attr_blob.get(size_off..size_off + 12).ok_or("unpack requires a buffer of 12 bytes")?;
    let word = |k: usize| u32::from_be_bytes(table[k * 4..k * 4 + 4].try_into().unwrap()) as usize;
    let (record_size, block_count, entry_size) = (word(0), word(1), word(2));
    if record_size != 12 || entry_size != 4 {
        return Err("unsupported CLIP BlockSize table".into());
    }
    if block_count != record_sizes.len() {
        return Err(format!("BlockSize count {block_count} != emitted records {}", record_sizes.len()));
    }
    // `out[a:b] = pack(">I", size)`: a slice assignment, which grows the
    // bytes if the table runs past their end.
    let mut out = attr_blob.to_vec();
    let table_off = size_off + 12;
    for (i, &size) in record_sizes.iter().enumerate() {
        let a = (table_off + i * 4).min(out.len());
        let b = (table_off + i * 4 + 4).min(out.len());
        out.splice(a..b, be32(size)?);
    }
    Ok(out)
}

/// `emit_container`.
pub fn emit_container(first_chunk_off: u64, chunks: &[([u8; 4], Vec<u8>)]) -> Vec<u8> {
    let body_len: usize = chunks.iter().map(|(_, p)| 16 + p.len()).sum();
    let pad = first_chunk_off as i128 - 24;
    let mut out = Vec::with_capacity(24 + pad.max(0) as usize + body_len);
    out.extend_from_slice(b"CSFCHUNK");
    out.extend_from_slice(&((24 + pad + body_len as i128) as u64).to_be_bytes());
    out.extend_from_slice(&first_chunk_off.to_be_bytes());
    out.resize(out.len() + pad.max(0) as usize, 0);
    for (cid, payload) in chunks {
        out.extend_from_slice(b"CHNK");
        out.extend_from_slice(cid);
        out.extend_from_slice(&(payload.len() as u64).to_be_bytes());
        out.extend_from_slice(payload);
    }
    out
}

/// `_emit_container_with_sqli_payload`: the chunks with the SQLi payload
/// replaced and Head's SQLi offset (bytes 8..16) pointing at it.
pub(crate) fn emit_with_sqli(first_chunk_off: u64, chunks: &[([u8; 4], Vec<u8>)], sqli: Vec<u8>) -> Vec<u8> {
    let mut chunks: Vec<([u8; 4], Vec<u8>)> = chunks.to_vec();
    for c in chunks.iter_mut().filter(|c| &c.0 == b"SQLi") {
        c.1 = sqli.clone();
    }
    let mut off = first_chunk_off;
    let mut sqli_offset = first_chunk_off;
    for (cid, payload) in &chunks {
        if cid == b"SQLi" {
            sqli_offset = off;
            break;
        }
        off += 16 + payload.len() as u64;
    }
    for c in chunks.iter_mut().filter(|c| &c.0 == b"Head") {
        let a = 8.min(c.1.len());
        let b = 16.min(c.1.len());
        c.1.splice(a..b, sqli_offset.to_be_bytes());
    }
    emit_container(first_chunk_off, &chunks)
}

fn sql(e: rusqlite::Error) -> String {
    format!("sqlite3: {e}")
}

/// `_clone_sqlite_connection`: `sqlite3.connect(":memory:")` and
/// `clip.db.backup(it)`, `clip.db` being the SQLi payload `deserialize`d.
fn clone_database(sqlite: &[u8]) -> Result<Connection, String> {
    let mut src = Connection::open_in_memory().map_err(sql)?;
    src.deserialize_read_exact(MAIN_DB, sqlite, sqlite.len(), false).map_err(sql)?;
    let mut dst = Connection::open_in_memory().map_err(sql)?;
    // CPython leaves foreign keys off; this SQLite build turns them on.
    dst.execute_batch("PRAGMA foreign_keys = OFF").map_err(sql)?;
    {
        let backup = Backup::new(&src, &mut dst).map_err(sql)?;
        backup.step(-1).map_err(sql)?;
    }
    Ok(dst)
}

/// `_invalidate_canvas_preview`: whether a CanvasPreview row lost its data.
fn invalidate_canvas_preview(con: &Connection) -> Result<bool, String> {
    let mut stmt = con.prepare("PRAGMA table_info(CanvasPreview)").map_err(sql)?;
    let columns: Vec<String> = stmt.query_map([], |r| r.get::<_, String>(1)).map_err(sql)?.collect::<Result<_, _>>().map_err(sql)?;
    if !columns.iter().any(|c| c == "ImageData") {
        return Ok(false);
    }
    let n = con.execute("UPDATE CanvasPreview SET ImageData = NULL WHERE ImageData IS NOT NULL", []).map_err(sql)?;
    Ok(n > 0)
}

fn serialize(con: &Connection) -> Result<Vec<u8>, String> {
    Ok(con.serialize(MAIN_DB).map_err(sql)?.to_vec())
}

/// What `replace_layer_pixels` wrote, and what Python warned about on the
/// way (the current pixels' decode: `_warn_offscreen_failure`,
/// `_warn_unknown_tile_size`).
#[derive(Debug, Default)]
pub struct WriteBack {
    /// The new file, or None when it stays as it is.
    pub bytes: Option<Vec<u8>>,
    pub decode_failure: Option<String>,
    pub unknown_sizes: Vec<(usize, usize)>,
}

/// `replace_layer_pixels` after the selector, the attribute check and the
/// PNG decode: `image` (`w` x `h` RGBA, no larger than the offscreen)
/// replaces the pixels of the offscreen `offscreen_id` (Attribute
/// `attr_blob`, Exta `outer_id`).
#[allow(clippy::too_many_arguments)]
pub fn replace_layer_pixels(
    raw: &[u8],
    offscreen_id: i64,
    attr_blob: &[u8],
    outer_id: &str,
    image: &[u8],
    w: usize,
    h: usize,
    invalidate_preview_on_noop: bool,
) -> Result<WriteBack, String> {
    let chunks = container::parse_container(raw)?;
    let first_chunk_off = u64::from_be_bytes(raw[16..24].try_into().unwrap());
    let extas = container::index_extas(raw, &chunks)?;
    let exta: &Chunk = extas.get(outer_id.as_bytes()).ok_or("CLIP layer has no backed Exta payload")?;
    let attr = parse_offscreen_attribute(attr_blob)?;
    let (pw, ph) = (attr.pixel_width, attr.pixel_height);
    if image.len() != w * h * 4 {
        return Err("RGBA bytes do not match the size".into());
    }
    if w > pw || h > ph {
        return Err(format!("replacement image {w}x{h} is larger than layer offscreen {pw}x{ph}"));
    }
    // `padded.alpha_composite(image)`: alpha 0 becomes (0, 0, 0, 0).
    let padded;
    let image: &[u8] = if (w, h) == (pw, ph) {
        image
    } else {
        let mut p = vec![0u8; pw * ph * 4];
        p.par_chunks_mut(pw * 4).take(h).enumerate().for_each(|(y, row)| {
            for x in 0..w {
                over_pixel(&mut row[x * 4..x * 4 + 4], &image[(y * w + x) * 4..(y * w + x) * 4 + 4]);
            }
        });
        padded = p;
        &padded
    };

    // The current pixels (`render_layer_rgba`): None when they do not decode.
    let mut out = WriteBack::default();
    let current = match decode_offscreen(raw, exta.payload_off, exta.payload_size, attr.tile_cols, attr.tile_rows, pw, ph) {
        Ok(d) => {
            out.unknown_sizes = d.unknown_sizes.clone();
            let mut full = vec![0u8; pw * ph * 4];
            for y in 0..d.h {
                let s = y * d.w * 4;
                let t = ((d.y + y) * pw + d.x) * 4;
                full[t..t + d.w * 4].copy_from_slice(&d.rgba[s..s + d.w * 4]);
            }
            Some(full)
        }
        Err(e) => {
            out.decode_failure = Some(e);
            None
        }
    };

    let chunk_payloads: Vec<([u8; 4], Vec<u8>)> = chunks.iter().map(|c| (c.cid, container::payload(raw, c).to_vec())).collect();
    let sqli_index = chunks.iter().position(|c| &c.cid == b"SQLi");
    let exta_after_sqli = sqli_index.is_some_and(|s| chunks.iter().enumerate().any(|(i, c)| &c.cid == b"Exta" && i > s));
    let sqlite = container::sqlite_payload(raw, &chunks)?;

    if current.as_deref() == Some(image) {
        if invalidate_preview_on_noop {
            if exta_after_sqli {
                return Err("CLIP write-back: an Exta chunk after SQLi is unsupported".into());
            }
            let con = clone_database(sqlite)?;
            con.execute_batch("BEGIN").map_err(sql)?;
            if invalidate_canvas_preview(&con)? {
                con.execute_batch("COMMIT").map_err(sql)?;
                out.bytes = Some(emit_with_sqli(first_chunk_off, &chunk_payloads, serialize(&con)?));
            }
        }
        return Ok(out);
    }

    let records = match &current {
        Some(_) => Some(records_by_block(container::payload(raw, exta))?),
        None => None,
    };
    let (cols, rows) = (attr.tile_cols, attr.tile_rows);
    let blocks: Vec<Result<Vec<u8>, String>> = (0..cols * rows)
        .into_par_iter()
        .map(|block| {
            let (x0, y0) = ((block % cols) * TILE, (block / cols) * TILE);
            let crop = crop_block(image, pw, ph, x0, y0);
            if let (Some(cur), Some(records)) = (&current, &records) {
                if let Some(rec) = u32::try_from(block).ok().and_then(|b| records.get(&b)) {
                    if crop_block(cur, pw, ph, x0, y0) == crop {
                        return Ok(rec.to_vec());
                    }
                }
            }
            // `tile.alpha_composite(crop)` onto transparent.
            let mut tile = vec![0u8; TILE * TILE * 4];
            for (d, s) in tile.chunks_exact_mut(4).zip(crop.chunks_exact(4)) {
                over_pixel(d, s);
            }
            build_tile_record(block, &tile)
        })
        .collect();
    let mut inner = Vec::new();
    let mut record_sizes = Vec::with_capacity(blocks.len());
    let mut checksums = Vec::with_capacity(blocks.len());
    for b in blocks {
        let b = b?;
        record_sizes.push(b.len());
        checksums.push(record_checksum(&b));
        inner.extend_from_slice(&b);
    }
    // `_trailing_tables(_exta_block_status(...), checksums)`.
    inner.extend_from_slice(&trailing_tables(source_block_status(container::payload(raw, exta)), &checksums)?);
    let outer_hex = outer_id.strip_prefix("extrnlid").unwrap_or(outer_id);
    if outer_hex.chars().count() != 32 {
        return Err(format!("bad CLIP external id: {outer_id}"));
    }
    if !outer_hex.is_ascii() {
        return Err("'ascii' codec can't encode the external id".into());
    }
    let mut exta_payload = Vec::with_capacity(56 + inner.len());
    exta_payload.extend_from_slice(&0x28u64.to_be_bytes());
    exta_payload.extend_from_slice(b"extrnlid");
    exta_payload.extend_from_slice(outer_hex.as_bytes());
    exta_payload.extend_from_slice(&(inner.len() as u64).to_be_bytes());
    exta_payload.extend_from_slice(&inner);
    let new_attr = replace_block_sizes(attr_blob, &record_sizes)?;

    let con = clone_database(sqlite)?;
    con.execute_batch("BEGIN").map_err(sql)?;
    con.execute("UPDATE Offscreen SET Attribute = ? WHERE MainId = ?", params![new_attr, offscreen_id]).map_err(sql)?;
    mark_thumbnail_stale(&con, offscreen_id)?;
    con.execute_batch("COMMIT").map_err(sql)?;

    let mut payloads = chunk_payloads;
    for (c, p) in chunks.iter().zip(payloads.iter_mut()) {
        if c.file_off == exta.file_off {
            p.1 = exta_payload.clone();
        }
    }
    if exta_after_sqli {
        return Err("CLIP write-back: an Exta chunk after SQLi is unsupported".into());
    }
    // `_compute_exta_offsets`: every Exta's file offset by its outer id (the
    // last of an id wins), with the payloads as they will be written.
    let mut offsets: std::collections::BTreeMap<String, u64> = std::collections::BTreeMap::new();
    let mut off = first_chunk_off;
    for (cid, payload) in &payloads {
        if cid == b"Exta" {
            let id = &payload[8.min(payload.len())..48.min(payload.len())];
            if !id.is_ascii() {
                return Err("'ascii' codec can't decode the Exta outer id".into());
            }
            offsets.insert(String::from_utf8(id.to_vec()).unwrap(), off);
        }
        off += 16 + payload.len() as u64;
    }
    con.execute_batch("BEGIN").map_err(sql)?;
    con.execute("DELETE FROM ExternalChunk", []).map_err(sql)?;
    {
        let mut insert = con.prepare("INSERT INTO ExternalChunk(ExternalID, Offset) VALUES (?, ?)").map_err(sql)?;
        for (id, offset) in &offsets {
            insert.execute(params![id, *offset as i64]).map_err(sql)?;
        }
    }
    invalidate_canvas_preview(&con)?;
    con.execute_batch("COMMIT").map_err(sql)?;
    out.bytes = Some(emit_with_sqli(first_chunk_off, &payloads, serialize(&con)?));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tile(alpha: u8) -> Vec<u8> {
        (0..TILE * TILE).flat_map(|i| [(i % 251) as u8, (i % 13) as u8, 7, alpha]).collect()
    }

    /// An Exta payload of `records` followed by `tail`.
    fn exta(records: &[Vec<u8>], tail: &[u8]) -> Vec<u8> {
        let inner: Vec<u8> = records.iter().flatten().chain(tail).copied().collect();
        let mut p = 0x28u64.to_be_bytes().to_vec();
        p.extend_from_slice(b"extrnlid");
        p.extend_from_slice(&[b'a'; 32]);
        p.extend_from_slice(&(inner.len() as u64).to_be_bytes());
        p.extend_from_slice(&inner);
        p
    }

    #[test]
    fn checksum_is_adler32_of_the_stored_blob() {
        let full = build_tile_record(0, &tile(255)).unwrap();
        let empty = build_tile_record(1, &tile(0)).unwrap();
        let body = 8 + 2 * BEGIN_NAME.len();
        let len = u32::from_be_bytes(full[body + 20..body + 24].try_into().unwrap()) as usize;
        let blob = &full[body + 24..body + 24 + len];
        assert_eq!(blob.len(), 4 + u32::from_le_bytes(blob[..4].try_into().unwrap()) as usize);
        // adler32 by its definition.
        let (mut a, mut b) = (1u32, 0u32);
        for &x in blob {
            a = (a + x as u32) % 65521;
            b = (b + a) % 65521;
        }
        assert_eq!(record_checksum(&full), (b << 16) | a);
        assert_eq!(record_checksum(&empty), 0);
    }

    #[test]
    fn tables_are_laid_out_as_csp_writes_them() {
        let t = trailing_tables(0, &[5, 0, 9]).unwrap();
        let mut want = Vec::new();
        for (name, entries) in [("BlockStatus", [0u32, 0, 0]), ("BlockCheckSum", [5, 0, 9])] {
            want.extend_from_slice(&(name.len() as u32).to_be_bytes());
            want.extend_from_slice(&utf16be(name));
            for v in [12u32, 3, 4].into_iter().chain(entries) {
                want.extend_from_slice(&v.to_be_bytes());
            }
        }
        assert_eq!(t, want);
        assert_eq!(&t[..4], &11u32.to_be_bytes());
    }

    #[test]
    fn the_walkers_stop_at_the_tables_and_status_is_kept() {
        let records = vec![build_tile_record(0, &tile(255)).unwrap(), build_tile_record(1, &tile(0)).unwrap()];
        let sums: Vec<u32> = records.iter().map(|r| record_checksum(r)).collect();
        let with = exta(&records, &trailing_tables(0, &sums).unwrap());
        assert_eq!(records_by_block(&with).unwrap().len(), 2);
        assert_eq!(source_block_status(&with), 0);
        let without = exta(&records, &[]);
        assert_eq!(records_by_block(&without).unwrap().len(), 2);
        assert_eq!(source_block_status(&without), 1);
        assert_eq!(source_block_status(&exta(&records, &trailing_tables(1, &sums).unwrap())), 1);
    }

    #[test]
    fn thumbnail_is_marked_stale_for_the_layer_only() {
        let con = Connection::open_in_memory().unwrap();
        con.execute_batch(
            "CREATE TABLE LayerThumbnail(MainId INTEGER, ThumbnailSmallNeedRefresh INTEGER, ThumbnailSmallNeedRefresh1 INTEGER, ThumbnailOffscreen INTEGER);
             CREATE TABLE Layer(MainId INTEGER, LayerRenderMipmap INTEGER, LayerRenderThumbnail INTEGER);
             CREATE TABLE Mipmap(MainId INTEGER, BaseMipmapInfo INTEGER);
             CREATE TABLE MipmapInfo(MainId INTEGER, Offscreen INTEGER);
             INSERT INTO LayerThumbnail VALUES (10, 0, 0, 99), (11, 0, 0, 98);
             INSERT INTO Layer VALUES (1, 20, 10), (2, 21, 11);
             INSERT INTO Mipmap VALUES (20, 30), (21, 31);
             INSERT INTO MipmapInfo VALUES (30, 40), (31, 41);",
        )
        .unwrap();
        mark_thumbnail_stale(&con, 40).unwrap();
        let rows: Vec<(i64, i64, i64, i64)> = con
            .prepare("SELECT MainId, ThumbnailSmallNeedRefresh, ThumbnailSmallNeedRefresh1, ThumbnailOffscreen FROM LayerThumbnail ORDER BY MainId")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(rows, vec![(10, 1, 1, 99), (11, 0, 0, 98)]);
        // No LayerThumbnail table: silently nothing.
        mark_thumbnail_stale(&Connection::open_in_memory().unwrap(), 40).unwrap();
    }
}
