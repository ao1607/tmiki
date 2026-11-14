//! `tmi_format::types` to and from the CBOR of `tmiki plugin dev`
//! (`tmi_plugin_dev`'s doc: records are maps keyed by the WIT's field
//! names, variants `{case: payload}`, enums their case names, resources
//! handle numbers). Decoding a plugin's value checks only that it fits the
//! WIT; `tmi_format::validate` checks the rest, as for a wasm plugin.

use tmi_format::types::*;
use tmi_plugin_dev::{Value, ValueError};

use crate::bindings::tmi::format::{sqlite, xml};

type R<T> = Result<T, ValueError>;

fn bad<T>(m: String) -> R<T> {
    Err(ValueError(m))
}

macro_rules! wit_enum {
    ($ty:ident, $dec:ident, $enc:ident, { $($v:ident => $n:literal),* $(,)? }) => {
        pub fn $dec(v: &Value, what: &str) -> R<$ty> {
            match v.enum_name(what)? {
                $($n => Ok($ty::$v),)*
                other => bad(format!("{what}: {} has no case {other:?}", stringify!($ty))),
            }
        }
        #[allow(dead_code, unreachable_patterns)]
        pub fn $enc(x: $ty) -> Value {
            Value::text(match x {
                $($ty::$v => $n,)*
                _ => unreachable!("not a WIT case"),
            })
        }
    };
}

wit_enum!(PredictorKind, predictor_kind, predictor_kind_value, { PngRows => "png-rows", TiffHorizontal => "tiff-horizontal" });
wit_enum!(ImageCodec, image_codec, image_codec_value, {
    Png => "png", JpegV1 => "jpeg-v1", WebpV1 => "webp-v1", Gif => "gif", Bmp => "bmp", Tiff => "tiff", Qoi => "qoi",
});
wit_enum!(Channel, channel, channel_value, { R => "r", G => "g", B => "b", A => "a", Gray => "gray", Pad => "pad" });
wit_enum!(Depth, depth, depth_value, {
    OneBitMsb => "one-bit-msb", Eight => "eight", SixteenBe => "sixteen-be", SixteenLe => "sixteen-le", Float32Le => "float32-le",
});
wit_enum!(Overlap, overlap, overlap_value, { Replace => "replace", Over => "over", Reject => "reject" });
wit_enum!(LayerKind, layer_kind, layer_kind_value, {
    Raster => "raster", Folder => "folder", Text => "text", Vector => "vector", Adjustment => "adjustment",
    Fill => "fill", Paper => "paper", Mask => "mask", Stencil => "stencil", Unknown => "unknown",
});
wit_enum!(BlendMode, blend_mode, blend_mode_value, {
    Normal => "normal", PassThrough => "pass-through", Multiply => "multiply", Screen => "screen",
    Darken => "darken", Lighten => "lighten", Difference => "difference", Subtract => "subtract",
    LinearDodge => "linear-dodge", LinearBurn => "linear-burn", ColorDodge => "color-dodge",
    ColorBurn => "color-burn", Overlay => "overlay", HardLight => "hard-light", SoftLight => "soft-light",
    Exclusion => "exclusion", Divide => "divide", DarkerColor => "darker-color", LighterColor => "lighter-color",
    Hue => "hue", Saturation => "saturation", Color => "color", Luminosity => "luminosity",
    HardMix => "hard-mix", LinearLight => "linear-light", VividLight => "vivid-light", PinLight => "pin-light",
    GlowAdd => "glow-add", GlowDodge => "glow-dodge", Other => "other",
});
wit_enum!(WriteCapability, write_capability, write_capability_value, { None => "none", Pixels => "pixels", PixelsAndProps => "pixels-and-props" });
wit_enum!(Compositor, compositor, compositor_value, { GenericV1 => "generic-v1" });
wit_enum!(Confidence, confidence, confidence_value, { No => "no", Maybe => "maybe", Yes => "yes" });
wit_enum!(ZlibKernel, zlib_kernel, zlib_kernel_value, { ZlibV131 => "zlib-v131" });

fn list<T>(v: &Value, what: &str, f: impl Fn(&Value, &str) -> R<T>) -> R<Vec<T>> {
    let a = v.array(what)?;
    let item = format!("{what}[]");
    a.iter().map(|x| f(x, &item)).collect()
}

