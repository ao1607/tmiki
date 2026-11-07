//! Layer blob container `TMIL` (codec "fpaeth-z3"); the layout is documented
//! in `tmi/layer_blob.py`, whose pure Python codec this mirrors.
//!
//! The painted rectangle's interleaved RGBA rows are cut into bands of about
//! 256 KiB; every row of a band is Paeth-filtered (PNG scanline format, the
//! first row predicting from zeros) and the band is one zstd frame at level
//! 3. Bands are filtered/compressed and decompressed/un-filtered on the rayon
//! pool; the output depends on the pixels only, never on the thread count.

use rayon::prelude::*;
use std::cell::RefCell;

const MAGIC: &[u8; 4] = b"TMIL";
const VERSION: u8 = 1;
const HEADER: usize = 40;
const BAND_BYTES: usize = 256 * 1024;
const LEVEL: i32 = 3;
const PAETH: u8 = 4;

thread_local! {
    // One zstd context per worker thread, reused across bands and layers.
    static CCTX: RefCell<Option<zstd::bulk::Compressor<'static>>> = const { RefCell::new(None) };
    static DCTX: RefCell<Option<zstd::bulk::Decompressor<'static>>> = const { RefCell::new(None) };
}

fn rows_per_band(w: usize) -> usize {
    (BAND_BYTES / (4 * w)).max(1)
}

fn compress(src: &[u8]) -> std::io::Result<Vec<u8>> {
    CCTX.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            *slot = Some(zstd::bulk::Compressor::new(LEVEL)?);
        }
        slot.as_mut().unwrap().compress(src)
    })
}

fn decompress_into(src: &[u8], dst: &mut [u8]) -> Result<usize, String> {
    DCTX.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            *slot = Some(zstd::bulk::Decompressor::new().map_err(|e| e.to_string())?);
        }
        slot.as_mut().unwrap().decompress_to_buffer(src, dst).map_err(|e| e.to_string())
    })
}

/// Paeth predictor written so the optimiser emits selects, not branches.
#[inline(always)]
fn paeth(a: i16, b: i16, c: i16) -> i16 {
    let pa = (b - c).abs();
    let pb = (a - c).abs();
    let pc = (a + b - 2 * c).abs();
    let bc = if pb <= pc { b } else { c };
    if pa <= pb && pa <= pc {
        a
    } else {
        bc
    }
}

/// Paeth residuals of row `cur` over `prev` (bpp 4). Reads the original rows
/// only, so there is no serial dependency.
fn paeth_filter(cur: &[u8], prev: &[u8], out: &mut [u8]) {
    let n = cur.len();
    for i in 0..4 {
        out[i] = cur[i].wrapping_sub(prev[i]);
    }
    for (((o, x), (a, b)), c) in out[4..n]
        .iter_mut()
        .zip(&cur[4..n])
        .zip(cur[..n - 4].iter().zip(&prev[4..n]))
        .zip(&prev[..n - 4])
    {
        *o = x.wrapping_sub(paeth(*a as i16, *b as i16, *c as i16) as u8);
    }
}

/// Paeth reconstruction for bpp 4: the left neighbour is the only serial
/// dependency, so the four channels run as lanes over exact 4-byte chunks.
fn unpaeth(row: &mut [u8], prev: &[u8]) {
    let mut left = [0i16; 4];
    let mut upleft = [0i16; 4];
    for (px, up) in row.chunks_exact_mut(4).zip(prev.chunks_exact(4)) {
        let up = [up[0] as i16, up[1] as i16, up[2] as i16, up[3] as i16];
        let mut out = [0u8; 4];
        for k in 0..4 {
            out[k] = px[k].wrapping_add(paeth(left[k], up[k], upleft[k]) as u8);
        }
        px.copy_from_slice(&out);
        left = [out[0] as i16, out[1] as i16, out[2] as i16, out[3] as i16];
        upleft = up;
    }
}

