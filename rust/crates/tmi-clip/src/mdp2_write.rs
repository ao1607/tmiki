//! MDP2 layer write-back: `replace_layer_pixels` of the Python reader
//! (`tmiref/mdp2.py`) after the PNG is decoded, byte for byte.
//!
//! The new pixels (the layer-sized image, padded like `Image.paste` onto a
//! transparent layer) are compared with the layer's own pixels; when they
//! differ the layer's tile stream is rebuilt (`_build_layer_payload`: every
//! 128px cell in row order, empty cells dropped, `zlib.compress` at level 6)
//! and the file re-emitted with every blob of that name replaced
//! (`_emit_with_replaced_blobs`).

use rayon::prelude::*;

use crate::mdp2::{decode_layer, image_size, parse_argb, Mdp2, Strictness, MAGIC, PAC_HEADER_SIZE, TILE};
use crate::zlib;

/// The layer types write-back accepts (`WRITABLE_LAYER_TYPES`).
pub fn writable(kind: &str) -> bool {
    matches!(kind, "32bpp" | "text" | "vector" | "8bpp" | "1bpp")
}

/// `_encode_tile`: cell (col, row) of `image` (`w` x `h` RGBA) as the
/// layer's tile bytes, None when they are all zero.
fn encode_tile(kind: &str, color_alpha: u8, image: &[u8], w: usize, h: usize, col: usize, row: usize) -> Option<Vec<u8>> {
    let (x0, y0) = (col * TILE, row * TILE);
    let cw = TILE.min(w.saturating_sub(x0));
    let ch = TILE.min(h.saturating_sub(y0));
    let px = |x: usize, y: usize| -> [u8; 4] {
        if x < cw && y < ch {
            let i = ((y0 + y) * w + x0 + x) * 4;
            [image[i], image[i + 1], image[i + 2], image[i + 3]]
        } else {
            [0; 4]
        }
    };
    let data: Vec<u8> = match kind {
        "32bpp" | "text" | "vector" => {
            let mut d = vec![0u8; TILE * TILE * 4];
            for y in 0..ch {
                for x in 0..cw {
                    let p = px(x, y);
                    d[(y * TILE + x) * 4..(y * TILE + x) * 4 + 4].copy_from_slice(&[p[2], p[1], p[0], p[3]]);
                }
            }
            d
        }
        _ => {
            // The painted value lives in alpha; undo the colour's alpha
            // scaling (`min(255, (v * 255 + ca // 2) // ca)`).
            let undo = |v: u8| -> u8 {
                if color_alpha != 0 && color_alpha != 255 {
                    let ca = color_alpha as u32;
                    ((v as u32 * 255 + ca / 2) / ca).min(255) as u8
                } else {
                    v
                }
            };
            if kind == "8bpp" {
                let mut d = vec![0u8; TILE * TILE];
                for y in 0..ch {
                    for x in 0..cw {
                        d[y * TILE + x] = undo(px(x, y)[3]);
                    }
                }
                d
            } else {
                // 1bpp: MSB first, 16 bytes a row.
                let mut d = vec![0u8; TILE * TILE / 8];
                for y in 0..ch {
                    for x in 0..cw {
                        if undo(px(x, y)[3]) >= 128 {
                            d[(y * TILE + x) / 8] |= 0x80 >> (x % 8);
                        }
                    }
                }
                d
            }
        }
    };
    data.iter().any(|&b| b != 0).then_some(data)
}

/// `_build_layer_payload`.
pub fn build_layer_payload(kind: &str, color: Option<&str>, image: &[u8], w: usize, h: usize) -> Vec<u8> {
    let cols = w.div_ceil(TILE).max(1);
    let rows = h.div_ceil(TILE).max(1);
    let color_alpha = parse_argb(color)[3];
    let tiles: Vec<(usize, usize, Vec<u8>)> = (0..rows * cols)
        .into_par_iter()
        .filter_map(|k| {
            let (row, col) = (k / cols, k % cols);
            encode_tile(kind, color_alpha, image, w, h, col, row).map(|d| (col, row, zlib::compress(&d, -1)))
        })
        .collect();
    if tiles.is_empty() {
        return 0u32.to_le_bytes().to_vec();
    }
    let mut buf = Vec::new();
    let le = |v: usize| (v as u32).to_le_bytes();
    let (c0, r0, z0) = &tiles[0];
    for v in [tiles.len(), TILE, *c0, *r0, 0, z0.len()] {
        buf.extend_from_slice(&le(v));
    }
    buf.extend_from_slice(z0);
    for (col, row, comp) in &tiles[1..] {
        buf.resize(buf.len().next_multiple_of(4), 0);
        for v in [*col, *row, 0, comp.len()] {
            buf.extend_from_slice(&le(v));
        }
        buf.extend_from_slice(comp);
    }
    buf.resize(buf.len().next_multiple_of(4), 0);
    buf
}

