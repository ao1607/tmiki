//! objects.py `ObjectStore.write_raw`: an already-compressed object checked
//! against its oid before it is stored, with Python's error texts.

use libz_sys as z;
use tmi_store::limits;
use tmi_store::oid::Sha256;
use tmi_store::ObjectStore;

extern "C" fn zalloc(_opaque: z::voidpf, items: z::uInt, size: z::uInt) -> z::voidpf {
    // SAFETY: plain C allocation, freed by `zfree`.
    unsafe { libc::malloc(items as usize * size as usize) }
}

extern "C" fn zfree(_opaque: z::voidpf, address: z::voidpf) {
    // SAFETY: `address` came from `zalloc`.
    unsafe { libc::free(address) }
}

/// How much of an object is inflated at a time.
const INFLATE_STEP: usize = 64 * 1024;

/// `py_inflate` collected whole (for small inputs and tests).
pub fn py_decompress(raw: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    py_inflate(raw, |piece| {
        out.extend_from_slice(piece);
        Ok(())
    })?;
    Ok(out)
}

/// CPython's `zlib.decompress(raw)` handed to `sink` in pieces of at most
/// 64 KiB, so nothing of a bomb is held; Err is `str(zlib.error)`, or the
/// sink's own refusal, which stops the inflating.
pub fn py_inflate(raw: &[u8], mut sink: impl FnMut(&[u8]) -> Result<(), String>) -> Result<(), String> {
    if raw.len() > u32::MAX as usize {
        return Err("input too large".into());
    }
    // SAFETY: the zlib stream API as CPython's zlib_decompress_impl uses it.
    unsafe {
        let mut strm = z::z_stream {
            next_in: raw.as_ptr() as *mut u8,
            avail_in: raw.len() as z::uInt,
            total_in: 0,
            next_out: std::ptr::null_mut(),
            avail_out: 0,
            total_out: 0,
            msg: std::ptr::null_mut(),
            state: std::ptr::null_mut(),
            zalloc,
            zfree,
            opaque: std::ptr::null_mut(),
            data_type: 0,
            adler: 0,
            reserved: 0,
        };
        let rc = z::inflateInit2_(&mut strm, 15, z::zlibVersion(), std::mem::size_of::<z::z_stream>() as i32);
        if rc != z::Z_OK {
            return Err(format!("Error {rc} while preparing to decompress data"));
        }
        let mut buf = vec![0u8; INFLATE_STEP];
        let err = loop {
            strm.next_out = buf.as_mut_ptr();
            strm.avail_out = INFLATE_STEP as z::uInt;
            let err = z::inflate(&mut strm, z::Z_FINISH);
            let produced = INFLATE_STEP - strm.avail_out as usize;
            if produced > 0 {
                if let Err(refusal) = sink(&buf[..produced]) {
                    z::inflateEnd(&mut strm);
                    return Err(refusal);
                }
            }
            match err {
                z::Z_OK | z::Z_BUF_ERROR | z::Z_STREAM_END => {}
                _ => break err,
            }
            if strm.avail_out != 0 {
                break err;
            }
        };        let msg = if strm.msg.is_null() {
            None
        } else {
            Some(std::ffi::CStr::from_ptr(strm.msg).to_string_lossy().into_owned())
        };
        z::inflateEnd(&mut strm);
        if err == z::Z_STREAM_END {
            return Ok(());
        }
        // zlibmodule.c zlib_error()
        let zmsg = msg.or_else(|| match err {
            z::Z_BUF_ERROR => Some("incomplete or truncated stream".into()),
            z::Z_STREAM_ERROR => Some("inconsistent stream state".into()),
            z::Z_DATA_ERROR => Some("invalid input data".into()),
            _ => None,
        });
        Err(match zmsg {
            Some(m) => format!("Error {err} while decompressing data: {}", m.chars().take(200).collect::<String>()),
            None => format!("Error {err} while decompressing data"),
        })
    }
}