/// Reverse PNG filter `kind` on `row` (residuals in, pixels out), bpp 4.
fn unfilter(kind: u8, row: &mut [u8], prev: &[u8]) -> Result<(), String> {
    let n = row.len();
    match kind {
        0 => {}
        1 => {
            for i in 4..n {
                row[i] = row[i].wrapping_add(row[i - 4]);
            }
        }
        2 => {
            for (x, &b) in row.iter_mut().zip(prev) {
                *x = x.wrapping_add(b);
            }
        }
        3 => {
            for i in 0..4.min(n) {
                row[i] = row[i].wrapping_add(prev[i] >> 1);
            }
            for i in 4..n {
                row[i] = row[i].wrapping_add(((row[i - 4] as u16 + prev[i] as u16) >> 1) as u8);
            }
        }
        4 => unpaeth(row, prev),
        _ => return Err(format!("unknown row filter type {kind}")),
    }
    Ok(())
}

fn encode_band(band: &[u8], stride: usize) -> std::io::Result<Vec<u8>> {
    let rows = band.len() / stride;
    let mut payload = vec![0u8; rows * (stride + 1)];
    let zero = vec![0u8; stride];
    for r in 0..rows {
        let cur = &band[r * stride..(r + 1) * stride];
        let prev = if r == 0 { &zero[..] } else { &band[(r - 1) * stride..r * stride] };
        let out = &mut payload[r * (stride + 1)..(r + 1) * (stride + 1)];
        out[0] = PAETH;
        paeth_filter(cur, prev, &mut out[1..]);
    }
    compress(&payload)
}

/// One band's frame decoded into `dst` (its rows of the rectangle).
fn decode_band(frame: &[u8], dst: &mut [u8], stride: usize) -> Result<(), String> {
    match zstd::zstd_safe::find_frame_compressed_size(frame) {
        Ok(n) if n == frame.len() => {}
        Ok(_) => return Err("band frame size does not match the band table".into()),
        Err(code) => return Err(format!("band is not a zstd frame ({})", zstd::zstd_safe::get_error_name(code))),
    }
    let rows = dst.len() / stride;
    let mut payload = vec![0u8; rows * (stride + 1)];
    let n = decompress_into(frame, &mut payload).map_err(|e| format!("corrupt band: {e}"))?;
    if n != payload.len() {
        return Err(format!("band holds {n} bytes, expected {}", payload.len()));
    }
    for r in 0..rows {
        let src = &payload[r * (stride + 1)..(r + 1) * (stride + 1)];
        let (done, rest) = dst.split_at_mut(r * stride);
        let row = &mut rest[..stride];
        row.copy_from_slice(&src[1..]);
        if r == 0 {
            unfilter(src[0], row, &vec![0u8; stride])?;
        } else {
            unfilter(src[0], row, &done[(r - 1) * stride..])?;
        }
    }
    Ok(())
}

fn u32_at(data: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(data[at..at + 4].try_into().unwrap())
}

pub struct Header {
    pub w: usize,
    pub h: usize,
    rows: usize,
    sizes: Vec<usize>,
}

