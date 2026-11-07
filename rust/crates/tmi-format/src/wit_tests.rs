//! `wit/tmi-format.wit` is valid WIT and the Rust types still mirror it.

use std::path::Path;

use wit_parser::{Resolve, TypeDefKind};

use crate::types::*;

fn resolve() -> (Resolve, wit_parser::PackageId) {
    let mut r = Resolve::default();
    let (pkg, _) = r.push_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("wit")).expect("the WIT parses");
    (r, pkg)
}

fn enum_cases(r: &Resolve, name: &str) -> Vec<String> {
    let t = r.types.iter().map(|(_, t)| t).find(|t| t.name.as_deref() == Some(name)).unwrap_or_else(|| panic!("no type {name}"));
    match &t.kind {
        TypeDefKind::Enum(e) => e.cases.iter().map(|c| c.name.clone()).collect(),
        TypeDefKind::Variant(v) => v.cases.iter().map(|c| c.name.clone()).collect(),
        k => panic!("{name} is {k:?}"),
    }
}

#[test]
fn the_wit_parses_and_has_both_worlds() {
    let (r, pkg) = resolve();
    assert_eq!(r.packages[pkg].name.to_string(), "tmi:format@0.1.0");
    for world in ["format-reader", "format-plugin"] {
        assert!(r.worlds.iter().any(|(_, w)| w.name == world), "world {world}");
    }
    for iface in ["types", "source", "codecs", "sqlite", "xml", "log", "reader", "edit", "writer"] {
        assert!(r.interfaces.iter().any(|(_, i)| i.name.as_deref() == Some(iface)), "interface {iface}");
    }
}

#[test]
fn enums_and_variants_have_the_cases_the_rust_types_have() {
    let (r, _) = resolve();
    let snake = |v: Vec<String>| v.into_iter().map(|s| s.replace('-', "_")).collect::<Vec<_>>();
    use BlendMode::*;
    let blends = [
        Normal, PassThrough, Multiply, Screen, Darken, Lighten, Difference, Subtract, LinearDodge, LinearBurn, ColorDodge, ColorBurn,
        Overlay, HardLight, SoftLight, Exclusion, Divide, DarkerColor, LighterColor, Hue, Saturation, Color, Luminosity, HardMix,
        LinearLight, VividLight, PinLight, GlowAdd, GlowDodge, Other,
    ];
    assert_eq!(snake(enum_cases(&r, "blend-mode")), blends.iter().map(|b| b.name().to_string()).collect::<Vec<_>>());
    use LayerKind::*;
    let kinds = [Raster, Folder, Text, Vector, Adjustment, Fill, Paper, Mask, Stencil, Unknown];
    assert_eq!(snake(enum_cases(&r, "layer-kind")), kinds.iter().map(|k| k.name().to_string()).collect::<Vec<_>>());
    // the counts of the others
    for (name, n) in [
        ("channel", 6),
        ("depth", 5),
        ("overlap", 3),
        ("compositor", 1),
        ("write-capability", 3),
        ("predictor-kind", 2),
        ("confidence", 3),
        ("codec", 9),
        ("pixel-source", 4),
        ("tile-content", 4),
        ("composite", 3),
        ("meta-value", 7),
        ("layer-pixels", 3),
        ("read-error", 4),
        ("color-profile", 3),
        ("bytes", 3),
        ("write-error", 3),
        ("prop-change", 5),
        ("piece", 3),
    ] {
        assert_eq!(enum_cases(&r, name).len(), n, "{name}");
    }
    // image-codec: the WIT's seven; Rust has one more, for the built-in PSD composite
    assert_eq!(enum_cases(&r, "image-codec"), ["png", "jpeg-v1", "webp-v1", "gif", "bmp", "tiff", "qoi"]);
    let _ = ImageCodec::PsdBuiltin;
}
