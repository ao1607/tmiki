//! The host's check of what a reader returned (PLUGINS.md 3.4). A violation
//! fails the import; nothing is repaired silently.

use std::collections::HashSet;

use crate::types::*;

/// Layers per file.
pub const MAX_LAYERS: usize = 100_000;
/// `context` bytes.
pub const MAX_CONTEXT: usize = 16 << 20;
/// All `inline` bytes of one structure.
pub const MAX_INLINE_TOTAL: usize = 256 << 20;

/// Keys the host writes itself: a reader may not set them.
pub fn reserved_key(key: &str) -> bool {
    matches!(key, "pixel_source" | "semantic_digest" | "semantic_digest_version" | "write_capability") || key.starts_with("staged_")
}

fn bad(msg: impl Into<String>) -> ReadError {
    ReadError::Corrupt(msg.into())
}

struct Walk {
    file_len: u64,
    inline: usize,
}

impl Walk {
    fn bytes(&mut self, b: &Bytes) -> Result<(), ReadError> {
        let span = |s: &Span| match s.offset.checked_add(s.len) {
            Some(e) if e <= self.file_len => Ok(()),
            _ => Err(bad(format!("span {}+{} is outside the {}-byte file", s.offset, s.len, self.file_len))),
        };
        match b {
            Bytes::Span(s) => span(s),
            Bytes::Spans(l) => l.iter().try_for_each(span),
            Bytes::Inline(v) => {
                if v.len() > crate::recipe::transform::MAX_INLINE {
                    return Err(bad("inline bytes over 64 MiB"));
                }
                self.inline += v.len();
                if self.inline > MAX_INLINE_TOTAL {
                    return Err(bad("inline bytes over 256 MiB in total"));
                }
                Ok(())
            }
        }
    }

    fn image(&mut self, i: &EncodedImage) -> Result<(), ReadError> {
        self.bytes(&i.data)
    }

    fn bitmap(&mut self, b: &Bitmap) -> Result<(), ReadError> {
        match &b.source {
            PixelSource::Empty => Ok(()),
            PixelSource::Image(p) => self.image(&p.image),
            PixelSource::Raw(r) => {
                if crate::recipe::raw_len(r.width, r.height) != Some(r.rgba.len()) {
                    return Err(bad(format!("raw pixels: {} bytes for {}x{}", r.rgba.len(), r.width, r.height)));
                }
                self.inline += r.rgba.len();
                if self.inline > MAX_INLINE_TOTAL {
                    return Err(bad("inline bytes over 256 MiB in total"));
                }
                Ok(())
            }
            PixelSource::Tiles(t) => t.tiles.iter().try_for_each(|t| match &t.content {
                TileContent::Raster(r) => self.bytes(&r.data),
                TileContent::Image(i) => self.image(i),
                TileContent::Fill(_) | TileContent::Empty => Ok(()),
            }),
        }
    }

    fn meta(&mut self, entries: &[MetaEntry]) -> Result<(), ReadError> {
        for e in entries {
            if reserved_key(&e.key) {
                return Err(bad(format!("the meta key {:?} is reserved", e.key)));
            }
            match &e.value {
                MetaValue::Number(n) if !n.is_finite() => return Err(bad(format!("meta {:?} is not finite", e.key))),
                MetaValue::Sha256Of(b) => self.bytes(b)?,
                _ => {}
            }
        }
        Ok(())
    }
}

