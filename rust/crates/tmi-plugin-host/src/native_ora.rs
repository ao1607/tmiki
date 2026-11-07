//! The OpenRaster test plugin (`rust/plugins/ora`) built natively: the same
//! reader code, its values converted to `tmi_format::types` the way its wasm
//! glue (`plugins/ora/src/wasm.rs`) converts them to the WIT's, and its one
//! host call (`codecs.transform`) answered by the same function the wasm
//! host uses. PLUGINS.md 6.4 item 4 compares the two.

use tmi_format::reader::{FormatReader, FormatWriter, ReadCx, Source};
use tmi_format::write::WriteCx;
use tmi_format::types::*;
use tmi_plugin_ora::model as m;

pub struct OraNative {
    info: PluginInfo,
}

impl Default for OraNative {
    fn default() -> Self {
        let i = tmi_plugin_ora::info();
        OraNative {
            info: PluginInfo {
                id: i.id.into(),
                version: i.version.into(),
                formats: vec![FormatInfo {
                    name: i.format_name.into(),
                    extensions: i.extensions.iter().map(|e| e.to_string()).collect(),
                    mime: i.mime.map(String::from),
                }],
                probe_bytes: i.probe_bytes,
                can_write: i.can_write,
            },
        }
    }
}

struct NativeHost<'a> {
    src: &'a Source<'a>,
}

impl m::Host for NativeHost<'_> {
    fn size(&self) -> u64 {
        self.src.size()
    }

    fn read(&self, offset: u64, len: u32) -> Vec<u8> {
        self.src.read(offset, len).to_vec()
    }

    fn inflate(&self, member: &m::Member, max_out: u32) -> Result<Vec<u8>, String> {
        let cx = ReadCx::new(None);
        let env = tmi_format::recipe::DecodeEnv { source: self.src.data, reader: None, cx: &cx };
        let span = span_bytes(member.span);
        let data = tmi_format::recipe::transform::resolve(self.src.data, &span).map_err(|e| e.to_string())?;
        tmi_format::recipe::transform::apply_capped(&env, &[Codec::Deflate], data, max_out as usize)
            .map(|c| c.into_owned())
            .map_err(|e| e.to_string())
    }
}

fn span_bytes(s: m::Span) -> Bytes {
    Bytes::Span(Span { offset: s.offset, len: s.len })
}

fn error(e: m::Error) -> ReadError {
    match e {
        m::Error::NotThisFormat(s) => ReadError::NotThisFormat(s),
        m::Error::Corrupt(s) => ReadError::Corrupt(s),
        m::Error::Unsupported(s) => ReadError::Unsupported(s),
        m::Error::TooLarge(s) => ReadError::TooLarge(s),
    }
}

fn png(member: m::Member) -> Bitmap {
    let image = EncodedImage {
        data: span_bytes(member.span),
        pre: if member.deflate { vec![Codec::Deflate] } else { vec![] },
        codec: ImageCodec::Png,
        frame: 0,
    };
    Bitmap { size: None, source: PixelSource::Image(PlacedImage { x: 0, y: 0, image }) }
}

fn blend(b: m::Blend) -> BlendMode {
    use m::Blend as B;
    match b {
        B::Normal => BlendMode::Normal,
        B::PassThrough => BlendMode::PassThrough,
        B::Multiply => BlendMode::Multiply,
        B::Screen => BlendMode::Screen,
        B::Overlay => BlendMode::Overlay,
        B::Darken => BlendMode::Darken,
        B::Lighten => BlendMode::Lighten,
        B::ColorDodge => BlendMode::ColorDodge,
        B::ColorBurn => BlendMode::ColorBurn,
        B::HardLight => BlendMode::HardLight,
        B::SoftLight => BlendMode::SoftLight,
        B::Difference => BlendMode::Difference,
        B::LinearDodge => BlendMode::LinearDodge,
        B::Hue => BlendMode::Hue,
        B::Saturation => BlendMode::Saturation,
        B::Color => BlendMode::Color,
        B::Luminosity => BlendMode::Luminosity,
        B::Other => BlendMode::Other,
    }
}

fn texts(entries: Vec<(String, String)>) -> Vec<MetaEntry> {
    entries.into_iter().map(|(key, v)| MetaEntry { key, value: MetaValue::Text(v) }).collect()
}

fn layer(l: m::Layer) -> Layer {
    Layer {
        id: l.id,
        stable_id: l.stable_id,
        parent: l.parent,
        name: l.name,
        kind: match l.kind {
            m::Kind::Raster => LayerKind::Raster,
            m::Kind::Folder => LayerKind::Folder,
        },
        visible: l.visible,
        opacity: l.opacity,
        blend: blend(l.blend),
        clip_to_below: false,
        offset_x: l.offset_x,
        offset_y: l.offset_y,
        pixels: match l.pixels {
            m::Pixels::None => LayerPixels::None,
            m::Pixels::Ready(member) => LayerPixels::Ready(png(member)),
            m::Pixels::Deferred(token) => LayerPixels::Deferred(token),
        },
        meta: texts(l.meta),
        semantic: texts(l.semantic),
        write: if l.writable { WriteCapability::Pixels } else { WriteCapability::None },
    }
}

