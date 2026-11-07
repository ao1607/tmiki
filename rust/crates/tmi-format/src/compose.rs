//! `composite::render(generic-v1)` (PLUGINS.md 2.2, P4): a structure and its
//! decoded layers handed to `tmi_render::generic`, the host's generic
//! compositor. Which layers it reads, and how a `Layer` becomes a
//! `generic::Node`.

use tmi_import::crop::LayerCrop;
use tmi_render::generic::{self, Mode, Node, Pixels};

use crate::types::*;

/// The `tmi-composite` text of a generic-v1 composite PNG
/// (`{"renderer":"generic/v1"}`, the shape of CLIP's `layered/v<N>`).
pub fn composite_text(c: Compositor) -> (String, String) {
    match c {
        Compositor::GenericV1 => ("tmi-composite".into(), format!("{{\"renderer\":\"{}\"}}", generic::RENDERER)),
    }
}

/// The render cache key (PLUGINS.md 6.2): a built-in's composite stays
/// keyed by the source digest alone (the key every record so far has);
/// a plugin's by `sha256(source_digest ‖ component_sha256 ‖ compositor)`,
/// so that a new version of the plugin does not reuse an old composite.
pub fn render_cache_key(digest: &str, component_sha256: Option<&str>, compositor: Option<Compositor>) -> String {
    match component_sha256 {
        None => digest.to_string(),
        Some(c) => {
            let name = match compositor {
                Some(Compositor::GenericV1) => "generic-v1",
                None => "",
            };
            tmi_store::oid::sha256_hex(format!("{digest}\0{c}\0{name}").as_bytes())
        }
    }
}

fn drawable(kind: LayerKind) -> bool {
    !matches!(kind, LayerKind::Mask | LayerKind::Stencil | LayerKind::Adjustment)
}

/// Which layers' pixels generic-v1 reads: visible (with every ancestor),
/// without children, drawable and with pixels.
pub fn reads_pixels(s: &Structure) -> Vec<bool> {
    let n = s.layers.len();
    let mut has_kids = vec![false; n];
    for l in &s.layers {
        if let Some(p) = l.parent {
            if let Some(h) = has_kids.get_mut(p as usize) {
                *h = true;
            }
        }
    }
    let mut shown = vec![false; n];
    for (i, l) in s.layers.iter().enumerate() {
        shown[i] = l.visible && l.parent.is_none_or(|p| (p as usize) < i && shown[p as usize]);
    }
    (0..n)
        .map(|i| {
            let l = &s.layers[i];
            shown[i] && !has_kids[i] && drawable(l.kind) && !matches!(l.pixels, LayerPixels::None)
        })
        .collect()
}

/// The generic-v1 composite of `s` over `background`: `crops[i]` holds layer
/// i's decoded pixels wherever `reads_pixels` says so (others are ignored).
pub fn render(s: &Structure, crops: &[Option<&LayerCrop>], background: [u8; 4]) -> Result<Vec<u8>, String> {
    let reads = reads_pixels(s);
    let nodes: Vec<Node> = s
        .layers
        .iter()
        .enumerate()
        .map(|(i, l)| Node {
            parent: l.parent.map(|p| p as usize),
            visible: l.visible,
            opacity: l.opacity,
            mode: Mode::from_name(l.blend.name()),
            clip: l.clip_to_below,
            drawable: drawable(l.kind),
            offset: (l.offset_x, l.offset_y),
            pixels: if reads[i] {
                crops.get(i).copied().flatten().and_then(|c| {
                    c.rect.map(|r| Pixels { x: r.0, y: r.1, w: r.2 - r.0, h: r.3 - r.1, rgba: &c.rgba })
                })
            } else {
                None
            },
        })
        .collect();
    generic::render(&nodes, s.canvas.width as usize, s.canvas.height as usize, background)
}
