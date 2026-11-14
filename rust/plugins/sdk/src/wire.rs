//! The WIT's types (wit-bindgen's) to and from the CBOR of `tmiki plugin dev`
//! (crate `tmi-plugin-dev`'s doc), on the plugin's side; tmiki's side is
//! `tmi-plugin-host/src/devwire.rs`.

use tmi_plugin_dev::{Value, ValueError};

use crate::*;

type R<T> = Result<T, ValueError>;

fn t(s: &str) -> Value {
    Value::text(s)
}

fn no_case<T>(what: &str, ty: &str, tag: &str) -> R<T> {
    Err(ValueError(format!("{what}: {ty} has no case {tag:?}")))
}

// -- plugin to tmiki ---------------------------------------------------------------

fn span_value(s: &Span) -> Value {
    Value::record([("offset", Value::Uint(s.offset)), ("len", Value::Uint(s.len))])
}

pub fn bytes_value(b: &Bytes) -> Value {
    match b {
        Bytes::Span(s) => Value::case("span", span_value(s)),
        Bytes::Spans(l) => Value::case("spans", Value::list(l, span_value)),
        Bytes::Inline(v) => Value::case("inline", Value::bytes(v.clone())),
    }
}

pub fn codec_value(c: &Codec) -> Value {
    match c {
        Codec::Zlib => Value::unit("zlib"),
        Codec::Deflate => Value::unit("deflate"),
        Codec::Gzip => Value::unit("gzip"),
        Codec::Zstd => Value::unit("zstd"),
        Codec::Lz4Block(n) => Value::case("lz4-block", Value::uint(*n)),
        Codec::Lzf(n) => Value::case("lzf", Value::uint(*n)),
        Codec::Packbits(n) => Value::case("packbits", Value::uint(*n)),
        Codec::Predictor(p) => Value::case(
            "predictor",
            Value::record([
                (
                    "kind",
                    t(match p.kind {
                        PredictorKind::PngRows => "png-rows",
                        PredictorKind::TiffHorizontal => "tiff-horizontal",
                    }),
                ),
                ("row-bytes", Value::uint(p.row_bytes)),
                ("bytes-per-pixel", Value::uint(p.bytes_per_pixel)),
            ]),
        ),
        Codec::Plugin(n) => Value::case("plugin", Value::uint(*n)),
    }
}

fn image_codec_value(c: ImageCodec) -> Value {
    t(match c {
        ImageCodec::Png => "png",
        ImageCodec::JpegV1 => "jpeg-v1",
        ImageCodec::WebpV1 => "webp-v1",
        ImageCodec::Gif => "gif",
        ImageCodec::Bmp => "bmp",
        ImageCodec::Tiff => "tiff",
        ImageCodec::Qoi => "qoi",
    })
}

pub fn encoded_image_value(i: &EncodedImage) -> Value {
    Value::record([
        ("data", bytes_value(&i.data)),
        ("pre", Value::list(&i.pre, codec_value)),
        ("codec", image_codec_value(i.codec)),
        ("frame", Value::uint(i.frame)),
    ])
}

fn rgba8_value(c: &Rgba8) -> Value {
    Value::record([("r", Value::uint(c.r)), ("g", Value::uint(c.g)), ("b", Value::uint(c.b)), ("a", Value::uint(c.a))])
}

pub fn layout_value(l: &Layout) -> Value {
    let channel = |c: &Channel| {
        t(match c {
            Channel::R => "r",
            Channel::G => "g",
            Channel::B => "b",
            Channel::A => "a",
            Channel::Gray => "gray",
            Channel::Pad => "pad",
        })
    };
    let depth = |d: Depth| {
        t(match d {
            Depth::OneBitMsb => "one-bit-msb",
            Depth::Eight => "eight",
            Depth::SixteenBe => "sixteen-be",
            Depth::SixteenLe => "sixteen-le",
            Depth::Float32Le => "float32-le",
        })
    };
    Value::record([
        ("groups", Value::list(&l.groups, |g| Value::record([("channels", Value::list(&g.channels, channel)), ("depth", depth(g.depth))]))),
        ("row-padding", Value::uint(l.row_padding)),
        ("tint", Value::option(l.tint.as_ref().map(rgba8_value))),
        ("premultiplied", Value::Bool(l.premultiplied)),
    ])
}

