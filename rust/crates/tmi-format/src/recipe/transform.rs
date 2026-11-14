//! `types.codec`: the byte-string transforms (PLUGINS.md 3.3, "answer is one
//! thing" kind: the same bytes out whatever the tmiki version), and taking a
//! `Bytes` out of the source file.

use std::borrow::Cow;
use std::io::Read;

use crate::types::*;

use super::DecodeEnv;

/// An `inline` byte string is at most this long (PLUGINS.md 2.1).
pub const MAX_INLINE: usize = 64 << 20;
/// No transform expands to more than this (a bomb guard; the tile and
/// bitmap checks are finer).
pub const MAX_EXPANDED: usize = 1 << 30;

fn corrupt(msg: impl Into<String>) -> ReadError {
    ReadError::Corrupt(msg.into())
}

/// The bytes of `Bytes::Span` inside `src` (the whole span must be in the file).
pub fn span_slice<'a>(src: &'a [u8], s: &Span) -> Result<&'a [u8], ReadError> {
    let end = s.offset.checked_add(s.len).filter(|&e| e <= src.len() as u64);
    match end {
        Some(end) => Ok(&src[s.offset as usize..end as usize]),
        None => Err(corrupt(format!("span {}+{} is outside the {}-byte file", s.offset, s.len, src.len()))),
    }
}

/// `bytes` as a byte string: a span borrows the file, spans are joined,
/// inline bytes are the plugin's own.
pub fn resolve<'a>(src: &'a [u8], b: &'a Bytes) -> Result<Cow<'a, [u8]>, ReadError> {
    match b {
        Bytes::Span(s) => Ok(Cow::Borrowed(span_slice(src, s)?)),
        Bytes::Spans(list) => {
            let total: u64 = list.iter().map(|s| s.len).fold(0u64, |a, l| a.saturating_add(l));
            if total > MAX_EXPANDED as u64 {
                return Err(corrupt("spans longer than 1 GiB"));
            }
            let mut out = Vec::with_capacity(total as usize);
            for s in list {
                out.extend_from_slice(span_slice(src, s)?);
            }
            Ok(Cow::Owned(out))
        }
        Bytes::Inline(v) => {
            if v.len() > MAX_INLINE {
                return Err(corrupt("inline bytes over 64 MiB"));
            }
            Ok(Cow::Borrowed(v))
        }
    }
}

/// Runs `codecs` in order over `data`. `expect`, when known, is the length
/// the last transform must give (also its output cap).
pub fn apply<'a>(env: &DecodeEnv, codecs: &[Codec], data: Cow<'a, [u8]>, expect: Option<usize>) -> Result<Cow<'a, [u8]>, ReadError> {
    let mut cur = data;
    for (i, c) in codecs.iter().enumerate() {
        let last = i + 1 == codecs.len();
        let cap = if last { expect.unwrap_or(MAX_EXPANDED).min(MAX_EXPANDED) } else { MAX_EXPANDED };
        let out = step(env, c, &cur, cap, if last { expect } else { None })?;
        cur = Cow::Owned(out);
    }
    Ok(cur)
}

/// `codecs.transform` (a plugin asks the host to run `codecs` over `data`):
/// like `apply`, but every step's output, the last one's included, is held
/// to `cap` bytes (`max-out`), so a plugin cannot make the host hold more.
pub fn apply_capped<'a>(env: &DecodeEnv, codecs: &[Codec], data: Cow<'a, [u8]>, cap: usize) -> Result<Cow<'a, [u8]>, ReadError> {
    let cap = cap.min(MAX_EXPANDED);
    let mut cur = data;
    for c in codecs {
        if let Codec::Lz4Block(n) | Codec::Lzf(n) | Codec::Packbits(n) = *c {
            if n as usize > cap {
                return Err(corrupt(format!("a transform announces {n} bytes, more than {cap}")));
            }
        }
        let out = step(env, c, &cur, cap, None)?;
        if out.len() > cap {
            return Err(corrupt(format!("more than {cap} bytes")));
        }
        cur = Cow::Owned(out);
    }
    if cur.len() > cap {
        return Err(corrupt(format!("more than {cap} bytes")));
    }
    Ok(cur)
}

fn read_capped(mut r: impl Read, cap: usize, what: &str) -> Result<Vec<u8>, ReadError> {
    let mut out = Vec::new();
    r.by_ref().take(cap as u64 + 1).read_to_end(&mut out).map_err(|e| corrupt(format!("{what}: {e}")))?;
    if out.len() > cap {
        return Err(corrupt(format!("{what}: more than {cap} bytes")));
    }
    Ok(out)
}