fn opt<T>(v: &Value, f: impl Fn(&Value) -> R<T>) -> R<Option<T>> {
    v.opt().map(f).transpose()
}

fn no_payload(p: &Value, what: &str, tag: &str) -> R<()> {
    if p.is_null() {
        Ok(())
    } else {
        bad(format!("{what}: case {tag} has no payload, got {}", p.kind()))
    }
}

fn no_case<T>(what: &str, ty: &str, tag: &str) -> R<T> {
    bad(format!("{what}: {ty} has no case {tag:?}"))
}

// -- plugin to tmiki ---------------------------------------------------------------

pub fn span(v: &Value, what: &str) -> R<Span> {
    Ok(Span { offset: v.field(what, "offset")?.u64(what)?, len: v.field(what, "len")?.u64(what)? })
}

pub fn bytes(v: &Value, what: &str) -> R<Bytes> {
    let (tag, p) = v.case_of(what)?;
    Ok(match tag {
        "span" => Bytes::Span(span(p, what)?),
        "spans" => Bytes::Spans(list(p, what, span)?),
        "inline" => Bytes::Inline(p.byte_vec(what)?),
        t => return no_case(what, "bytes", t),
    })
}

pub fn codec(v: &Value, what: &str) -> R<Codec> {
    let (tag, p) = v.case_of(what)?;
    Ok(match tag {
        "zlib" => no_payload(p, what, tag).map(|_| Codec::Zlib)?,
        "deflate" => no_payload(p, what, tag).map(|_| Codec::Deflate)?,
        "gzip" => no_payload(p, what, tag).map(|_| Codec::Gzip)?,
        "zstd" => no_payload(p, what, tag).map(|_| Codec::Zstd)?,
        "lz4-block" => Codec::Lz4Block(p.u32(what)?),
        "lzf" => Codec::Lzf(p.u32(what)?),
        "packbits" => Codec::Packbits(p.u32(what)?),
        "predictor" => Codec::Predictor(Predictor {
            kind: predictor_kind(p.field(what, "kind")?, what)?,
            row_bytes: p.field(what, "row-bytes")?.u32(what)?,
            bytes_per_pixel: p.field(what, "bytes-per-pixel")?.u8(what)?,
        }),
        "plugin" => Codec::Plugin(p.u32(what)?),
        t => return no_case(what, "codec", t),
    })
}

pub fn codecs(v: &Value, what: &str) -> R<Vec<Codec>> {
    list(v, what, codec)
}

pub fn encoded_image(v: &Value, what: &str) -> R<EncodedImage> {
    Ok(EncodedImage {
        data: bytes(v.field(what, "data")?, what)?,
        pre: codecs(v.field(what, "pre")?, what)?,
        codec: image_codec(v.field(what, "codec")?, what)?,
        frame: v.field(what, "frame")?.u32(what)?,
    })
}

fn rgba8(v: &Value, what: &str) -> R<Rgba8> {
    Ok(Rgba8 {
        r: v.field(what, "r")?.u8(what)?,
        g: v.field(what, "g")?.u8(what)?,
        b: v.field(what, "b")?.u8(what)?,
        a: v.field(what, "a")?.u8(what)?,
    })
}

pub fn layout(v: &Value, what: &str) -> R<Layout> {
    Ok(Layout {
        groups: list(v.field(what, "groups")?, what, |g, w| {
            Ok(Group { channels: list(g.field(w, "channels")?, w, channel)?, depth: depth(g.field(w, "depth")?, w)? })
        })?,
        row_padding: v.field(what, "row-padding")?.u32(what)?,
        tint: opt(v.field(what, "tint")?, |t| rgba8(t, what))?,
        premultiplied: v.field(what, "premultiplied")?.bool(what)?,
    })
}

fn tile(v: &Value, what: &str) -> R<Tile> {
    let c = v.field(what, "content")?;
    let (tag, p) = c.case_of(what)?;
    let content = match tag {
        "raster" => TileContent::Raster(Raster {
            data: bytes(p.field(what, "data")?, what)?,
            codecs: codecs(p.field(what, "codecs")?, what)?,
            layout: layout(p.field(what, "layout")?, what)?,
        }),
        "image" => TileContent::Image(encoded_image(p, what)?),
        "fill" => TileContent::Fill(rgba8(p, what)?),
        "empty" => no_payload(p, what, tag).map(|_| TileContent::Empty)?,
        t => return no_case(what, "tile-content", t),
    };
    Ok(Tile {
        x: v.field(what, "x")?.i32(what)?,
        y: v.field(what, "y")?.i32(what)?,
        width: v.field(what, "width")?.u32(what)?,
        height: v.field(what, "height")?.u32(what)?,
        content,
    })
}