fn tile_value(x: &Tile) -> Value {
    let content = match &x.content {
        TileContent::Raster(r) => Value::case(
            "raster",
            Value::record([("data", bytes_value(&r.data)), ("codecs", Value::list(&r.codecs, codec_value)), ("layout", layout_value(&r.layout))]),
        ),
        TileContent::Image(i) => Value::case("image", encoded_image_value(i)),
        TileContent::Fill(c) => Value::case("fill", rgba8_value(c)),
        TileContent::Empty => Value::unit("empty"),
    };
    Value::record([
        ("x", Value::int(x.x as i64)),
        ("y", Value::int(x.y as i64)),
        ("width", Value::uint(x.width)),
        ("height", Value::uint(x.height)),
        ("content", content),
    ])
}

pub fn bitmap_value(b: &Bitmap) -> Value {
    let source = match &b.source {
        PixelSource::Empty => Value::unit("empty"),
        PixelSource::Image(p) => Value::case(
            "image",
            Value::record([("x", Value::int(p.x as i64)), ("y", Value::int(p.y as i64)), ("image", encoded_image_value(&p.image))]),
        ),
        PixelSource::Tiles(tiled) => Value::case(
            "tiles",
            Value::record([
                ("tiles", Value::list(&tiled.tiles, tile_value)),
                (
                    "overlap",
                    t(match tiled.overlap {
                        Overlap::Replace => "replace",
                        Overlap::Over => "over",
                        Overlap::Reject => "reject",
                    }),
                ),
            ]),
        ),
        PixelSource::Raw(r) => Value::case(
            "raw",
            Value::record([
                ("x", Value::int(r.x as i64)),
                ("y", Value::int(r.y as i64)),
                ("width", Value::uint(r.width)),
                ("height", Value::uint(r.height)),
                ("rgba", Value::bytes(r.rgba.clone())),
            ]),
        ),
    };
    Value::record([("size", Value::option(b.size.map(|(w, h)| Value::Array(vec![Value::uint(w), Value::uint(h)])))), ("source", source)])
}

fn layer_kind_value(k: LayerKind) -> Value {
    t(match k {
        LayerKind::Raster => "raster",
        LayerKind::Folder => "folder",
        LayerKind::Text => "text",
        LayerKind::Vector => "vector",
        LayerKind::Adjustment => "adjustment",
        LayerKind::Fill => "fill",
        LayerKind::Paper => "paper",
        LayerKind::Mask => "mask",
        LayerKind::Stencil => "stencil",
        LayerKind::Unknown => "unknown",
    })
}

fn blend_name(b: BlendMode) -> &'static str {
    use BlendMode::*;
    match b {
        Normal => "normal",
        PassThrough => "pass-through",
        Multiply => "multiply",
        Screen => "screen",
        Darken => "darken",
        Lighten => "lighten",
        Difference => "difference",
        Subtract => "subtract",
        LinearDodge => "linear-dodge",
        LinearBurn => "linear-burn",
        ColorDodge => "color-dodge",
        ColorBurn => "color-burn",
        Overlay => "overlay",
        HardLight => "hard-light",
        SoftLight => "soft-light",
        Exclusion => "exclusion",
        Divide => "divide",
        DarkerColor => "darker-color",
        LighterColor => "lighter-color",
        Hue => "hue",
        Saturation => "saturation",
        Color => "color",
        Luminosity => "luminosity",
        HardMix => "hard-mix",
        LinearLight => "linear-light",
        VividLight => "vivid-light",
        PinLight => "pin-light",
        GlowAdd => "glow-add",
        GlowDodge => "glow-dodge",
        Other => "other",
    }
}

const BLENDS: [BlendMode; 30] = {
    use BlendMode::*;
    [
        Normal, PassThrough, Multiply, Screen, Darken, Lighten, Difference, Subtract, LinearDodge, LinearBurn, ColorDodge, ColorBurn, Overlay,
        HardLight, SoftLight, Exclusion, Divide, DarkerColor, LighterColor, Hue, Saturation, Color, Luminosity, HardMix, LinearLight, VividLight,
        PinLight, GlowAdd, GlowDodge, Other,
    ]
};

fn meta_value(m: &MetaEntry) -> Value {
    let value = match &m.value {
        MetaValue::Null => Value::unit("null"),
        MetaValue::Boolean(b) => Value::case("boolean", Value::Bool(*b)),
        MetaValue::Integer(i) => Value::case("integer", Value::int(*i)),
        MetaValue::Number(f) => Value::case("number", Value::Float(*f)),
        MetaValue::Text(s) => Value::case("text", t(s)),
        MetaValue::Sha256Of(b) => Value::case("sha256-of", bytes_value(b)),
        MetaValue::Json(s) => Value::case("json", t(s)),
    };
    Value::record([("key", t(&m.key)), ("value", value)])
}

