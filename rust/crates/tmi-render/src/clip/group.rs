//! Sibling groups (`_composite_group_surface`, `_prerender_group`, the
//! clipping pass, `_BlendRun`, pass-through folders and correction layers;
//! clip.py:1749-2040, 2236-2309, 2482-2543, 3511-3715).

use tmi_clip::model::Node;
use tmi_pillow::chops::multiply;
use tmi_pillow::paste::paste_rgba_mask_l;

use super::mask::mask_is_enabled;
use super::report::{BandState, TraceRecord, Warning};
use super::{composite_name, edge, opacity, tone, Ctx, Res};
use crate::blend::BlendMode;
use crate::buffer::{intersect, size, union, Buf, Layer, Rect, Surface};
use crate::composite::{apply_ops, Op, OP_BLEND, OP_CLEAR, OP_OVER};
use crate::filter::{Decoded, Filter};

/// A node's layer effects (stage 1b).
struct Effects {
    tone: Option<tone::Tone>,
    edge: Option<edge::Edge>,
}

/// `bounds` grown by `margin` on every side.
fn dilate(r: Rect, margin: i64) -> Rect {
    [r[0] - margin, r[1] - margin, r[2] + margin, r[3] + margin]
}

/// `_clipping_base_index`: the nearest earlier sibling that is not clipped;
/// None when there is none or it is a correction layer (CSP then draws the
/// clipped layers unclipped, probe_clip_tonecurve).
fn clipping_base(ctx: &Ctx<'_>, layers: &[usize], start: usize) -> Option<usize> {
    (0..start).rev().find(|&j| !ctx.node(layers[j]).clip).filter(|&j| ctx.node(layers[j]).layer_type != Some(4098))
}

/// The mode a sibling blends in, warning about an unknown id as Python does.
fn mode_of(st: &mut BandState, node: &Node) -> &'static str {
    match composite_name(node.composite) {
        Some(m) => m,
        None => {
            st.warnings.push(Warning::Composite(node.composite));
            "normal"
        }
    }
}

/// `_fold_clipped`: the clipping group `base` with `source` blended onto its
/// colour as over an opaque backdrop; the base's alpha and rectangle stay.
fn fold_clipped(base: &Buf, source: &Buf, mode: BlendMode, opacity: f32) -> Buf {
    let mut group = base.clone();
    let Some(o) = intersect(source.bounds(), base.bounds()) else {
        return group;
    };
    let mut backdrop = [0u8; 4];
    let mut out = [0u8; 4];
    for y in o[1]..o[3] {
        let src = source.row(y, o[0], o[2]);
        let at = (((y - base.y) as usize) * base.w + (o[0] - base.x) as usize) * 4;
        let dst = &mut group.px[at..at + src.len()];
        for (d, s) in dst.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
            backdrop.copy_from_slice(d);
            backdrop[3] = 255;
            crate::blend::blend_pixel_clip(&mut out, &backdrop, s, mode, opacity);
            d[..3].copy_from_slice(&out[..3]);
        }
    }
    group
}

/// `_BlendRun`: consecutive blends applied in one pass over the surface.
struct Run {
    steps: Vec<(u8, usize, Rect, BlendMode, f32)>,
    extent: Option<Rect>,
}

impl Run {
    fn add(&mut self, target: &Surface, i: usize, buf: &Buf, mode: &str, opacity: f64) {
        if opacity <= 0.0 {
            return;
        }
        let over = (mode == "normal" || mode == "pass_through") && opacity >= 0.999;
        let Some(src) = intersect(buf.bounds(), target.canvas) else {
            return;
        };
        if self.steps.is_empty() {
            self.extent = target.bounds();
        }
        if !target.fixed {
            match self.extent {
                None => self.extent = Some(src),
                Some(e) => {
                    let grown = union(e, src);
                    if grown != e {
                        self.steps.push((OP_CLEAR, usize::MAX, e, BlendMode::Normal, 1.0));
                        self.extent = Some(grown);
                    }
                }
            }
        }
        let mode = BlendMode::from_name(mode).unwrap_or(BlendMode::Normal);
        self.steps.push((if over { OP_OVER } else { OP_BLEND }, i, src, mode, opacity as f32));
    }