pub fn bitmap(v: &Value, what: &str) -> R<Bitmap> {
    let size = opt(v.field(what, "size")?, |s| {
        let t = s.tuple(what, 2)?;
        Ok((t[0].u32(what)?, t[1].u32(what)?))
    })?;
    let src = v.field(what, "source")?;
    let (tag, p) = src.case_of(what)?;
    let source = match tag {
        "empty" => no_payload(p, what, tag).map(|_| PixelSource::Empty)?,
        "image" => PixelSource::Image(PlacedImage {
            x: p.field(what, "x")?.i32(what)?,
            y: p.field(what, "y")?.i32(what)?,
            image: encoded_image(p.field(what, "image")?, what)?,
        }),
        "tiles" => PixelSource::Tiles(Tiled { tiles: list(p.field(what, "tiles")?, what, tile)?, overlap: overlap(p.field(what, "overlap")?, what)? }),
        "raw" => PixelSource::Raw(RawPixels {
            x: p.field(what, "x")?.i32(what)?,
            y: p.field(what, "y")?.i32(what)?,
            width: p.field(what, "width")?.u32(what)?,
            height: p.field(what, "height")?.u32(what)?,
            rgba: p.field(what, "rgba")?.byte_vec(what)?,
        }),
        t => return no_case(what, "pixel-source", t),
    };
    Ok(Bitmap { size, source })
}

fn meta_entry(v: &Value, what: &str) -> R<MetaEntry> {
    let key = v.field(what, "key")?.string(what)?;
    let w = format!("{what}.{key}");
    let (tag, p) = v.field(what, "value")?.case_of(&w)?;
    let value = match tag {
        "null" => no_payload(p, &w, tag).map(|_| MetaValue::Null)?,
        "boolean" => MetaValue::Boolean(p.bool(&w)?),
        "integer" => MetaValue::Integer(p.i64(&w)?),
        "number" => MetaValue::Number(p.f64(&w)?),
        "text" => MetaValue::Text(p.string(&w)?),
        "sha256-of" => MetaValue::Sha256Of(bytes(p, &w)?),
        "json" => MetaValue::Json(p.string(&w)?),
        t => return no_case(&w, "meta-value", t),
    };
    Ok(MetaEntry { key, value })
}

fn metas(v: &Value, what: &str) -> R<Vec<MetaEntry>> {
    list(v, what, meta_entry)
}

fn warning(v: &Value, what: &str) -> R<Warning> {
    Ok(Warning { code: v.field(what, "code")?.string(what)?, message: crate::convert::shown(v.field(what, "message")?.string(what)?) })
}

fn layer(v: &Value, what: &str) -> R<Layer> {
    let id = v.field(what, "id")?.string(what)?;
    let w = format!("layer {id:?}");
    let w = w.as_str();
    let (tag, p) = v.field(w, "pixels")?.case_of(w)?;
    let pixels = match tag {
        "none" => no_payload(p, w, tag).map(|_| LayerPixels::None)?,
        "ready" => LayerPixels::Ready(bitmap(p, w)?),
        "deferred" => LayerPixels::Deferred(p.u32(w)?),
        t => return no_case(w, "layer-pixels", t),
    };
    Ok(Layer {
        stable_id: v.field(w, "stable-id")?.string(w)?,
        parent: opt(v.field(w, "parent")?, |p| p.u32(w))?,
        name: v.field(w, "name")?.string(w)?,
        kind: layer_kind(v.field(w, "kind")?, w)?,
        visible: v.field(w, "visible")?.bool(w)?,
        opacity: v.field(w, "opacity")?.f64(w)?,
        blend: blend_mode(v.field(w, "blend")?, w)?,
        clip_to_below: v.field(w, "clip-to-below")?.bool(w)?,
        offset_x: v.field(w, "offset-x")?.i64(w)?,
        offset_y: v.field(w, "offset-y")?.i64(w)?,
        pixels,
        meta: metas(v.field(w, "meta")?, w)?,
        semantic: metas(v.field(w, "semantic")?, w)?,
        write: write_capability(v.field(w, "write")?, w)?,
        id,
    })
}