/// A zlib (`header`) or raw deflate stream that must end, expanding to at
/// most `cap` bytes; bytes after its end are ignored. Gives what
/// `read_capped` over flate2's reader gives, in one buffer of the expected
/// size instead of a growing one (tiles: one allocation each).
fn inflate(input: &[u8], cap: usize, expect: Option<usize>, header: bool, what: &str) -> Result<Vec<u8>, ReadError> {
    use flate2::{Decompress, FlushDecompress, Status};
    let mut d = Decompress::new(header);
    let mut out: Vec<u8> = Vec::with_capacity(expect.map_or(input.len().saturating_mul(4).max(1 << 12), |e| e + 1).min(cap.saturating_add(1)).min(1 << 26));
    loop {
        let fed = d.total_in() as usize;
        let status = d.decompress_vec(&input[fed..], &mut out, FlushDecompress::Finish).map_err(|e| corrupt(format!("{what}: {e}")))?;
        if out.len() > cap {
            return Err(corrupt(format!("{what}: more than {cap} bytes")));
        }
        match status {
            Status::StreamEnd => return Ok(out),
            _ if out.len() == out.capacity() => out.reserve(out.capacity().min(cap + 1 - out.len()).max(1)),
            _ => return Err(corrupt(format!("{what}: the stream ends early"))),
        }
    }
}

fn step(env: &DecodeEnv, c: &Codec, input: &[u8], cap: usize, expect: Option<usize>) -> Result<Vec<u8>, ReadError> {
    match *c {
        Codec::Zlib => inflate(input, cap, expect, true, "zlib"),
        Codec::Deflate => inflate(input, cap, expect, false, "deflate"),
        Codec::Gzip => read_capped(flate2::read::GzDecoder::new(input), cap, "gzip"),
        Codec::Zstd => {
            let d = zstd::stream::read::Decoder::new(input).map_err(|e| corrupt(format!("zstd: {e}")))?;
            read_capped(d, cap, "zstd")
        }
        Codec::Lz4Block(n) => lz4_block(input, announced(n, cap, "lz4 block")?),
        Codec::Lzf(n) => lzf(input, announced(n, cap, "lzf")?),
        Codec::Packbits(n) => packbits(input, announced(n, cap, "packbits")?),
        Codec::Predictor(p) => predictor(input, &p),
        Codec::Plugin(id) => {
            let Some(reader) = env.reader else {
                return Err(ReadError::Unsupported(format!("codec plugin({id}) without a plugin")));
            };
            let out = reader.decode_chunk(id, input, expect.unwrap_or(0).min(u32::MAX as usize) as u32)?;
            if out.len() > cap {
                return Err(corrupt(format!("plugin codec {id} gave {} bytes, expected at most {cap}", out.len())));
            }
            Ok(out)
        }
    }
}

/// The length a length-announcing transform (lz4, lzf, packbits) claims, held
/// to the step's output `cap` before anything is allocated.
fn announced(n: u32, cap: usize, what: &str) -> Result<usize, ReadError> {
    if n as usize > cap {
        return Err(corrupt(format!("{what}: announces {n} bytes, more than {cap}")));
    }
    Ok(n as usize)
}

/// How much to reserve up front for an announced `n`: no more than the input
/// can plausibly expand to (lz4 reaches 255 output bytes per input byte, lzf
/// and packbits less), so a lie in `n` costs no memory before the data backs
/// it. The vector grows as the output really appears.
fn prealloc(n: usize, input_len: usize) -> usize {
    n.min(input_len.saturating_mul(256).saturating_add(64))
}

/// An LZ4 block (https://github.com/lz4/lz4/blob/dev/doc/lz4_Block_format.md)
/// that expands to exactly `n` bytes.
pub fn lz4_block(input: &[u8], n: usize) -> Result<Vec<u8>, ReadError> {
    let mut out: Vec<u8> = Vec::with_capacity(prealloc(n, input.len()));
    let mut i = 0usize;
    let bad = |m: &str| corrupt(format!("lz4 block: {m}"));
    while i < input.len() {
        let token = input[i];
        i += 1;
        let mut lit = (token >> 4) as usize;
        if lit == 15 {
            loop {
                let b = *input.get(i).ok_or_else(|| bad("truncated literal length"))?;
                i += 1;
                lit += b as usize;
                if b != 255 {
                    break;
                }
            }
        }
        let lit_end = i.checked_add(lit).filter(|&e| e <= input.len()).ok_or_else(|| bad("literals run past the input"))?;
        if out.len() + lit > n {
            return Err(bad("more output than announced"));
        }
        out.extend_from_slice(&input[i..lit_end]);
        i = lit_end;
        if i == input.len() {
            break; // the last sequence has no match
        }
        let off = u16::from_le_bytes([
            *input.get(i).ok_or_else(|| bad("truncated offset"))?,
            *input.get(i + 1).ok_or_else(|| bad("truncated offset"))?,
        ]) as usize;
        i += 2;
        if off == 0 || off > out.len() {
            return Err(bad("offset outside the output"));
        }
        let mut len = (token & 15) as usize;
        if len == 15 {
            loop {
                let b = *input.get(i).ok_or_else(|| bad("truncated match length"))?;
                i += 1;
                len += b as usize;
                if b != 255 {
                    break;
                }
            }
        }
        len += 4;
        if out.len() + len > n {
            return Err(bad("more output than announced"));
        }
        let start = out.len() - off;
        for k in 0..len {
            let b = out[start + k];
            out.push(b);
        }
    }
    if out.len() != n {
        return Err(bad("less output than announced"));
    }
    Ok(out)
}