fn warning_value(w: &Warning) -> Value {
    Value::record([("code", t(&w.code)), ("message", t(&w.message))])
}

fn layer_value(l: &Layer) -> Value {
    let pixels = match &l.pixels {
        LayerPixels::None => Value::unit("none"),
        LayerPixels::Ready(b) => Value::case("ready", bitmap_value(b)),
        LayerPixels::Deferred(n) => Value::case("deferred", Value::uint(*n)),
    };
    Value::record([
        ("id", t(&l.id)),
        ("stable-id", t(&l.stable_id)),
        ("parent", Value::option(l.parent.map(Value::uint))),
        ("name", t(&l.name)),
        ("kind", layer_kind_value(l.kind)),
        ("visible", Value::Bool(l.visible)),
        ("opacity", Value::Float(l.opacity)),
        ("blend", t(blend_name(l.blend))),
        ("clip-to-below", Value::Bool(l.clip_to_below)),
        ("offset-x", Value::int(l.offset_x)),
        ("offset-y", Value::int(l.offset_y)),
        ("pixels", pixels),
        ("meta", Value::list(&l.meta, meta_value)),
        ("semantic", Value::list(&l.semantic, meta_value)),
        (
            "write",
            t(match l.write {
                WriteCapability::None => "none",
                WriteCapability::Pixels => "pixels",
                WriteCapability::PixelsAndProps => "pixels-and-props",
            }),
        ),
    ])
}

pub fn structure_value(s: &Structure) -> Value {
    let profile = match &s.canvas.profile {
        ColorProfile::None => Value::unit("none"),
        ColorProfile::Embedded(b) => Value::case("embedded", bytes_value(b)),
        ColorProfile::Named(n) => Value::case("named", t(n)),
    };
    let composite = match &s.composite {
        Composite::Render(Compositor::GenericV1) => Value::case("render", t("generic-v1")),
        Composite::Image(b) => Value::case("image", bitmap_value(b)),
        Composite::None => Value::unit("none"),
    };
    Value::record([
        ("format", t(&s.format)),
        (
            "canvas",
            Value::record([
                ("width", Value::uint(s.canvas.width)),
                ("height", Value::uint(s.canvas.height)),
                ("dpi", Value::option(s.canvas.dpi.map(Value::Float))),
                ("profile", profile),
            ]),
        ),
        ("layers", Value::list(&s.layers, layer_value)),
        ("composite", composite),
        ("file-meta", Value::list(&s.file_meta, meta_value)),
        ("context", Value::bytes(s.context.clone())),
        ("warnings", Value::list(&s.warnings, warning_value)),
    ])
}

pub fn read_error_value(e: &ReadError) -> Value {
    match e {
        ReadError::NotThisFormat(m) => Value::case("not-this-format", t(m)),
        ReadError::Corrupt(m) => Value::case("corrupt", t(m)),
        ReadError::Unsupported(m) => Value::case("unsupported", t(m)),
        ReadError::TooLarge(m) => Value::case("too-large", t(m)),
    }
}

pub fn confidence_value(c: Confidence) -> Value {
    t(match c {
        Confidence::No => "no",
        Confidence::Maybe => "maybe",
        Confidence::Yes => "yes",
    })
}

pub fn plugin_info_value(i: &PluginInfo) -> Value {
    Value::record([
        ("id", t(&i.id)),
        ("version", t(&i.version)),
        (
            "formats",
            Value::list(&i.formats, |f| {
                Value::record([
                    ("name", t(&f.name)),
                    ("extensions", Value::list(&f.extensions, |e| t(e))),
                    ("mime", Value::option(f.mime.as_deref().map(t))),
                ])
            }),
        ),
        ("probe-bytes", Value::uint(i.probe_bytes)),
        ("can-write", Value::Bool(i.can_write)),
    ])
}

