//! `writer.write-layers` (PLUGINS.md 7.3's write flow): the zip rebuilt
//! around new layer PNGs.
//!
//! - Every member the edit does not touch is copied as it is
//!   (`piece::source`: its local header, its data and whatever followed it
//!   up to the next member), so its bytes never pass through the plugin.
//! - An edited layer's `src` member becomes a stored member holding the PNG
//!   the host encodes (`edit.encode-image(px, png-v1)`): a new local header
//!   (`bytes`, the original's fields with method 0, the blob's CRC and
//!   length, no data descriptor, no extra field), then the blob.
//! - `mergedimage.png`, when there is one, is rebuilt the same way from
//!   `edit.composite-after-edit`, and `Thumbnails/thumbnail.png` from its
//!   256 px thumbnail (OpenRaster asks for both to show the document).
//! - The central directory is copied record by record with the local header
//!   offsets moved (and, for a rebuilt member, its method, CRC and sizes),
//!   then the end of central directory record with the directory's new
//!   offset (its comment kept).
//!
//! Writing the same pixels again gives the same bytes (the host's PNG
//! encoder is fixed, `png-v1`), and so does writing into a file this writer
//! already wrote.
//!
//! A test-only attribute of `<image>`, `tmi-test-write-fault`, makes the
//! writer misbehave so the host's checks can be seen to stop it
//! (`tmi-plugin-host`'s tests, the conformance scenario `plugins`):
//! `other-layer` also writes the first edit's pixels into another layer,
//! `ignore-edit` keeps the edited layers' old bytes, `bad-span` points a
//! piece past the end of the file.

use std::collections::HashMap;

use crate::model::*;
use crate::zip::Directory;

