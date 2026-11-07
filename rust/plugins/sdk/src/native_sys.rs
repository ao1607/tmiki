//! The host on a native target: each call goes to tmi over the RPC of
//! `tmiki plugin dev` ([`crate::native::import`]), answered before it
//! returns. A host function that traps there unwinds the plugin's call
//! (the call is over; tmi already knows why).

use std::marker::PhantomData;

use tmi_plugin_dev::Value;

use crate::native::{import, misfit};
use crate::wire;
use crate::*;

fn u64_of(v: Value, func: &str) -> u64 {
    v.u64(func).unwrap_or_else(|e| misfit(func, e))
}

fn u32_of(v: Value, func: &str) -> u32 {
    v.u32(func).unwrap_or_else(|e| misfit(func, e))
}

fn bytes_of(v: Value, func: &str) -> Vec<u8> {
    match v {
        Value::Bytes(b) => b,
        other => misfit(func, tmi_plugin_dev::ValueError(format!("expected a byte string, got {}", other.kind()))),
    }
}

/// A `result<T, string>` answer.
fn result_of<T>(v: Value, func: &str, ok: impl FnOnce(&Value) -> Result<T, tmi_plugin_dev::ValueError>) -> Result<T, String> {
    match v.result(func) {
        Ok(Ok(x)) => Ok(ok(x).unwrap_or_else(|e| misfit(func, e))),
        Ok(Err(e)) => Err(e.string(func).unwrap_or_else(|e| misfit(func, e))),
        Err(e) => misfit(func, e),
    }
}

pub struct Source<'a> {
    pub(crate) handle: u64,
    pub(crate) _call: PhantomData<&'a ()>,
}

impl Source<'_> {
    pub fn size(&self) -> u64 {
        u64_of(import("source.source-file.size", vec![Value::Uint(self.handle)]), "source.source-file.size")
    }

    pub fn read(&self, offset: u64, len: u32) -> Vec<u8> {
        let f = "source.source-file.read";
        bytes_of(import(f, vec![Value::Uint(self.handle), Value::Uint(offset), Value::uint(len)]), f)
    }

    pub fn name(&self) -> String {
        let f = "source.source-file.name";
        import(f, vec![Value::Uint(self.handle)]).string(f).unwrap_or_else(|e| misfit(f, e))
    }
}

pub fn transform(data: &Bytes, list: &[Codec], max_out: u32) -> Result<Vec<u8>, String> {
    let f = "codecs.transform";
    let v = import(f, vec![wire::bytes_value(data), Value::list(list, wire::codec_value), Value::uint(max_out)]);
    result_of(v, f, |b| b.byte_vec(f))
}

pub fn crc32(data: &Bytes) -> u32 {
    u32_of(import("codecs.crc32", vec![wire::bytes_value(data)]), "codecs.crc32")
}

pub fn sha256(data: &Bytes) -> Vec<u8> {
    bytes_of(import("codecs.sha256", vec![wire::bytes_value(data)]), "codecs.sha256")
}

pub fn image_info(img: &EncodedImage) -> Result<ImageHeader, String> {
    let f = "codecs.image-info";
    result_of(import(f, vec![wire::encoded_image_value(img)]), f, wire::image_header)
}

pub struct Database {
    handle: u64,
}

impl Database {
    pub fn open(image: &Bytes, writable: bool) -> Result<Database, String> {
        let f = if writable { "sqlite.database.open-writable" } else { "sqlite.database.open" };
        result_of(import(f, vec![wire::bytes_value(image)]), f, |h| h.u64(f)).map(|handle| Database { handle })
    }

    pub fn query(&self, sql: &str, params: &[SqlValue]) -> Result<Vec<Vec<SqlValue>>, String> {
        let f = "sqlite.database.query";
        let v = import(f, vec![Value::Uint(self.handle), Value::text(sql), Value::list(params, wire::sql_value_value)]);
        result_of(v, f, |rows| {
            rows.array(f)?.iter().map(|r| r.array(f)?.iter().map(|x| wire::sql_value(x, f)).collect()).collect()
        })
    }

    pub fn execute(&self, sql: &str, params: &[SqlValue]) -> Result<u64, String> {
        let f = "sqlite.database.execute";
        let v = import(f, vec![Value::Uint(self.handle), Value::text(sql), Value::list(params, wire::sql_value_value)]);
        result_of(v, f, |n| n.u64(f))
    }

