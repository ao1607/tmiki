//! The Rust types of `wit/tmi-format.wit`'s `types` interface, one for one
//! (PLUGINS.md 8.1). A wasm plugin's bindgen types convert into these
//! mechanically (P5); built-in readers build them directly.
//!
//! WIT names become Rust names in the usual way (`stable-id` is `stable_id`,
//! `lz4-block` is `Lz4Block`); a field called `type` or `box` does not occur.

/// A part of the source file (absolute offset).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub offset: u64,
    pub len: u64,
}

/// Bytes the host takes out of the file without going through the plugin.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Bytes {
    Span(Span),
    /// A concatenation (a split stream).
    Spans(Vec<Span>),
    /// Made by the plugin (up to 64 MiB each).
    Inline(Vec<u8>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PredictorKind {
    /// Each row is one filter-type byte, then the row (PNG's five filters).
    PngRows,
    /// Each row is stored as differences of the sample `bytes_per_pixel`
    /// bytes before it (TIFF predictor 2, bytewise).
    TiffHorizontal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Predictor {
    pub kind: PredictorKind,
    pub row_bytes: u32,
    pub bytes_per_pixel: u8,
}

/// A byte-string transform. A list of them is applied in order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    Zlib,
    Deflate,
    Gzip,
    Zstd,
    /// The size after expansion.
    Lz4Block(u32),
    Lzf(u32),
    /// The size after expansion.
    Packbits(u32),
    Predictor(Predictor),
    /// The plugin's own codec: the host calls `reader.decode-chunk`.
    Plugin(u32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ImageCodec {
    Png,
    JpegV1,
    WebpV1,
    Gif,
    Bmp,
    Tiff,
    Qoi,
    /// Not in the WIT: the flattened composite of a Photoshop file, which
    /// stays a built-in (PLUGINS.md Q4). A plugin cannot name it.
    PsdBuiltin,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncodedImage {
    pub data: Bytes,
    /// Transforms stripped before the image.
    pub pre: Vec<Codec>,
    pub codec: ImageCodec,
    pub frame: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Channel {
    R,
    G,
    B,
    A,
    Gray,
    Pad,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Depth {
    OneBitMsb,
    Eight,
    SixteenBe,
    SixteenLe,
    Float32Le,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Group {
    pub channels: Vec<Channel>,
    pub depth: Depth,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgba8 {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    pub groups: Vec<Group>,
    pub row_padding: u32,
    pub tint: Option<Rgba8>,
    pub premultiplied: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Raster {
    pub data: Bytes,
    pub codecs: Vec<Codec>,
    pub layout: Layout,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TileContent {
    Raster(Raster),
    Image(EncodedImage),
    Fill(Rgba8),
    Empty,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tile {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub content: TileContent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Overlap {
    Replace,
    Over,
    Reject,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tiled {
    pub tiles: Vec<Tile>,
    pub overlap: Overlap,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlacedImage {
    pub x: i32,
    pub y: i32,
    pub image: EncodedImage,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawPixels {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PixelSource {
    Empty,
    Image(PlacedImage),
    Tiles(Tiled),
    Raw(RawPixels),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bitmap {
    pub size: Option<(u32, u32)>,
    pub source: PixelSource,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayerKind {
    Raster,
    Folder,
    Text,
    Vector,
    Adjustment,
    Fill,
    Paper,
    Mask,
    Stencil,
    Unknown,
}

impl LayerKind {
    /// The `kind` string a record holds.
    pub fn name(self) -> &'static str {
        match self {
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
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlendMode {
    Normal,
    PassThrough,
    Multiply,
    Screen,
    Darken,
    Lighten,
    Difference,
    Subtract,
    LinearDodge,
    LinearBurn,
    ColorDodge,
    ColorBurn,
    Overlay,
    HardLight,
    SoftLight,
    Exclusion,
    Divide,
    DarkerColor,
    LighterColor,
    Hue,
    Saturation,
    Color,
    Luminosity,
    HardMix,
    LinearLight,
    VividLight,
    PinLight,
    GlowAdd,
    GlowDodge,
    Other,
}

impl BlendMode {
    /// The `blend_mode` string a record holds (snake_case).
    pub fn name(self) -> &'static str {
        use BlendMode::*;
        match self {
            Normal => "normal",
            PassThrough => "pass_through",
            Multiply => "multiply",
            Screen => "screen",
            Darken => "darken",
            Lighten => "lighten",
            Difference => "difference",
            Subtract => "subtract",
            LinearDodge => "linear_dodge",
            LinearBurn => "linear_burn",
            ColorDodge => "color_dodge",
            ColorBurn => "color_burn",
            Overlay => "overlay",
            HardLight => "hard_light",
            SoftLight => "soft_light",
            Exclusion => "exclusion",
            Divide => "divide",
            DarkerColor => "darker_color",
            LighterColor => "lighter_color",
            Hue => "hue",
            Saturation => "saturation",
            Color => "color",
            Luminosity => "luminosity",
            HardMix => "hard_mix",
            LinearLight => "linear_light",
            VividLight => "vivid_light",
            PinLight => "pin_light",
            GlowAdd => "glow_add",
            GlowDodge => "glow_dodge",
            Other => "other",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteCapability {
    None,
    Pixels,
    PixelsAndProps,
}

#[derive(Clone, Debug, PartialEq)]
pub enum MetaValue {
    Null,
    Boolean(bool),
    Integer(i64),
    Number(f64),
    Text(String),
    /// The host writes the SHA-256 in hex.
    Sha256Of(Bytes),
    /// A nested value, as JSON text.
    Json(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct MetaEntry {
    pub key: String,
    pub value: MetaValue,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LayerPixels {
    None,
    Ready(Bitmap),
    Deferred(u32),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Layer {
    pub id: String,
    pub stable_id: String,
    pub parent: Option<u32>,
    pub name: String,
    pub kind: LayerKind,
    pub visible: bool,
    pub opacity: f64,
    pub blend: BlendMode,
    pub clip_to_below: bool,
    pub offset_x: i64,
    pub offset_y: i64,
    pub pixels: LayerPixels,
    pub meta: Vec<MetaEntry>,
    pub semantic: Vec<MetaEntry>,
    pub write: WriteCapability,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ColorProfile {
    None,
    Embedded(Bytes),
    Named(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Canvas {
    pub width: u32,
    pub height: u32,
    pub dpi: Option<f64>,
    pub profile: ColorProfile,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Compositor {
    GenericV1,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Composite {
    /// The host's generic compositor (white background).
    Render(Compositor),
    /// A composite embedded in the file; used when it is the canvas' size.
    Image(Bitmap),
    None,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Warning {
    pub code: String,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Structure {
    pub format: String,
    pub canvas: Canvas,
    pub layers: Vec<Layer>,
    pub composite: Composite,
    pub file_meta: Vec<MetaEntry>,
    pub context: Vec<u8>,
    pub warnings: Vec<Warning>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadError {
    /// The file is not of this format (a built-in raster reader: stored as an opaque blob).
    NotThisFormat(String),
    Corrupt(String),
    Unsupported(String),
    TooLarge(String),
    /// Not in the WIT: the host's own verdict on a wasm plugin's call (a
    /// trap, fuel, the time or memory limit, a refused component; PLUGINS.md
    /// P5). The message says which. Never `NotThisFormat`: a plugin that
    /// fails is not a reason to store the file as an opaque blob.
    Plugin(String),
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadError::NotThisFormat(m) => write!(f, "not this format: {m}"),
            ReadError::Corrupt(m) => write!(f, "corrupt: {m}"),
            ReadError::Unsupported(m) => write!(f, "unsupported: {m}"),
            ReadError::TooLarge(m) => write!(f, "too large: {m}"),
            ReadError::Plugin(m) => write!(f, "{m}"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Confidence {
    No,
    Maybe,
    Yes,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FormatInfo {
    pub name: String,
    pub extensions: Vec<String>,
    pub mime: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginInfo {
    pub id: String,
    pub version: String,
    pub formats: Vec<FormatInfo>,
    pub probe_bytes: u32,
    pub can_write: bool,
}

/// `codecs.image-info`.
#[derive(Clone, Debug, PartialEq)]
pub struct ImageHeader {
    pub width: u32,
    pub height: u32,
    pub frames: u32,
    pub has_alpha: bool,
    pub dpi: Option<(f64, f64)>,
    pub icc: bool,
}

// -- the write-back side (`writer`; PLUGINS.md P7 implements it) ------------

#[derive(Clone, Debug, PartialEq)]
pub enum PropChange {
    Name(String),
    Visible(bool),
    Opacity(f64),
    Blend(BlendMode),
    Offset(i64, i64),
}

/// `layer-edit`: the new pixels are named by their index in the `pixels`
/// list `write-layers` takes (a borrow cannot sit inside a record).
#[derive(Clone, Debug, PartialEq)]
pub struct LayerEdit {
    pub stable_id: String,
    pub new_pixels: Option<u32>,
    pub props: Vec<PropChange>,
}

/// `edit.zlib-kernel`: whose deflate the bytes are (PLUGINS.md 3.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ZlibKernel {
    /// zlib 1.3.1 as CPython's `zlib.compress` drives it (`tmi_clip::zlib`).
    ZlibV131,
}

/// `edit.encoding`: one step of `encode-tile` (the level as zlib takes it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoding {
    Raw,
    Zlib(ZlibKernel, i8),
    Deflate(ZlibKernel, i8),
}

/// `edit.image-encoding`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImageEncoding {
    PngV1,
    JpegKeepV1(Vec<u8>),
    WebpV1(u8),
}

/// `writer.piece`; a blob is named by the host's number for it
/// (`write::WriteCx`; the WIT's `blob` resource).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Piece {
    Source(Span),
    Bytes(Vec<u8>),
    Blob(u32),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WritePlan {
    pub pieces: Vec<Piece>,
    pub warnings: Vec<Warning>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WriteError {
    Unsupported(String),
    Invalid(String),
    Corrupt(String),
    /// Not in the WIT: the host's verdict on a wasm plugin's call (a trap,
    /// fuel, a limit), as `ReadError::Plugin`.
    Plugin(String),
}

impl std::fmt::Display for WriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WriteError::Unsupported(m) => write!(f, "unsupported: {m}"),
            WriteError::Invalid(m) => write!(f, "invalid: {m}"),
            WriteError::Corrupt(m) => write!(f, "corrupt: {m}"),
            WriteError::Plugin(m) => write!(f, "{m}"),
        }
    }
}