/// The checks of `write_raw(oid, raw)` without the write: Err is the
/// ValueError's text.
///
/// The object is inflated in pieces and hashed as it goes; one past its
/// type's cap (`tmi_store::limits`) is refused before the rest is inflated,
/// and a blob's bytes are never held. The payload of a commit / tree / tag /
/// chunked object is kept (at most `MAX_META_OBJECT_SIZE`) for the checks of
/// its shape: JSON nested past `json::MAX_DEPTH`, a malformed chunk manifest
/// and (trees) an invalid path are refused before anything is stored.
pub fn verify(oid: &str, raw: &[u8]) -> Result<(), String> {
    let corrupt = |e: String| format!("corrupt object data for {oid}: {e}");
    // the store reads no file larger than this (bytes after the zlib stream
    // are ignored, so a short object could otherwise carry gigabytes)
    if raw.len() as u64 > tmi_store::store::MAX_OBJECT_FILE {
        return Err(corrupt(format!("{} bytes is larger than any object", raw.len())));
    }
    let mut inflating = Inflating::default();
    py_inflate(raw, |piece| inflating.feed(piece)).map_err(corrupt)?;
    let Some(object_type) = inflating.object_type.take() else {
        return Err(corrupt("not enough values to unpack (expected 2, got 1)".into()));
    };
    let actual = inflating.digest.hexdigest();
    if actual != oid {
        return Err(format!("object hash mismatch for {oid}: got {actual}"));
    }
    let payload = inflating.kept;
    let short = &oid[..oid.len().min(12)];
    if limits::JSON_TYPES.contains(&object_type.as_str()) {
        match tmi_store::json::loads_bytes(&payload) {
            Err(e) if e.is_too_deep() => return Err(format!("refusing {object_type} {short}: {e}")),
            Ok(manifest) if object_type == "chunked" => {
                limits::check_chunk_manifest(&manifest).map_err(|e| format!("refusing chunked {short}: {e}"))?
            }
            _ => {}
        }
    }
    if object_type == "tree" {
        check_tree_paths(oid, &payload)?;
    }
    Ok(())
}

/// An object being inflated by `verify`: its type header, then its size held
/// to the type's cap, its hash, and the payload unless it is a blob.
#[derive(Default)]
struct Inflating {
    head: Vec<u8>,
    object_type: Option<String>,
    limit: usize,
    size: usize,
    digest: Sha256,
    kept: Vec<u8>,
}

impl Inflating {
    fn feed(&mut self, piece: &[u8]) -> Result<(), String> {
        if self.object_type.is_none() {
            self.head.extend_from_slice(piece);
            let Some(nul) = self.head.iter().position(|&b| b == 0) else {
                if self.head.len() > limits::MAX_TYPE_LENGTH {
                    return Err("object type header too long".into());
                }
                return Ok(());
            };
            if let Some(i) = self.head[..nul].iter().position(|b| !b.is_ascii()) {
                return Err(format!(
                    "'ascii' codec can't decode byte 0x{:02x} in position {i}: ordinal not in range(128)",
                    self.head[i]
                ));
            }
            let object_type = String::from_utf8_lossy(&self.head[..nul]).into_owned();
            self.digest.update(&self.head[..=nul]);
            self.limit = limits::size_cap_for(&object_type);
            self.object_type = Some(object_type);
            let head = std::mem::take(&mut self.head);
            return self.payload(&head[nul + 1..]);
        }
        self.payload(piece)
    }

    fn payload(&mut self, piece: &[u8]) -> Result<(), String> {
        self.digest.update(piece);
        self.size += piece.len();
        let object_type = self.object_type.as_deref().unwrap_or_default();
        if self.size > self.limit {
            return Err(format!("{object_type} object is larger than {} bytes", self.limit));
        }
        if object_type != "blob" {
            self.kept.extend_from_slice(piece);
        }
        Ok(())
    }
}

/// What `verify` says of a tree that records an invalid path (the receiving
/// gate: nothing lands in the store, no ref moves).
pub const REFUSED_TREE: &str = "refusing tree ";

