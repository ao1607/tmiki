//! The built-in readers. P1 has one: the raster images (PNG, JPEG, GIF, BMP /
//! DIB, TIFF, WebP and the flattened composite of a PSD), which was
//! `tmi-import/src/raster.rs`. CLIP and MDP2 move behind the same trait in
//! P2 and P3 and stay on their own paths until then.
//!
//! The rule of P0e is kept whole: a file is an image exactly when
//! `tmi_image::import::open_for_import` reads it; otherwise `read_structure`
//! says `NotThisFormat` and the file is stored as an opaque blob. The reader
//! describes the result as a recipe (one layer, `image:0`, the whole file as
//! an `encoded-image`) and offers the pixels it has already decoded to the
//! host, so the file is decoded once.

use tmi_image::import::{open_for_import, Reject};
use tmi_image::{sniff_any, Format};

use crate::reader::{FormatReader, ImageKey, ReadCx, Source};
use crate::recipe::too_large;
use crate::types::*;

pub struct RasterReader {
    info: PluginInfo,
}

impl Default for RasterReader {
    fn default() -> Self {
        RasterReader::new()
    }
}

impl RasterReader {
    pub fn new() -> RasterReader {
        let fmt = |name: &str, ext: &[&str], mime: &str| FormatInfo {
            name: name.into(),
            extensions: ext.iter().map(|e| e.to_string()).collect(),
            mime: Some(mime.into()),
        };
        RasterReader {
            info: PluginInfo {
                id: "raster".into(),
                version: "0.1.0".into(),
                formats: vec![
                    fmt("PNG", &["png"], "image/png"),
                    fmt("JPEG", &["jpg", "jpeg"], "image/jpeg"),
                    fmt("GIF", &["gif"], "image/gif"),
                    fmt("BMP", &["bmp", "dib"], "image/bmp"),
                    fmt("TIFF", &["tif", "tiff"], "image/tiff"),
                    fmt("WebP", &["webp"], "image/webp"),
                    fmt("Photoshop", &["psd"], "image/vnd.adobe.photoshop"),
                ],
                probe_bytes: 64 << 10,
                can_write: false,
            },
        }
    }
}

fn is_png_or_jpeg(head: &[u8]) -> bool {
    head.starts_with(b"\x89PNG\r\n\x1a\n") || (head.len() >= 3 && head[0] == 0xFF && head[1] == 0xD8 && head[2] == 0xFF)
}

fn codec_of(f: Format) -> ImageCodec {
    match f {
        Format::Png => ImageCodec::Png,
        Format::Jpeg => ImageCodec::JpegV1,
        Format::Gif => ImageCodec::Gif,
        Format::Bmp | Format::Dib => ImageCodec::Bmp,
        Format::Tiff => ImageCodec::Tiff,
        Format::WebP => ImageCodec::WebpV1,
        Format::Psd => ImageCodec::PsdBuiltin,
    }
}

impl FormatReader for RasterReader {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    /// `Yes` for a signature tmiki's decoders know; `Maybe` for anything else,
    /// since the verdict is the decoders' (the fallback of the registry).
    fn probe(&self, _name: &str, head: &[u8], _size: u64) -> Confidence {
        if is_png_or_jpeg(head) || sniff_any(head).is_some() {
            Confidence::Yes
        } else {
            Confidence::Maybe
        }
    }

    fn builtin(&self) -> bool {
        true
    }

    fn read_structure(&self, src: &Source, cx: &ReadCx) -> Result<Structure, ReadError> {
        let accepted = match open_for_import(src.data, cx.max_pixels) {
            Ok(a) => a,
            Err(Reject::Opaque(why)) => return Err(ReadError::NotThisFormat(why)),
            Err(Reject::TooLarge(w, h)) => return Err(too_large(w, h)),
        };
        let codec = codec_of(accepted.format);
        let (width, height) = (accepted.image.width as u32, accepted.image.height as u32);
        let span = Span { offset: 0, len: src.data.len() as u64 };
        cx.offer_decoded(ImageKey { offset: 0, len: span.len, codec, frame: 0 }, accepted.image);
        let bitmap = Bitmap {
            size: None,
            source: PixelSource::Image(PlacedImage {
                x: 0,
                y: 0,
                image: EncodedImage { data: Bytes::Span(span), pre: vec![], codec, frame: 0 },
            }),
        };
        let layer = Layer {
            id: "image:0".into(),
            stable_id: "image:0".into(),
            parent: None,
            name: crate::path_stem(src.name).to_string(),
            kind: LayerKind::Raster,
            visible: true,
            opacity: 1.0,
            blend: BlendMode::Normal,
            clip_to_below: false,
            offset_x: 0,
            offset_y: 0,
            pixels: LayerPixels::Ready(bitmap.clone()),
            meta: vec![MetaEntry { key: "source_path_name".into(), value: MetaValue::Text(src.name.to_string()) }],
            semantic: vec![],
            write: WriteCapability::None,
        };
        Ok(Structure {
            // the name Pillow gives the format, lower case (the source_format of a record)
            format: accepted.format.name().to_string(),
            canvas: Canvas {
                width,
                height,
                dpi: accepted.dpi,
                // only that a profile exists is recorded, not its bytes
                profile: if accepted.icc { ColorProfile::Embedded(Bytes::Inline(Vec::new())) } else { ColorProfile::None },
            },
            layers: vec![layer],
            // the composite is the image itself
            composite: Composite::Image(bitmap),
            file_meta: vec![],
            context: vec![],
            warnings: vec![],
        })
    }

    /// importers.py: `jpeg` for a `.jpg` / `.jpeg` name or a JPEG, `png` for a
    /// `.png` name or a PNG, else the format's own name.
    fn source_format(&self, name: &str, structure: &Structure) -> String {
        let suffix = crate::path_suffix(name).to_lowercase();
        let fmt = structure.format.as_str();
        if suffix == ".jpg" || suffix == ".jpeg" || fmt == "jpeg" {
            "jpeg".into()
        } else if suffix == ".png" || fmt == "png" {
            "png".into()
        } else {
            fmt.into()
        }
    }
}
