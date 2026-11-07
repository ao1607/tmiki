//! tmi/objects.py `ObjectStore`: content-addressed objects under
//! `.tmi/objects/<oid[:2]>/<oid[2:]>`, each `zlib(type + "\0" + payload)`.

use crate::chunking::{storage_segments, Segment};
use crate::json::{self, Int, Map, Value};
use crate::limits;
use crate::oid::object_id;
use crate::zlib;
use rayon::prelude::*;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;

const SAMPLE_SIZE: usize = 64 * 1024;
const SAMPLE_COUNT: usize = 3;
const COMPRESS_IF_RATIO_BELOW: f64 = 0.9;
/// objects.py `CHUNKED_MIN_SIZE`.
pub const CHUNKED_MIN_SIZE: usize = 1024 * 1024;

#[derive(Debug)]
pub enum StoreError {
    /// `FileNotFoundError(f"missing object {oid}")`
    Missing(String),
    /// `ValueError(f"object hash mismatch for {oid}: got {actual}")`
    HashMismatch { oid: String, actual: String },
    /// `ValueError(f"object {oid} is {type}, expected {expected}")`
    WrongType { oid: String, actual: String, expected: String },
    /// A chunked object that does not reassemble to its size.
    BadChunked(String),
    /// Anything else: unreadable or undecodable data, bad JSON, I/O.
    Other(String),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Missing(oid) => write!(f, "missing object {oid}"),
            StoreError::HashMismatch { oid, actual } => write!(f, "object hash mismatch for {oid}: got {actual}"),
            StoreError::WrongType { oid, actual, expected } => {
                write!(f, "object {oid} is {actual}, expected {expected}")
            }
            StoreError::BadChunked(msg) | StoreError::Other(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for StoreError {}

pub type Result<T> = std::result::Result<T, StoreError>;

/// objects.py `_zlib_level_for`: level 1 when deflate helps, else 0.
pub fn zlib_level_for(data: &[u8]) -> i32 {
    if data.len() <= 2 * SAMPLE_SIZE {
        return 1;
    }
    let count = (data.len() / (8 * SAMPLE_SIZE)).clamp(1, SAMPLE_COUNT);
    let step = (data.len() - SAMPLE_SIZE) / (count - 1).max(1);
    let compressed: usize = (0..count)
        .map(|i| {
            let a = i * step;
            let b = (a + SAMPLE_SIZE).min(data.len());
            zlib::compressed_len(&data[a..b], 1)
        })
        .sum();
    if (compressed as f64) < COMPRESS_IF_RATIO_BELOW * (SAMPLE_SIZE * count) as f64 {
        1
    } else {
        0
    }
}

pub struct ObjectStore {
    pub objects_dir: PathBuf,
}

impl ObjectStore {
    pub fn new(objects_dir: impl Into<PathBuf>) -> Self {
        ObjectStore { objects_dir: objects_dir.into() }
    }

    pub fn path_for(&self, oid: &str) -> PathBuf {
        let split = oid.char_indices().nth(2).map_or(oid.len(), |(i, _)| i);
        self.objects_dir.join(&oid[..split]).join(&oid[split..])
    }

    pub fn exists(&self, oid: &str) -> bool {
        self.path_for(oid).exists()
    }

    /// `read(oid)`: (type, payload), verified against the oid.
    pub fn read(&self, oid: &str) -> Result<(String, Vec<u8>)> {
        let path = self.path_for(oid);
        let raw = match fs::read(&path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(StoreError::Missing(oid.to_string())),
            Err(e) => return Err(StoreError::Other(format!("{}: {e}", path.display()))),
        };
        let data = limits::decompress_object(&raw).map_err(|e| StoreError::Other(format!("Error decompressing {oid}: {e}")))?;
        let nul = data
            .iter()
            .position(|&b| b == 0)
            .ok_or_else(|| StoreError::Other(format!("object {oid} has no type header")))?;
        let object_type = std::str::from_utf8(&data[..nul])
            .ok()
            .filter(|t| t.is_ascii())
            .ok_or_else(|| StoreError::Other(format!("object {oid} has a non-ASCII type")))?
            .to_string();
        let payload = data[nul + 1..].to_vec();
        let actual = object_id(&object_type, &payload);
        if actual != oid {
            return Err(StoreError::HashMismatch { oid: oid.to_string(), actual });
        }
        Ok((object_type, payload))
    }

    /// The object's type without keeping its payload (still verified).
    pub fn read_type(&self, oid: &str) -> Result<String> {
        Ok(self.read(oid)?.0)
    }

    pub fn read_json(&self, oid: &str, expected_type: Option<&str>) -> Result<Value> {
        let (object_type, payload) = self.read(oid)?;
        if let Some(expected) = expected_type {
            if !expected.is_empty() && object_type != expected {
                return Err(StoreError::WrongType {
                    oid: oid.to_string(),
                    actual: object_type,
                    expected: expected.to_string(),
                });
            }
        }
        json::loads_bytes(&payload).map_err(|e| StoreError::Other(format!("object {oid}: {e}")))
    }

    /// `read_blob(oid)`: a blob, or a chunked blob reassembled.
    pub fn read_blob(&self, oid: &str) -> Result<Vec<u8>> {
        let (object_type, payload) = self.read(oid)?;
        match object_type.as_str() {
            "chunked" => {
                let manifest = json::loads_bytes(&payload).map_err(|e| StoreError::Other(format!("object {oid}: {e}")))?;
                self.assemble_chunked(oid, &manifest)
            }
            "blob" => Ok(payload),
            other => Err(StoreError::WrongType {
                oid: oid.to_string(),
                actual: other.to_string(),
                expected: "blob".into(),
            }),
        }
    }

    /// A plain blob (a chunked part or glue): a chunked object here is refused.
    fn read_plain_blob(&self, oid: &str) -> Result<Vec<u8>> {
        let (object_type, payload) = self.read(oid)?;
        if object_type != "blob" {
            return Err(StoreError::WrongType { oid: oid.to_string(), actual: object_type, expected: "blob".into() });
        }
        Ok(payload)
    }

    fn assemble_chunked(&self, oid: &str, manifest: &Value) -> Result<Vec<u8>> {
        let bad = || StoreError::Other(format!("chunked object {oid} has a malformed manifest"));
        limits::check_chunk_manifest(manifest)
            .map_err(|why| StoreError::Other(format!("chunked object {oid} has a malformed manifest: {why}")))?;
        let size = manifest.get("size").and_then(Value::as_i64).ok_or_else(bad)?;
        let parts = manifest.get("parts").and_then(Value::as_array).ok_or_else(bad)?;
        let glue = match manifest.get("glue") {
            Some(g) if g.truthy() => self.read_plain_blob(g.as_str().ok_or_else(bad)?)?,
            _ => Vec::new(),
        };
        // The parts are read a few at a time and the running total is held to
        // the declared size, so a manifest that repeats a part or lists far
        // more than it declares stops early.
        let refs: Vec<&str> = parts.iter().filter_map(Value::as_str).collect();
        let mut read: Vec<Vec<u8>> = Vec::with_capacity(refs.len());
        let mut total: i64 = 0;
        for batch in refs.chunks(16) {
            let batch_pieces: Vec<Result<Vec<u8>>> = batch.par_iter().map(|r| self.read_plain_blob(r)).collect();
            for piece in batch_pieces {
                let piece = piece?;
                total += piece.len() as i64;
                if total > size {
                    return Err(StoreError::BadChunked(format!(
                        "chunked object {oid} reassembles to more than {size} bytes"
                    )));
                }
                read.push(piece);
            }
        }
        let mut pieces = read.into_iter().map(Ok::<Vec<u8>, StoreError>);
        let mut out = Vec::with_capacity(size as usize);
        let mut pos = 0usize;
        for part in parts {
            match part {
                Value::Int(Int::Small(n)) if *n >= 0 => {
                    let n = *n as usize;
                    let end = (pos + n).min(glue.len());
                    out.extend_from_slice(&glue[pos.min(glue.len())..end]);
                    pos += n;
                }
                Value::Str(_) => out.extend_from_slice(&pieces.next().unwrap()?),
                _ => return Err(bad()),
            }
        }
        if out.len() as i64 != size || pos != glue.len() {
            return Err(StoreError::BadChunked(format!(
                "chunked object {oid} reassembles to {} bytes, expected {size}",
                out.len()
            )));
        }
        Ok(out)
    }

    /// `write(type, payload)`.
    pub fn write(&self, object_type: &str, payload: &[u8]) -> Result<String> {
        let oid = object_id(object_type, payload);
        self.write_as(&oid, object_type, payload)?;
        Ok(oid)
    }

    fn write_as(&self, oid: &str, object_type: &str, payload: &[u8]) -> Result<()> {
        let path = self.path_for(oid);
        if !path.exists() {
            let mut data = Vec::with_capacity(object_type.len() + 1 + payload.len());
            data.extend_from_slice(object_type.as_bytes());
            data.push(0);
            data.extend_from_slice(payload);
            let level = zlib_level_for(&data);
            write_file_atomic(&path, &zlib::compress(&data, level))?;
        }
        Ok(())
    }

    pub fn write_json(&self, object_type: &str, value: &Value) -> Result<String> {
        self.write(object_type, &json::canonical_bytes(value))
    }

    /// `write_raw(oid, raw)`: store already-compressed bytes after checking
    /// they decode and hash to `oid`.
    pub fn write_raw(&self, oid: &str, raw: &[u8]) -> Result<()> {
        let corrupt = |e: String| StoreError::Other(format!("corrupt object data for {oid}: {e}"));
        let data = limits::decompress_object(raw).map_err(|e| corrupt(e.to_string()))?;
        let nul = data.iter().position(|&b| b == 0).ok_or_else(|| corrupt("no type header".into()))?;
        let object_type = std::str::from_utf8(&data[..nul]).map_err(|e| corrupt(e.to_string()))?;
        let actual = object_id(object_type, &data[nul + 1..]);
        if actual != oid {
            return Err(StoreError::Other(format!("object hash mismatch for {oid}: got {actual}")));
        }
        if limits::JSON_TYPES.contains(&object_type) {
            // Refused before it is stored: a payload nested too deep would
            // overflow a reader's stack, a malformed chunk manifest would make
            // every read of the blob costly.
            match json::loads_bytes(&data[nul + 1..]) {
                Err(e) if e.is_too_deep() => {
                    return Err(StoreError::Other(format!("refusing {object_type} {}: {e}", &oid[..oid.len().min(12)])))
                }
                Ok(manifest) if object_type == "chunked" => limits::check_chunk_manifest(&manifest)
                    .map_err(|e| StoreError::Other(format!("refusing chunked {}: {e}", &oid[..oid.len().min(12)])))?,
                _ => {}
            }
        }
        let path = self.path_for(oid);
        if path.exists() {
            return Ok(());
        }
        write_file_atomic(&path, raw)
    }

    pub fn write_large_blob(&self, payload: &[u8]) -> Result<String> {
        Ok(self.write_large_blobs(&[payload])?.remove(0))
    }

    /// `write_large_blobs(payloads)`: chunked objects for payloads of
    /// CHUNKED_MIN_SIZE or more with at least two shareable pieces.
    pub fn write_large_blobs(&self, payloads: &[&[u8]]) -> Result<Vec<String>> {
        self.large_blobs(payloads, true)
    }

    /// The oids `write_large_blobs` would return, writing nothing.
    pub fn large_blob_oids(&self, payloads: &[&[u8]]) -> Result<Vec<String>> {
        self.large_blobs(payloads, false)
    }

    fn large_blobs(&self, payloads: &[&[u8]], write: bool) -> Result<Vec<String>> {
        if payloads.len() == 1 && payloads[0].len() < CHUNKED_MIN_SIZE {
            if !write {
                return Ok(vec![object_id("blob", payloads[0])]);
            }
            return Ok(vec![self.write("blob", payloads[0])?]);
        }
        let plans: Vec<Option<Vec<Segment>>> = payloads
            .par_iter()
            .map(|p| {
                if p.len() < CHUNKED_MIN_SIZE {
                    return None;
                }
                let segments = storage_segments(p);
                (segments.iter().filter(|s| s.shareable).count() >= 2).then_some(segments)
            })
            .collect();
        enum Layout {
            Whole(usize),
            Chunked { size: usize, parts: Vec<(bool, usize)>, glue: Option<usize> },
        }
        let mut spans: Vec<std::borrow::Cow<[u8]>> = Vec::new();
        let mut layouts = Vec::new();
        for (payload, plan) in payloads.iter().zip(&plans) {
            let Some(segments) = plan else {
                layouts.push(Layout::Whole(spans.len()));
                spans.push(std::borrow::Cow::Borrowed(payload));
                continue;
            };
            let mut parts = Vec::new();
            let mut glue = Vec::new();
            for seg in segments {
                if seg.shareable {
                    parts.push((true, spans.len()));
                    spans.push(std::borrow::Cow::Borrowed(&payload[seg.start..seg.end]));
                } else {
                    parts.push((false, seg.end - seg.start));
                    glue.extend_from_slice(&payload[seg.start..seg.end]);
                }
            }
            let glue_span = if glue.is_empty() {
                None
            } else {
                spans.push(std::borrow::Cow::Owned(glue));
                Some(spans.len() - 1)
            };
            layouts.push(Layout::Chunked { size: payload.len(), parts, glue: glue_span });
        }
        let oids: Vec<String> = spans.par_iter().map(|s| object_id("blob", s)).collect();
        let mut unique: Vec<(usize, &str)> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for (i, oid) in oids.iter().enumerate() {
            if seen.insert(oid.as_str()) {
                unique.push((i, oid));
            }
        }
        if write {
            unique
                .par_iter()
                .map(|(i, oid)| self.write_as(oid, "blob", &spans[*i]))
                .collect::<Result<Vec<()>>>()?;
        }
        let mut out = Vec::new();
        for layout in layouts {
            match layout {
                Layout::Whole(i) => out.push(oids[i].clone()),
                Layout::Chunked { size, parts, glue } => {
                    let mut manifest = Map::new();
                    manifest.insert("size".into(), Value::Int(Int::Small(size as i64)));
                    manifest.insert("glue".into(), glue.map_or(Value::Null, |g| Value::Str(oids[g].clone())));
                    manifest.insert(
                        "parts".into(),
                        Value::Array(
                            parts
                                .into_iter()
                                .map(|(shared, v)| {
                                    if shared {
                                        Value::Str(oids[v].clone())
                                    } else {
                                        Value::Int(Int::Small(v as i64))
                                    }
                                })
                                .collect(),
                        ),
                    );
                    let manifest = Value::Object(manifest);
                    out.push(if write {
                        self.write_json("chunked", &manifest)?
                    } else {
                        object_id("chunked", &json::canonical_bytes(&manifest))
                    });
                }
            }
        }
        Ok(out)
    }

    /// `iter_object_ids()`: every stored oid, sorted.
    pub fn iter_object_ids(&self) -> Vec<String> {
        let mut ids = Vec::new();
        let Ok(dirs) = fs::read_dir(&self.objects_dir) else {
            return ids;
        };
        for sub in dirs.flatten() {
            let name = sub.file_name().to_string_lossy().into_owned();
            if name.chars().count() != 2 || !sub.path().is_dir() {
                continue;
            }
            let Ok(files) = fs::read_dir(sub.path()) else { continue };
            for f in files.flatten() {
                let fname = f.file_name().to_string_lossy().into_owned();
                if fname.chars().count() == 62 && f.path().is_file() {
                    ids.push(format!("{name}{fname}"));
                }
            }
        }
        ids.sort();
        ids
    }
}

fn random_suffix() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let mut h = RandomState::new().build_hasher();
    h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos()));
    h.write_u32(std::process::id());
    format!("{:016x}", h.finish())
}