/// Whether the layer-sized `image` equals the layer's own pixels
/// (`render_layer_rgba(apply_opacity=False)`).
fn same_as_current(doc: &Mdp2, raw: &[u8], index: usize, image: &[u8], w: usize, h: usize) -> Result<bool, String> {
    let layer = &doc.layers[index];
    if image_size(layer)? != (w, h) {
        return Ok(false);
    }
    if !layer.renderable {
        return Ok(image.iter().all(|&b| b == 0));
    }
    let px = decode_layer(doc, raw, index, Strictness::Image)?;
    let stride = w * 4;
    let Some((x0, y0, x1, y1)) = px.rect else { return Ok(image.iter().all(|&b| b == 0)) };
    let rw = (x1 - x0) * 4;
    Ok(image.par_chunks(stride.max(1)).enumerate().all(|(y, row)| {
        if y < y0 || y >= y1 {
            return row.iter().all(|&b| b == 0);
        }
        row[..x0 * 4].iter().all(|&b| b == 0)
            && row[x1 * 4..].iter().all(|&b| b == 0)
            && row[x0 * 4..x1 * 4] == px.rgba[(y - y0) * rw..(y - y0 + 1) * rw]
    }))
}

/// `_emit_with_replaced_blobs(mdp, {name: payload})`.
pub fn emit_with_replaced_blob(doc: &Mdp2, raw: &[u8], name: &str, payload: &[u8]) -> Result<Vec<u8>, String> {
    let mut pack: Vec<u8> = Vec::with_capacity(raw.len());
    for blob in &doc.blobs {
        if blob.name == name {
            // The header as far as the file has it (a last record may be cut).
            let mut header = raw[blob.file_off.min(raw.len())..(blob.file_off + PAC_HEADER_SIZE).min(raw.len())].to_vec();
            if header.len() < 20 {
                return Err("pack_into requires a buffer of at least 20 bytes".into());
            }
            let size = u32::try_from(payload.len()).map_err(|_| "MDP2 layer payload too large")?;
            header[4..8].copy_from_slice(&(PAC_HEADER_SIZE as u32 + size).to_le_bytes());
            header[12..16].copy_from_slice(&size.to_le_bytes());
            header[16..20].copy_from_slice(&size.to_le_bytes());
            pack.extend_from_slice(&header);
            pack.extend_from_slice(payload);
        } else {
            pack.extend_from_slice(blob.record(raw));
        }
    }
    let xml = &raw[20..20 + doc.xml_size as usize];
    let mut out = Vec::with_capacity(20 + xml.len() + pack.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&doc.reserved.to_le_bytes());
    out.extend_from_slice(&doc.xml_size.to_le_bytes());
    out.extend_from_slice(&u32::try_from(pack.len()).map_err(|_| "'I' format requires 0 <= number <= 4294967295")?.to_le_bytes());
    out.extend_from_slice(xml);
    out.extend_from_slice(&pack);
    Ok(out)
}

/// `replace_layer_pixels` after the selector and the PNG: `image` is the
/// replacement's RGBA (`w` x `h`, no larger than the layer). None when it
/// equals the layer's pixels (Python returns the file unchanged).
pub fn replace_layer_pixels(doc: &Mdp2, raw: &[u8], index: usize, image: &[u8], w: usize, h: usize) -> Result<Option<Vec<u8>>, String> {
    let layer = &doc.layers[index];
    let bin = layer.bin_name.as_deref().filter(|b| !b.is_empty() && doc.blob_by_name.contains_key(*b)).ok_or("MDP2 layer has no writable pixel blob")?;
    if !writable(&layer.kind) {
        return Err(format!("MDP2 write-back not supported for layer type {:?}", layer.kind));
    }
    if w.checked_mul(h).and_then(|n| n.checked_mul(4)) != Some(image.len()) {
        return Err("RGBA bytes do not match the size".into());
    }
    if w as i64 > layer.width || h as i64 > layer.height {
        return Err(format!(
            "replacement image {w}x{h} is larger than layer {}x{}; stage the whole file instead",
            layer.width, layer.height
        ));
    }
    // `Image.new` + `paste`: the layer-sized image (w, h >= 0 here).
    if layer.width > i32::MAX as i64 || layer.height > i32::MAX as i64 {
        return Err("Python int too large to convert to C int".into());
    }
    let (lw, lh) = (layer.width as usize, layer.height as usize);
    // the layer-sized image is made whole: a declared size past what memory
    // holds is refused (the caller holds it to the pixel limit)
    if (lw as u128) * (lh as u128) * 4 > isize::MAX as u128 {
        return Err(format!("MDP2 layer of {lw}x{lh} pixels is too large to write back"));
    }
    let padded;
    let image = if (w, h) == (lw, lh) {
        image
    } else {
        let mut p = vec![0u8; lw * lh * 4];
        for y in 0..h {
            p[y * lw * 4..y * lw * 4 + w * 4].copy_from_slice(&image[y * w * 4..(y + 1) * w * 4]);
        }
        padded = p;
        &padded
    };
    if same_as_current(doc, raw, index, image, lw, lh)? {
        return Ok(None);
    }
    let payload = build_layer_payload(&layer.kind, layer.color.as_deref(), image, lw, lh);
    emit_with_replaced_blob(doc, raw, bin, &payload).map(Some)
}