impl FormatReader for OraNative {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn probe(&self, _name: &str, head: &[u8], _size: u64) -> Confidence {
        match tmi_plugin_ora::probe(head) {
            m::Confidence::No => Confidence::No,
            m::Confidence::Maybe => Confidence::Maybe,
            m::Confidence::Yes => Confidence::Yes,
        }
    }

    fn read_structure(&self, src: &Source, _cx: &ReadCx) -> Result<Structure, ReadError> {
        let s = tmi_plugin_ora::read_structure(&NativeHost { src }).map_err(error)?;
        Ok(Structure {
            format: tmi_plugin_ora::info().format_name.into(),
            canvas: Canvas { width: s.width, height: s.height, dpi: s.dpi, profile: ColorProfile::None },
            layers: s.layers.into_iter().map(layer).collect(),
            composite: match s.composite {
                m::Composite::Image(member) => Composite::Image(png(member)),
                m::Composite::Render => Composite::Render(Compositor::GenericV1),
            },
            file_meta: vec![],
            context: s.context,
            warnings: s.warnings.into_iter().map(|(code, message)| Warning { code, message }).collect(),
        })
    }

    fn read_layer(&self, _src: &Source, context: &[u8], token: u32, _cx: &ReadCx) -> Result<Bitmap, ReadError> {
        tmi_plugin_ora::read_layer(context, token).map(png).map_err(error)
    }
}

/// The native writer's host: `WriteCx` itself (its numbers are the
/// handles the shared code uses, as the wasm glue's are).
struct NativeWriteHost<'a> {
    src: &'a Source<'a>,
    cx: &'a mut WriteCx,
}

impl m::Host for NativeWriteHost<'_> {
    fn size(&self) -> u64 {
        self.src.size()
    }

    fn read(&self, offset: u64, len: u32) -> Vec<u8> {
        self.src.read(offset, len).to_vec()
    }

    fn inflate(&self, member: &m::Member, max_out: u32) -> Result<Vec<u8>, String> {
        NativeHost { src: self.src }.inflate(member, max_out)
    }
}

impl m::WriteHost for NativeWriteHost<'_> {
    fn encode_png(&mut self, pixels: u32) -> Result<u32, String> {
        self.cx.encode_image(pixels, &ImageEncoding::PngV1)
    }

    fn blob_info(&self, blob: u32) -> (u64, u32) {
        self.cx.blob(blob).expect("a blob the host made")
    }

    fn composite_after_edit(&mut self) -> Result<u32, String> {
        self.cx.composite_after_edit()
    }

    fn thumbnail(&mut self, pixels: u32, max_side: u32) -> u32 {
        self.cx.thumbnail(pixels, max_side).expect("a thumbnail of the host's pixels")
    }
}

impl FormatWriter for OraNative {
    fn write_layers(&self, src: &Source, edits: &[LayerEdit], cx: &mut WriteCx) -> Result<WritePlan, WriteError> {
        let mut list = Vec::with_capacity(edits.len());
        for e in edits {
            if !e.props.is_empty() {
                return Err(WriteError::Unsupported("ora writes layer pixels only, not properties".into()));
            }
            let px = e.new_pixels.ok_or_else(|| WriteError::Invalid(format!("{}: no new pixels", e.stable_id)))?;
            if px as usize >= cx.pixel_count() {
                return Err(WriteError::Invalid(format!("{}: pixels {px} is not given", e.stable_id)));
            }
            list.push(m::Edit { stable_id: e.stable_id.clone(), pixels: px });
        }
        let plan = tmi_plugin_ora::write::write_layers(&mut NativeWriteHost { src, cx }, &list).map_err(|e| match e {
            m::WriteFail::Unsupported(s) => WriteError::Unsupported(s),
            m::WriteFail::Invalid(s) => WriteError::Invalid(s),
            m::WriteFail::Corrupt(s) => WriteError::Corrupt(s),
        })?;
        Ok(WritePlan {
            pieces: plan
                .pieces
                .into_iter()
                .map(|p| match p {
                    m::Piece::Source(s) => Piece::Source(Span { offset: s.offset, len: s.len }),
                    m::Piece::Bytes(b) => Piece::Bytes(b),
                    m::Piece::Blob(id) => Piece::Blob(id),
                })
                .collect(),
            warnings: plan.warnings.into_iter().map(|(code, message)| Warning { code, message }).collect(),
        })
    }
}