/// The plan, its blobs as tmiki's handles.
pub fn write_plan_value(p: WritePlan) -> Value {
    Value::record([
        (
            "pieces",
            Value::list(p.pieces, |piece| match piece {
                Piece::Source(s) => Value::case("source", span_value(&s)),
                Piece::Bytes(b) => Value::case("bytes", Value::Bytes(b)),
                Piece::Blob(b) => Value::case("blob", Value::uint(b.0.handle)),
            }),
        ),
        ("warnings", Value::list(&p.warnings, warning_value)),
    ])
}

pub fn write_error_value(e: &WriteError) -> Value {
    match e {
        WriteError::Unsupported(m) => Value::case("unsupported", t(m)),
        WriteError::Invalid(m) => Value::case("invalid", t(m)),
        WriteError::Corrupt(m) => Value::case("corrupt", t(m)),
    }
}

pub fn encoding_value(e: &Encoding) -> Value {
    let pair = |k: ZlibKernel, l: i8| {
        let k = match k {
            ZlibKernel::ZlibV131 => "zlib-v131",
        };
        Value::Array(vec![t(k), Value::int(l as i64)])
    };
    match *e {
        Encoding::Raw => Value::unit("raw"),
        Encoding::Zlib((k, l)) => Value::case("zlib", pair(k, l)),
        Encoding::Deflate((k, l)) => Value::case("deflate", pair(k, l)),
    }
}

pub fn image_encoding_value(e: &ImageEncoding) -> Value {
    match e {
        ImageEncoding::PngV1 => Value::unit("png-v1"),
        ImageEncoding::JpegKeepV1(b) => Value::case("jpeg-keep-v1", Value::bytes(b.clone())),
        ImageEncoding::WebpV1(q) => Value::case("webp-v1", Value::uint(*q)),
    }
}

pub fn sql_value_value(v: &SqlValue) -> Value {
    match v {
        SqlValue::Null => Value::unit("null"),
        SqlValue::Integer(i) => Value::case("integer", Value::int(*i)),
        SqlValue::Real(f) => Value::case("real", Value::Float(*f)),
        SqlValue::Text(s) => Value::case("text", t(s)),
        SqlValue::Blob(b) => Value::case("blob", Value::bytes(b.clone())),
    }
}

// -- tmiki to plugin ---------------------------------------------------------------

pub fn layer_edit(v: &Value, what: &str) -> R<LayerEdit> {
    let props = v
        .field(what, "props")?
        .array(what)?
        .iter()
        .map(|p| {
            let (tag, x) = p.case_of(what)?;
            Ok(match tag {
                "name" => PropChange::Name(x.string(what)?),
                "visible" => PropChange::Visible(x.bool(what)?),
                "opacity" => PropChange::Opacity(x.f64(what)?),
                "blend" => {
                    let name = x.enum_name(what)?;
                    PropChange::Blend(BLENDS.iter().copied().find(|b| blend_name(*b) == name).ok_or_else(|| ValueError(format!("{what}: blend-mode has no case {name:?}")))?)
                }
                "offset" => {
                    let o = x.tuple(what, 2)?;
                    PropChange::Offset((o[0].i64(what)?, o[1].i64(what)?))
                }
                other => return no_case(what, "prop-change", other),
            })
        })
        .collect::<R<Vec<_>>>()?;
    Ok(LayerEdit {
        stable_id: v.field(what, "stable-id")?.string(what)?,
        new_pixels: v.field(what, "new-pixels")?.opt().map(|n| n.u32(what)).transpose()?,
        props,
    })
}

pub fn image_header(v: &Value) -> R<ImageHeader> {
    let w = "image-header";
    Ok(ImageHeader {
        width: v.field(w, "width")?.u32(w)?,
        height: v.field(w, "height")?.u32(w)?,
        frames: v.field(w, "frames")?.u32(w)?,
        has_alpha: v.field(w, "has-alpha")?.bool(w)?,
        dpi: v
            .field(w, "dpi")?
            .opt()
            .map(|d| -> R<(f64, f64)> {
                let p = d.tuple(w, 2)?;
                Ok((p[0].f64(w)?, p[1].f64(w)?))
            })
            .transpose()?,
        icc: v.field(w, "icc")?.bool(w)?,
    })
}

pub fn sql_value(v: &Value, what: &str) -> R<SqlValue> {
    let (tag, p) = v.case_of(what)?;
    Ok(match tag {
        "null" => SqlValue::Null,
        "integer" => SqlValue::Integer(p.i64(what)?),
        "real" => SqlValue::Real(p.f64(what)?),
        "text" => SqlValue::Text(p.string(what)?),
        "blob" => SqlValue::Blob(p.byte_vec(what)?),
        other => return no_case(what, "sql-value", other),
    })
}