/// objects.py `_write_file_atomic`: a sibling temp file renamed over the
/// target. On Windows another writer may be finishing the same object; a
/// refused replace is fine when the object is already there (objects are
/// content-addressed, so theirs is ours).
pub fn write_file_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let io = |e: std::io::Error| {
        if is_no_space(&e) {
            NO_SPACE.store(true, Ordering::SeqCst);
        }
        StoreError::Other(format!("{}: {e}", path.display()))
    };
    let parent = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(parent).map_err(io)?;
    if let Some(e) = failpoint_no_space() {
        return Err(io(e));
    }
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let (tmp, mut file) = loop {
        let tmp = parent.join(format!("{name}.{}.tmp", random_suffix()));
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600); // tempfile.mkstemp
        }
        match opts.open(&tmp) {
            Ok(f) => break (tmp, f),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(io(e)),
        }
    };
    let result = (|| {
        file.write_all(data)?;
        drop(file);
        match fs::rename(&tmp, path) {
            Ok(()) => {
                note_new_file(path);
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied && path.exists() => Ok(()),
            Err(e) => Err(e),
        }
    })();
    let _ = fs::remove_file(&tmp);
    result.map_err(io)
}

// ---- a full disk (issue #42; tmi/diskspace.py is the Python side) ----

static TRACKED: Mutex<Option<Vec<PathBuf>>> = Mutex::new(None);
static NO_SPACE: AtomicBool = AtomicBool::new(false);
static WRITES: AtomicUsize = AtomicUsize::new(0);