pub fn structure(v: &Value) -> R<Structure> {
    let what = "structure";
    let c = v.field(what, "canvas")?;
    let (tag, p) = c.field("canvas", "profile")?.case_of("canvas.profile")?;
    let profile = match tag {
        "none" => no_payload(p, "canvas.profile", tag).map(|_| ColorProfile::None)?,
        "embedded" => ColorProfile::Embedded(bytes(p, "canvas.profile")?),
        "named" => ColorProfile::Named(p.string("canvas.profile")?),
        t => return no_case("canvas.profile", "color-profile", t),
    };
    let canvas = Canvas {
        width: c.field("canvas", "width")?.u32("canvas.width")?,
        height: c.field("canvas", "height")?.u32("canvas.height")?,
        dpi: opt(c.field("canvas", "dpi")?, |d| d.f64("canvas.dpi"))?,
        profile,
    };
    let (tag, p) = v.field(what, "composite")?.case_of("composite")?;
    let composite = match tag {
        "render" => Composite::Render(compositor(p, "composite")?),
        "image" => Composite::Image(bitmap(p, "composite")?),
        "none" => no_payload(p, "composite", tag).map(|_| Composite::None)?,
        t => return no_case("composite", "composite", t),
    };
    Ok(Structure {
        format: v.field(what, "format")?.string("structure.format")?,
        canvas,
        layers: list(v.field(what, "layers")?, "layers", layer)?,
        composite,
        file_meta: metas(v.field(what, "file-meta")?, "file-meta")?,
        context: v.field(what, "context")?.byte_vec("structure.context")?,
        warnings: list(v.field(what, "warnings")?, "warnings", warning)?,
    })
}

pub fn read_error(v: &Value) -> R<ReadError> {
    let what = "read-error";
    let (tag, p) = v.case_of(what)?;
    let m = p.string(what)?;
    let m = crate::convert::shown(m);
    Ok(match tag {
        "not-this-format" => ReadError::NotThisFormat(m),
        "corrupt" => ReadError::Corrupt(m),
        "unsupported" => ReadError::Unsupported(m),
        "too-large" => ReadError::TooLarge(m),
        t => return no_case(what, "read-error", t),
    })
}

pub fn plugin_info(v: &Value) -> R<PluginInfo> {
    let what = "plugin-info";
    Ok(PluginInfo {
        id: v.field(what, "id")?.string(what)?,
        version: v.field(what, "version")?.string(what)?,
        formats: list(v.field(what, "formats")?, "formats", |f, w| {
            Ok(FormatInfo {
                name: f.field(w, "name")?.string(w)?,
                extensions: list(f.field(w, "extensions")?, w, |e, w| e.string(w))?,
                mime: opt(f.field(w, "mime")?, |m| m.string(w))?,
            })
        })?,
        probe_bytes: v.field(what, "probe-bytes")?.u32(what)?,
        can_write: v.field(what, "can-write")?.bool(what)?,
    })
}

pub fn write_plan(v: &Value) -> R<WritePlan> {
    let what = "write-plan";
    Ok(WritePlan {
        pieces: list(v.field(what, "pieces")?, "pieces", |p, w| {
            let (tag, x) = p.case_of(w)?;
            Ok(match tag {
                "source" => Piece::Source(span(x, w)?),
                "bytes" => Piece::Bytes(x.byte_vec(w)?),
                "blob" => Piece::Blob(x.u32(w)?),
                t => return no_case(w, "piece", t),
            })
        })?,
        warnings: list(v.field(what, "warnings")?, "warnings", warning)?,
    })
}

pub fn write_error(v: &Value) -> R<WriteError> {
    let what = "write-error";
    let (tag, p) = v.case_of(what)?;
    let m = p.string(what)?;
    Ok(match tag {
        "unsupported" => WriteError::Unsupported(m),
        "invalid" => WriteError::Invalid(m),
        "corrupt" => WriteError::Corrupt(m),
        t => return no_case(what, "write-error", t),
    })
}

