//! What the reader returns, before either side converts it: the subset of
//! `tmi:format/types` an OpenRaster file uses.

/// A part of the source file (absolute offset).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub offset: u64,
    pub len: u64,
}

/// A zip member's stored bytes: deflated or not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Member {
    pub span: Span,
    pub deflate: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Raster,
    Folder,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Blend {
    Normal,
    PassThrough,
    Multiply,
    Screen,
    Overlay,
    Darken,
    Lighten,
    ColorDodge,
    ColorBurn,
    HardLight,
    SoftLight,
    Difference,
    LinearDodge,
    Hue,
    Saturation,
    Color,
    Luminosity,
    Other,
}

/// A layer's pixels: a PNG member placed at (0, 0) of its own bitmap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pixels {
    None,
    Ready(Member),
    Deferred(u32),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Layer {
    pub id: String,
    pub stable_id: String,
    pub parent: Option<u32>,
    pub name: String,
    pub kind: Kind,
    pub visible: bool,
    pub opacity: f64,
    pub blend: Blend,
    pub offset_x: i64,
    pub offset_y: i64,
    pub pixels: Pixels,
    /// Text entries of `meta`.
    pub meta: Vec<(String, String)>,
    /// Text entries of `semantic`.
    pub semantic: Vec<(String, String)>,
    /// `write-capability::pixels` (else `none`).
    pub writable: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Composite {
    /// `mergedimage.png`.
    Image(Member),
    /// `render(generic-v1)`.
    Render,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Structure {
    pub width: u32,
    pub height: u32,
    pub dpi: Option<f64>,
    pub layers: Vec<Layer>,
    pub composite: Composite,
    pub context: Vec<u8>,
    /// (code, message).
    pub warnings: Vec<(String, String)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    NotThisFormat(String),
    Corrupt(String),
    Unsupported(String),
    TooLarge(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Confidence {
    No,
    Maybe,
    Yes,
}

pub struct Info {
    pub id: &'static str,
    pub version: &'static str,
    pub format_name: &'static str,
    pub extensions: &'static [&'static str],
    pub mime: Option<&'static str>,
    pub probe_bytes: u32,
    /// `plugin-info.can-write`.
    pub can_write: bool,
}

// -- the writer --------------------------------------------------------------

/// A layer to write: its stable id and the host's handle of its new pixels.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edit {
    pub stable_id: String,
    pub pixels: u32,
}

/// `writer.piece`: a blob is the host's handle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Piece {
    Source(Span),
    Bytes(Vec<u8>),
    Blob(u32),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    pub pieces: Vec<Piece>,
    /// (code, message).
    pub warnings: Vec<(String, String)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WriteFail {
    Unsupported(String),
    Invalid(String),
    Corrupt(String),
}

/// The host's `edit` helpers, as the writer sees them (handles are numbers).
pub trait WriteHost: Host {
    /// `edit.encode-image(px, png-v1)`: a blob handle.
    fn encode_png(&mut self, pixels: u32) -> Result<u32, String>;
    /// `blob.len` and `blob.crc32`.
    fn blob_info(&self, blob: u32) -> (u64, u32);
    /// `edit.composite-after-edit`: a pixels handle.
    fn composite_after_edit(&mut self) -> Result<u32, String>;
    /// `edit.thumbnail(px, max-side)`: a pixels handle.
    fn thumbnail(&mut self, pixels: u32, max_side: u32) -> u32;
}

/// The file and the host's helper, as the plugin sees them.
pub trait Host {
    /// `source-file.size`.
    fn size(&self) -> u64;
    /// `source-file.read`: short only at the end of the file.
    fn read(&self, offset: u64, len: u32) -> Vec<u8>;
    /// `codecs.transform(span, [deflate], max)`.
    fn inflate(&self, member: &Member, max_out: u32) -> Result<Vec<u8>, String>;
}
