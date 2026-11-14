//! The plugin itself: [`Ora`] implements `tmi_plugin_sdk::Plugin` over the
//! reader and writer of this crate, the same code for the component
//! (`export_plugin!` in lib.rs, `wasm32-wasip2`) and for the development
//! plugin (`src/bin/ora-dev.rs`, `tmiki plugin dev`).

use crate::model as m;
use tmi_plugin_sdk::{self as sdk, codecs, edit, Plugin, SourceFile};

pub struct Ora;

struct SdkHost<'a, 'b> {
    src: &'a SourceFile<'b>,
}

impl m::Host for SdkHost<'_, '_> {
    fn size(&self) -> u64 {
        self.src.size()
    }

    fn read(&self, offset: u64, len: u32) -> Vec<u8> {
        self.src.read(offset, len)
    }

    fn inflate(&self, member: &m::Member, max_out: u32) -> Result<Vec<u8>, String> {
        codecs::transform(&span_bytes(member.span), &[sdk::Codec::Deflate], max_out)
    }
}

fn span_bytes(s: m::Span) -> sdk::Bytes {
    sdk::Bytes::Span(sdk::Span { offset: s.offset, len: s.len })
}

fn error(e: m::Error) -> sdk::ReadError {
    match e {
        m::Error::NotThisFormat(s) => sdk::ReadError::NotThisFormat(s),
        m::Error::Corrupt(s) => sdk::ReadError::Corrupt(s),
        m::Error::Unsupported(s) => sdk::ReadError::Unsupported(s),
        m::Error::TooLarge(s) => sdk::ReadError::TooLarge(s),
    }
}

fn png(member: m::Member) -> sdk::Bitmap {
    let image = sdk::EncodedImage {
        data: span_bytes(member.span),
        pre: if member.deflate { vec![sdk::Codec::Deflate] } else { vec![] },
        codec: sdk::ImageCodec::Png,
        frame: 0,
    };
    sdk::Bitmap { size: None, source: sdk::PixelSource::Image(sdk::PlacedImage { x: 0, y: 0, image }) }
}

fn blend(b: m::Blend) -> sdk::BlendMode {
    use m::Blend::*;
    match b {
        Normal => sdk::BlendMode::Normal,
        PassThrough => sdk::BlendMode::PassThrough,
        Multiply => sdk::BlendMode::Multiply,
        Screen => sdk::BlendMode::Screen,
        Overlay => sdk::BlendMode::Overlay,
        Darken => sdk::BlendMode::Darken,
        Lighten => sdk::BlendMode::Lighten,
        ColorDodge => sdk::BlendMode::ColorDodge,
        ColorBurn => sdk::BlendMode::ColorBurn,
        HardLight => sdk::BlendMode::HardLight,
        SoftLight => sdk::BlendMode::SoftLight,
        Difference => sdk::BlendMode::Difference,
        LinearDodge => sdk::BlendMode::LinearDodge,
        Hue => sdk::BlendMode::Hue,
        Saturation => sdk::BlendMode::Saturation,
        Color => sdk::BlendMode::Color,
        Luminosity => sdk::BlendMode::Luminosity,
        Other => sdk::BlendMode::Other,
    }
}

fn texts(entries: Vec<(String, String)>) -> Vec<sdk::MetaEntry> {
    entries.into_iter().map(|(key, v)| sdk::MetaEntry { key, value: sdk::MetaValue::Text(v) }).collect()
}

fn layer(l: m::Layer) -> sdk::Layer {
    sdk::Layer {
        id: l.id,
        stable_id: l.stable_id,
        parent: l.parent,
        name: l.name,
        kind: match l.kind {
            m::Kind::Raster => sdk::LayerKind::Raster,
            m::Kind::Folder => sdk::LayerKind::Folder,
        },
        visible: l.visible,
        opacity: l.opacity,
        blend: blend(l.blend),
        clip_to_below: false,
        offset_x: l.offset_x,
        offset_y: l.offset_y,
        pixels: match l.pixels {
            m::Pixels::None => sdk::LayerPixels::None,
            m::Pixels::Ready(member) => sdk::LayerPixels::Ready(png(member)),
            m::Pixels::Deferred(token) => sdk::LayerPixels::Deferred(token),
        },
        meta: texts(l.meta),
        semantic: texts(l.semantic),
        write: if l.writable { sdk::WriteCapability::Pixels } else { sdk::WriteCapability::None },
    }
}

