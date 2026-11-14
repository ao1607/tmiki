//! The `pixel_source` key of a plugin's layer (PLUGINS.md 3.3): a plugin
//! never names its own key (it could then pin an old `pixel_sha256` to new
//! pixels); the host makes it from the recipe. The recipe is written out in
//! a fixed text form with every byte range (`span`, `spans`, `inline`, raw
//! pixels) replaced by a numbered payload, so the offsets drop out and only
//! what the pixels are decoded from counts: `pixel_source_key("recipe",
//! <text>, payloads)`. A recipe that runs the plugin's own codec also names
//! the component (its decoding is the component's code).
//!
//! The text form is tmiki's and fixed (a record holds the key): never derive
//! it from `Debug`.

use crate::types::*;

struct Writer<'a> {
    source: &'a [u8],
    text: String,
    payloads: Vec<Vec<&'a [u8]>>,
    component_sha256: &'a str,
}

impl<'a> Writer<'a> {
    fn push(&mut self, s: &str) {
        self.text.push_str(s);
    }

    fn payload(&mut self, pieces: Vec<&'a [u8]>) {
        let n = self.payloads.len();
        self.payloads.push(pieces);
        self.text.push_str(&format!("${n}"));
    }

    fn span(&self, s: &Span) -> &'a [u8] {
        let start = (s.offset as usize).min(self.source.len());
        let end = start.saturating_add(s.len as usize).min(self.source.len());
        &self.source[start..end]
    }

    fn bytes(&mut self, b: &'a Bytes) {
        let pieces = match b {
            Bytes::Span(s) => vec![self.span(s)],
            Bytes::Spans(list) => list.iter().map(|s| self.span(s)).collect(),
            Bytes::Inline(data) => vec![data.as_slice()],
        };
        self.payload(pieces);
    }

    fn codec(&mut self, c: &Codec) {
        let t = match c {
            Codec::Zlib => "zlib".to_string(),
            Codec::Deflate => "deflate".to_string(),
            Codec::Gzip => "gzip".to_string(),
            Codec::Zstd => "zstd".to_string(),
            Codec::Lz4Block(n) => format!("lz4-block:{n}"),
            Codec::Lzf(n) => format!("lzf:{n}"),
            Codec::Packbits(n) => format!("packbits:{n}"),
            Codec::Predictor(p) => format!(
                "predictor:{}:{}:{}",
                match p.kind {
                    PredictorKind::PngRows => "png-rows",
                    PredictorKind::TiffHorizontal => "tiff-horizontal",
                },
                p.row_bytes,
                p.bytes_per_pixel
            ),
            Codec::Plugin(id) => format!("plugin:{}:{id}", self.component_sha256),
        };
        self.push(&t);
    }

    fn codecs(&mut self, list: &[Codec]) {
        self.push("[");
        for (i, c) in list.iter().enumerate() {
            if i > 0 {
                self.push(",");
            }
            self.codec(c);
        }
        self.push("]");
    }

    fn image(&mut self, e: &'a EncodedImage) {
        let codec = match e.codec {
            ImageCodec::Png => "png",
            ImageCodec::JpegV1 => "jpeg-v1",
            ImageCodec::WebpV1 => "webp-v1",
            ImageCodec::Gif => "gif",
            ImageCodec::Bmp => "bmp",
            ImageCodec::Tiff => "tiff",
            ImageCodec::Qoi => "qoi",
            ImageCodec::PsdBuiltin => "psd-builtin",
        };
        self.push(&format!("image({codec},frame={},pre=", e.frame));
        self.codecs(&e.pre);
        self.push(",data=");
        self.bytes(&e.data);
        self.push(")");
    }

    fn rgba(&mut self, c: &Rgba8) {
        self.push(&format!("#{:02x}{:02x}{:02x}{:02x}", c.r, c.g, c.b, c.a));
    }

    fn layout(&mut self, l: &Layout) {
        self.push("layout(groups=[");
        for (i, g) in l.groups.iter().enumerate() {
            if i > 0 {
                self.push(",");
            }
            let chans: Vec<&str> = g
                .channels
                .iter()
                .map(|c| match c {
                    Channel::R => "r",
                    Channel::G => "g",
                    Channel::B => "b",
                    Channel::A => "a",
                    Channel::Gray => "gray",
                    Channel::Pad => "pad",
                })
                .collect();
            let depth = match g.depth {
                Depth::OneBitMsb => "1-msb",
                Depth::Eight => "8",
                Depth::SixteenBe => "16-be",
                Depth::SixteenLe => "16-le",
                Depth::Float32Le => "f32-le",
            };
            self.push(&format!("{}/{depth}", chans.join("+")));
        }
        self.push(&format!("],pad={},tint=", l.row_padding));
        match &l.tint {
            Some(t) => self.rgba(t),
            None => self.push("none"),
        }
        self.push(&format!(",premultiplied={})", l.premultiplied));
    }

    fn bitmap(&mut self, b: &'a Bitmap) {
        match b.size {
            Some((w, h)) => self.push(&format!("bitmap({w}x{h},")),
            None => self.push("bitmap(none,"),
        }
        match &b.source {
            PixelSource::Empty => self.push("empty"),
            PixelSource::Image(p) => {
                self.push(&format!("at({},{},", p.x, p.y));
                self.image(&p.image);
                self.push(")");
            }
            PixelSource::Raw(r) => {
                self.push(&format!("raw({},{},{}x{},", r.x, r.y, r.width, r.height));
                self.payload(vec![r.rgba.as_slice()]);
                self.push(")");
            }
            PixelSource::Tiles(t) => {
                let overlap = match t.overlap {
                    Overlap::Replace => "replace",
                    Overlap::Over => "over",
                    Overlap::Reject => "reject",
                };
                self.push(&format!("tiles({overlap},["));
                for (i, tile) in t.tiles.iter().enumerate() {
                    if i > 0 {
                        self.push(",");
                    }
                    self.push(&format!("tile({},{},{}x{},", tile.x, tile.y, tile.width, tile.height));
                    match &tile.content {
                        TileContent::Empty => self.push("empty"),
                        TileContent::Fill(c) => {
                            self.push("fill(");
                            self.rgba(c);
                            self.push(")");
                        }
                        TileContent::Image(e) => self.image(e),
                        TileContent::Raster(r) => {
                            self.push("raster(");
                            self.codecs(&r.codecs);
                            self.push(",");
                            self.layout(&r.layout);
                            self.push(",data=");
                            self.bytes(&r.data);
                            self.push(")");
                        }
                    }
                    self.push(")");
                }
                self.push("])");
            }
        }
        self.push(")");
    }
}