/// Checks `s` against a file of `file_len` bytes.
pub fn check(s: &Structure, file_len: u64) -> Result<(), ReadError> {
    let mut w = Walk { file_len, inline: 0 };
    if s.layers.len() > MAX_LAYERS {
        return Err(bad(format!("{} layers (at most {MAX_LAYERS})", s.layers.len())));
    }
    if s.context.len() > MAX_CONTEXT {
        return Err(bad("context over 16 MiB"));
    }
    let mut ids = HashSet::new();
    for (i, l) in s.layers.iter().enumerate() {
        if !ids.insert(l.stable_id.as_str()) {
            return Err(bad(format!("stable-id {:?} is used twice", l.stable_id)));
        }
        if let Some(p) = l.parent.filter(|&p| p as usize >= i) {
            return Err(bad(format!("layer {i} names layer {p} as its parent, which is not before it")));
        }
        if !(l.opacity.is_finite() && (0.0..=1.0).contains(&l.opacity)) {
            return Err(bad(format!("layer {i} has the opacity {}", l.opacity)));
        }
        w.meta(&l.meta)?;
        w.meta(&l.semantic)?;
        if let LayerPixels::Ready(b) = &l.pixels {
            w.bitmap(b)?;
        }
    }
    w.meta(&s.file_meta)?;
    if let Composite::Image(b) = &s.composite {
        w.bitmap(b)?;
    }
    if let ColorProfile::Embedded(b) = &s.canvas.profile {
        w.bytes(b)?;
    }
    if s.canvas.dpi.is_some_and(|d| !d.is_finite()) {
        return Err(bad("the canvas dpi is not finite"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layer(id: &str, parent: Option<u32>) -> Layer {
        Layer {
            id: id.into(),
            stable_id: id.into(),
            parent,
            name: id.into(),
            kind: LayerKind::Raster,
            visible: true,
            opacity: 1.0,
            blend: BlendMode::Normal,
            clip_to_below: false,
            offset_x: 0,
            offset_y: 0,
            pixels: LayerPixels::None,
            meta: vec![],
            semantic: vec![],
            write: WriteCapability::None,
        }
    }

    fn structure(layers: Vec<Layer>) -> Structure {
        Structure {
            format: "t".into(),
            canvas: Canvas { width: 1, height: 1, dpi: None, profile: ColorProfile::None },
            layers,
            composite: Composite::None,
            file_meta: vec![],
            context: vec![],
            warnings: vec![],
        }
    }

    #[test]
    fn a_good_structure_passes() {
        check(&structure(vec![layer("a", None), layer("b", Some(0))]), 10).unwrap();
    }

    #[test]
    fn violations_fail() {
        assert!(check(&structure(vec![layer("a", None), layer("a", None)]), 10).is_err(), "duplicate stable-id");
        assert!(check(&structure(vec![layer("a", Some(0))]), 10).is_err(), "parent not before");
        assert!(check(&structure(vec![layer("a", None), layer("b", Some(5))]), 10).is_err(), "parent out of range");
        let mut l = layer("a", None);
        l.opacity = 1.5;
        assert!(check(&structure(vec![l]), 10).is_err(), "opacity");
        let mut l = layer("a", None);
        l.opacity = f64::NAN;
        assert!(check(&structure(vec![l]), 10).is_err(), "nan opacity");
        let mut l = layer("a", None);
        l.meta.push(MetaEntry { key: "pixel_source".into(), value: MetaValue::Null });
        assert!(check(&structure(vec![l]), 10).is_err(), "reserved key");
        let mut l = layer("a", None);
        l.meta.push(MetaEntry { key: "staged_x".into(), value: MetaValue::Null });
        assert!(check(&structure(vec![l]), 10).is_err(), "staged_ key");
        let mut l = layer("a", None);
        l.meta.push(MetaEntry { key: "x".into(), value: MetaValue::Number(f64::INFINITY) });
        assert!(check(&structure(vec![l]), 10).is_err(), "inf");
    }

    #[test]
    fn spans_must_be_inside_the_file() {
        let mut l = layer("a", None);
        let img = |s: Span| EncodedImage { data: Bytes::Span(s), pre: vec![], codec: ImageCodec::Png, frame: 0 };
        let bm = |s: Span| Bitmap { size: None, source: PixelSource::Image(PlacedImage { x: 0, y: 0, image: img(s) }) };
        l.pixels = LayerPixels::Ready(bm(Span { offset: 4, len: 6 }));
        check(&structure(vec![l.clone()]), 10).unwrap();
        l.pixels = LayerPixels::Ready(bm(Span { offset: 4, len: 7 }));
        assert!(check(&structure(vec![l.clone()]), 10).is_err());
        l.pixels = LayerPixels::Ready(bm(Span { offset: u64::MAX, len: 2 }));
        assert!(check(&structure(vec![l]), 10).is_err(), "overflow");
        let mut s = structure(vec![]);
        s.composite = Composite::Image(bm(Span { offset: 0, len: 11 }));
        assert!(check(&s, 10).is_err(), "composite span");
    }

    #[test]
    fn too_many_layers_or_context_fail() {
        let many: Vec<Layer> = (0..MAX_LAYERS + 1).map(|i| layer(&i.to_string(), None)).collect();
        assert!(check(&structure(many), 0).is_err());
        let mut s = structure(vec![]);
        s.context = vec![0; MAX_CONTEXT + 1];
        assert!(check(&s, 0).is_err());
    }
}