/// liblzf's format (Krita's tiles), expanding to exactly `n` bytes.
pub fn lzf(input: &[u8], n: usize) -> Result<Vec<u8>, ReadError> {
    let mut out: Vec<u8> = Vec::with_capacity(prealloc(n, input.len()));
    let mut i = 0usize;
    let bad = |m: &str| corrupt(format!("lzf: {m}"));
    while i < input.len() {
        let ctrl = input[i] as usize;
        i += 1;
        if ctrl < 32 {
            let len = ctrl + 1;
            let end = i.checked_add(len).filter(|&e| e <= input.len()).ok_or_else(|| bad("literals run past the input"))?;
            if out.len() + len > n {
                return Err(bad("more output than announced"));
            }
            out.extend_from_slice(&input[i..end]);
            i = end;
        } else {
            let mut len = ctrl >> 5;
            if len == 7 {
                len += *input.get(i).ok_or_else(|| bad("truncated length"))? as usize;
                i += 1;
            }
            let low = *input.get(i).ok_or_else(|| bad("truncated offset"))? as usize;
            i += 1;
            let back = ((ctrl & 0x1f) << 8) + low + 1;
            len += 2;
            if back > out.len() {
                return Err(bad("reference before the start"));
            }
            if out.len() + len > n {
                return Err(bad("more output than announced"));
            }
            let start = out.len() - back;
            for k in 0..len {
                let b = out[start + k];
                out.push(b);
            }
        }
    }
    if out.len() != n {
        return Err(bad("less output than announced"));
    }
    Ok(out)
}

/// PackBits (PSD, TIFF), expanding to exactly `n` bytes.
pub fn packbits(input: &[u8], n: usize) -> Result<Vec<u8>, ReadError> {
    let mut out: Vec<u8> = Vec::with_capacity(prealloc(n, input.len()));
    let mut i = 0usize;
    let bad = |m: &str| corrupt(format!("packbits: {m}"));
    while i < input.len() {
        let c = input[i] as i8;
        i += 1;
        if c >= 0 {
            let len = c as usize + 1;
            let end = i.checked_add(len).filter(|&e| e <= input.len()).ok_or_else(|| bad("literals run past the input"))?;
            if out.len() + len > n {
                return Err(bad("more output than announced"));
            }
            out.extend_from_slice(&input[i..end]);
            i = end;
        } else if c != -128 {
            let len = 1 - c as isize;
            let b = *input.get(i).ok_or_else(|| bad("truncated run"))?;
            i += 1;
            if out.len() + len as usize > n {
                return Err(bad("more output than announced"));
            }
            out.resize(out.len() + len as usize, b);
        }
    }
    if out.len() != n {
        return Err(bad("less output than announced"));
    }
    Ok(out)
}

/// `Codec::Predictor`.
pub fn predictor(input: &[u8], p: &Predictor) -> Result<Vec<u8>, ReadError> {
    let rb = p.row_bytes as usize;
    let bpp = p.bytes_per_pixel as usize;
    if rb == 0 || bpp == 0 {
        return Err(corrupt("predictor with an empty row or pixel"));
    }
    match p.kind {
        PredictorKind::TiffHorizontal => {
            if input.len() % rb != 0 {
                return Err(corrupt("predictor: the data is not a whole number of rows"));
            }
            let mut out = input.to_vec();
            for row in out.chunks_exact_mut(rb) {
                for i in bpp..rb {
                    row[i] = row[i].wrapping_add(row[i - bpp]);
                }
            }
            Ok(out)
        }
        PredictorKind::PngRows => {
            if input.len() % (rb + 1) != 0 {
                return Err(corrupt("predictor: the data is not a whole number of filtered rows"));
            }
            let rows = input.len() / (rb + 1);
            let mut out = vec![0u8; rows * rb];
            for y in 0..rows {
                let ft = input[y * (rb + 1)];
                let line = &input[y * (rb + 1) + 1..(y + 1) * (rb + 1)];
                let (done, rest) = out.split_at_mut(y * rb);
                let prev: &[u8] = if y == 0 { &[] } else { &done[(y - 1) * rb..] };
                let cur = &mut rest[..rb];
                for i in 0..rb {
                    let a = if i >= bpp { cur[i - bpp] } else { 0 };
                    let b = if y > 0 { prev[i] } else { 0 };
                    let c = if y > 0 && i >= bpp { prev[i - bpp] } else { 0 };
                    let x = line[i];
                    cur[i] = match ft {
                        0 => x,
                        1 => x.wrapping_add(a),
                        2 => x.wrapping_add(b),
                        3 => x.wrapping_add(((a as u16 + b as u16) / 2) as u8),
                        4 => x.wrapping_add(paeth(a, b, c)),
                        _ => return Err(corrupt(format!("predictor: filter type {ft}"))),
                    };
                }
            }
            Ok(out)
        }
    }
}

fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let (ia, ib, ic) = (a as i32, b as i32, c as i32);
    let p = ia + ib - ic;
    let (pa, pb, pc) = ((p - ia).abs(), (p - ib).abs(), (p - ic).abs());
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}