    fn flush(&mut self, target: &mut Surface, rendered: &[Layer]) {
        let steps = std::mem::take(&mut self.steps);
        if !steps.iter().any(|s| s.0 != OP_CLEAR) {
            return;
        }
        let mut img = if target.fixed {
            target.img.take().unwrap()
        } else {
            let ext = self.extent.unwrap();
            match target.img.take() {
                None => Buf::filled(ext, [0, 0, 0, 0]),
                Some(b) if b.bounds() == ext => b,
                Some(b) => {
                    let mut n = Buf::filled(ext, [0, 0, 0, 0]);
                    n.paste(&b);
                    n
                }
            }
        };
        let (ox, oy) = (img.x, img.y);
        let ops: Vec<Op<'_>> = steps
            .iter()
            .map(|&(kind, i, r, mode, op)| {
                let (w, h) = size(r);
                let (src, src_w, sx, sy) = match &rendered.get(i) {
                    Some(Layer::Buf(b)) if kind != OP_CLEAR => (&b.px[..], b.w, (r[0] - b.x) as usize, (r[1] - b.y) as usize),
                    _ => (&[][..], 0, 0, 0),
                };
                Op { kind, src, src_w, sx, sy, w, h, dx: (r[0] - ox) as usize, dy: (r[1] - oy) as usize, mode, opacity: op }
            })
            .collect();
        let (w, h) = (img.w, img.h);
        apply_ops(&mut img.px, w, h, &ops);
        target.img = Some(img);
    }
}