/// The writer's host: the source file and the `edit` resources, which the
/// shared code names by number (the edits' pixels first, then what
/// `composite-after-edit` and `thumbnail` made; blobs in the order made).
struct SdkWriteHost<'a, 'b> {
    src: &'a SourceFile<'b>,
    given: Vec<edit::Pixels>,
    made: Vec<edit::Pixels>,
    blobs: Vec<Option<edit::Blob>>,
}

impl SdkWriteHost<'_, '_> {
    fn pixels(&self, id: u32) -> &edit::Pixels {
        let i = id as usize;
        if i < self.given.len() {
            &self.given[i]
        } else {
            &self.made[i - self.given.len()]
        }
    }

    fn handle(&self) -> u32 {
        (self.given.len() + self.made.len()) as u32
    }
}

impl m::Host for SdkWriteHost<'_, '_> {
    fn size(&self) -> u64 {
        self.src.size()
    }

    fn read(&self, offset: u64, len: u32) -> Vec<u8> {
        self.src.read(offset, len)
    }

    fn inflate(&self, member: &m::Member, max_out: u32) -> Result<Vec<u8>, String> {
        SdkHost { src: self.src }.inflate(member, max_out)
    }
}

impl m::WriteHost for SdkWriteHost<'_, '_> {
    fn encode_png(&mut self, pixels: u32) -> Result<u32, String> {
        let blob = edit::encode_image(self.pixels(pixels), &sdk::ImageEncoding::PngV1)?;
        self.blobs.push(Some(blob));
        Ok((self.blobs.len() - 1) as u32)
    }

    fn blob_info(&self, blob: u32) -> (u64, u32) {
        let b = self.blobs[blob as usize].as_ref().expect("a blob not yet placed");
        (b.len(), b.crc32())
    }

    fn composite_after_edit(&mut self) -> Result<u32, String> {
        let px = edit::composite_after_edit()?;
        let id = self.handle();
        self.made.push(px);
        Ok(id)
    }

    fn thumbnail(&mut self, pixels: u32, max_side: u32) -> u32 {
        let px = edit::thumbnail(self.pixels(pixels), max_side);
        let id = self.handle();
        self.made.push(px);
        id
    }
}

/// The development plugin misbehaving on purpose (tmi-plugin-host's tests
/// of `tmiki plugin dev`): a stored `stack.xml` whose `<image>` carries
/// `tmi-test-dev-fault="..."` makes `read-structure` hang, panic, exit, print
/// to stdout, call a host function wrongly, or answer with a value that
/// does not fit the WIT.
#[cfg(feature = "dev")]
fn dev_fault(src: &SourceFile) {
    let head = src.read(0, 4096);
    let marker = b"tmi-test-dev-fault=\"";
    let Some(at) = head.windows(marker.len()).position(|w| w == marker) else { return };
    let rest = &head[at + marker.len()..];
    let what = String::from_utf8_lossy(&rest[..rest.iter().position(|&b| b == b'"').unwrap_or(0)]).into_owned();
    match what.as_str() {
        "hang" => std::thread::sleep(std::time::Duration::from_secs(3600)),
        "panic" => panic!("the test fault asked for a panic"),
        "exit" => std::process::exit(3),
        "stdout" => {
            use std::io::Write;
            let mut out = std::io::stdout();
            let _ = out.write_all(b"hello from the plugin\n");
            let _ = out.flush();
            std::thread::sleep(std::time::Duration::from_secs(3600));
        }
        "bad-handle" => {
            // pixels that were never given: tmiki traps the import
            let _ = codecs::crc32(&sdk::Bytes::Span(sdk::Span { offset: u64::MAX, len: 1 }));
        }
        _ => {}
    }
}

