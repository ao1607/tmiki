//! The host on `wasm32`: wit-bindgen's imports, as they are.

use crate::bindings::tmi::format::{codecs, edit, log, source, sqlite, xml};
use crate::*;

pub struct Source<'a>(pub(crate) &'a source::SourceFile);

impl Source<'_> {
    pub fn size(&self) -> u64 {
        self.0.size()
    }

    pub fn read(&self, offset: u64, len: u32) -> Vec<u8> {
        self.0.read(offset, len)
    }

    pub fn name(&self) -> String {
        self.0.name()
    }
}

pub fn transform(data: &Bytes, list: &[Codec], max_out: u32) -> Result<Vec<u8>, String> {
    codecs::transform(data, list, max_out)
}

pub fn crc32(data: &Bytes) -> u32 {
    codecs::crc32(data)
}

pub fn sha256(data: &Bytes) -> Vec<u8> {
    codecs::sha256(data)
}

pub fn image_info(img: &EncodedImage) -> Result<ImageHeader, String> {
    codecs::image_info(img)
}

pub struct Database(sqlite::Database);

impl Database {
    pub fn open(image: &Bytes, writable: bool) -> Result<Database, String> {
        if writable { sqlite::Database::open_writable(image) } else { sqlite::Database::open(image) }.map(Database)
    }

    pub fn query(&self, sql: &str, params: &[SqlValue]) -> Result<Vec<Vec<SqlValue>>, String> {
        self.0.query(sql, params)
    }

    pub fn execute(&self, sql: &str, params: &[SqlValue]) -> Result<u64, String> {
        self.0.execute(sql, params)
    }

    pub fn serialize(&self) -> Result<Vec<u8>, String> {
        self.0.serialize()
    }
}

pub fn xml_parse(text: &[u8]) -> Result<Vec<Element>, String> {
    xml::parse(text)
}

pub fn log_emit(level: Level, message: &str) {
    log::emit(level, message)
}

pub struct Pixels(pub(crate) edit::Pixels);

impl Pixels {
    pub fn width(&self) -> u32 {
        self.0.width()
    }

    pub fn height(&self) -> u32 {
        self.0.height()
    }

    pub fn bbox(&self) -> Option<(u32, u32, u32, u32)> {
        self.0.bbox()
    }

    pub fn read(&self, x: u32, y: u32, w: u32, h: u32, layout: &Layout) -> Vec<u8> {
        self.0.read(x, y, w, h, layout)
    }
}

pub struct Blob(pub(crate) edit::Blob);

impl Blob {
    pub fn len(&self) -> u64 {
        self.0.len()
    }

    pub fn crc32(&self) -> u32 {
        self.0.crc32()
    }
}

#[allow(clippy::too_many_arguments)]
pub fn encode_tile(px: &Pixels, x: u32, y: u32, w: u32, h: u32, layout: &Layout, steps: &[Encoding]) -> Result<Blob, String> {
    edit::encode_tile(&px.0, x, y, w, h, layout, steps).map(Blob)
}

pub fn encode_image(px: &Pixels, enc: &ImageEncoding) -> Result<Blob, String> {
    edit::encode_image(&px.0, enc).map(Blob)
}

pub fn composite_after_edit() -> Result<Pixels, String> {
    edit::composite_after_edit().map(Pixels)
}

pub fn thumbnail(px: &Pixels, max_side: u32) -> Pixels {
    Pixels(edit::thumbnail(&px.0, max_side))
}