/// Start recording the files `write_file_atomic` creates (objects and render
/// cache entries), and forget an earlier "disk full".
pub fn track_begin() {
    NO_SPACE.store(false, Ordering::SeqCst);
    *TRACKED.lock().unwrap() = Some(Vec::new());
}

/// Stop recording: the files created since `track_begin` (or `track_keep`).
pub fn track_end() -> Vec<PathBuf> {
    TRACKED.lock().unwrap().take().unwrap_or_default()
}

/// What was written so far is referred to now: a later failure must not
/// remove it.
pub fn track_keep() {
    if let Some(v) = TRACKED.lock().unwrap().as_mut() {
        v.clear();
    }
}

/// Whether a write found the disk full since `track_begin`.
pub fn no_space() -> bool {
    NO_SPACE.load(Ordering::SeqCst)
}

fn note_new_file(path: &Path) {
    if let Some(v) = TRACKED.lock().unwrap().as_mut() {
        v.push(path.to_path_buf());
    }
}

/// `OSError.errno in (ENOSPC, EDQUOT)` (Windows' ERROR_DISK_FULL and
/// ERROR_HANDLE_DISK_FULL are ENOSPC to Python).
pub fn is_no_space(e: &std::io::Error) -> bool {
    match e.raw_os_error() {
        #[cfg(windows)]
        Some(112 | 39) => true,
        #[cfg(target_os = "linux")]
        Some(28 | 122) => true,
        #[cfg(target_os = "macos")]
        Some(28 | 69) => true,
        #[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
        Some(28) => true,
        _ => false,
    }
}

/// `TMI_TEST_ENOSPC_AFTER=N`: the (N+1)th new object / cache file fails as a
/// full disk does. For the tests; nothing sets it otherwise.
fn failpoint_no_space() -> Option<std::io::Error> {
    let limit = std::env::var("TMI_TEST_ENOSPC_AFTER").ok().filter(|v| !v.is_empty())?;
    let allowed: usize = limit.parse().ok()?;
    if WRITES.fetch_add(1, Ordering::SeqCst) >= allowed {
        #[cfg(windows)]
        return Some(std::io::Error::from_raw_os_error(112));
        #[cfg(not(windows))]
        return Some(std::io::Error::from_raw_os_error(28));
    }
    None
}