pub fn encoding(v: &Value, what: &str) -> R<Encoding> {
    let (tag, p) = v.case_of(what)?;
    let pair = |p: &Value| -> R<(ZlibKernel, i8)> {
        let t = p.tuple(what, 2)?;
        Ok((zlib_kernel(&t[0], what)?, t[1].i8(what)?))
    };
    Ok(match tag {
        "raw" => no_payload(p, what, tag).map(|_| Encoding::Raw)?,
        "zlib" => {
            let (k, l) = pair(p)?;
            Encoding::Zlib(k, l)
        }
        "deflate" => {
            let (k, l) = pair(p)?;
            Encoding::Deflate(k, l)
        }
        t => return no_case(what, "encoding", t),
    })
}

pub fn image_encoding(v: &Value, what: &str) -> R<ImageEncoding> {
    let (tag, p) = v.case_of(what)?;
    Ok(match tag {
        "png-v1" => no_payload(p, what, tag).map(|_| ImageEncoding::PngV1)?,
        "jpeg-keep-v1" => ImageEncoding::JpegKeepV1(p.byte_vec(what)?),
        "webp-v1" => ImageEncoding::WebpV1(p.u8(what)?),
        t => return no_case(what, "image-encoding", t),
    })
}

pub fn sql_value(v: &Value, what: &str) -> R<sqlite::SqlValue> {
    let (tag, p) = v.case_of(what)?;
    Ok(match tag {
        "null" => no_payload(p, what, tag).map(|_| sqlite::SqlValue::Null)?,
        "integer" => sqlite::SqlValue::Integer(p.i64(what)?),
        "real" => sqlite::SqlValue::Real(p.f64(what)?),
        "text" => sqlite::SqlValue::Text(p.string(what)?),
        "blob" => sqlite::SqlValue::Blob(p.byte_vec(what)?),
        t => return no_case(what, "sql-value", t),
    })
}

// -- a reader's answers as values ------------------------------------------------
//
// The inverses of the decoders above: what a plugin sends for these values.
// `tmiki plugin test` compares readers in this form (canonical CBOR,
// `Value::canonical`), so a wasm plugin's answers and a development
// plugin's are written the one way the RPC writes them.

pub fn span_value(s: &Span) -> Value {
    Value::record([("offset", Value::Uint(s.offset)), ("len", Value::Uint(s.len))])
}

pub fn bytes_value(b: &Bytes) -> Value {
    match b {
        Bytes::Span(s) => Value::case("span", span_value(s)),
        Bytes::Spans(l) => Value::case("spans", Value::list(l, span_value)),
        Bytes::Inline(d) => Value::case("inline", Value::bytes(d.clone())),
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
                ("kind", predictor_kind_value(p.kind)),
                ("row-bytes", Value::uint(p.row_bytes)),
                ("bytes-per-pixel", Value::uint(p.bytes_per_pixel)),
            ]),
        ),
        Codec::Plugin(n) => Value::case("plugin", Value::uint(*n)),
    }
}

pub fn codecs_value(l: &[Codec]) -> Value {
    Value::list(l, codec_value)
}

/// An image codec's case name (`(psd built-in)`: the host's own, never a
/// plugin's).
pub fn image_codec_name(c: ImageCodec) -> &'static str {
    match c {
        ImageCodec::Png => "png",
        ImageCodec::JpegV1 => "jpeg-v1",
        ImageCodec::WebpV1 => "webp-v1",
        ImageCodec::Gif => "gif",
        ImageCodec::Bmp => "bmp",
        ImageCodec::Tiff => "tiff",
        ImageCodec::Qoi => "qoi",
        ImageCodec::PsdBuiltin => "(psd built-in)",
    }
}

pub fn encoded_image_value(i: &EncodedImage) -> Value {
    Value::record([
        ("data", bytes_value(&i.data)),
        ("pre", codecs_value(&i.pre)),
        ("codec", Value::text(image_codec_name(i.codec))),
        ("frame", Value::uint(i.frame)),
    ])
}

fn rgba8_value(c: &Rgba8) -> Value {
    Value::record([("r", Value::uint(c.r)), ("g", Value::uint(c.g)), ("b", Value::uint(c.b)), ("a", Value::uint(c.a))])
}