fn u16le(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn put16(b: &mut [u8], at: usize, v: u16) {
    b[at..at + 2].copy_from_slice(&v.to_le_bytes());
}

fn put32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

fn corrupt(m: impl Into<String>) -> WriteFail {
    WriteFail::Corrupt(m.into())
}

fn read_err(e: Error) -> WriteFail {
    match e {
        Error::NotThisFormat(m) | Error::Corrupt(m) => WriteFail::Corrupt(m),
        Error::Unsupported(m) | Error::TooLarge(m) => WriteFail::Unsupported(m),
    }
}

/// What replaces a member: its new data (a blob of the host).
struct Rebuilt {
    blob: u32,
    len: u32,
    crc: u32,
}

const MERGED: &str = "mergedimage.png";
const THUMBNAIL: &str = "Thumbnails/thumbnail.png";

/// `writer.write-layers`.
pub fn write_layers(host: &mut dyn WriteHost, edits: &[Edit]) -> Result<Plan, WriteFail> {
    if !crate::info().can_write {
        return Err(WriteFail::Unsupported("this build of the ora plugin reads only".into()));
    }
    let read = crate::read_all(host).map_err(read_err)?;
    let fault = read.fault.as_deref();
    let dir = Directory::open(host).map_err(read_err)?;
    let archive = crate::zip::Archive::open(host).map_err(read_err)?;
    let size = host.size();

    // what each member becomes, by its local header's offset
    let mut rebuilt: HashMap<u64, Rebuilt> = HashMap::new();
    let mut rebuild = |host: &mut dyn WriteHost, name: &str, pixels: u32| -> Result<(), WriteFail> {
        let local = archive.local(name).ok_or_else(|| corrupt(format!("{name} is not in the archive")))?;
        let blob = host.encode_png(pixels).map_err(|e| WriteFail::Invalid(format!("{name}: {e}")))?;
        let (len, crc) = host.blob_info(blob);
        let len = u32::try_from(len).map_err(|_| WriteFail::Unsupported(format!("{name}: over 4 GiB (zip64 is not written)")))?;
        rebuilt.insert(local, Rebuilt { blob, len, crc });
        Ok(())
    };
    let source_of = |sid: &str| read.sources.iter().find(|(s, _)| s == sid).map(|(_, src)| src.clone());
    for e in edits {
        let src = source_of(&e.stable_id).ok_or_else(|| WriteFail::Invalid(format!("no raster layer {}", e.stable_id)))?;
        if fault != Some("ignore-edit") {
            rebuild(host, &src, e.pixels)?;
        }
    }
    if fault == Some("other-layer") {
        if let (Some(first), Some((_, other))) = (edits.first(), read.sources.iter().find(|(s, _)| edits.iter().all(|e| &e.stable_id != s))) {
            rebuild(host, other, first.pixels)?;
        }
    }
    if !edits.is_empty() && archive.member(MERGED).is_some() {
        let composite = host.composite_after_edit().map_err(|e| WriteFail::Unsupported(format!("{MERGED}: {e}")))?;
        rebuild(host, MERGED, composite)?;
        if archive.member(THUMBNAIL).is_some() {
            let thumb = host.thumbnail(composite, 256);
            rebuild(host, THUMBNAIL, thumb)?;
        }
    }

    // the members in file order, each running to the next (or the directory)
    let mut starts: Vec<u64> = dir.entries.iter().map(|e| e.local).collect();
    starts.sort_unstable();
    starts.dedup();
    if starts.last().is_some_and(|&s| s >= dir.cd_offset) {
        return Err(corrupt("a local header at or past the central directory"));
    }
    let mut pieces = Vec::new();
    let mut at: u64 = 0; // the written file's length so far
    let mut moved: HashMap<u64, u64> = HashMap::new(); // old local offset -> new
    let push_source = |pieces: &mut Vec<Piece>, at: &mut u64, offset: u64, len: u64| {
        if len > 0 {
            pieces.push(Piece::Source(Span { offset, len }));
            *at += len;
        }
    };
    let first = starts.first().copied().unwrap_or(dir.cd_offset);
    push_source(&mut pieces, &mut at, 0, first);
    for (i, &start) in starts.iter().enumerate() {
        let end = starts.get(i + 1).copied().unwrap_or(dir.cd_offset);
        moved.insert(start, at);
        match rebuilt.get(&start) {
            None => push_source(&mut pieces, &mut at, start, end - start),
            Some(r) => {
                let head = host.read(start, 30);
                if head.len() < 30 || head[..4] != *b"PK\x03\x04" {
                    return Err(corrupt("a member without a local header"));
                }
                let n = u16le(&head, 26) as usize;
                let name = host.read(start + 30, n as u32);
                let mut h = head.clone();
                put16(&mut h, 6, u16le(&head, 6) & !0x0008); // no data descriptor
                put16(&mut h, 8, 0); // stored
                put32(&mut h, 14, r.crc);
                put32(&mut h, 18, r.len);
                put32(&mut h, 22, r.len);
                put16(&mut h, 28, 0); // no extra field
                h.extend_from_slice(&name);
                at += h.len() as u64;
                pieces.push(Piece::Bytes(h));
                pieces.push(Piece::Blob(r.blob));
                at += r.len as u64;
            }
        }
    }
    let cd_start = at;
    let mut cd = Vec::with_capacity(dir.cd_size as usize);
    for e in &dir.entries {
        if e.record.offset + e.record.len > dir.cd_offset + dir.cd_size {
            return Err(corrupt(format!("{}: its central directory entry runs past the directory", e.name)));
        }
        let mut rec = host.read(e.record.offset, e.record.len as u32);
        let new_local = *moved.get(&e.local).ok_or_else(|| corrupt("a member the directory does not place"))?;
        let new_local = u32::try_from(new_local).map_err(|_| WriteFail::Unsupported("over 4 GiB (zip64 is not written)".into()))?;
        put32(&mut rec, 42, new_local);
        if let Some(r) = rebuilt.get(&e.local) {
            let flags = u16le(&rec, 8);
            put16(&mut rec, 8, flags & !0x0008);
            put16(&mut rec, 10, 0);
            put32(&mut rec, 16, r.crc);
            put32(&mut rec, 20, r.len);
            put32(&mut rec, 24, r.len);
        }
        cd.extend_from_slice(&rec);
    }
    at += cd.len() as u64;
    let mut eocd = host.read(dir.eocd.offset, dir.eocd.len as u32);
    let cd_start32 = u32::try_from(cd_start).map_err(|_| WriteFail::Unsupported("over 4 GiB (zip64 is not written)".into()))?;
    put32(&mut eocd, 12, cd.len() as u32);
    put32(&mut eocd, 16, cd_start32);
    pieces.push(Piece::Bytes(cd));
    pieces.push(Piece::Bytes(eocd));
    let _ = at;
    if fault == Some("bad-span") {
        pieces.push(Piece::Source(Span { offset: size, len: 10 }));
    }
    Ok(Plan { pieces, warnings: Vec::new() })
}