    pub fn serialize(&self) -> Result<Vec<u8>, String> {
        let f = "sqlite.database.serialize";
        result_of(import(f, vec![Value::Uint(self.handle)]), f, |b| b.byte_vec(f))
    }
}

impl Drop for Database {
    fn drop(&mut self) {
        // not while unwinding from a host trap: the call is already over
        if !std::thread::panicking() {
            import("sqlite.database.drop", vec![Value::Uint(self.handle)]);
        }
    }
}

pub fn xml_parse(text: &[u8]) -> Result<Vec<Element>, String> {
    let f = "xml.parse";
    result_of(import(f, vec![Value::bytes(text)]), f, |els| els.array(f)?.iter().map(|e| wire::element(e, f)).collect())
}

pub fn log_emit(level: Level, message: &str) {
    let level = match level {
        Level::Debug => "debug",
        Level::Info => "info",
    };
    import("log.emit", vec![Value::text(level), Value::text(message)]);
}

pub struct Pixels {
    pub(crate) handle: u32,
}

impl Pixels {
    pub fn width(&self) -> u32 {
        u32_of(import("edit.pixels.width", vec![Value::uint(self.handle)]), "edit.pixels.width")
    }

    pub fn height(&self) -> u32 {
        u32_of(import("edit.pixels.height", vec![Value::uint(self.handle)]), "edit.pixels.height")
    }

    pub fn bbox(&self) -> Option<(u32, u32, u32, u32)> {
        let f = "edit.pixels.bbox";
        let v = import(f, vec![Value::uint(self.handle)]);
        let read = |v: &Value| -> Result<(u32, u32, u32, u32), tmi_plugin_dev::ValueError> {
            let t = v.tuple(f, 4)?;
            Ok((t[0].u32(f)?, t[1].u32(f)?, t[2].u32(f)?, t[3].u32(f)?))
        };
        v.opt().map(|b| read(b).unwrap_or_else(|e| misfit(f, e)))
    }

    pub fn read(&self, x: u32, y: u32, w: u32, h: u32, layout: &Layout) -> Vec<u8> {
        let f = "edit.pixels.read";
        let args = vec![Value::uint(self.handle), Value::uint(x), Value::uint(y), Value::uint(w), Value::uint(h), wire::layout_value(layout)];
        bytes_of(import(f, args), f)
    }
}

pub struct Blob {
    pub(crate) handle: u32,
}

impl Blob {
    pub fn len(&self) -> u64 {
        u64_of(import("edit.blob.len", vec![Value::uint(self.handle)]), "edit.blob.len")
    }

    pub fn crc32(&self) -> u32 {
        u32_of(import("edit.blob.crc32", vec![Value::uint(self.handle)]), "edit.blob.crc32")
    }
}

#[allow(clippy::too_many_arguments)]
pub fn encode_tile(px: &Pixels, x: u32, y: u32, w: u32, h: u32, layout: &Layout, steps: &[Encoding]) -> Result<Blob, String> {
    let f = "edit.encode-tile";
    let args = vec![
        Value::uint(px.handle),
        Value::uint(x),
        Value::uint(y),
        Value::uint(w),
        Value::uint(h),
        wire::layout_value(layout),
        Value::list(steps, wire::encoding_value),
    ];
    result_of(import(f, args), f, |b| b.u32(f)).map(|handle| Blob { handle })
}

pub fn encode_image(px: &Pixels, enc: &ImageEncoding) -> Result<Blob, String> {
    let f = "edit.encode-image";
    result_of(import(f, vec![Value::uint(px.handle), wire::image_encoding_value(enc)]), f, |b| b.u32(f)).map(|handle| Blob { handle })
}

pub fn composite_after_edit() -> Result<Pixels, String> {
    let f = "edit.composite-after-edit";
    result_of(import(f, vec![]), f, |p| p.u32(f)).map(|handle| Pixels { handle })
}

pub fn thumbnail(px: &Pixels, max_side: u32) -> Pixels {
    let f = "edit.thumbnail";
    Pixels { handle: u32_of(import(f, vec![Value::uint(px.handle), Value::uint(max_side)]), f) }
}