/// The `pixel_source` of a layer whose pixels `bitmap` describes, in the
/// file `source`, read by the component `component_sha256`.
pub fn pixel_source(source: &[u8], bitmap: &Bitmap, component_sha256: &str) -> String {
    let mut w = Writer { source, text: String::new(), payloads: Vec::new(), component_sha256 };
    w.bitmap(bitmap);
    let payloads: Vec<&[&[u8]]> = w.payloads.iter().map(|p| p.as_slice()).collect();
    tmi_import::pixel_source_key("recipe", &w.text, &payloads)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raster(span: Span) -> Bitmap {
        Bitmap {
            size: Some((4, 2)),
            source: PixelSource::Tiles(Tiled {
                tiles: vec![Tile {
                    x: 0,
                    y: 0,
                    width: 4,
                    height: 2,
                    content: TileContent::Raster(Raster {
                        data: Bytes::Span(span),
                        codecs: vec![Codec::Zlib],
                        layout: Layout { groups: vec![Group { channels: vec![Channel::A], depth: Depth::Eight }], row_padding: 0, tint: None, premultiplied: false },
                    }),
                }],
                overlap: Overlap::Over,
            }),
        }
    }

    #[test]
    fn offsets_do_not_count_bytes_do() {
        let file = b"xxABCDyyABCDzzABCE";
        let a = pixel_source(file, &raster(Span { offset: 2, len: 4 }), "c");
        let b = pixel_source(file, &raster(Span { offset: 8, len: 4 }), "c");
        let c = pixel_source(file, &raster(Span { offset: 14, len: 4 }), "c");
        assert_eq!(a, b, "the same bytes at another offset");
        assert_ne!(a, c, "other bytes");
        // the component counts only for a plugin codec
        assert_eq!(a, pixel_source(file, &raster(Span { offset: 2, len: 4 }), "d"));
        let mut p = raster(Span { offset: 2, len: 4 });
        if let PixelSource::Tiles(t) = &mut p.source {
            if let TileContent::Raster(r) = &mut t.tiles[0].content {
                r.codecs = vec![Codec::Plugin(1)];
            }
        }
        assert_ne!(pixel_source(file, &p, "c"), pixel_source(file, &p, "d"));
        // a fixed text form: a change here changes every plugin record
        assert_eq!(a, GOLDEN);
    }

    const GOLDEN: &str = "d52c2be88b207dacfa3fcb52b0c74ea988ce44a174451a502866e99a57e299ee";
}
