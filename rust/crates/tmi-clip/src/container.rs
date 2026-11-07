//! The CSFCHUNK container (`parse_container` / `Container.index_extas`,
//! tmi/formats/clip.py:379-406 and :47-56).

use std::collections::HashMap;

pub const MAGIC: &[u8; 8] = b"CSFCHUNK";

#[derive(Clone, Debug)]
pub struct Chunk {
    pub cid: [u8; 4],
    pub file_off: usize,
    pub payload_off: usize,
    pub payload_size: usize,
}

fn be_u64(data: &[u8], at: usize) -> Result<u64, String> {
    data.get(at..at + 8)
        .map(|b| u64::from_be_bytes(b.try_into().unwrap()))
        .ok_or_else(|| format!("truncated at 0x{at:X}"))
}

/// The chunk table, as `parse_container` reads it. Err where Python raises.
pub fn parse_container(data: &[u8]) -> Result<Vec<Chunk>, String> {
    if data.get(..8) != Some(&MAGIC[..]) {
        return Err("not a CLIP file (bad magic)".into());
    }
    let file_size = be_u64(data, 8)?;
    let first = be_u64(data, 16)?;
    if file_size != data.len() as u64 {
        return Err(format!("file_size header {file_size} != actual {}", data.len()));
    }
    let mut chunks = Vec::new();
    let mut off = usize::try_from(first).map_err(|_| "first chunk offset overflows")?;
    while off < data.len() {
        if data.get(off..off + 4) != Some(&b"CHNK"[..]) {
            return Err(format!("expected CHNK at 0x{off:X}"));
        }
        let cid: [u8; 4] = data
            .get(off + 4..off + 8)
            .ok_or("truncated chunk header")?
            .try_into()
            .unwrap();
        let size = be_u64(data, off + 8)?;
        let size = usize::try_from(size).map_err(|_| "chunk size overflows")?;
        chunks.push(Chunk { cid, file_off: off, payload_off: off + 16, payload_size: size });
        off = off.checked_add(16).and_then(|o| o.checked_add(size)).ok_or("chunk size overflows")?;
    }
    Ok(chunks)
}

/// Exta chunks by their 40-byte outer id (`index_extas`; a later chunk with
/// the same id wins). The id is the ASCII text CLIP's SQLite points at.
pub fn index_extas(data: &[u8], chunks: &[Chunk]) -> Result<HashMap<Vec<u8>, Chunk>, String> {
    let mut out = HashMap::new();
    for chunk in chunks.iter().filter(|c| &c.cid == b"Exta") {
        let a = (chunk.payload_off + 8).min(data.len());
        let b = (chunk.payload_off + 48).min(data.len()).max(a);
        let id = &data[a..b];
        if !id.is_ascii() {
            return Err("Exta outer id is not ASCII".into());
        }
        out.insert(id.to_vec(), chunk.clone());
    }
    Ok(out)
}

/// The SQLite database's bytes (the first SQLi chunk).
pub fn sqlite_payload<'a>(data: &'a [u8], chunks: &[Chunk]) -> Result<&'a [u8], String> {
    let chunk = chunks.iter().find(|c| &c.cid == b"SQLi").ok_or("no SQLi chunk")?;
    let a = chunk.payload_off.min(data.len());
    let b = (chunk.payload_off + chunk.payload_size).min(data.len());
    Ok(&data[a..b.max(a)])
}

/// The payload bytes of `chunk` (Python-style slice: clamped to the data).
pub fn payload<'a>(data: &'a [u8], chunk: &Chunk) -> &'a [u8] {
    let a = chunk.payload_off.min(data.len());
    let b = chunk.payload_off.saturating_add(chunk.payload_size).min(data.len());
    &data[a..b.max(a)]
}
