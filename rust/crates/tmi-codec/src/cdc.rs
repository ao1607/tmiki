//! Content-defined chunking cut points (tmi/chunking.py `cdc_cuts`).
//!
//! A position p (an exclusive piece end) is a candidate when the wrapping sum
//! of `gear[data[i]]` over the `window` bytes before it has its top `bits`
//! bits zero. The numpy reference evaluates exactly the positions
//! `start + window + 1 ..= end`, block by block; here the range is split into
//! fixed blocks scanned on the rayon pool, each warming up its own window, so
//! the candidates (and hence the cuts) never depend on the thread count.
//! The gear table, sizes and bit count all come from Python, which stays the
//! single source of those constants.

use rayon::prelude::*;

const SCAN_BLOCK: usize = 1 << 20;

fn candidates(data: &[u8], gear: &[u64; 256], first: usize, last: usize, window: usize, shift: u32) -> Vec<usize> {
    // positions first..=last; position p hashes data[p - window .. p]
    if first > last {
        return Vec::new();
    }
    let blocks: Vec<(usize, usize)> = (first..=last)
        .step_by(SCAN_BLOCK)
        .map(|a| (a, (a + SCAN_BLOCK - 1).min(last)))
        .collect();
    let per_block: Vec<Vec<usize>> = blocks
        .par_iter()
        .map(|&(a, b)| {
            let mut out = Vec::new();
            let mut h: u64 = 0;
            for &byte in &data[a - window..a] {
                h = h.wrapping_add(gear[byte as usize]);
            }
            if h >> shift == 0 {
                out.push(a);
            }
            for p in a + 1..=b {
                h = h
                    .wrapping_add(gear[data[p - 1] as usize])
                    .wrapping_sub(gear[data[p - 1 - window] as usize]);
                if h >> shift == 0 {
                    out.push(p);
                }
            }
            out
        })
        .collect();
    per_block.concat()
}

fn greedy(cands: &[usize], start: usize, end: usize, piece_min: usize, piece_max: usize) -> Vec<usize> {
    let mut cuts = Vec::new();
    let mut last = start;
    while end - last > piece_min {
        let i = cands.partition_point(|&c| c < last + piece_min);
        let mut cut = if i < cands.len() { cands[i] } else { end };
        if cut - last > piece_max {
            cut = last + piece_max;
        }
        cuts.push(cut);
        last = cut;
    }
    if last < end {
        cuts.push(end);
    }
    cuts
}

/// The cut points of `data[start..end]`: every exclusive piece end, the last
/// one `end`. Err for parameters the scan cannot honour.
#[allow(clippy::too_many_arguments)]
pub fn cdc_cuts(
    data: &[u8],
    start: usize,
    end: usize,
    gear: &[u64],
    piece_min: usize,
    piece_max: usize,
    bits: u32,
    window: usize,
) -> Result<Vec<usize>, String> {
    if start > end || end > data.len() {
        return Err("cdc range out of bounds".into());
    }
    let gear: &[u64; 256] = gear.try_into().map_err(|_| "gear table must have 256 entries".to_string())?;
    if bits == 0 || bits > 64 || window == 0 || piece_min < window {
        return Err("bad cdc parameters".into());
    }
    if end - start <= piece_min {
        return Ok(vec![end]);
    }
    let shift = 64 - bits;
    let cands = candidates(data, gear, start + window + 1, end, window, shift);
    Ok(greedy(&cands, start, end, piece_min, piece_max))
}