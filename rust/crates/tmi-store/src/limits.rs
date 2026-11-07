//! Caps on what a stored or received object may inflate to, and the shape a
//! chunk manifest may take (objects.py has the same numbers and words).
//!
//! A hostile object is a few bytes stored and gigabytes inflated, and a hostile
//! chunk manifest can list one part over and over, so nothing is inflated or
//! assembled without a bound.

use crate::json::{Int, Value};
use std::io::Read;

/// Commits, trees, tags and chunk manifests: JSON the readers hold whole.
pub const MAX_META_OBJECT_SIZE: usize = 32 * 1024 * 1024;
/// A plain blob: a whole file under `CHUNKED_MIN_SIZE`, a chunk, a chunked
/// payload's glue.
pub const MAX_BLOB_SIZE: usize = 1024 * 1024 * 1024;
/// The declared size of a chunked blob.
pub const MAX_CHUNKED_SIZE: i64 = 4 * 1024 * 1024 * 1024;
/// How many parts (object ids and glue counts) a chunk manifest may list.
pub const MAX_CHUNK_PARTS: usize = 262_144;
/// The longest object type name before the NUL.
pub const MAX_TYPE_LENGTH: usize = 32;
/// Object types whose payload is JSON.
pub const JSON_TYPES: [&str; 4] = ["commit", "tag", "tree", "chunked"];

/// The most an object of this type may inflate to (payload bytes).
pub fn size_cap_for(object_type: &str) -> usize {
    if object_type == "blob" {
        MAX_BLOB_SIZE
    } else {
        MAX_META_OBJECT_SIZE
    }
}

pub fn is_oid(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// `zlib.decompress(raw)` of an object file (one stream, bytes after its end
/// ignored) that stops with an error once the payload is past its type's cap,
/// instead of inflating the rest.
pub fn decompress_object(raw: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut decoder = flate2::read::ZlibDecoder::new(raw);
    let mut out: Vec<u8> = Vec::with_capacity(raw.len().saturating_mul(2).min(1 << 20));
    let mut buf = vec![0u8; 1 << 16];
    // header_len = type + NUL, once the NUL is seen
    let mut header_len: Option<usize> = None;
    let mut limit = MAX_TYPE_LENGTH + 1;
    loop {
        let n = decoder.read(&mut buf)?;
        if n == 0 {
            return Ok(out);
        }
        out.extend_from_slice(&buf[..n]);
        if header_len.is_none() {
            let scan = out.len().min(MAX_TYPE_LENGTH + 1);
            if let Some(nul) = out[..scan].iter().position(|&b| b == 0) {
                let object_type = String::from_utf8_lossy(&out[..nul]).into_owned();
                header_len = Some(nul + 1);
                limit = nul + 1 + size_cap_for(&object_type);
            } else if out.len() > MAX_TYPE_LENGTH {
                return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "object type header too long"));
            }
        }
        if out.len() > limit {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("object is larger than {} bytes", limit - header_len.unwrap_or(0)),
            ));
        }
    }
}

/// The shape of a chunked object's manifest, checked when it is received and
/// when it is read: `size` within `MAX_CHUNKED_SIZE`, at most `MAX_CHUNK_PARTS`
/// parts, each an object id or a count of glue bytes, the glue counts within
/// `size`. What the parts add up to is checked as they are read.
pub fn check_chunk_manifest(manifest: &Value) -> Result<(), String> {
    if manifest.as_object().is_none() {
        return Err("chunked manifest is not an object".into());
    }
    let size = match manifest.get("size") {
        Some(Value::Int(Int::Small(n))) if (0..=MAX_CHUNKED_SIZE).contains(n) => *n,
        _ => return Err("chunked manifest has a bad size".into()),
    };
    let Some(parts) = manifest.get("parts").and_then(Value::as_array).filter(|p| p.len() <= MAX_CHUNK_PARTS) else {
        return Err("chunked manifest has a bad part list".into());
    };
    if let Some(glue) = manifest.get("glue").filter(|g| g.truthy()) {
        if !glue.as_str().is_some_and(is_oid) {
            return Err("chunked manifest has a bad glue id".into());
        }
    }
    let mut glue_bytes: i64 = 0;
    for part in parts {
        match part {
            Value::Str(s) => {
                if !is_oid(s) {
                    return Err("chunked manifest names a part that is not an object id".into());
                }
            }
            Value::Int(Int::Small(n)) if *n >= 0 => glue_bytes = glue_bytes.saturating_add(*n),
            Value::Int(Int::Big(digits)) if !digits.starts_with('-') => {
                return Err("chunked manifest takes more glue than its size".into())
            }
            _ => return Err("chunked manifest has a bad part".into()),
        }
    }
    if glue_bytes > size {
        return Err("chunked manifest takes more glue than its size".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::loads;

    fn manifest(text: &str) -> Result<(), String> {
        check_chunk_manifest(&loads(text).unwrap())
    }

    #[test]
    fn manifest_shapes() {
        let id = "a".repeat(64);
        assert!(manifest(&format!(r#"{{"size":12,"glue":null,"parts":["{id}",2]}}"#)).is_ok());
        assert!(manifest(r#"{"size":-1,"parts":[]}"#).unwrap_err().contains("bad size"));
        assert!(manifest(r#"{"size":99999999999999,"parts":[]}"#).unwrap_err().contains("bad size"));
        assert!(manifest(r#"{"size":1,"parts":["../../x"]}"#).unwrap_err().contains("not an object id"));
        assert!(manifest(r#"{"size":5,"parts":[10]}"#).unwrap_err().contains("more glue"));
        assert!(manifest(r#"{"size":5,"parts":[1.5]}"#).unwrap_err().contains("bad part"));
        assert!(manifest(r#"{"size":5,"glue":"x","parts":[]}"#).unwrap_err().contains("glue id"));
        let many = format!(r#"{{"size":5,"parts":[{}0]}}"#, "0,".repeat(MAX_CHUNK_PARTS));
        assert!(manifest(&many).unwrap_err().contains("part list"));
    }

    #[test]
    fn decompress_object_stops_at_the_cap() {
        let mut data = b"commit\0".to_vec();
        data.resize(7 + MAX_META_OBJECT_SIZE + 1, b'0');
        let raw = crate::zlib::compress(&data, 1);
        let err = decompress_object(&raw).unwrap_err();
        assert!(err.to_string().contains("larger than"));
        data.pop();
        assert_eq!(decompress_object(&crate::zlib::compress(&data, 1)).unwrap().len(), data.len());
    }
}
