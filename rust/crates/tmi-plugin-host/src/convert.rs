//! The bindgen types of `tmi:format/types` into `tmi_format::types`, one for
//! one (PLUGINS.md 8.1: "machine conversion, tested"). Nothing is checked
//! here; `tmi_format::validate` checks the result.

use tmi_format::types as t;

use crate::bindings::wit as w;

pub fn span(s: w::Span) -> t::Span {
    t::Span { offset: s.offset, len: s.len }
}

pub fn bytes(b: w::Bytes) -> t::Bytes {
    match b {
        w::Bytes::Span(s) => t::Bytes::Span(span(s)),
        w::Bytes::Spans(l) => t::Bytes::Spans(l.into_iter().map(span).collect()),
        w::Bytes::Inline(v) => t::Bytes::Inline(v),
    }
}

pub fn codec(c: w::Codec) -> t::Codec {
    match c {
        w::Codec::Zlib => t::Codec::Zlib,
        w::Codec::Deflate => t::Codec::Deflate,
        w::Codec::Gzip => t::Codec::Gzip,
        w::Codec::Zstd => t::Codec::Zstd,
        w::Codec::Lz4Block(n) => t::Codec::Lz4Block(n),
        w::Codec::Lzf(n) => t::Codec::Lzf(n),
        w::Codec::Packbits(n) => t::Codec::Packbits(n),
        w::Codec::Predictor(p) => t::Codec::Predictor(t::Predictor {
            kind: match p.kind {
                w::PredictorKind::PngRows => t::PredictorKind::PngRows,
                w::PredictorKind::TiffHorizontal => t::PredictorKind::TiffHorizontal,
            },
            row_bytes: p.row_bytes,
            bytes_per_pixel: p.bytes_per_pixel,
        }),
        w::Codec::Plugin(id) => t::Codec::Plugin(id),
    }
}

pub fn codecs(l: Vec<w::Codec>) -> Vec<t::Codec> {
    l.into_iter().map(codec).collect()
}

pub fn image_codec(c: w::ImageCodec) -> t::ImageCodec {
    match c {
        w::ImageCodec::Png => t::ImageCodec::Png,
        w::ImageCodec::JpegV1 => t::ImageCodec::JpegV1,
        w::ImageCodec::WebpV1 => t::ImageCodec::WebpV1,
        w::ImageCodec::Gif => t::ImageCodec::Gif,
        w::ImageCodec::Bmp => t::ImageCodec::Bmp,
        w::ImageCodec::Tiff => t::ImageCodec::Tiff,
        w::ImageCodec::Qoi => t::ImageCodec::Qoi,
    }
}

pub fn encoded_image(i: w::EncodedImage) -> t::EncodedImage {
    t::EncodedImage { data: bytes(i.data), pre: codecs(i.pre), codec: image_codec(i.codec), frame: i.frame }
}

fn channel(c: w::Channel) -> t::Channel {
    match c {
        w::Channel::R => t::Channel::R,
        w::Channel::G => t::Channel::G,
        w::Channel::B => t::Channel::B,
        w::Channel::A => t::Channel::A,
        w::Channel::Gray => t::Channel::Gray,
        w::Channel::Pad => t::Channel::Pad,
    }
}

fn depth(d: w::Depth) -> t::Depth {
    match d {
        w::Depth::OneBitMsb => t::Depth::OneBitMsb,
        w::Depth::Eight => t::Depth::Eight,
        w::Depth::SixteenBe => t::Depth::SixteenBe,
        w::Depth::SixteenLe => t::Depth::SixteenLe,
        w::Depth::Float32Le => t::Depth::Float32Le,
    }
}

fn rgba8(c: w::Rgba8) -> t::Rgba8 {
    t::Rgba8 { r: c.r, g: c.g, b: c.b, a: c.a }
}

pub fn layout(l: w::Layout) -> t::Layout {
    t::Layout {
        groups: l.groups.into_iter().map(|g| t::Group { channels: g.channels.into_iter().map(channel).collect(), depth: depth(g.depth) }).collect(),
        row_padding: l.row_padding,
        tint: l.tint.map(rgba8),
        premultiplied: l.premultiplied,
    }
}