impl Ctx<'_> {
    /// `_composite_group_surface`: composite `layers` (bottom first) onto
    /// `target`.
    pub fn composite_group(&self, st: &mut BandState, target: &mut Surface, layers: &[usize]) -> Res<()> {
        st.depth += 1;
        let r = self.group_body(st, target, layers);
        st.depth -= 1;
        r
    }

    /// Python pre-renders every sibling, clips them all, then composites
    /// them in turn, holding every sibling's pixels at once. Each step of a
    /// sibling depends only on itself and on earlier siblings (its clipping
    /// base), so this does the three steps sibling by sibling and lets go of
    /// a buffer once it is composited (unless it is a later sibling's
    /// clipping base): same pixels, a fraction of the memory. The warnings
    /// and trace records of each phase are collected apart and joined in
    /// Python's order.
    fn group_body(&self, st: &mut BandState, target: &mut Surface, layers: &[usize]) -> Res<()> {
        let n = layers.len();
        let window = target.canvas;
        let tracing = st.trace.is_some();
        let mut keep = vec![false; n];
        for j in 0..n {
            if self.node(layers[j]).clip {
                if let Some(b) = clipping_base(self, layers, j) {
                    keep[b] = true;
                }
            }
        }
        let mut rendered: Vec<Layer> = vec![Layer::None; n];
        // A clipping base waiting for its clipped layers before it is composited.
        let mut pending: Option<usize> = None;
        let outer_warn = std::mem::take(&mut st.warnings);
        let outer_trace = st.trace.take();
        let (mut pre_warn, mut comp_warn) = (Vec::new(), Vec::new());
        let (mut pre_nested, mut pre_rec, mut clip_rec, mut apply_rec) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        let record = |st: &BandState, out: &mut Vec<TraceRecord>, phase, main_id, buf: Option<&Buf>, empty: Option<&str>| {
            out.push(st.make_record(phase, main_id, window, buf, empty));
        };

        for i in 0..n {
            let node = self.node(layers[i]);
            // Pre-render (nested folders run their own groups here).
            st.warnings = std::mem::take(&mut pre_warn);
            if tracing {
                st.trace = Some(std::mem::take(&mut pre_nested));
            }
            let pre = self.prerender_one(st, target, node);
            pre_warn = std::mem::take(&mut st.warnings);
            if tracing {
                pre_nested = st.trace.take().unwrap();
            }
            let (layer, pass_through, mut adjust) = pre?;
            rendered[i] = layer;
            if tracing {
                match &rendered[i] {
                    Layer::None => {}
                    Layer::Empty => record(st, &mut pre_rec, "prerender", node.main_id, None, Some("empty")),
                    Layer::Buf(b) => record(st, &mut pre_rec, "prerender", node.main_id, Some(b), None),
                }
            }

            // Clipping: the base and the layers clipped to it are one group
            // (`_composite_group_body`); a clipped layer folds into its base,
            // which is composited once its last clipped layer is in.
            // (A clipped pass-through folder is drawn unclipped in its place,
            // as before.)
            let clipped_to = if node.clip && !pass_through { clipping_base(self, layers, i) } else { None };
            if let Some(base) = clipped_to {
                if !matches!(rendered[i], Layer::None) || adjust.is_some() {
                    if let Some(filter) = adjust.take() {
                        if let Layer::Buf(b) = &rendered[base] {
                            let g = self.fold_adjustment(node, &filter, b, target)?;
                            rendered[base] = Layer::Buf(g);
                        }
                    } else {
                        if let (Layer::Buf(b), Layer::Buf(s)) = (&rendered[base], &rendered[i]) {
                            let mode = composite_name(node.composite).and_then(BlendMode::from_name).unwrap_or(BlendMode::Normal);
                            let g = fold_clipped(b, s, mode, opacity(node)? as f32);
                            rendered[base] = Layer::Buf(g);
                        }
                        rendered[i] = Layer::None;
                    }
                    if tracing {
                        match &rendered[base] {
                            Layer::Buf(b) => record(st, &mut clip_rec, "clip", node.main_id, Some(b), None),
                            _ => record(st, &mut clip_rec, "clip", node.main_id, None, Some("empty")),
                        }
                    }
                }
                continue;
            }

            // Composite: first a base still waiting for its clipped layers.
            if let Some(j) = pending.take() {
                self.composite_step(st, target, j, None, false, &rendered, layers, &mut comp_warn, &mut apply_rec, tracing)?;
                rendered[j] = Layer::None;
            }
            if keep[i] && adjust.is_none() && !pass_through {
                pending = Some(i);
                continue;
            }
            self.composite_step(st, target, i, adjust.as_ref(), pass_through, &rendered, layers, &mut comp_warn, &mut apply_rec, tracing)?;
            if !keep[i] {
                rendered[i] = Layer::None;
            }
        }
        if let Some(j) = pending.take() {
            self.composite_step(st, target, j, None, false, &rendered, layers, &mut comp_warn, &mut apply_rec, tracing)?;
        }
        target.trim();
        st.warnings = outer_warn;
        st.warnings.extend(pre_warn);
        st.warnings.extend(comp_warn);
        st.trace = outer_trace.map(|mut t| {
            t.extend(pre_nested);
            t.extend(pre_rec);
            t.extend(clip_rec);
            t.extend(apply_rec);
            t
        });
        Ok(())
    }

    /// Sibling `i`'s step of the composite loop with its warnings and trace
    /// records collected into the composite phase's.
    #[allow(clippy::too_many_arguments)]
    fn composite_step(
        &self,
        st: &mut BandState,
        target: &mut Surface,
        i: usize,
        adjust: Option<&Filter>,
        pass_through: bool,
        rendered: &[Layer],
        layers: &[usize],
        comp_warn: &mut Vec<Warning>,
        apply_rec: &mut Vec<TraceRecord>,
        tracing: bool,
    ) -> Res<()> {
        let node = self.node(layers[i]);
        st.warnings = std::mem::take(comp_warn);
        if tracing {
            st.trace = Some(std::mem::take(apply_rec));
        }
        let applied = self.composite_one(st, target, node, adjust, pass_through, rendered, layers, i);
        *comp_warn = std::mem::take(&mut st.warnings);
        if tracing {
            *apply_rec = st.trace.take().unwrap();
        }
        if applied? && tracing {
            let rec = st.make_record("apply", node.main_id, target.canvas, target.img.as_ref(), None);
            apply_rec.push(rec);
        }
        Ok(())
    }

    /// `_fold_adjustment`: the clipping group `base` re-graded by the clipped
    /// correction layer `node` (its opacity and mask; the base's alpha stays).
    fn fold_adjustment(&self, node: &Node, filter: &Filter, base: &Buf, target: &Surface) -> Res<Buf> {
        let op = opacity(node)?;
        if op <= 0.0 {
            return Ok(base.clone());
        }
        let bounds = base.bounds();
        let (w, h) = size(bounds);
        let mask = self
            .layer_mask_crop(node, bounds, target.canvas)?
            .filter(|(r, _)| size(*r) == (w, h))
            .map(|(_, m)| m);
        let mut out = base.clone();
        regrade_into(&mut out.px, &base.px, w, filter, op as f32, mask.as_deref(), None);
        for (o, s) in out.px.chunks_exact_mut(4).zip(base.px.chunks_exact(4)) {
            o[3] = s[3];
        }
        Ok(out)
    }

    /// One sibling's step of the composite loop; whether it was applied
    /// (what the trace records an "apply" for).
    #[allow(clippy::too_many_arguments)]
    fn composite_one(
        &self,
        st: &mut BandState,
        target: &mut Surface,
        node: &Node,
        adjust: Option<&Filter>,
        pass_through: bool,
        rendered: &[Layer],
        layers: &[usize],
        i: usize,
    ) -> Res<bool> {
        if let Some(filter) = adjust {
            self.apply_adjustment(target, node, filter, rendered, layers, i)?;
            return Ok(true);
        }
        if pass_through {
            self.pass_through(st, target, node)?;
            return Ok(true);
        }
        let mode = mode_of(st, node);
        let Layer::Buf(b) = &rendered[i] else {
            return Ok(false);
        };
        let mut run = Run { steps: Vec::new(), extent: None };
        run.add(target, i, b, mode, opacity(node)?);
        run.flush(target, rendered);
        Ok(true)
    }

    /// `_prerender_group` for one sibling: (buffer, pass-through?, filter).
    fn prerender_one(&self, st: &mut BandState, target: &Surface, node: &Node) -> Res<(Layer, bool, Option<Filter>)> {
        let full = target.full;
        if !node.visible {
            return Ok((Layer::None, false, None));
        }
        match self.filter_of(node)? {
            Decoded::Filter(f) => return Ok((Layer::None, false, Some(f))),
            Decoded::Unsupported(k) => st.warnings.push(Warning::Filter(k)),
            Decoded::None => {}
        }
        let mode = mode_of(st, node);
        let fx = self.effects(node)?;
        // The window this node's own pixels are needed over: the target's,
        // grown by a フチ outline's reach (`_dilate_bounds`).
        let window = match &fx.edge {
            Some(e) => intersect(dilate(target.canvas, e.margin()), full).unwrap_or(target.canvas),
            None => target.canvas,
        };
        if node.folder {
            if mode == "pass_through" {
                return Ok((Layer::None, true, None));
            }
            if node.comic_frame {
                // A コマ枠 stores no pixels for its mask or border: both are
                // rebuilt from its outline (an empty frame still has a border).
                let mut group = Surface::empty(window, full);
                self.composite_group(st, &mut group, &node.children)?;
                return Ok((self.frame_folder(node, group.img, full, window)?, false, None));
            }
            let mut group = Surface::empty(window, full);
            self.composite_group(st, &mut group, &node.children)?;
            let layer = match group.img {
                None => Layer::Empty,
                Some(b) => self.apply_layer_mask(node, b, full, &fx)?,
            };
            return Ok((layer, false, None));
        }
        let mut buf = self.layer_buffer(node, full, window)?;
        if matches!(buf, Layer::None) {
            if let Some(s) = self.synthesize(node, window)? {
                buf = Layer::from_trimmed(s.trim());
            }
        }
        let layer = match buf {
            Layer::None => Layer::None,
            Layer::Empty => Layer::Empty,
            Layer::Buf(b) => self.apply_layer_mask(node, b, full, &fx)?,
        };
        Ok((layer, false, None))
    }

    /// The node's トーン and フチ (`parse_tone_effect`, `parse_edge_effect`).
    fn effects(&self, node: &Node) -> Res<Effects> {
        let st = self.doc.style(node.main_id);
        Ok(Effects {
            edge: edge::parse_edge_effect(&st, node.main_id)?,
            tone: tone::parse_tone_effect(&st)?,
        })
    }

    /// `_apply_layer_mask_to_buffer`: トーン, the layer mask, then フチ.
    fn apply_layer_mask(&self, node: &Node, buf: Buf, canvas_bounds: Rect, fx: &Effects) -> Res<Layer> {
        let mut buf = buf;
        if let Some(t) = &fx.tone {
            let screened = tone::apply_tone(&buf, t, self.doc.resolution).map_err(|e| format!("layer {}: {e}", node.main_id))?;
            match screened.trim() {
                Some(b) => buf = b,
                None => return Ok(Layer::Empty),
            }
        }
        if let Some((_, mask)) = self.layer_mask_crop(node, buf.bounds(), canvas_bounds)? {
            for (p, m) in buf.px.chunks_exact_mut(4).zip(mask) {
                p[3] = multiply(p[3], m);
            }
            match buf.trim() {
                Some(b) => buf = b,
                None => return Ok(Layer::Empty),
            }
        }
        match &fx.edge {
            Some(e) => Ok(Layer::Buf(edge::apply_edge(buf, e, canvas_bounds).map_err(|x| format!("layer {}: {x}", node.main_id))?)),
            None => Ok(Layer::Buf(buf)),
        }
    }

    /// `_composite_pass_through_folder` + `_mix_pass_through_result`.
    fn pass_through(&self, st: &mut BandState, target: &mut Surface, node: &Node) -> Res<()> {
        let op = opacity(node)?;
        let has_mask = mask_is_enabled(node);
        if op >= 0.999 && !has_mask {
            return self.composite_group(st, target, &node.children);
        }
        let before = target.img.clone();
        self.composite_group(st, target, &node.children)?;

        let before_bounds = before.as_ref().map(Buf::bounds);
        let after_bounds = target.bounds();
        let union_bounds = match (before_bounds, after_bounds) {
            (None, None) => return Ok(()),
            (Some(b), None) => b,
            (None, Some(a)) => a,
            (Some(b), Some(a)) => union(b, a),
        };
        let Some(u) = intersect(union_bounds, target.canvas) else {
            if !target.fixed {
                target.img = None;
            }
            return Ok(());
        };
        let mut mixed = match &before {
            Some(b) => b.onto_transparent(u),
            None => Buf::filled(u, [0, 0, 0, 0]),
        };
        let after = match &target.img {
            Some(a) => a.onto_transparent(u),
            None => Buf::filled(u, [0, 0, 0, 0]),
        };
        let (w, h) = size(u);
        let mut weight = vec![tmi_numpy::py_round(255.0 * op) as u8; w * h];
        if let Some((_, mask)) = self.layer_mask_crop(node, u, target.canvas)? {
            for (x, m) in weight.iter_mut().zip(mask) {
                *x = multiply(*x, m);
            }
        }
        paste_rgba_mask_l(&mut mixed.px, &after.px, &weight);
        if target.fixed {
            target.img.as_mut().unwrap().paste(&mixed);
        } else {
            target.img = mixed.trim();
        }
        Ok(())
    }

    /// `_apply_adjustment_to_surface` (accel path: `_native_regrade` for a
    /// per-channel filter, the numpy regrade otherwise; the same bytes).
    fn apply_adjustment(&self, target: &mut Surface, node: &Node, filter: &Filter, rendered: &[Layer], layers: &[usize], idx: usize) -> Res<()> {
        let Some(img_bounds) = target.bounds() else {
            return Ok(());
        };
        let op = opacity(node)?;
        if op <= 0.0 {
            return Ok(());
        }
        let (w, h) = size(img_bounds);
        let mask = self
            .layer_mask_crop(node, img_bounds, target.canvas)?
            .filter(|(r, _)| size(*r) == (w, h))
            .map(|(_, m)| m);
        let mut base_alpha = None;
        if node.clip {
            if let Some(b) = clipping_base(self, layers, idx) {
                let Layer::Buf(base) = &rendered[b] else {
                    return Ok(());
                };
                let Some(o) = intersect(base.bounds(), img_bounds) else {
                    return Ok(());
                };
                let mut plane = vec![0u8; w * h];
                for y in o[1]..o[3] {
                    let row = base.row(y, o[0], o[2]);
                    let at = ((y - img_bounds[1]) as usize) * w + (o[0] - img_bounds[0]) as usize;
                    for (k, p) in row.chunks_exact(4).enumerate() {
                        plane[at + k] = p[3];
                    }
                }
                base_alpha = Some(plane);
            }
        }
        let img = target.img.as_ref().unwrap();
        let mut out = vec![0u8; img.px.len()];
        regrade_into(&mut out, &img.px, w, filter, op as f32, mask.as_deref(), base_alpha.as_deref());
        target.img.as_mut().unwrap().px = out;
        Ok(())
    }
}

/// `_regrade_pixels`: `px` (RGBA, `w` wide) re-graded by `filter` into `out`.
fn regrade_into(out: &mut [u8], px: &[u8], w: usize, filter: &Filter, opacity: f32, mask: Option<&[u8]>, base_alpha: Option<&[u8]>) {
    match filter {
        Filter::Lut(lut) => crate::adjust::regrade(out, px, w, lut, opacity, mask, base_alpha),
        _ => {
            // Per distinct colour, then the same weighting as the kernel.
            let mut cache: std::collections::HashMap<[u8; 3], [f32; 3]> = std::collections::HashMap::new();
            let mut lut = Vec::with_capacity(px.len() / 4);
            for p in px.chunks_exact(4) {
                let key = [p[0], p[1], p[2]];
                lut.push(*cache.entry(key).or_insert_with(|| filter.colour(key)));
            }
            crate::adjust::regrade_with(out, px, w, &lut, opacity, mask, base_alpha);
        }
    }
}