pub fn parse(data: &[u8]) -> Result<Header, String> {
    if data.len() < HEADER {
        return Err(format!("layer blob truncated: {} bytes, header needs {HEADER}", data.len()));
    }
    if &data[..4] != MAGIC {
        return Err("not a TMIL layer blob".into());
    }
    if data[4] != VERSION {
        return Err(format!("unsupported TMIL layer blob version {}", data[4]));
    }
    if data[5..8] != [0, 0, 0] {
        return Err("reserved header bytes are not zero".into());
    }
    let f: Vec<u64> = (0..8).map(|i| u32_at(data, 8 + 4 * i) as u64).collect();
    let (width, height, x0, y0, w, h, rows, count) = (f[0], f[1], f[2], f[3], f[4], f[5], f[6], f[7]);
    if w == 0 || h == 0 {
        if w != 0 || h != 0 || x0 != 0 || y0 != 0 || rows != 0 || count != 0 || data.len() != HEADER {
            return Err("inconsistent header for an empty layer".into());
        }
        return Ok(Header { w: 0, h: 0, rows: 0, sizes: Vec::new() });
    }
    if x0 + w > width || y0 + h > height {
        return Err(format!("rectangle {w}x{h}+{x0}+{y0} outside the {width}x{height} layer"));
    }
    if rows == 0 || count != h.div_ceil(rows) {
        return Err(format!("{count} bands of {rows} rows do not cover {h} rows"));
    }
    let table_end = HEADER + 4 * count as usize;
    if data.len() < table_end {
        return Err("truncated in the band table".into());
    }
    let sizes: Vec<usize> = (0..count as usize).map(|i| u32_at(data, HEADER + 4 * i) as usize).collect();
    let total = table_end as u64 + sizes.iter().map(|&s| s as u64).sum::<u64>();
    if total != data.len() as u64 {
        return Err(format!("blob is {} bytes, its band table says {total}", data.len()));
    }
    Ok(Header { w: w as usize, h: h as usize, rows: rows as usize, sizes })
}

/// TMIL blob of a `width` x `height` RGBA rectangle at (`x0`, `y0`) in a layer
/// of `layer_width` x `layer_height` (an empty rectangle: an all-zero layer).
pub fn encode(
    rgba: &[u8],
    width: u32,
    height: u32,
    layer_width: u32,
    layer_height: u32,
    x0: u32,
    y0: u32,
) -> Result<Vec<u8>, String> {
    let (w, h) = (width as usize, height as usize);
    if rgba.len() != w * h * 4 {
        return Err(format!("rgba buffer is {} bytes, expected {w}x{h}x4", rgba.len()));
    }
    let empty = w == 0 || h == 0;
    let (x0, y0) = if empty { (0, 0) } else { (x0, y0) };
    if (w == 0) != (h == 0)
        || x0 as u64 + width as u64 > layer_width as u64
        || y0 as u64 + height as u64 > layer_height as u64
    {
        return Err(format!(
            "rectangle {width}x{height}+{x0}+{y0} does not fit the {layer_width}x{layer_height} layer"
        ));
    }
    let stride = 4 * w;
    let rows = if empty { 0 } else { rows_per_band(w) };
    let frames: Vec<Vec<u8>> = if empty {
        Vec::new()
    } else {
        rgba.par_chunks(rows * stride)
            .map(|band| encode_band(band, stride))
            .collect::<std::io::Result<_>>()
            .map_err(|e| format!("zstd compression failed: {e}"))?
    };
    let mut out = Vec::with_capacity(HEADER + 4 * frames.len() + frames.iter().map(Vec::len).sum::<usize>());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&[VERSION, 0, 0, 0]);
    for v in [layer_width, layer_height, x0, y0, width, height, rows as u32, frames.len() as u32] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    for f in &frames {
        out.extend_from_slice(&(f.len() as u32).to_le_bytes());
    }
    for f in &frames {
        out.extend_from_slice(f);
    }
    Ok(out)
}

/// Decode the rectangle's RGBA rows of a blob whose `header` is `parse(data)`
/// into `dst` (`4 * w * h` bytes).
pub fn decode_into(data: &[u8], header: &Header, dst: &mut [u8]) -> Result<(), String> {
    let stride = 4 * header.w;
    if header.w == 0 {
        return Ok(());
    }
    let mut frames = Vec::with_capacity(header.sizes.len());
    let mut at = HEADER + 4 * header.sizes.len();
    for &size in &header.sizes {
        frames.push(&data[at..at + size]);
        at += size;
    }
    dst.par_chunks_mut(header.rows * stride)
        .zip(frames)
        .try_for_each(|(band, frame)| decode_band(frame, band, stride))
}