fn tile(x: w::Tile) -> t::Tile {
    t::Tile {
        x: x.x,
        y: x.y,
        width: x.width,
        height: x.height,
        content: match x.content {
            w::TileContent::Raster(r) => t::TileContent::Raster(t::Raster { data: bytes(r.data), codecs: codecs(r.codecs), layout: layout(r.layout) }),
            w::TileContent::Image(i) => t::TileContent::Image(encoded_image(i)),
            w::TileContent::Fill(c) => t::TileContent::Fill(rgba8(c)),
            w::TileContent::Empty => t::TileContent::Empty,
        },
    }
}

pub fn bitmap(b: w::Bitmap) -> t::Bitmap {
    t::Bitmap {
        size: b.size,
        source: match b.source {
            w::PixelSource::Empty => t::PixelSource::Empty,
            w::PixelSource::Image(p) => t::PixelSource::Image(t::PlacedImage { x: p.x, y: p.y, image: encoded_image(p.image) }),
            w::PixelSource::Tiles(tl) => t::PixelSource::Tiles(t::Tiled {
                tiles: tl.tiles.into_iter().map(tile).collect(),
                overlap: match tl.overlap {
                    w::Overlap::Replace => t::Overlap::Replace,
                    w::Overlap::Over => t::Overlap::Over,
                    w::Overlap::Reject => t::Overlap::Reject,
                },
            }),
            w::PixelSource::Raw(r) => t::PixelSource::Raw(t::RawPixels { x: r.x, y: r.y, width: r.width, height: r.height, rgba: r.rgba }),
        },
    }
}

fn kind(k: w::LayerKind) -> t::LayerKind {
    match k {
        w::LayerKind::Raster => t::LayerKind::Raster,
        w::LayerKind::Folder => t::LayerKind::Folder,
        w::LayerKind::Text => t::LayerKind::Text,
        w::LayerKind::Vector => t::LayerKind::Vector,
        w::LayerKind::Adjustment => t::LayerKind::Adjustment,
        w::LayerKind::Fill => t::LayerKind::Fill,
        w::LayerKind::Paper => t::LayerKind::Paper,
        w::LayerKind::Mask => t::LayerKind::Mask,
        w::LayerKind::Stencil => t::LayerKind::Stencil,
        w::LayerKind::Unknown => t::LayerKind::Unknown,
    }
}

pub fn blend(b: w::BlendMode) -> t::BlendMode {
    use w::BlendMode as B;
    match b {
        B::Normal => t::BlendMode::Normal,
        B::PassThrough => t::BlendMode::PassThrough,
        B::Multiply => t::BlendMode::Multiply,
        B::Screen => t::BlendMode::Screen,
        B::Darken => t::BlendMode::Darken,
        B::Lighten => t::BlendMode::Lighten,
        B::Difference => t::BlendMode::Difference,
        B::Subtract => t::BlendMode::Subtract,
        B::LinearDodge => t::BlendMode::LinearDodge,
        B::LinearBurn => t::BlendMode::LinearBurn,
        B::ColorDodge => t::BlendMode::ColorDodge,
        B::ColorBurn => t::BlendMode::ColorBurn,
        B::Overlay => t::BlendMode::Overlay,
        B::HardLight => t::BlendMode::HardLight,
        B::SoftLight => t::BlendMode::SoftLight,
        B::Exclusion => t::BlendMode::Exclusion,
        B::Divide => t::BlendMode::Divide,
        B::DarkerColor => t::BlendMode::DarkerColor,
        B::LighterColor => t::BlendMode::LighterColor,
        B::Hue => t::BlendMode::Hue,
        B::Saturation => t::BlendMode::Saturation,
        B::Color => t::BlendMode::Color,
        B::Luminosity => t::BlendMode::Luminosity,
        B::HardMix => t::BlendMode::HardMix,
        B::LinearLight => t::BlendMode::LinearLight,
        B::VividLight => t::BlendMode::VividLight,
        B::PinLight => t::BlendMode::PinLight,
        B::GlowAdd => t::BlendMode::GlowAdd,
        B::GlowDodge => t::BlendMode::GlowDodge,
        B::Other => t::BlendMode::Other,
    }
}

