//! Differential hooks for PNG / JPEG against Pillow (tests/test_raster_p0d.py):
//!
//!   tmi-conform raster-dump      stdin: one path per line; stdout: one JSON
//!                                line each: the header facts and the sha256
//!                                of the RGBA pixels, or why it declines
//!   tmi-conform writeback-dump   stdin: `original<TAB>rgba<TAB>w<TAB>h<TAB>jpeg<TAB>out`
//!                                per line (original "-": none); writes the
//!                                write-back's bytes to `out`, one JSON line
//!                                each ({"ok": true} or {"error": ...})

use std::io::BufRead;
use tmi_store::json::{self, Int, Map, Value};
use tmi_pillow::image as tmi_pillow_image;
use tmi_store::oid::sha256_hex;

fn int(v: i64) -> Value {
    Value::Int(Int::Small(v))
}

fn print(m: Map) {
    println!("{}", json::dumps(&Value::Object(m), &json::DumpOptions::CANONICAL));
}

/// `raster.decode(data, meta=True)` without the Pillow fallback (and no
/// pixel limit): header facts, then tmi_image's decode.
fn dump(path: &str, zip_only: bool, lenient: bool) -> Map {
    let mut m = Map::new();
    m.insert("file".into(), Value::Str(path.to_string()));
    let data = match std::fs::read(path) {
        Ok(d) => d,
        Err(e) => {
            m.insert("declined".into(), Value::Str(format!("read: {e}")));
            return m;
        }
    };
    let h = match tmi_import::header::header(&data) {
        Ok(Some(h)) => h,
        Ok(None) => {
            m.insert("declined".into(), Value::Str("not a PNG or JPEG".into()));
            return m;
        }
        Err(e) => {
            m.insert("declined".into(), Value::Str(e));
            return m;
        }
    };
    if h.format == "jpeg" {
        let mpo = tmi_import::header::jpeg_open(&data).map(|i| i.mpo).unwrap_or(false);
        m.insert("mpo".into(), Value::Bool(mpo));
    }
    if lenient && h.format == "jpeg" {
        m.insert("maxdq".into(), tmi_image::jpeg::max_dequantized(&data).map_or(Value::Null, |v| int(v as i64)));
    }
    m.insert("format".into(), Value::Str(h.format.into()));
    m.insert("size".into(), Value::Array(vec![int(h.size.0 as i64), int(h.size.1 as i64)]));
    m.insert("dpi".into(), h.dpi.map_or(Value::Null, Value::Float));
    m.insert("icc".into(), Value::Bool(h.icc));
    let decoded = if zip_only && h.format == "png" {
        tmi_image::png_dec::decode_png_with(&data, true)
            .map_err(tmi_image::DecodeError::Png)
            .and_then(|d| tmi_pillow_convert(d.image))
            .map(|i| (i, tmi_image::Format::Png))
    } else if lenient && h.format == "jpeg" {
        tmi_image::jpeg::decode_jpeg_with(&data, true)
            .map_err(tmi_image::DecodeError::Jpeg)
            .and_then(tmi_pillow_convert)
            .map(|i| (i, tmi_image::Format::Jpeg))
    } else {
        tmi_image::decode_rgba(&data)
    };
    match decoded {
        Ok((img, _)) if (img.width, img.height) == h.size => {
            m.insert("rgba".into(), Value::Str(sha256_hex(&img.data)));
        }
        Ok((img, _)) => {
            m.insert("declined".into(), Value::Str(format!("decoded size {}x{} differs", img.width, img.height)));
        }
        Err(e) => {
            m.insert("declined".into(), Value::Str(format!("tmi_image: {e:?}")));
        }
    }
    m
}

fn tmi_pillow_convert(img: tmi_pillow_image::Image) -> Result<tmi_pillow_image::Image, tmi_image::DecodeError> {
    if img.mode == tmi_pillow_image::Mode::RGBA {
        return Ok(img);
    }
    tmi_pillow::convert::convert(&img, tmi_pillow_image::Mode::RGBA).map_err(tmi_image::DecodeError::Convert)
}

/// `args`: `--zip` decodes every PNG through png_dec's ZipDecode (not the png crate).
pub fn raster(args: &[String]) {
    let zip_only = args.iter().any(|a| a == "--zip");
    let lenient = args.iter().any(|a| a == "--lenient");
    for line in std::io::stdin().lock().lines() {
        let path = line.unwrap();
        if !path.is_empty() {
            print(dump(&path, zip_only, lenient));
        }
    }
}

pub fn writeback(_args: &[String]) {
    for line in std::io::stdin().lock().lines() {
        let line = line.unwrap();
        let f: Vec<&str> = line.split('\t').collect();
        let mut m = Map::new();
        if f.len() != 6 {
            m.insert("error".into(), Value::Str(format!("bad request line: {line}")));
            print(m);
            continue;
        }
        let original = if f[0] == "-" { None } else { Some(std::fs::read(f[0]).unwrap()) };
        let rgba = tmi_import::Rgba { width: f[2].parse().unwrap(), height: f[3].parse().unwrap(), data: std::fs::read(f[1]).unwrap() };
        match tmi_diffmerge::writeback::encode_raster_like(&rgba, original.as_deref(), f[4] == "1") {
            Ok(bytes) => {
                std::fs::write(f[5], bytes).unwrap();
                m.insert("ok".into(), Value::Bool(true));
            }
            Err(e) => {
                m.insert("error".into(), Value::Str(format!("{e:?}")));
            }
        }
        print(m);
    }
}