pub fn layout_value(l: &Layout) -> Value {
    Value::record([
        ("groups", Value::list(&l.groups, |g| Value::record([("channels", Value::list(&g.channels, |c| channel_value(*c))), ("depth", depth_value(g.depth))]))),
        ("row-padding", Value::uint(l.row_padding)),
        ("tint", Value::option(l.tint.as_ref().map(rgba8_value))),
        ("premultiplied", Value::Bool(l.premultiplied)),
    ])
}

fn tile_value(t: &Tile) -> Value {
    let content = match &t.content {
        TileContent::Raster(r) => {
            Value::case("raster", Value::record([("data", bytes_value(&r.data)), ("codecs", codecs_value(&r.codecs)), ("layout", layout_value(&r.layout))]))
        }
        TileContent::Image(i) => Value::case("image", encoded_image_value(i)),
        TileContent::Fill(c) => Value::case("fill", rgba8_value(c)),
        TileContent::Empty => Value::unit("empty"),
    };
    Value::record([
        ("x", Value::int(t.x.into())),
        ("y", Value::int(t.y.into())),
        ("width", Value::uint(t.width)),
        ("height", Value::uint(t.height)),
        ("content", content),
    ])
}

pub fn bitmap_value(b: &Bitmap) -> Value {
    let source = match &b.source {
        PixelSource::Empty => Value::unit("empty"),
        PixelSource::Image(p) => {
            Value::case("image", Value::record([("x", Value::int(p.x.into())), ("y", Value::int(p.y.into())), ("image", encoded_image_value(&p.image))]))
        }
        PixelSource::Tiles(t) => Value::case("tiles", Value::record([("tiles", Value::list(&t.tiles, tile_value)), ("overlap", overlap_value(t.overlap))])),
        PixelSource::Raw(r) => Value::case(
            "raw",
            Value::record([
                ("x", Value::int(r.x.into())),
                ("y", Value::int(r.y.into())),
                ("width", Value::uint(r.width)),
                ("height", Value::uint(r.height)),
                ("rgba", Value::bytes(r.rgba.clone())),
            ]),
        ),
    };
    Value::record([("size", Value::option(b.size.map(|(w, h)| Value::Array(vec![Value::uint(w), Value::uint(h)])))), ("source", source)])
}

fn meta_value(entries: &[MetaEntry]) -> Value {
    Value::list(entries, |e| {
        let value = match &e.value {
            MetaValue::Null => Value::unit("null"),
            MetaValue::Boolean(b) => Value::case("boolean", Value::Bool(*b)),
            MetaValue::Integer(i) => Value::case("integer", Value::int(*i)),
            MetaValue::Number(n) => Value::case("number", Value::Float(*n)),
            MetaValue::Text(s) => Value::case("text", Value::text(s.clone())),
            MetaValue::Sha256Of(b) => Value::case("sha256-of", bytes_value(b)),
            MetaValue::Json(s) => Value::case("json", Value::text(s.clone())),
        };
        Value::record([("key", Value::text(e.key.clone())), ("value", value)])
    })
}

fn warning_value(w: &Warning) -> Value {
    Value::record([("code", Value::text(w.code.clone())), ("message", Value::text(w.message.clone()))])
}

pub fn layer_value(l: &Layer) -> Value {
    let pixels = match &l.pixels {
        LayerPixels::None => Value::unit("none"),
        LayerPixels::Ready(b) => Value::case("ready", bitmap_value(b)),
        LayerPixels::Deferred(t) => Value::case("deferred", Value::uint(*t)),
    };
    Value::record([
        ("id", Value::text(l.id.clone())),
        ("stable-id", Value::text(l.stable_id.clone())),
        ("parent", Value::option(l.parent.map(Value::uint))),
        ("name", Value::text(l.name.clone())),
        ("kind", layer_kind_value(l.kind)),
        ("visible", Value::Bool(l.visible)),
        ("opacity", Value::Float(l.opacity)),
        ("blend", blend_mode_value(l.blend)),
        ("clip-to-below", Value::Bool(l.clip_to_below)),
        ("offset-x", Value::int(l.offset_x)),
        ("offset-y", Value::int(l.offset_y)),
        ("pixels", pixels),
        ("meta", meta_value(&l.meta)),
        ("semantic", meta_value(&l.semantic)),
        ("write", write_capability_value(l.write)),
    ])
}