/// A repository tree's entries name files the working tree will be written
/// with; refuse the tree when one of the paths breaks `recpath`'s rule.
/// Every tree is read (a forged one need not look like tmiki's own JSON); a
/// payload that is not JSON is left for the reader to refuse.
fn check_tree_paths(oid: &str, payload: &[u8]) -> Result<(), String> {
    let Ok(tree) = tmi_store::json::loads_bytes(payload) else {
        return Ok(());
    };
    if tree.get("kind").and_then(|k| k.as_str()) != Some("repository") {
        return Ok(());
    }
    let Some(entries) = tree.get("entries").and_then(|e| e.as_array()) else {
        return Ok(());
    };
    for entry in entries {
        let Some(path) = entry.get("path").and_then(|p| p.as_str()) else {
            continue;
        };
        if let Some(problem) = tmi_repo::recpath::record_path_problem(path) {
            return Err(format!(
                "{REFUSED_TREE}{}: it records an invalid path: {} ({problem})",
                &oid[..oid.len().min(12)],
                tmi_repo::recpath::display_path(path)
            ));
        }
    }
    Ok(())
}

/// Store checked bytes (`write_raw` after `verify`): nothing when the
/// object is already there.
pub fn store(store: &ObjectStore, oid: &str, raw: &[u8]) -> Result<(), String> {
    let path = store.path_for(oid);
    if path.exists() {
        return Ok(());
    }
    tmi_repo::fsio::mark_mutated();
    tmi_store::store::write_file_atomic(&path, raw).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pack(object_type: &str, payload: &[u8]) -> (String, Vec<u8>) {
        let oid = tmi_store::oid::object_id(object_type, payload);
        let mut data = object_type.as_bytes().to_vec();
        data.push(0);
        data.extend_from_slice(payload);
        (oid, tmi_store::zlib::compress(&data, 1))
    }

    fn refused(object_type: &str, payload: &[u8], text: &str) {
        let (oid, raw) = pack(object_type, payload);
        let err = verify(&oid, &raw).unwrap_err();
        assert!(err.contains(text), "{err}");
    }

    #[test]
    fn deep_json_is_refused_not_a_stack_overflow() {
        let deep = [vec![b'['; 3000], vec![b']'; 3000]].concat();
        for kind in ["commit", "tree", "tag", "chunked"] {
            refused(kind, &deep, "nested deeper than 256");
        }
        let (oid, raw) = pack("commit", br#"{"a":[[[1]]]}"#);
        assert!(verify(&oid, &raw).is_ok());
    }

    #[test]
    fn bad_chunk_manifest_is_refused_on_receive() {
        refused("chunked", br#"{"size":5,"parts":["../../x"]}"#, "not an object id");
        refused("chunked", br#"{"size":-1,"parts":[]}"#, "bad size");
        refused("chunked", br#"{"size":5,"parts":[9]}"#, "more glue than its size");
        let id = "ab".repeat(32);
        let (oid, raw) = pack("chunked", format!(r#"{{"size":5,"glue":null,"parts":["{id}",1]}}"#).as_bytes());
        assert!(verify(&oid, &raw).is_ok());
    }

    #[test]
    fn inflate_bomb_is_refused_at_the_cap() {
        // a commit of zeros past the 32 MiB cap for JSON objects: 33 MiB of
        // input to the compressor, a few dozen KiB stored
        refused("commit", &vec![b'0'; limits::MAX_META_OBJECT_SIZE + 1], "larger than");
        // a blob past MAX_BLOB_SIZE, compressed as a stream so it is never held
        use std::io::Write;
        let mut hasher_input = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
        hasher_input.write_all(b"blob\0").unwrap();
        let zeros = vec![0u8; 1 << 20];
        for _ in 0..(limits::MAX_BLOB_SIZE >> 20) + 1 {
            hasher_input.write_all(&zeros).unwrap();
        }
        let raw = hasher_input.finish().unwrap();
        assert!(raw.len() < 64 << 20);
        let err = verify(&"0".repeat(64), &raw).unwrap_err();
        assert!(err.contains("blob object is larger than"), "{err}");
    }

    #[test]
    fn decompress_errors_read_like_cpython() {
        assert_eq!(py_decompress(b"not zlib").unwrap_err(), "Error -3 while decompressing data: incorrect header check");
        let full = tmi_store::zlib::compress(b"blob\0hello hello hello", 1);
        assert_eq!(py_decompress(&full).unwrap(), b"blob\0hello hello hello");
        assert_eq!(
            py_decompress(&full[..full.len() - 3]).unwrap_err(),
            "Error -5 while decompressing data: incomplete or truncated stream"
        );
        assert!(verify("00", &full).unwrap_err().starts_with("object hash mismatch for 00: got "));
    }
}