fn meta(l: Vec<w::MetaEntry>) -> Vec<t::MetaEntry> {
    l.into_iter()
        .map(|e| t::MetaEntry {
            key: e.key,
            value: match e.value {
                w::MetaValue::Null => t::MetaValue::Null,
                w::MetaValue::Boolean(b) => t::MetaValue::Boolean(b),
                w::MetaValue::Integer(i) => t::MetaValue::Integer(i),
                w::MetaValue::Number(n) => t::MetaValue::Number(n),
                w::MetaValue::Text(s) => t::MetaValue::Text(s),
                w::MetaValue::Sha256Of(b) => t::MetaValue::Sha256Of(bytes(b)),
                w::MetaValue::Json(s) => t::MetaValue::Json(s),
            },
        })
        .collect()
}

fn layer(l: w::Layer) -> t::Layer {
    t::Layer {
        id: l.id,
        stable_id: l.stable_id,
        parent: l.parent,
        name: l.name,
        kind: kind(l.kind),
        visible: l.visible,
        opacity: l.opacity,
        blend: blend(l.blend),
        clip_to_below: l.clip_to_below,
        offset_x: l.offset_x,
        offset_y: l.offset_y,
        pixels: match l.pixels {
            w::LayerPixels::None => t::LayerPixels::None,
            w::LayerPixels::Ready(b) => t::LayerPixels::Ready(bitmap(b)),
            w::LayerPixels::Deferred(token) => t::LayerPixels::Deferred(token),
        },
        meta: meta(l.meta),
        semantic: meta(l.semantic),
        write: match l.write {
            w::WriteCapability::None => t::WriteCapability::None,
            w::WriteCapability::Pixels => t::WriteCapability::Pixels,
            w::WriteCapability::PixelsAndProps => t::WriteCapability::PixelsAndProps,
        },
    }
}

pub fn structure(s: w::Structure) -> t::Structure {
    t::Structure {
        format: s.format,
        canvas: t::Canvas {
            width: s.canvas.width,
            height: s.canvas.height,
            dpi: s.canvas.dpi,
            profile: match s.canvas.profile {
                w::ColorProfile::None => t::ColorProfile::None,
                w::ColorProfile::Embedded(b) => t::ColorProfile::Embedded(bytes(b)),
                w::ColorProfile::Named(n) => t::ColorProfile::Named(n),
            },
        },
        layers: s.layers.into_iter().map(layer).collect(),
        composite: match s.composite {
            w::Composite::Render(w::Compositor::GenericV1) => t::Composite::Render(t::Compositor::GenericV1),
            w::Composite::Image(b) => t::Composite::Image(bitmap(b)),
            w::Composite::None => t::Composite::None,
        },
        file_meta: meta(s.file_meta),
        context: s.context,
        warnings: s.warnings.into_iter().map(|x| t::Warning { code: x.code, message: shown(x.message) }).collect(),
    }
}

/// Text a plugin made that tmi will print: its terminal controls escaped
/// (tmi_format::safe_text; PLUGINS.md 11).
pub(crate) fn shown(m: String) -> String {
    match tmi_format::safe_text::message(&m) {
        std::borrow::Cow::Borrowed(_) => m,
        std::borrow::Cow::Owned(s) => s,
    }
}

pub fn read_error(e: w::ReadError) -> t::ReadError {
    match e {
        w::ReadError::NotThisFormat(m) => t::ReadError::NotThisFormat(shown(m)),
        w::ReadError::Corrupt(m) => t::ReadError::Corrupt(shown(m)),
        w::ReadError::Unsupported(m) => t::ReadError::Unsupported(shown(m)),
        w::ReadError::TooLarge(m) => t::ReadError::TooLarge(shown(m)),
    }
}

pub fn confidence(c: w::Confidence) -> t::Confidence {
    match c {
        w::Confidence::No => t::Confidence::No,
        w::Confidence::Maybe => t::Confidence::Maybe,
        w::Confidence::Yes => t::Confidence::Yes,
    }
}