pub fn structure_value(s: &Structure) -> Value {
    let profile = match &s.canvas.profile {
        ColorProfile::None => Value::unit("none"),
        ColorProfile::Embedded(b) => Value::case("embedded", bytes_value(b)),
        ColorProfile::Named(n) => Value::case("named", Value::text(n.clone())),
    };
    let composite = match &s.composite {
        Composite::Render(c) => Value::case("render", compositor_value(*c)),
        Composite::Image(b) => Value::case("image", bitmap_value(b)),
        Composite::None => Value::unit("none"),
    };
    Value::record([
        ("format", Value::text(s.format.clone())),
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
        ("file-meta", meta_value(&s.file_meta)),
        ("context", Value::bytes(s.context.clone())),
        ("warnings", Value::list(&s.warnings, warning_value)),
    ])
}

/// A `read-error` (`(host)`: an error the host made, never a plugin's).
pub fn read_error_value(e: &ReadError) -> Value {
    match e {
        ReadError::NotThisFormat(m) => Value::case("not-this-format", Value::text(m.clone())),
        ReadError::Corrupt(m) => Value::case("corrupt", Value::text(m.clone())),
        ReadError::Unsupported(m) => Value::case("unsupported", Value::text(m.clone())),
        ReadError::TooLarge(m) => Value::case("too-large", Value::text(m.clone())),
        ReadError::Plugin(m) => Value::case("(host)", Value::text(m.clone())),
    }
}

pub fn plugin_info_value(i: &PluginInfo) -> Value {
    Value::record([
        ("id", Value::text(i.id.clone())),
        ("version", Value::text(i.version.clone())),
        (
            "formats",
            Value::list(&i.formats, |f| {
                Value::record([
                    ("name", Value::text(f.name.clone())),
                    ("extensions", Value::list(&f.extensions, |e| Value::text(e.clone()))),
                    ("mime", Value::option(f.mime.clone().map(Value::Text))),
                ])
            }),
        ),
        ("probe-bytes", Value::uint(i.probe_bytes)),
        ("can-write", Value::Bool(i.can_write)),
    ])
}

// -- tmiki to plugin ---------------------------------------------------------------

pub fn layer_edit_value(e: &LayerEdit) -> Value {
    Value::record([
        ("stable-id", Value::text(e.stable_id.clone())),
        ("new-pixels", Value::option(e.new_pixels.map(Value::uint))),
        (
            "props",
            Value::list(&e.props, |p| match p {
                PropChange::Name(n) => Value::case("name", Value::text(n.clone())),
                PropChange::Visible(b) => Value::case("visible", Value::Bool(*b)),
                PropChange::Opacity(o) => Value::case("opacity", Value::Float(*o)),
                PropChange::Blend(b) => Value::case("blend", blend_mode_value(*b)),
                PropChange::Offset(x, y) => Value::case("offset", Value::Array(vec![Value::int(*x), Value::int(*y)])),
            }),
        ),
    ])
}

pub fn image_header_value(h: &ImageHeader) -> Value {
    Value::record([
        ("width", Value::uint(h.width)),
        ("height", Value::uint(h.height)),
        ("frames", Value::uint(h.frames)),
        ("has-alpha", Value::Bool(h.has_alpha)),
        ("dpi", Value::option(h.dpi.map(|(x, y)| Value::Array(vec![Value::Float(x), Value::Float(y)])))),
        ("icc", Value::Bool(h.icc)),
    ])
}

pub fn sql_value_value(v: &sqlite::SqlValue) -> Value {
    match v {
        sqlite::SqlValue::Null => Value::unit("null"),
        sqlite::SqlValue::Integer(i) => Value::case("integer", Value::int(*i)),
        sqlite::SqlValue::Real(f) => Value::case("real", Value::Float(*f)),
        sqlite::SqlValue::Text(s) => Value::case("text", Value::text(s.clone())),
        sqlite::SqlValue::Blob(b) => Value::case("blob", Value::bytes(b.clone())),
    }
}

pub fn element_value(e: &xml::Element) -> Value {
    Value::record([
        ("name", Value::text(e.name.clone())),
        ("namespace", Value::option(e.namespace.clone().map(Value::Text))),
        ("attributes", Value::list(&e.attributes, |(k, v)| Value::Array(vec![Value::text(k.clone()), Value::text(v.clone())]))),
        ("text", Value::text(e.text.clone())),
        ("parent", Value::option(e.parent.map(Value::uint))),
    ])
}
