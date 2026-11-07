//! A reader's answers as values, for comparing and for `--dump`. The values
//! are the RPC's (`tmi_plugin_host::devwire`: the inverses of what tmi reads
//! from a development plugin), so a wasm plugin's answers and a development
//! plugin's are compared in the one form PLUGINS.md 7.1 gives them.

use tmi_format::types::*;
use tmi_plugin_host::devwire;

pub use devwire::{bitmap_value as bitmap, layer_value as layer, read_error_value as read_error, structure_value as structure};

fn image_codec(c: ImageCodec) -> &'static str {
    devwire::image_codec_name(c)
}

/// One line per layer, for `tmiki plugin test --dump` (PLUGINS.md 7.3).
pub fn describe(s: &Structure) -> Vec<String> {
    let mut out = vec![format!("canvas {}x{}  format {:?}", s.canvas.width, s.canvas.height, s.format)];
    for l in &s.layers {
        let pixels = match &l.pixels {
            LayerPixels::None => "no pixels".to_string(),
            LayerPixels::Deferred(t) => format!("deferred({t})"),
            LayerPixels::Ready(b) => describe_bitmap(b),
        };
        out.push(format!(
            "layer {}  {}  {:?}  offset ({},{})  {}{}",
            l.stable_id,
            l.kind.name(),
            l.name,
            l.offset_x,
            l.offset_y,
            pixels,
            if l.visible { "" } else { "  hidden" }
        ));
    }
    out.push(match &s.composite {
        Composite::Render(_) => "composite  render generic-v1".to_string(),
        Composite::Image(b) => format!("composite  {}", describe_bitmap(b)),
        Composite::None => "composite  none".to_string(),
    });
    for w in &s.warnings {
        out.push(format!("warning {}: {}", w.code, w.message));
    }
    out
}

fn describe_bytes(b: &Bytes) -> String {
    match b {
        Bytes::Span(s) => format!("span(0x{:X}, {})", s.offset, s.len),
        Bytes::Spans(l) => format!("{} spans", l.len()),
        Bytes::Inline(d) => format!("inline {} bytes", d.len()),
    }
}

fn describe_bitmap(b: &Bitmap) -> String {
    match &b.source {
        PixelSource::Empty => "empty".into(),
        PixelSource::Image(p) => format!(
            "image {}  {}  pre [{}]",
            image_codec(p.image.codec),
            describe_bytes(&p.image.data),
            p.image.pre.iter().map(|c| format!("{:?}", c).to_lowercase()).collect::<Vec<_>>().join(", ")
        ),
        PixelSource::Tiles(t) => format!("{} tiles ({:?})", t.tiles.len(), t.overlap),
        PixelSource::Raw(r) => format!("raw {}x{}", r.width, r.height),
    }
}