pub fn plugin_info(i: w::PluginInfo) -> t::PluginInfo {
    t::PluginInfo {
        id: i.id,
        version: i.version,
        formats: i.formats.into_iter().map(|f| t::FormatInfo { name: f.name, extensions: f.extensions, mime: f.mime }).collect(),
        probe_bytes: i.probe_bytes,
        can_write: i.can_write,
    }
}

// -- the writer (P7) ---------------------------------------------------------

use crate::bindings::writer_export as wr;

fn blend_back(b: t::BlendMode) -> w::BlendMode {
    use t::BlendMode as B;
    match b {
        B::Normal => w::BlendMode::Normal,
        B::PassThrough => w::BlendMode::PassThrough,
        B::Multiply => w::BlendMode::Multiply,
        B::Screen => w::BlendMode::Screen,
        B::Darken => w::BlendMode::Darken,
        B::Lighten => w::BlendMode::Lighten,
        B::Difference => w::BlendMode::Difference,
        B::Subtract => w::BlendMode::Subtract,
        B::LinearDodge => w::BlendMode::LinearDodge,
        B::LinearBurn => w::BlendMode::LinearBurn,
        B::ColorDodge => w::BlendMode::ColorDodge,
        B::ColorBurn => w::BlendMode::ColorBurn,
        B::Overlay => w::BlendMode::Overlay,
        B::HardLight => w::BlendMode::HardLight,
        B::SoftLight => w::BlendMode::SoftLight,
        B::Exclusion => w::BlendMode::Exclusion,
        B::Divide => w::BlendMode::Divide,
        B::DarkerColor => w::BlendMode::DarkerColor,
        B::LighterColor => w::BlendMode::LighterColor,
        B::Hue => w::BlendMode::Hue,
        B::Saturation => w::BlendMode::Saturation,
        B::Color => w::BlendMode::Color,
        B::Luminosity => w::BlendMode::Luminosity,
        B::HardMix => w::BlendMode::HardMix,
        B::LinearLight => w::BlendMode::LinearLight,
        B::VividLight => w::BlendMode::VividLight,
        B::PinLight => w::BlendMode::PinLight,
        B::GlowAdd => w::BlendMode::GlowAdd,
        B::GlowDodge => w::BlendMode::GlowDodge,
        B::Other => w::BlendMode::Other,
    }
}

/// A property change into the WIT's (the host hands these to the plugin).
pub fn prop_change(p: &t::PropChange) -> wr::PropChange {
    match p {
        t::PropChange::Name(n) => wr::PropChange::Name(n.clone()),
        t::PropChange::Visible(v) => wr::PropChange::Visible(*v),
        t::PropChange::Opacity(o) => wr::PropChange::Opacity(*o),
        t::PropChange::Blend(b) => wr::PropChange::Blend(blend_back(*b)),
        t::PropChange::Offset(x, y) => wr::PropChange::Offset((*x, *y)),
    }
}

/// A `write-plan`: a blob piece is named by the host's number for it (the
/// handle's rep, which `tmi_format::write::stitch` checks).
pub fn write_plan(p: wr::WritePlan) -> t::WritePlan {
    t::WritePlan {
        pieces: p
            .pieces
            .into_iter()
            .map(|x| match x {
                wr::Piece::Source(s) => t::Piece::Source(span(s)),
                wr::Piece::Bytes(b) => t::Piece::Bytes(b),
                wr::Piece::Blob(b) => t::Piece::Blob(b.rep()),
            })
            .collect(),
        warnings: p.warnings.into_iter().map(|x| t::Warning { code: x.code, message: shown(x.message) }).collect(),
    }
}

pub fn write_error(e: wr::WriteError) -> t::WriteError {
    match e {
        wr::WriteError::Unsupported(m) => t::WriteError::Unsupported(m),
        wr::WriteError::Invalid(m) => t::WriteError::Invalid(m),
        wr::WriteError::Corrupt(m) => t::WriteError::Corrupt(m),
    }
}