pub fn element(v: &Value, what: &str) -> R<Element> {
    Ok(Element {
        name: v.field(what, "name")?.string(what)?,
        namespace: v.field(what, "namespace")?.opt().map(|n| n.string(what)).transpose()?,
        attributes: v
            .field(what, "attributes")?
            .array(what)?
            .iter()
            .map(|a| {
                let p = a.tuple(what, 2)?;
                Ok((p[0].string(what)?, p[1].string(what)?))
            })
            .collect::<R<Vec<_>>>()?,
        text: v.field(what, "text")?.string(what)?,
        parent: v.field(what, "parent")?.opt().map(|n| n.u32(what)).transpose()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A structure with every case of every type a reader returns: its
    /// CBOR is `tmi-plugin-dev/testdata/structure.cbor`, which tmiki's side
    /// (`tmi-plugin-host`) reads and writes the same way.
    pub fn kitchen_sink() -> Structure {
        let span = |o, l| Span { offset: o, len: l };
        let all_codecs = vec![
            Codec::Zlib,
            Codec::Deflate,
            Codec::Gzip,
            Codec::Zstd,
            Codec::Lz4Block(10),
            Codec::Lzf(11),
            Codec::Packbits(12),
            Codec::Predictor(Predictor { kind: PredictorKind::TiffHorizontal, row_bytes: 64, bytes_per_pixel: 4 }),
            Codec::Predictor(Predictor { kind: PredictorKind::PngRows, row_bytes: 9, bytes_per_pixel: 1 }),
            Codec::Plugin(7),
        ];
        let image = |codec| EncodedImage { data: Bytes::Span(span(100, 20)), pre: vec![Codec::Deflate], codec, frame: 0 };
        let layout = Layout {
            groups: vec![
                Group { channels: vec![Channel::A], depth: Depth::Eight },
                Group { channels: vec![Channel::B, Channel::G, Channel::R, Channel::Pad], depth: Depth::SixteenBe },
                Group { channels: vec![Channel::Gray], depth: Depth::OneBitMsb },
                Group { channels: vec![Channel::Gray], depth: Depth::SixteenLe },
                Group { channels: vec![Channel::R], depth: Depth::Float32Le },
            ],
            row_padding: 1,
            tint: Some(Rgba8 { r: 1, g: 2, b: 3, a: 255 }),
            premultiplied: true,
        };
        let tiles = Bitmap {
            size: Some((256, 128)),
            source: PixelSource::Tiles(Tiled {
                tiles: vec![
                    Tile {
                        x: -5,
                        y: 0,
                        width: 128,
                        height: 128,
                        content: TileContent::Raster(Raster { data: Bytes::Spans(vec![span(0, 4), span(8, 4)]), codecs: all_codecs.clone(), layout }),
                    },
                    Tile { x: 128, y: 0, width: 128, height: 128, content: TileContent::Image(image(ImageCodec::Qoi)) },
                    Tile { x: 0, y: 128, width: 1, height: 1, content: TileContent::Fill(Rgba8 { r: 9, g: 8, b: 7, a: 6 }) },
                    Tile { x: 1, y: 128, width: 1, height: 1, content: TileContent::Empty },
                ],
                overlap: Overlap::Over,
            }),
        };
        let meta = vec![
            MetaEntry { key: "n".into(), value: MetaValue::Null },
            MetaEntry { key: "b".into(), value: MetaValue::Boolean(true) },
            MetaEntry { key: "i".into(), value: MetaValue::Integer(-42) },
            MetaEntry { key: "f".into(), value: MetaValue::Number(0.25) },
            MetaEntry { key: "t".into(), value: MetaValue::Text("名前".into()) },
            MetaEntry { key: "h".into(), value: MetaValue::Sha256Of(Bytes::Inline(vec![1, 2, 3])) },
            MetaEntry { key: "j".into(), value: MetaValue::Json("{\"a\":[1,2]}".into()) },
        ];
        let kinds = [
            LayerKind::Raster,
            LayerKind::Folder,
            LayerKind::Text,
            LayerKind::Vector,
            LayerKind::Adjustment,
            LayerKind::Fill,
            LayerKind::Paper,
            LayerKind::Mask,
            LayerKind::Stencil,
            LayerKind::Unknown,
        ];
        let codecs_img = [ImageCodec::Png, ImageCodec::JpegV1, ImageCodec::WebpV1, ImageCodec::Gif, ImageCodec::Bmp, ImageCodec::Tiff, ImageCodec::Qoi];
        let mut layers = Vec::new();
        for (i, blend) in BLENDS.iter().enumerate() {
            let pixels = match i % 6 {
                0 => LayerPixels::None,
                1 => LayerPixels::Deferred(i as u32),
                2 => LayerPixels::Ready(tiles.clone()),
                3 => LayerPixels::Ready(Bitmap { size: None, source: PixelSource::Image(PlacedImage { x: 3, y: -4, image: image(codecs_img[i % 7]) }) }),
                4 => LayerPixels::Ready(Bitmap { size: Some((2, 1)), source: PixelSource::Raw(RawPixels { x: 0, y: 0, width: 2, height: 1, rgba: vec![0, 1, 2, 3, 4, 5, 6, 7] }) }),
                _ => LayerPixels::Ready(Bitmap { size: Some((4, 4)), source: PixelSource::Empty }),
            };
            layers.push(Layer {
                id: format!("L{i}"),
                stable_id: format!("s{i}"),
                parent: if i == 0 { None } else { Some(0) },
                name: format!("layer {i}"),
                kind: kinds[i % kinds.len()],
                visible: i % 2 == 0,
                opacity: i as f64 / 30.0,
                blend: *blend,
                clip_to_below: i % 3 == 0,
                offset_x: -(i as i64) * 1000,
                offset_y: i64::MAX - i as i64,
                pixels,
                meta: if i == 1 { meta.clone() } else { vec![] },
                semantic: if i == 2 { meta.clone() } else { vec![] },
                write: [WriteCapability::None, WriteCapability::Pixels, WriteCapability::PixelsAndProps][i % 3],
            });
        }
        Structure {
            format: "Kitchen sink".into(),
            canvas: Canvas { width: 256, height: 129, dpi: Some(350.5), profile: ColorProfile::Embedded(Bytes::Span(span(1, 2))) },
            layers,
            composite: Composite::Image(Bitmap { size: None, source: PixelSource::Image(PlacedImage { x: 0, y: 0, image: image(ImageCodec::Png) }) }),
            file_meta: vec![MetaEntry { key: "k".into(), value: MetaValue::Text("v".into()) }],
            context: vec![0, 255, 7],
            warnings: vec![Warning { code: "c".into(), message: "m".into() }],
        }
    }

    fn fixture() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../crates/tmi-plugin-dev/testdata/structure.cbor")
    }

    #[test]
    fn the_kitchen_sink_is_the_shared_fixture() {
        let bytes = structure_value(&kitchen_sink()).encode();
        if std::env::var_os("TMI_UPDATE_FIXTURES").is_some() {
            std::fs::create_dir_all(fixture().parent().unwrap()).unwrap();
            std::fs::write(fixture(), &bytes).unwrap();
        }
        let want = std::fs::read(fixture()).expect("the fixture (TMI_UPDATE_FIXTURES=1 writes it)");
        assert!(bytes == want, "the CBOR of the kitchen sink is not the fixture's ({} vs {} bytes)", bytes.len(), want.len());
    }

    #[test]
    fn other_cases_round_trip() {
        let e = Value::record([
            ("stable-id", Value::text("s1")),
            ("new-pixels", Value::Uint(0)),
            (
                "props",
                Value::Array(vec![
                    Value::case("name", Value::text("n")),
                    Value::case("visible", Value::Bool(false)),
                    Value::case("opacity", Value::Uint(1)),
                    Value::case("blend", Value::text("glow-dodge")),
                    Value::case("offset", Value::Array(vec![Value::int(-1), Value::int(2)])),
                ]),
            ),
        ]);
        let edit = layer_edit(&e, "e").unwrap();
        assert_eq!(edit.new_pixels, Some(0));
        assert!(matches!(edit.props[3], PropChange::Blend(BlendMode::GlowDodge)));
        assert!(matches!(edit.props[4], PropChange::Offset((-1, 2))));
        for v in [SqlValue::Null, SqlValue::Integer(-3), SqlValue::Real(1.5), SqlValue::Text("x".into()), SqlValue::Blob(vec![1])] {
            let back = sql_value(&sql_value_value(&v), "v").unwrap();
            assert_eq!(format!("{back:?}"), format!("{v:?}"));
        }
    }
}