fn write_error(e: m::WriteFail) -> sdk::WriteError {
    match e {
        m::WriteFail::Unsupported(s) => sdk::WriteError::Unsupported(s),
        m::WriteFail::Invalid(s) => sdk::WriteError::Invalid(s),
        m::WriteFail::Corrupt(s) => sdk::WriteError::Corrupt(s),
    }
}

impl Plugin for Ora {
    fn info() -> sdk::PluginInfo {
        let i = crate::info();
        sdk::PluginInfo {
            id: i.id.into(),
            version: i.version.into(),
            formats: vec![sdk::FormatInfo {
                name: i.format_name.into(),
                extensions: i.extensions.iter().map(|e| e.to_string()).collect(),
                mime: i.mime.map(String::from),
            }],
            probe_bytes: i.probe_bytes,
            can_write: i.can_write,
        }
    }

    fn probe(_name: &str, head: &[u8], _size: u64) -> sdk::Confidence {
        match crate::probe(head) {
            m::Confidence::No => sdk::Confidence::No,
            m::Confidence::Maybe => sdk::Confidence::Maybe,
            m::Confidence::Yes => sdk::Confidence::Yes,
        }
    }

    fn read_structure(src: &SourceFile) -> Result<sdk::Structure, sdk::ReadError> {
        #[cfg(feature = "dev")]
        dev_fault(src);
        let s = crate::read_structure(&SdkHost { src }).map_err(error)?;
        Ok(sdk::Structure {
            format: crate::info().format_name.into(),
            canvas: sdk::Canvas { width: s.width, height: s.height, dpi: s.dpi, profile: sdk::ColorProfile::None },
            layers: s.layers.into_iter().map(layer).collect(),
            composite: match s.composite {
                m::Composite::Image(member) => sdk::Composite::Image(png(member)),
                m::Composite::Render => sdk::Composite::Render(sdk::Compositor::GenericV1),
            },
            file_meta: vec![],
            context: s.context,
            warnings: s.warnings.into_iter().map(|(code, message)| sdk::Warning { code, message }).collect(),
        })
    }

    fn read_layer(_src: &SourceFile, context: &[u8], token: u32) -> Result<sdk::Bitmap, sdk::ReadError> {
        crate::read_layer(context, token).map(png).map_err(error)
    }

    fn decode_chunk(_codec_id: u32, _input: &[u8], _out_len: u32) -> Result<Vec<u8>, sdk::ReadError> {
        Err(sdk::ReadError::Unsupported("ora has no codec of its own".into()))
    }

    fn write_layers(src: &SourceFile, edits: Vec<sdk::LayerEdit>, pixels: Vec<edit::Pixels>) -> Result<sdk::WritePlan, sdk::WriteError> {
        let mut list = Vec::with_capacity(edits.len());
        for e in edits {
            if !e.props.is_empty() {
                return Err(sdk::WriteError::Unsupported("ora writes layer pixels only, not properties".into()));
            }
            let px = e.new_pixels.ok_or_else(|| sdk::WriteError::Invalid(format!("{}: no new pixels", e.stable_id)))?;
            if px as usize >= pixels.len() {
                return Err(sdk::WriteError::Invalid(format!("{}: pixels {px} is not given", e.stable_id)));
            }
            list.push(m::Edit { stable_id: e.stable_id, pixels: px });
        }
        let mut host = SdkWriteHost { src, given: pixels, made: Vec::new(), blobs: Vec::new() };
        let plan = crate::write::write_layers(&mut host, &list).map_err(write_error)?;
        let mut pieces = Vec::with_capacity(plan.pieces.len());
        for p in plan.pieces {
            pieces.push(match p {
                m::Piece::Source(s) => sdk::Piece::Source(sdk::Span { offset: s.offset, len: s.len }),
                m::Piece::Bytes(b) => sdk::Piece::Bytes(b),
                m::Piece::Blob(id) => {
                    let blob = host.blobs.get_mut(id as usize).and_then(Option::take);
                    sdk::Piece::Blob(blob.ok_or_else(|| sdk::WriteError::Invalid(format!("blob {id} placed twice")))?)
                }
            });
        }
        Ok(sdk::WritePlan { pieces, warnings: plan.warnings.into_iter().map(|(code, message)| sdk::Warning { code, message }).collect() })
    }
}
