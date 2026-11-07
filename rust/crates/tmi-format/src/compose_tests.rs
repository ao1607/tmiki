//! generic-v1 pinned on hand-made OpenRaster-like documents (PLUGINS.md P4):
//! a stack of layers and nested stacks with `composite-op`, opacity,
//! visibility, offsets and `isolation`, written here as a tree (siblings
//! bottom to top, as the WIT `structure` lists them), with generated pixels.
//! The golden values are the SHA-256 of the composite's RGBA rows.

use tmi_import::crop::LayerCrop;
use tmi_store::oid::sha256_hex;

use crate::compose;
use crate::types::*;

/// An OpenRaster `layer` or `stack`.
#[derive(Clone)]
enum Ora {
    Layer { op: &'static str, opacity: f64, visible: bool, clip: bool, kind: LayerKind, x: i64, y: i64, w: usize, h: usize, seed: u32 },
    Stack { op: &'static str, opacity: f64, visible: bool, clip: bool, children: Vec<Ora> },
}

fn layer(op: &'static str, opacity: f64, x: i64, y: i64, w: usize, h: usize, seed: u32) -> Ora {
    Ora::Layer { op, opacity, visible: true, clip: false, kind: LayerKind::Raster, x, y, w, h, seed }
}

fn stack(op: &'static str, opacity: f64, children: Vec<Ora>) -> Ora {
    Ora::Stack { op, opacity, visible: true, clip: false, children }
}

fn clipped(mut o: Ora) -> Ora {
    match &mut o {
        Ora::Layer { clip, .. } | Ora::Stack { clip, .. } => *clip = true,
    }
    o
}

fn hidden(mut o: Ora) -> Ora {
    match &mut o {
        Ora::Layer { visible, .. } | Ora::Stack { visible, .. } => *visible = false,
    }
    o
}

fn of_kind(mut o: Ora, k: LayerKind) -> Ora {
    if let Ora::Layer { kind, .. } = &mut o {
        *kind = k;
    }
    o
}

/// Every value of every channel somewhere, alpha 0 and 255 included.
fn pixels(w: usize, h: usize, seed: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(w * h * 4);
    let mut s = seed.wrapping_mul(2654435761).wrapping_add(12345);
    for y in 0..h {
        for x in 0..w {
            s = s.wrapping_mul(1103515245).wrapping_add(12345);
            let n = (s >> 8) as usize;
            let a = match (x + y + seed as usize) % 7 {
                0 => 0,
                1 | 2 => 255,
                _ => (n >> 16) as u8,
            };
            out.extend_from_slice(&[(x * 255 / w.max(1)) as u8, (y * 255 / h.max(1)) as u8, n as u8, a]);
        }
    }
    out
}

fn blend(op: &str) -> BlendMode {
    use BlendMode::*;
    [
        Normal, PassThrough, Multiply, Screen, Darken, Lighten, Difference, Subtract, LinearDodge, LinearBurn, ColorDodge,
        ColorBurn, Overlay, HardLight, SoftLight, Exclusion, Divide, DarkerColor, LighterColor, Hue, Saturation, Color,
        Luminosity, HardMix, LinearLight, VividLight, PinLight, GlowAdd, GlowDodge, Other,
    ]
    .into_iter()
    .find(|m| m.name() == op)
    .unwrap_or_else(|| panic!("no blend mode {op}"))
}

/// The structure (and each layer's decoded pixels) of a document.
fn flatten(doc: &[Ora], w: u32, h: u32) -> (Structure, Vec<Option<LayerCrop>>) {
    fn walk(o: &Ora, parent: Option<u32>, layers: &mut Vec<Layer>, crops: &mut Vec<Option<LayerCrop>>) {
        let i = layers.len() as u32;
        let base = |op: &str, opacity: f64, visible: bool, clip: bool, kind: LayerKind| Layer {
            id: format!("l{i}"),
            stable_id: format!("l{i}"),
            parent,
            name: format!("l{i}"),
            kind,
            visible,
            opacity,
            blend: blend(op),
            clip_to_below: clip,
            offset_x: 0,
            offset_y: 0,
            pixels: LayerPixels::None,
            meta: vec![],
            semantic: vec![],
            write: WriteCapability::None,
        };
        match o {
            Ora::Layer { op, opacity, visible, clip, kind, x, y, w, h, seed } => {
                let mut l = base(op, *opacity, *visible, *clip, *kind);
                (l.offset_x, l.offset_y) = (*x, *y);
                l.pixels = LayerPixels::Ready(Bitmap { size: Some((*w as u32, *h as u32)), source: PixelSource::Empty });
                layers.push(l);
                crops.push(Some(LayerCrop::from_full(&pixels(*w, *h, *seed), *w, *h)));
            }
            Ora::Stack { op, opacity, visible, clip, children } => {
                layers.push(base(op, *opacity, *visible, *clip, LayerKind::Folder));
                crops.push(None);
                for c in children {
                    walk(c, Some(i), layers, crops);
                }
            }
        }
    }
    let (mut layers, mut crops) = (Vec::new(), Vec::new());
    for o in doc {
        walk(o, None, &mut layers, &mut crops);
    }
    let s = Structure {
        format: "ora-like".into(),
        canvas: Canvas { width: w, height: h, dpi: None, profile: ColorProfile::None },
        layers,
        composite: Composite::Render(Compositor::GenericV1),
        file_meta: vec![],
        context: vec![],
        warnings: vec![],
    };
    (s, crops)
}

fn render(doc: &[Ora], w: u32, h: u32) -> Vec<u8> {
    let (s, crops) = flatten(doc, w, h);
    crate::validate::check(&s, 0).unwrap();
    let refs: Vec<Option<&LayerCrop>> = crops.iter().map(|c| c.as_ref()).collect();
    let a = compose::render(&s, &refs, [255, 255, 255, 255]).unwrap();
    assert_eq!(a.len(), (w * h * 4) as usize);
    a
}

const MODES: [&str; 30] = [
    "normal", "pass_through", "multiply", "screen", "darken", "lighten", "difference", "subtract", "linear_dodge",
    "linear_burn", "color_dodge", "color_burn", "overlay", "hard_light", "soft_light", "exclusion", "divide",
    "darker_color", "lighter_color", "hue", "saturation", "color", "luminosity", "hard_mix", "linear_light",
    "vivid_light", "pin_light", "glow_add", "glow_dodge", "other",
];

/// A mode over an opaque backdrop (the root) and over a half-transparent
/// one (inside an isolated stack), at full and at 0.75 opacity.
fn mode_doc(op: &'static str) -> Vec<Ora> {
    vec![
        layer("normal", 1.0, 0, 0, 40, 40, 1),
        stack("normal", 1.0, vec![layer("normal", 0.6, 30, 0, 34, 40, 2), layer(op, 0.75, 26, 4, 30, 30, 3)]),
        layer(op, 1.0, -6, 20, 40, 30, 4),
    ]
}

fn check_golden(got: &[(String, String)], want: &[(&str, &str)]) {
    let report: String = got.iter().map(|(k, v)| format!("        (\"{k}\", \"{v}\"),\n")).collect();
    assert_eq!(got.len(), want.len(), "golden table:\n{report}");
    for ((k, v), (wk, wv)) in got.iter().zip(want) {
        assert_eq!((k.as_str(), v.as_str()), (*wk, *wv), "golden table:\n{report}");
    }
}

#[test]
fn every_blend_mode_is_pinned() {
    let got: Vec<(String, String)> = MODES.iter().map(|op| (op.to_string(), sha256_hex(&render(&mode_doc(op), 64, 48)))).collect();
    check_golden(&got, GOLDEN_MODES);
}

#[test]
fn glow_other_and_pass_through_leaves_are_other_modes() {
    let r = |op| render(&mode_doc(op), 64, 48);
    assert_eq!(r("glow_add"), r("linear_dodge"));
    assert_eq!(r("glow_dodge"), r("color_dodge"));
    assert_eq!(r("other"), r("normal"));
    assert_eq!(r("pass_through"), r("normal"), "a layer without children");
    // and the modes are not all the same
    let distinct: std::collections::HashSet<Vec<u8>> = MODES.iter().map(|op| r(op)).collect();
    assert_eq!(distinct.len(), MODES.len() - 4);
}

/// Stacks, pass-through, clipping, visibility, masks and offsets.
fn structure_doc() -> Vec<Ora> {
    vec![
        layer("normal", 1.0, 0, 0, 80, 60, 10),
        // an isolated multiply stack at 0.6 with a clipping chain inside
        stack(
            "multiply",
            0.6,
            vec![
                layer("normal", 1.0, 4, 4, 40, 30, 11),
                clipped(layer("screen", 0.8, 10, 0, 40, 40, 12)),
                clipped(layer("normal", 1.0, 0, 20, 30, 30, 13)),
                layer("overlay", 0.5, 30, 10, 20, 20, 14),
            ],
        ),
        // pass-through: the children see the canvas
        stack("pass_through", 1.0, vec![layer("difference", 1.0, 40, 20, 30, 30, 15), clipped(layer("hue", 1.0, 45, 25, 30, 30, 16))]),
        // a clipping stack, cut by the layer under it
        layer("normal", 0.9, 50, 0, 30, 20, 17),
        clipped(stack("normal", 1.0, vec![layer("color_burn", 1.0, 40, 0, 40, 40, 18)])),
        // nothing of these is drawn
        of_kind(layer("normal", 1.0, 0, 0, 80, 60, 19), LayerKind::Mask),
        of_kind(layer("normal", 1.0, 0, 0, 80, 60, 20), LayerKind::Stencil),
        of_kind(layer("normal", 1.0, 0, 0, 80, 60, 21), LayerKind::Adjustment),
        hidden(stack("normal", 1.0, vec![layer("normal", 1.0, 0, 0, 80, 60, 22)])),
        // far and partly outside the canvas
        layer("normal", 1.0, -30, -20, 40, 30, 23),
        layer("normal", 1.0, 1 << 41, 0, 10, 10, 24),
        layer("lighten", 0.4, 70, 50, 30, 30, 25),
    ]
}

#[test]
fn stacks_clipping_and_visibility_are_pinned() {
    let got = vec![("structure".to_string(), sha256_hex(&render(&structure_doc(), 80, 60)))];
    check_golden(&got, GOLDEN_STRUCTURE);
}

#[test]
fn what_is_not_drawn_changes_nothing() {
    let doc = structure_doc();
    let want = render(&doc, 80, 60);
    let without: Vec<Ora> = doc[..5].to_vec().into_iter().chain(doc[9..].to_vec()).collect();
    assert_eq!(render(&without, 80, 60), want, "masks, stencils, adjustments, hidden stacks");
    // a pass-through stack's own clipping flag does nothing (its opacity
    // fades it: `pass_through_opacity_*` below)
    let mut p = doc.clone();
    if let Ora::Stack { clip, .. } = &mut p[2] {
        *clip = true;
    }
    assert_eq!(render(&p, 80, 60), want);
    // a layer clipped to a hidden base, to a pass-through stack, or with no
    // base at all, is not drawn
    for (base, at) in [(hidden(layer("normal", 1.0, 0, 0, 80, 60, 30)), 1), (stack("pass_through", 1.0, vec![layer("normal", 1.0, 0, 0, 9, 9, 31)]), 1)] {
        let mut d = doc.clone();
        d.insert(at, base.clone());
        d.insert(at + 1, clipped(layer("normal", 1.0, 0, 0, 80, 60, 32)));
        let mut e = doc.clone();
        e.insert(at, base);
        assert_eq!(render(&d, 80, 60), render(&e, 80, 60));
    }
    let lone = vec![clipped(layer("normal", 1.0, 0, 0, 8, 8, 33))];
    assert_eq!(render(&lone, 8, 8), vec![255; 8 * 8 * 4]);
}

/// A pass-through stack at `outer` on top of the canvas, with clipped
/// layers inside and, as its last child, a pass-through stack at `inner`
/// (with its own clipped layer). `outer_shown` / `inner_shown` false hide them.
fn through_doc(outer: f64, inner: f64, outer_shown: bool, inner_shown: bool) -> Vec<Ora> {
    let mut inner_stack = stack("pass_through", inner, vec![layer("overlay", 1.0, 20, 0, 30, 40, 45), clipped(layer("hue", 1.0, 15, 5, 30, 30, 46))]);
    if !inner_shown {
        inner_stack = hidden(inner_stack);
    }
    let mut outer_stack = stack(
        "pass_through",
        outer,
        vec![
            layer("screen", 1.0, 5, 5, 40, 30, 42),
            clipped(layer("difference", 0.8, 0, 10, 50, 30, 43)),
            layer("normal", 0.6, 30, 20, 40, 30, 44),
            inner_stack,
        ],
    );
    if !outer_shown {
        outer_stack = hidden(outer_stack);
    }
    vec![layer("normal", 1.0, 0, 0, 64, 48, 40), layer("multiply", 0.7, 10, 5, 40, 30, 41), outer_stack]
}

/// The same, with both stacks' children drawn straight on the canvas.
fn through_doc_inlined() -> Vec<Ora> {
    vec![
        layer("normal", 1.0, 0, 0, 64, 48, 40),
        layer("multiply", 0.7, 10, 5, 40, 30, 41),
        layer("screen", 1.0, 5, 5, 40, 30, 42),
        clipped(layer("difference", 0.8, 0, 10, 50, 30, 43)),
        layer("normal", 0.6, 30, 20, 40, 30, 44),
        layer("overlay", 1.0, 20, 0, 30, 40, 45),
        clipped(layer("hue", 1.0, 15, 5, 30, 30, 46)),
    ]
}

/// A pass-through stack at 0.5 inside an isolated stack (a transparent backdrop).
fn through_isolated_doc() -> Vec<Ora> {
    vec![
        layer("normal", 1.0, 0, 0, 64, 48, 50),
        stack(
            "multiply",
            0.9,
            vec![
                layer("normal", 0.6, 0, 0, 40, 40, 51),
                stack("pass_through", 0.5, vec![layer("screen", 1.0, 20, 10, 40, 30, 52), clipped(layer("normal", 1.0, 25, 0, 30, 48, 53))]),
            ],
        ),
    ]
}

/// The f64 reference of the fade, mixed premultiplied: alpha
/// `floor(Ba + (Ra - Ba) * o)`, colour `floor((Bc Ba (1 - o) + Rc Ra o) /
/// (Ba (1 - o) + Ra o))` (the straight lerp where both weights are 0), `o = o8 / 255`.
fn faded(b: &[u8], r: &[u8], o8: u32) -> Vec<u8> {
    let o = o8 as f64 / 255.0;
    let fl = |v: f64| (v + 1e-9).floor() as u8;
    let mut out = Vec::with_capacity(b.len());
    for (b, r) in b.chunks_exact(4).zip(r.chunks_exact(4)) {
        let (wb, wr) = (b[3] as f64 * (1.0 - o), r[3] as f64 * o);
        for c in 0..3 {
            let (bc, rc) = (b[c] as f64, r[c] as f64);
            out.push(fl(if wb + wr == 0.0 { bc + (rc - bc) * o } else { (bc * wb + rc * wr) / (wb + wr) }));
        }
        out.push(fl(wb + wr));
    }
    out
}

/// The straight per-channel lerp `floor(B + (R - B) * o)`.
fn faded_straight(b: &[u8], r: &[u8], o8: u32) -> Vec<u8> {
    b.iter().zip(r).map(|(&b, &r)| (b as f64 + (r as f64 - b as f64) * o8 as f64 / 255.0 + 1e-9).floor() as u8).collect()
}

#[test]
fn the_fade_mixes_premultiplied() {
    use tmi_render::generic::fade_pixel;
    let f = |b: [u8; 4], r: [u8; 4], o8: u8| {
        let mut d = b;
        fade_pixel(&mut d, &r, o8);
        d
    };
    // red over transparent at 50 %: red at half alpha, not dark red
    assert_eq!(f([0, 0, 0, 0], [255, 0, 0, 255], 128), [255, 0, 0, 128]);
    assert_eq!(f([0, 0, 255, 0], [255, 0, 0, 255], 1), [255, 0, 0, 1]);
    let vals: Vec<u8> = (0..=255).step_by(17).chain([1, 128, 254]).collect();
    for &ba in &vals {
        for &ra in &vals {
            for &(bc, rc) in &[(0u8, 255u8), (255, 0), (10, 200), (77, 77), (254, 1)] {
                let (b, r) = ([bc, rc, 128, ba], [rc, bc, 3, ra]);
                // exact at the ends
                assert_eq!(f(b, r, 0), b);
                assert_eq!(f(b, r, 255), r);
                let mut prev = b;
                for o8 in 0..=255u8 {
                    let got = f(b, r, o8);
                    assert_eq!(got.to_vec(), faded(&b, &r, o8 as u32), "{b:?} {r:?} o8 {o8}");
                    if ba == 255 && ra == 255 {
                        assert_eq!(got.to_vec(), faded_straight(&b, &r, o8 as u32), "opaque: the straight lerp");
                    }
                    // monotone, alpha and colour, between B and R
                    for c in 0..4 {
                        let (g, p, e) = (got[c] as i32, prev[c] as i32, r[c] as i32);
                        assert!((g - p) * (e - p) >= 0 && (g - p).abs() <= (e - p).abs(), "{b:?} {r:?} o8 {o8} channel {c}");
                    }
                    prev = got;
                }
            }
        }
    }
}

#[test]
fn pass_through_opacity_is_pinned() {
    let (w, h) = (64, 48);
    let got = vec![
        ("o0".to_string(), sha256_hex(&render(&through_doc(0.0, 1.0, true, true), w, h))),
        ("o128".to_string(), sha256_hex(&render(&through_doc(128.0 / 255.0, 1.0, true, true), w, h))),
        ("o255".to_string(), sha256_hex(&render(&through_doc(1.0, 1.0, true, true), w, h))),
        ("nested".to_string(), sha256_hex(&render(&through_doc(0.5, 0.25, true, true), w, h))),
        ("isolated".to_string(), sha256_hex(&render(&through_isolated_doc(), w, h))),
    ];
    check_golden(&got, GOLDEN_THROUGH);
}

#[test]
fn pass_through_opacity_fades_toward_the_backdrop() {
    let (w, h) = (64, 48);
    let r = |d: Vec<Ora>| render(&d, w, h);
    let full = r(through_doc(1.0, 1.0, true, true));
    let backdrop = r(through_doc(1.0, 1.0, false, true));
    // o = 255 is the old pass-through (children straight on the canvas),
    // o = 0 is the backdrop
    assert_eq!(full, r(through_doc_inlined()));
    assert_eq!(r(through_doc(0.0, 1.0, true, true)), backdrop);
    assert_eq!(r(through_doc(0.0, 0.25, true, true)), backdrop);
    // every o8 against the f64 reference, and monotone between B and R
    let mut prev = backdrop.clone();
    for o8 in 0..=255u32 {
        let got = r(through_doc(o8 as f64 / 255.0, 1.0, true, true));
        assert_eq!(got, faded(&backdrop, &full, o8), "o8 {o8}");
        // the root is opaque, so this is the straight lerp too
        assert_eq!(got, faded_straight(&backdrop, &full, o8), "o8 {o8}");
        for ((&g, &p), &f) in got.iter().zip(&prev).zip(&full) {
            assert!((g as i32 - p as i32) * (f as i32 - p as i32) >= 0 && (g as i32 - p as i32).abs() <= (f as i32 - p as i32).abs(), "o8 {o8}: not monotone");
        }
        prev = got;
    }
    assert_ne!(full, backdrop);
    // nested: the inner stack fades toward what the outer one drew under it,
    // and the outer one fades that toward the canvas
    let under_inner = r(through_doc(1.0, 1.0, true, false));
    let inner = faded(&under_inner, &full, 64);
    assert_eq!(r(through_doc(1.0, 0.25, true, true)), inner);
    assert_eq!(r(through_doc(0.5, 0.25, true, true)), faded(&backdrop, &inner, 128));
}

#[test]
fn opacity_is_rounded_to_eight_bits() {
    use tmi_render::generic::opacity8;
    assert_eq!((opacity8(0.0), opacity8(1.0), opacity8(0.5), opacity8(127.0 / 255.0), opacity8(0.501_960_7)), (0, 255, 128, 127, 128));
    for a in 0..=255u32 {
        assert_eq!(opacity8(a as f64 / 255.0) as u32, a);
    }
}

#[test]
fn trees_too_deep_or_out_of_order_fail() {
    let mut nested = layer("normal", 1.0, 0, 0, 1, 1, 0);
    for _ in 0..tmi_render::generic::MAX_DEPTH {
        nested = stack("normal", 1.0, vec![nested]);
    }
    let (s, crops) = flatten(&[nested.clone()], 1, 1);
    let refs: Vec<Option<&LayerCrop>> = crops.iter().map(|c| c.as_ref()).collect();
    assert!(compose::render(&s, &refs, [255; 4]).is_ok(), "{} levels", tmi_render::generic::MAX_DEPTH);
    let (s, crops) = flatten(&[stack("normal", 1.0, vec![nested])], 1, 1);
    let refs: Vec<Option<&LayerCrop>> = crops.iter().map(|c| c.as_ref()).collect();
    assert!(compose::render(&s, &refs, [255; 4]).is_err());
}

const GOLDEN_MODES: &[(&str, &str)] = &[
    ("normal", "7e7819991374b7eeaaafd17139ed7fadaea9f73d1ce59261056930a7eb422edd"),
    ("pass_through", "7e7819991374b7eeaaafd17139ed7fadaea9f73d1ce59261056930a7eb422edd"),
    ("multiply", "9618d25e152a55e0470f6829886603a9851226108a27fdc3160de3549311a6e6"),
    ("screen", "1654785bfed9bd691e8b9595cd807e11911e5faae51098ea7ccc15fb63662d69"),
    ("darken", "5e359163511ebe17e58c96c478313177893b0254e761a3d26764de13f8e9d3cf"),
    ("lighten", "d425c9af4f87affe689e2bf024b7a29902ea3b34b64b1b9e46029e486e6e5322"),
    ("difference", "5d229f25ceb819720a95cb4bea1ed00530a06d43ed00ad1aede8c52b0001e95c"),
    ("subtract", "501d9ec75234d3f8189de2f78cbdd815b04fcb8bc174cb3ab04f88a350e49778"),
    ("linear_dodge", "9a1c95f53f1f51b2705cd290782e39d2419de7f278b2ec2709e0d809bfca9bf2"),
    ("linear_burn", "c2b2c19fd103c4be9800baef3a38277f01bc95ceca757283e145165a0a31c987"),
    ("color_dodge", "5c4e692f0db23db423a891776e7956209a22f545f0b483f985aeb370474adca6"),
    ("color_burn", "235da7610fdfa842df61825d2d93171c456c6907b0d254ab58bac9478b4ab635"),
    ("overlay", "571403356c3fb5af934768db9f4be934d6cbc53791f1631edaa21a95683eee51"),
    ("hard_light", "8e4172fc4032e57d7e804bd7bd3fb9e4c5e27d23e5d9a223b404496350aa85c3"),
    ("soft_light", "50e4076b9eba4ce40d9177cf8ea22fdcce1ed01d043f1e24294d776ddaeb2349"),
    ("exclusion", "d3f9bfa0cf1a8e99f24734c7fb7695bf784afd41e5ed8a2559b320cc01dc5e54"),
    ("divide", "4ef9fdb7d28e6afb5bc3063f6b18fb0226dc38933e34d6c0f1380d58e17c7755"),
    ("darker_color", "431de8d41d91dd0aa0588cec30ffadb54380e805c18548ad0378f491a4aa03de"),
    ("lighter_color", "dd9ccaad67753addc5f6025704071fd2d1bc764c9dd9c61131f4ad0cf7c7c5d7"),
    ("hue", "21254135c3efb7dd9997fb9a011d923e07745dceb21a5195a62adb8554b25c5d"),
    ("saturation", "88dd7f46646bd4b6f2bad7924464af3d4f3c70dc823d9b4be8154761b87e8c30"),
    ("color", "12c5676a5664b73fadb0f67f322da27cb237aa0131498edcd1c6de019d43206c"),
    ("luminosity", "0807db5a522af5b6f14595fe6fa4b126755399e75e4fd7c95f1f29491e7a1f6a"),
    ("hard_mix", "0f75242329d8687c6919bf28e22b75d57c4bc3c5a42adf7502c7c67681c6a443"),
    ("linear_light", "d4d651c9fa69c604e3e54b1dd40d5ae70d56713dfc9bfb1bc903322c931ba9f3"),
    ("vivid_light", "eeb226deb2f9871a9a42b50f719d3591e2dfbc0d52a7c97a5315fbe36859ac4a"),
    ("pin_light", "db19c78ae848ff4feebd78b26ae45fd71abace7d472c29042f3602a28c3d9301"),
    ("glow_add", "9a1c95f53f1f51b2705cd290782e39d2419de7f278b2ec2709e0d809bfca9bf2"),
    ("glow_dodge", "5c4e692f0db23db423a891776e7956209a22f545f0b483f985aeb370474adca6"),
    ("other", "7e7819991374b7eeaaafd17139ed7fadaea9f73d1ce59261056930a7eb422edd"),
];
const GOLDEN_STRUCTURE: &[(&str, &str)] = &[("structure", "d3f3307064864beba1330edae6ad2592fd8d08d718a390808cfd982b2c5ad1fc")];
const GOLDEN_THROUGH: &[(&str, &str)] = &[
    ("o0", "35151c79e0f4b16672ebd2cfa7c1c7eb4c2bcdb11544c0f28641b11a6e061aa0"),
    ("o128", "dc0adc7b2b1d58656ca65d7ace047bd7da55779ed105b6e7e2f47899e01e0580"),
    ("o255", "d3924a5c90c996409dbcb06236dc2850d7d496292fa46b76859c51dc03d8243e"),
    ("nested", "d9d2b0c10c9816f16b4ccd41a5d1516416ab2f6929c4c7b61dd2765265665f2f"),
    ("isolated", "a435bf63e6e433065865fa22a99b57cdb1e1278a295dead85dbc3d4dca95c47e"),
];