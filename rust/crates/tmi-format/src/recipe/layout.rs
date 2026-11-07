//! `types.layout`: decoded tile bytes (groups of channels) into RGBA8.
//!
//! The decoded bytes are the groups in order. A group holds every pixel of
//! the tile: its rows, top to bottom, each row `ceil(width * channels *
//! bits / 8)` bytes of interleaved samples followed by `row_padding` bytes.
//! Samples become 8 bits as follows (PLUGINS.md "P1 の結果"):
//! - `one-bit-msb`: the bits of a row, most significant first, 0 -> 0, 1 -> 255;
//! - `eight`: as is;
//! - `sixteen-be` / `sixteen-le`: the high byte (`v >> 8`);
//! - `float32-le`: `round(clamp(v, 0, 1) * 255)` computed in f64, NaN -> 0.
//!
//! A pixel starts as `[0, 0, 0, 255]`; each channel is written in group order
//! (a later group overwrites an earlier one), `gray` writes r, g and b, `pad`
//! is skipped. Then `premultiplied` undoes the premultiplication
//! (`tmi_pillow::convert::unpremultiply_rgba`, Pillow's RGBa -> RGBA), then the
//! tint: a layout with no colour channel (no r, g, b or gray) takes
//! `tint.rgb`, and with a tint the alpha becomes `a * tint.a / 255`
//! (truncated).

use crate::types::*;

fn corrupt(msg: impl Into<String>) -> ReadError {
    ReadError::Corrupt(msg.into())
}

fn bits(d: Depth) -> usize {
    match d {
        Depth::OneBitMsb => 1,
        Depth::Eight => 8,
        Depth::SixteenBe | Depth::SixteenLe => 16,
        Depth::Float32Le => 32,
    }
}

/// Bytes of one row of one group, without the padding.
pub fn row_bytes(g: &Group, width: usize) -> usize {
    (width * g.channels.len() * bits(g.depth)).div_ceil(8)
}

/// The length the decoded bytes of a `width` x `height` tile must have.
pub fn expected_len(layout: &Layout, width: usize, height: usize) -> Result<usize, ReadError> {
    if layout.groups.is_empty() {
        return Err(corrupt("a layout without groups"));
    }
    let mut total = 0usize;
    for g in &layout.groups {
        if g.channels.is_empty() {
            return Err(corrupt("a group without channels"));
        }
        let stride = row_bytes(g, width).checked_add(layout.row_padding as usize).ok_or_else(|| corrupt("row too long"))?;
        total = stride
            .checked_mul(height)
            .and_then(|p| total.checked_add(p))
            .filter(|&t| t <= super::transform::MAX_EXPANDED)
            .ok_or_else(|| corrupt("tile data over 1 GiB"))?;
    }
    Ok(total)
}

