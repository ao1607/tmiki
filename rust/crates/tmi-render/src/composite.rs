//! Batched layer compositing (`composite_ops`).
//!
//! `_composite_group_surface` in `tmi/formats/clip.py` blends a folder's
//! layers onto its surface one after another: Pillow's `alpha_composite` for
//! a normal layer at full opacity, the `blend_rgba` kernel otherwise. Every
//! step is a function of one pixel's own values, so a run of them can be
//! applied to each row of the surface independently: this module takes the
//! surface and the whole run at once and works through the rows on rayon,
//! without the GIL. The bytes are those of the one-at-a-time path:
//!
//! * `OVER` is Pillow's `ImagingAlphaComposite` (`tmi_pillow::over_pixel`);
//! * `BLEND` is `blend::blend_pixel_clip` (CSP's rounding; `blend_rgba`,
//!   which MDP2 composites with, runs `blend_pixel`);
//! * `CLEAR` stands for the surface growing (`_CompositeSurface.ensure`):
//!   Pillow re-composites the old surface onto a transparent one, which keeps
//!   every pixel with alpha > 0 and turns the others into (0, 0, 0, 0).

use rayon::prelude::*;
use tmi_pillow::over_pixel;

use crate::blend::{blend_pixel_clip, BlendMode};

pub const OP_OVER: u8 = 0;
pub const OP_BLEND: u8 = 1;
pub const OP_CLEAR: u8 = 2;

/// One step: the `w` x `h` rectangle at (`sx`, `sy`) of the RGBA `src`
/// (`src_w` pixels wide) lands at (`dx`, `dy`) of the target. `CLEAR` uses
/// only the target rectangle.
pub struct Op<'a> {
    pub kind: u8,
    pub src: &'a [u8],
    pub src_w: usize,
    pub sx: usize,
    pub sy: usize,
    pub w: usize,
    pub h: usize,
    pub dx: usize,
    pub dy: usize,
    pub mode: BlendMode,
    pub opacity: f32,
}

fn apply_rows(rows: &mut [u8], y0: usize, tw: usize, ops: &[Op<'_>]) {
    let stride = tw * 4;
    let nrows = rows.len() / stride;
    for op in ops {
        let a = op.dy.max(y0);
        let b = (op.dy + op.h).min(y0 + nrows);
        if a >= b || op.w == 0 {
            continue;
        }
        for y in a..b {
            let row = &mut rows[(y - y0) * stride + op.dx * 4..(y - y0) * stride + (op.dx + op.w) * 4];
            match op.kind {
                OP_CLEAR => {
                    for px in row.chunks_exact_mut(4) {
                        if px[3] == 0 {
                            px.copy_from_slice(&[0, 0, 0, 0]);
                        }
                    }
                }
                _ => {
                    let s0 = ((op.sy + y - op.dy) * op.src_w + op.sx) * 4;
                    let srow = &op.src[s0..s0 + op.w * 4];
                    if op.kind == OP_OVER {
                        for (d, s) in row.chunks_exact_mut(4).zip(srow.chunks_exact(4)) {
                            over_pixel(d, s);
                        }
                    } else {
                        for (d, s) in row.chunks_exact_mut(4).zip(srow.chunks_exact(4)) {
                            let base = [d[0], d[1], d[2], d[3]];
                            blend_pixel_clip(d, &base, s, op.mode, op.opacity);
                        }
                    }
                }
            }
        }
    }
}

/// Apply `ops` in order to the `width` x `height` RGBA surface `buf`, on the
/// rayon pool (rows are independent, so the result does not depend on the
/// number of threads). The op rectangles must lie inside `buf` and their
/// sources.
pub fn apply_ops(buf: &mut [u8], width: usize, height: usize, ops: &[Op<'_>]) {
    if width == 0 || height == 0 || ops.is_empty() {
        return;
    }
    let stride = width * 4;
    // Bands of rows: enough to spread over the pool, few enough that every
    // op's per-band bookkeeping stays negligible.
    let rows = (height / (4 * rayon::current_num_threads().max(1))).clamp(1, 64);
    buf.par_chunks_mut(stride * rows).enumerate().for_each(|(k, chunk)| {
        apply_rows(chunk, k * rows, width, ops);
    });
}
