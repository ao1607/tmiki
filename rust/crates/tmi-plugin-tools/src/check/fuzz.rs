//! PLUGINS.md 7.1 item 5 (`--fuzz N`): the corpus damaged N ways, each
//! imported. An import may succeed or fail; tmiki must not panic. A plugin
//! that traps (or runs out of fuel or time) on a damaged file is a warning:
//! it should have answered with a `read-error`.
//!
//! The damage is from a fixed seed (xorshift64*), so a run is repeatable:
//! case `k` is file `k mod files`, with the k-th draw of mutations.

use std::panic::{catch_unwind, AssertUnwindSafe};

use rayon::prelude::*;
use tmi_format::pipeline::Importer;
use tmi_format::reader::FormatReader;
use tmi_import::Limits;

use super::readers;
use super::report::Report;

/// Smaller than the checks' limits: a damaged header may name a huge canvas.
const LIMITS: Limits = Limits { layered: 1 << 24, raster: 1 << 24 };

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Rng {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next() % n as u64) as usize
        }
    }
}

/// Case `k` of `data`: the damaged bytes and what was done.
pub fn mutate(data: &[u8], k: u64) -> (Vec<u8>, String) {
    let mut r = Rng::new(k + 1);
    let mut d = data.to_vec();
    let mut what = Vec::new();
    for _ in 0..1 + r.below(3) {
        match r.below(6) {
            0 if !d.is_empty() => {
                let n = 1 + r.below(8);
                for _ in 0..n {
                    let at = r.below(d.len());
                    d[at] ^= 1 << r.below(8);
                }
                what.push(format!("{n} bit flips"));
            }
            1 if !d.is_empty() => {
                let at = r.below(d.len());
                d.truncate(at);
                what.push(format!("cut at {at}"));
            }
            2 if !d.is_empty() => {
                let at = r.below(d.len());
                let n = (1 + r.below(64)).min(d.len() - at);
                let b = if r.below(2) == 0 { 0x00 } else { 0xff };
                d[at..at + n].fill(b);
                what.push(format!("{n} bytes at {at} set to 0x{b:02x}"));
            }
            3 if !d.is_empty() => {
                // a 32-bit field somewhere made huge or zero
                let at = r.below(d.len().saturating_sub(4).max(1));
                let v: u32 = [0, 1, 0x7fff_ffff, 0xffff_ffff][r.below(4)];
                for (i, b) in v.to_le_bytes().iter().enumerate() {
                    if let Some(x) = d.get_mut(at + i) {
                        *x = *b;
                    }
                }
                what.push(format!("u32 {v:#x} at {at}"));
            }
            4 if !d.is_empty() => {
                let at = r.below(d.len());
                let n = (1 + r.below(256)).min(d.len() - at);
                let chunk = d[at..at + n].to_vec();
                let to = r.below(d.len());
                d.splice(to..to, chunk);
                what.push(format!("{n} bytes from {at} copied to {to}"));
            }
            _ => {
                let at = r.below(d.len() + 1);
                let n = 1 + r.below(32);
                let bytes: Vec<u8> = (0..n).map(|_| r.next() as u8).collect();
                d.splice(at..at, bytes);
                what.push(format!("{n} random bytes inserted at {at}"));
            }
        }
    }
    (d, what.join(", "))
}

enum Verdict {
    Read,
    Refused,
    Trap(String),
    Panic(String),
}

fn verdict_of(error: &str) -> Verdict {
    let host = ["crashed", "ran out of fuel", "ran past the time limit", "went over its memory limit", "overflowed its stack"];
    if host.iter().any(|h| error.contains(h)) {
        Verdict::Trap(error.to_string())
    } else {
        Verdict::Refused
    }
}

pub fn run(report: &mut Report, reader: &dyn FormatReader, importer: &Importer, files: &[(String, Vec<u8>)], n: u64) {
    const CHECK: &str = "5 fuzz";
    if files.is_empty() {
        return;
    }
    let results: Vec<(u64, String, Verdict)> = (0..n)
        .into_par_iter()
        .map(|k| {
            let (name, data) = &files[(k % files.len() as u64) as usize];
            let (bad, what) = mutate(data, k);
            let v = match catch_unwind(AssertUnwindSafe(|| readers::import(reader, importer, name, &bad, LIMITS))) {
                Ok(Ok(_)) => Verdict::Read,
                Ok(Err(e)) => verdict_of(&e),
                Err(p) => Verdict::Panic(p.downcast_ref::<String>().cloned().or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default()),
            };
            (k, format!("{name} ({what})"), v)
        })
        .collect();
    let (mut read, mut refused, mut traps, mut panics) = (0, 0, Vec::new(), Vec::new());
    for (k, what, v) in results {
        match v {
            Verdict::Read => read += 1,
            Verdict::Refused => refused += 1,
            Verdict::Trap(e) => traps.push((k, what, e)),
            Verdict::Panic(e) => panics.push((k, what, e)),
        }
    }
    report.notes.push(format!("fuzz: {n} damaged files: {read} read, {refused} refused with a read-error, {} traps, {} host panics", traps.len(), panics.len()));
    for (k, what, e) in panics.iter().take(10) {
        report.fail(CHECK, None, format!("case {k}, {what}: tmiki panicked ({e})"));
    }
    for (k, what, e) in traps.iter().take(10) {
        report.warn(CHECK, None, format!("case {k}, {what}: {e} (a damaged file should give a read-error)"));
    }
    if traps.len() > 10 {
        report.warn(CHECK, None, format!("{} more traps", traps.len() - 10));
    }
    if panics.is_empty() {
        report.ok(CHECK, None);
    }
}