/// The RGBA8 of `data` (`expected_len` bytes) as a `width` x `height` tile.
pub fn assemble(layout: &Layout, data: &[u8], width: usize, height: usize) -> Result<Vec<u8>, ReadError> {
    if data.len() != expected_len(layout, width, height)? {
        return Err(corrupt(format!(
            "tile data is {} bytes, the layout of a {width}x{height} tile needs {}",
            data.len(),
            expected_len(layout, width, height)?
        )));
    }
    let n = width * height;
    let mut out = vec![0u8; n * 4];
    // (every alpha is written below when a group has one)
    if !layout.groups.iter().any(|g| g.channels.iter().any(|c| matches!(c, Channel::A))) {
        for px in out.chunks_exact_mut(4) {
            px[3] = 255;
        }
    }
    let mut has_color = false;
    let mut off = 0usize;
    for g in &layout.groups {
        let rb = row_bytes(g, width);
        let stride = rb + layout.row_padding as usize;
        // where each channel of this group goes: (its index, a mask of the
        // RGBA bytes: r 1, g 2, b 4, a 8), `pad` left out, in channel order
        let ops: Vec<(usize, u8)> = g
            .channels
            .iter()
            .enumerate()
            .map(|(k, c)| {
                (
                    k,
                    match c {
                        Channel::R => 1,
                        Channel::G => 2,
                        Channel::B => 4,
                        Channel::A => 8,
                        Channel::Gray => 7,
                        Channel::Pad => 0,
                    },
                )
            })
            .filter(|&(_, m)| m != 0)
            .collect();
        has_color |= ops.iter().any(|&(_, m)| m & 7 != 0);
        let nch = g.channels.len();
        for y in 0..height {
            let row = &data[off + y * stride..off + y * stride + rb];
            let dst = &mut out[y * width * 4..(y + 1) * width * 4];
            match g.depth {
                Depth::Eight => fill_eight(dst, row, width, nch, &ops),
                Depth::OneBitMsb => fill(dst, row, nch, &ops, |r, s| if r[s >> 3] >> (7 - (s & 7)) & 1 != 0 { 255 } else { 0 }),
                Depth::SixteenBe => fill(dst, row, nch, &ops, |r, s| r[2 * s]),
                Depth::SixteenLe => fill(dst, row, nch, &ops, |r, s| r[2 * s + 1]),
                Depth::Float32Le => fill(dst, row, nch, &ops, |r, s| {
                    let v = f32::from_le_bytes([r[4 * s], r[4 * s + 1], r[4 * s + 2], r[4 * s + 3]]);
                    if v.is_nan() {
                        0
                    } else {
                        (v.clamp(0.0, 1.0) as f64 * 255.0).round() as u8
                    }
                }),
            }
        }
        off += stride * height;
    }
    if layout.premultiplied {
        tmi_pillow::convert::unpremultiply_rgba(&mut out);
    }
    if let Some(t) = layout.tint {
        for px in out.chunks_exact_mut(4) {
            if !has_color {
                px[0] = t.r;
                px[1] = t.g;
                px[2] = t.b;
            }
            px[3] = (px[3] as u32 * t.a as u32 / 255) as u8;
        }
    }
    Ok(out)
}

#[inline(always)]
fn fill(dst: &mut [u8], row: &[u8], nch: usize, ops: &[(usize, u8)], sample: impl Fn(&[u8], usize) -> u8) {
    // the common one-channel groups (an alpha plane, a grey plane) first
    if let [(0, m)] = *ops {
        if nch == 1 {
            for (x, px) in dst.chunks_exact_mut(4).enumerate() {
                let v = sample(row, x);
                put(px, m, v);
            }
            return;
        }
    }
    for (x, px) in dst.chunks_exact_mut(4).enumerate() {
        for &(k, m) in ops {
            put(px, m, sample(row, x * nch + k));
        }
    }
}

#[inline(always)]
fn put(px: &mut [u8], m: u8, v: u8) {
    if m & 1 != 0 {
        px[0] = v;
    }
    if m & 2 != 0 {
        px[1] = v;
    }
    if m & 4 != 0 {
        px[2] = v;
    }
    if m & 8 != 0 {
        px[3] = v;
    }
}

/// `fill` of 8-bit samples, with the layouts tmi's own formats use spelled
/// out (the same writes, in the same order).
fn fill_eight(dst: &mut [u8], row: &[u8], width: usize, nch: usize, ops: &[(usize, u8)]) {
    let row = &row[..width * nch];
    match (nch, ops) {
        (4, [(0, 4), (1, 2), (2, 1), (3, 8)]) => {
            for (d, s) in dst.chunks_exact_mut(4).zip(row.chunks_exact(4)) {
                d.copy_from_slice(&[s[2], s[1], s[0], s[3]]);
            }
        }
        (4, [(0, 1), (1, 2), (2, 4), (3, 8)]) => dst.copy_from_slice(row),
        (4, [(0, 4), (1, 2), (2, 1)]) => {
            for (d, s) in dst.chunks_exact_mut(4).zip(row.chunks_exact(4)) {
                d[0] = s[2];
                d[1] = s[1];
                d[2] = s[0];
            }
        }
        (1, [(0, 8)]) => {
            for (d, &s) in dst.chunks_exact_mut(4).zip(row) {
                d[3] = s;
            }
        }
        (1, [(0, 7)]) => {
            for (d, &s) in dst.chunks_exact_mut(4).zip(row) {
                d[0] = s;
                d[1] = s;
                d[2] = s;
            }
        }
        _ => fill(dst, row, nch, ops, |r, s| r[s]),
    }
}
