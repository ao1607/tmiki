//! What one call's store holds, and the host helpers of the WIT
//! (PLUGINS.md 2.1, 3.3): `source`, `codecs`, `sqlite`, `xml`, `log`.
//!
//! Every helper's output is fixed by its name (3.3): the codecs are the
//! recipe decoder's own (`tmi_format::recipe::transform`), SQLite is the
//! workspace's 3.50.4, the XML is roxmltree with tmi-clip's MDP2 rules.

use std::borrow::Cow;

use wasmtime::component::{Resource, ResourceTable};
use wasmtime::ResourceLimiter;
use wasmtime_wasi::p2::pipe::MemoryOutputPipe;
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

use crate::bindings::tmi::format::{codecs, edit, log, source, sqlite, types, xml};
use crate::bindings::wit as w;
use crate::convert;

/// The `source-file` resource: the call's one file (the bytes are in
/// [`HostState`]).
pub struct SourceFileRes;

/// The `edit.pixels` resource: the handle's number is the pixels' number in
/// the call's [`tmi_format::write::WriteCx`].
pub struct PixelsRes;

/// The `edit.blob` resource: the handle's number is the blob's number in the
/// call's `WriteCx`.
pub struct BlobRes;

/// The `database` resource.
pub struct DatabaseRes {
    conn: rusqlite::Connection,
    /// The bytes it was opened from (counted against the call's maxMemory).
    image_len: usize,
}

/// Bytes one call may hand out through `log.emit`: every line counts its
/// bytes and [`LOG_ENTRY_COST`] (an empty line is not free).
pub const LOG_CAPACITY: usize = 1 << 20;
/// What a `log.emit` line costs beyond its text: the stored `(level, String)`
/// is 32 bytes and the allocator keeps a header besides. So at most
/// `LOG_CAPACITY / LOG_ENTRY_COST` = 16,384 lines, whatever their length.
pub const LOG_ENTRY_COST: usize = 64;
/// Bytes one `sqlite.query` may return.
pub const QUERY_CAPACITY: usize = 256 << 20;
/// Databases one call may hold open at once (`sqlite.database` resources).
pub const MAX_OPEN_DATABASES: usize = 8;
/// A statement may run this many SQLite virtual machine instructions
/// (`sqlite3_progress_handler`; counted, so the same on every machine). Host
/// functions are out of reach of fuel and the epoch, so a statement is
/// stopped from inside.
pub const SQL_MAX_VM_STEPS: u64 = 500_000_000;
/// `xml.parse`: elements nested deeper than this are refused before the
/// parser sees them (roxmltree recurses once per level and the worker's stack
/// is finite; the documents tmi reads nest a dozen levels).
pub const XML_MAX_DEPTH: usize = 256;
/// `xml.parse`: nodes (elements, text, comments) in one document.
pub const XML_MAX_NODES: u32 = 1 << 20;
/// `xml.parse`: bytes of one document (the answer is about 30 times larger
/// than the text of an empty element).
pub const XML_MAX_INPUT: usize = 16 << 20;
/// `xml.parse`: bytes of the answer (names, namespaces, attributes, text and
/// a little per element), counted before each is copied.
pub const XML_MAX_ANSWER: usize = 4 * XML_MAX_INPUT;

/// The source file of a call, borrowed for the call's length.
pub struct SourceView {
    pub name: String,
    ptr: *const u8,
    len: usize,
}

// SAFETY: a `SourceView` points at bytes the caller of `Plugin::call`
// borrows for the whole call and that nobody writes; the store holding it
// is dropped before that call returns (plugin.rs).
unsafe impl Send for SourceView {}

impl SourceView {
    /// # Safety
    /// `data` must outlive every use of the view (the store it goes into
    /// must be dropped first).
    pub unsafe fn new(name: &str, data: &[u8]) -> SourceView {
        SourceView { name: name.to_string(), ptr: data.as_ptr(), len: data.len() }
    }

    pub fn bytes(&self) -> &[u8] {
        // SAFETY: see `new`.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

/// The store's own limiter: `StoreLimits`' memory rule over the sum of an
/// instance's memories, remembering whether it said no (PLUGINS.md 3.1:
/// `memory.grow` gives -1 past the limit, which is deterministic; a plugin
/// that then traps is reported as over its memory limit).
pub struct Limiter {
    pub max_memory: usize,
    pub in_use: usize,
    pub denied: bool,
}

impl ResourceLimiter for Limiter {
    fn memory_growing(&mut self, current: usize, desired: usize, _maximum: Option<usize>) -> wasmtime::Result<bool> {
        let after = self.in_use - current.min(self.in_use) + desired;
        if after > self.max_memory {
            self.denied = true;
            return Ok(false);
        }
        self.in_use = after;
        Ok(true)
    }

    fn table_growing(&mut self, _current: usize, desired: usize, _maximum: Option<usize>) -> wasmtime::Result<bool> {
        // tables hold function references; 1M entries is far beyond any plugin
        Ok(desired <= 1 << 20)
    }
}

/// One call's store data.
pub struct HostState {
    pub wasi: WasiCtx,
    pub table: ResourceTable,
    pub limiter: Limiter,
    pub source: Option<SourceView>,
    pub stdout: MemoryOutputPipe,
    pub stderr: MemoryOutputPipe,
    /// `log.emit` lines, in order.
    pub log: Vec<(log::Level, String)>,
    log_bytes: usize,
    pub log_truncated: bool,
    /// `sqlite.database` resources alive now.
    open_databases: usize,
    /// The image bytes of those databases.
    db_image_bytes: usize,
    /// The `edit` resources of a `write-layers` call (None in every other
    /// call: `edit` then traps).
    pub write: Option<tmi_format::write::WriteCx>,
    /// `Settings::sqlite_reverse_unordered_selects`.
    pub reverse_unordered_selects: bool,
}

impl HostState {
    pub fn new(source: Option<SourceView>, max_memory: usize) -> HostState {
        let (wasi, stdout, stderr) = crate::wasi::context();
        HostState {
            wasi,
            table: ResourceTable::new(),
            limiter: Limiter { max_memory, in_use: 0, denied: false },
            source,
            stdout,
            stderr,
            log: Vec::new(),
            log_bytes: 0,
            log_truncated: false,
            open_databases: 0,
            db_image_bytes: 0,
            write: None,
            reverse_unordered_selects: false,
        }
    }

    fn source_bytes(&self) -> wasmtime::Result<&[u8]> {
        match &self.source {
            Some(s) => Ok(s.bytes()),
            None => wasmtime::bail!("this call has no source file"),
        }
    }

    /// `bytes` as a byte string (a span must be inside the file).
    fn resolve(&self, b: w::Bytes) -> Result<Cow<'_, [u8]>, String> {
        resolve(self.source.as_ref().map(|s| s.bytes()), &convert::bytes(b)).map(|c| Cow::Owned(c.into_owned()))
    }

    fn source_or_empty(&self) -> &[u8] {
        self.source.as_ref().map_or(&[][..], |s| s.bytes())
    }
}

// -- the helpers both backends answer with (wasm here, `tmiki plugin dev` in
// dev.rs): one function per WIT import, over tmi_format's types --------------

/// `bytes` as a byte string; `source`: the call's file, if it has one (a span
/// must be inside it).
pub fn resolve<'a>(source: Option<&'a [u8]>, b: &'a tmi_format::types::Bytes) -> Result<Cow<'a, [u8]>, String> {
    if source.is_none() && !matches!(b, tmi_format::types::Bytes::Inline(_)) {
        return Err("a span, but this call has no source file".into());
    }
    tmi_format::recipe::transform::resolve(source.unwrap_or(&[]), b).map_err(|e| e.to_string())
}

/// `codecs.transform`: the recipe decoder's transforms (`codec::plugin`
/// excluded), every stage held to `max_out`.
pub fn transform(source: Option<&[u8]>, data: &tmi_format::types::Bytes, list: &[tmi_format::types::Codec], max_out: u32) -> Result<Vec<u8>, String> {
    let input = resolve(source, data)?;
    let cx = tmi_format::reader::ReadCx::new(None);
    let env = tmi_format::recipe::DecodeEnv { source: source.unwrap_or(&[]), reader: None, cx: &cx };
    tmi_format::recipe::transform::apply_capped(&env, list, input, max_out as usize).map(Cow::into_owned).map_err(|e| e.to_string())
}

/// `codecs.crc32` (an error is a trap: the bytes are not there).
pub fn crc32(source: Option<&[u8]>, data: &tmi_format::types::Bytes) -> Result<u32, String> {
    let b = resolve(source, data)?;
    let mut c = flate2::Crc::new();
    c.update(&b);
    Ok(c.sum())
}

/// `codecs.sha256` (an error is a trap).
pub fn sha256(source: Option<&[u8]>, data: &tmi_format::types::Bytes) -> Result<Vec<u8>, String> {
    let b = resolve(source, data)?;
    Ok(ring::digest::digest(&ring::digest::SHA256, &b).as_ref().to_vec())
}

/// `codecs.image-info`: PNG and `jpeg-v1` only (P5): the header tmi's import
/// reads (`tmi_image::header`), no pixels. `has-alpha` is whether the mode
/// Pillow opens the image in has an alpha band (RGBA, LA, PA, RGBa, La): a
/// palette or grey image with a tRNS chunk says false.
pub fn image_info(source: Option<&[u8]>, img: &tmi_format::types::EncodedImage, max_expanded: usize) -> Result<tmi_format::types::ImageHeader, String> {
    use tmi_format::types::ImageCodec;
    if !matches!(img.codec, ImageCodec::Png | ImageCodec::JpegV1) {
        return Err(format!("image-info reads png and jpeg-v1 headers only, not {:?}", img.codec));
    }
    let src = source.unwrap_or(&[]);
    let data = tmi_format::recipe::transform::resolve(src, &img.data).map_err(|e| e.to_string())?;
    let cx = tmi_format::reader::ReadCx::new(None);
    let env = tmi_format::recipe::DecodeEnv { source: src, reader: None, cx: &cx };
    let data = tmi_format::recipe::transform::apply_capped(&env, &img.pre, data, max_expanded.min(tmi_format::recipe::transform::MAX_EXPANDED)).map_err(|e| e.to_string())?;
    let h = match tmi_image::header::header(&data) {
        Ok(Some(h)) => h,
        Ok(None) => return Err("not a png or jpeg".into()),
        Err(e) => return Err(e),
    };
    let want = if img.codec == ImageCodec::Png { "png" } else { "jpeg" };
    if h.format != want {
        return Err(format!("the data is {}, not {want}", h.format));
    }
    Ok(tmi_format::types::ImageHeader {
        width: h.size.0 as u32,
        height: h.size.1 as u32,
        frames: 1,
        has_alpha: matches!(h.mode, Some("RGBA" | "LA" | "PA" | "RGBa" | "La")),
        dpi: h.dpi.map(|d| (d, d)),
        icc: h.icc,
    })
}

impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView { ctx: &mut self.wasi, table: &mut self.table }
    }
}

// -- source --------------------------------------------------------------

impl source::HostSourceFile for HostState {
    fn size(&mut self, r: Resource<SourceFileRes>) -> wasmtime::Result<u64> {
        self.table.get(&r)?;
        Ok(self.source_bytes()?.len() as u64)
    }

    fn read(&mut self, r: Resource<SourceFileRes>, offset: u64, len: u32) -> wasmtime::Result<Vec<u8>> {
        self.table.get(&r)?;
        let data = self.source_bytes()?;
        let start = offset.min(data.len() as u64) as usize;
        let end = start.saturating_add(len as usize).min(data.len());
        Ok(data[start..end].to_vec())
    }

    fn name(&mut self, r: Resource<SourceFileRes>) -> wasmtime::Result<String> {
        self.table.get(&r)?;
        Ok(self.source.as_ref().map(|s| s.name.clone()).unwrap_or_default())
    }

    fn drop(&mut self, r: Resource<SourceFileRes>) -> wasmtime::Result<()> {
        self.table.delete(r)?;
        Ok(())
    }
}

impl source::Host for HostState {}
impl types::Host for HostState {}

// -- codecs --------------------------------------------------------------

impl codecs::Host for HostState {
    fn transform(&mut self, data: w::Bytes, list: Vec<w::Codec>, max_out: u32) -> wasmtime::Result<Result<Vec<u8>, String>> {
        // the answer goes into the plugin's memory: no stage may make more
        // than the room it has left (not 1 GiB of host memory per call)
        let room = self.limiter.max_memory.saturating_sub(self.limiter.in_use);
        let max_out = max_out.min(room.min(u32::MAX as usize) as u32);
        Ok(transform(self.source.as_ref().map(|s| s.bytes()), &convert::bytes(data), &convert::codecs(list), max_out))
    }

    fn crc32(&mut self, data: w::Bytes) -> wasmtime::Result<u32> {
        crc32(self.source.as_ref().map(|s| s.bytes()), &convert::bytes(data)).map_err(wasmtime::Error::msg)
    }

    fn sha256(&mut self, data: w::Bytes) -> wasmtime::Result<Vec<u8>> {
        sha256(self.source.as_ref().map(|s| s.bytes()), &convert::bytes(data)).map_err(wasmtime::Error::msg)
    }

    /// [`image_info`].
    fn image_info(&mut self, img: w::EncodedImage) -> wasmtime::Result<Result<codecs::ImageHeader, String>> {
        // (image-info reads a span of the file even when the call has none: as before)
        let src = Some(self.source_or_empty());
        // the header is read from at most the plugin's maxMemory of expanded data
        Ok(image_info(src, &convert::encoded_image(img), self.limiter.max_memory).map(|h| codecs::ImageHeader {
            width: h.width,
            height: h.height,
            frames: h.frames,
            has_alpha: h.has_alpha,
            dpi: h.dpi,
            icc: h.icc,
        }))
    }
}

// -- sqlite --------------------------------------------------------------

fn sql_value(v: sqlite::SqlValue) -> rusqlite::types::Value {
    use rusqlite::types::Value;
    match v {
        sqlite::SqlValue::Null => Value::Null,
        sqlite::SqlValue::Integer(i) => Value::Integer(i),
        sqlite::SqlValue::Real(f) => Value::Real(f),
        sqlite::SqlValue::Text(s) => Value::Text(s),
        sqlite::SqlValue::Blob(b) => Value::Blob(b),
    }
}

/// Pragmas a plugin may read but not set: they would undo the limits below
/// (`temp_store` puts temporary tables and sorts in files in the user's temp
/// directory; `max_page_count`, `page_size` and the heap limits (process-wide:
/// they would reach the other SQLite users of the process, CLIP's import
/// among them) move the memory budget; `threads` starts helpers). The two
/// directory pragmas are refused whatever the value.
const SET_REFUSED_PRAGMAS: [&str; 7] = ["temp_store", "max_page_count", "page_size", "soft_heap_limit", "hard_heap_limit", "threads", "mmap_size"];

/// The memory one database of a call may take, besides the image it was
/// opened from: the call's `plugin.maxMemory` over the databases a call may
/// hold (8) and the two places pages live (the database and the temporary
/// tables), so that all of them together stay within one `maxMemory`. At
/// least [`MIN_DB_BUDGET`].
fn db_budget(max_memory: usize) -> usize {
    (max_memory / (2 * MAX_OPEN_DATABASES)).max(MIN_DB_BUDGET)
}

/// Floor of [`db_budget`]: a plugin with a tiny `plugin.maxMemory` still gets
/// databases that work.
const MIN_DB_BUDGET: usize = 1 << 20;

/// What a plugin's SQLite may not do: reach a file (no ATTACH, no VACUUM
/// INTO, which attaches its target, no directory pragmas, no extensions),
/// keep temporary data in a file, or take more memory than `budget` (growth of
/// the database, growth of the temporary tables, one value) and the memory
/// the plugin's `max_memory` allows (`db_budget`). SQLite's heap limits are
/// per process, so the limits here are the ones a connection has of its own:
/// `max_page_count` of `main` and `temp`, `SQLITE_LIMIT_LENGTH`, and the
/// memory-only `temp_store`.
///
/// What is not bounded by these: one statement's own memory (a row of many
/// large values, the transient tables of `SELECT DISTINCT`, `ORDER BY` and
/// `GROUP BY`, in memory under `temp_store=MEMORY`): one VM step can make a
/// value of `length` bytes, so the step budget does not bound it. The
/// process-wide [`PLUGIN_SQLITE_HEAP_LIMIT`] does, and [`MAX_SQL_COLUMNS`]
/// keeps one row smaller.
fn lock_down(conn: &rusqlite::Connection, image_len: usize, budget: usize) -> rusqlite::Result<()> {
    use rusqlite::hooks::{AuthAction, Authorization};
    use rusqlite::limits::Limit;
    conn.set_limit(Limit::SQLITE_LIMIT_ATTACHED, 0)?;
    // one result row is held whole by SQLite: fewer columns of at most
    // `length` bytes each (wider tables than this are not seen in practice)
    conn.set_limit(Limit::SQLITE_LIMIT_COLUMN, MAX_SQL_COLUMNS)?;
    set_heap_limit();
    // a value (a blob, a string, one row) of the size of the budget, and
    // never below a CLIP-sized tile many times over
    let length = budget.max(16 << 20).min(i32::MAX as usize) as i32;
    conn.set_limit(Limit::SQLITE_LIMIT_LENGTH, length)?;
    conn.set_limit(Limit::SQLITE_LIMIT_SQL_LENGTH, length)?;
    // temporary tables and sorts in memory, whatever SQLite was built to do
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    // the database may grow by the budget past the image it holds; the
    // temporary tables by the budget (SQLite never sets the cap below the
    // size the database already has)
    let page_size: i64 = conn.query_row("PRAGMA page_size", [], |r| r.get(0))?;
    let page_size = page_size.max(512) as usize;
    conn.pragma_update(None, "max_page_count", ((image_len + budget) / page_size).clamp(1, 0x7fff_ffff) as i64)?;
    conn.pragma_update(Some("temp"), "max_page_count", (budget / 4096).clamp(1, 0x7fff_ffff) as i64)?;
    conn.authorizer(Some(|ctx: rusqlite::hooks::AuthContext<'_>| match ctx.action {
        AuthAction::Attach { .. } | AuthAction::Detach { .. } => Authorization::Deny,
        AuthAction::Pragma { pragma_name, .. }
            if pragma_name.eq_ignore_ascii_case("temp_store_directory") || pragma_name.eq_ignore_ascii_case("data_store_directory") =>
        {
            Authorization::Deny
        }
        AuthAction::Pragma { pragma_name, pragma_value: Some(_) } if SET_REFUSED_PRAGMAS.iter().any(|p| pragma_name.eq_ignore_ascii_case(p)) => Authorization::Deny,
        _ => Authorization::Allow,
    }))?;
    Ok(())
}

/// Columns a plugin's statement may have (SQLite's default is 2000).
const MAX_SQL_COLUMNS: i32 = 1000;

/// SQLite's heap limit for the process, set when a plugin first opens a
/// database. Nothing per connection bounds one statement's own memory (a
/// row of large values, a sort or DISTINCT over large rows under
/// `temp_store=MEMORY`); past this SQLite fails the statement with "out of
/// memory" instead of the process running out. It is process-wide, so it is
/// far above what the built-in readers need (a CLIP or MDP2 database is part
/// of a file of at most a few GiB, held once or twice).
const PLUGIN_SQLITE_HEAP_LIMIT: i64 = 8 << 30;

fn set_heap_limit() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // SAFETY: a plain setter, safe to call at any time from any thread.
        let current = unsafe { rusqlite::ffi::sqlite3_hard_heap_limit64(-1) };
        if current <= 0 || current > PLUGIN_SQLITE_HEAP_LIMIT {
            unsafe { rusqlite::ffi::sqlite3_hard_heap_limit64(PLUGIN_SQLITE_HEAP_LIMIT) };
        }
    });
}

/// [`open_database_with`] for a plugin that may take as much as
/// `Settings::default()` allows (`tmiki plugin dev`, whose native plugin is the
/// author's own process).
pub fn open_database(bytes: &[u8], read_only: bool, reverse: bool) -> Result<rusqlite::Connection, String> {
    open_database_with(bytes, read_only, reverse, crate::engine::Settings::default().max_memory)
}

/// `sqlite.database.open` / `open-writable` of the image `bytes`; `reverse`:
/// `PRAGMA reverse_unordered_selects=ON` (`Settings::sqlite_reverse_unordered_selects`);
/// `max_memory`: the call's `plugin.maxMemory`, which the database's own
/// memory is held to (see [`lock_down`]).
pub fn open_database_with(bytes: &[u8], read_only: bool, reverse: bool, max_memory: usize) -> Result<rusqlite::Connection, String> {
    let opened = (|| -> rusqlite::Result<rusqlite::Connection> {
        let mut conn = rusqlite::Connection::open_in_memory()?;
        // an empty image is an empty database (as SQLite takes a 0-byte file)
        if bytes.is_empty() {
            if read_only {
                conn.pragma_update(None, "query_only", true)?;
            }
        } else {
            conn.deserialize_read_exact(rusqlite::MAIN_DB, bytes, bytes.len(), read_only)?;
        }
        lock_down(&conn, bytes.len(), db_budget(max_memory))?;
        if reverse {
            conn.pragma_update(None, "reverse_unordered_selects", true)?;
        }
        Ok(conn)
    })();
    opened.map_err(|e| e.to_string())
}

/// Runs `f` with the statement step budget (`SQL_MAX_VM_STEPS`) installed.
fn with_step_budget<T>(conn: &rusqlite::Connection, f: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    const EVERY: i32 = 1000;
    let mut steps = 0u64;
    let _ = conn.progress_handler(
        EVERY,
        Some(move || {
            steps += EVERY as u64;
            steps > SQL_MAX_VM_STEPS
        }),
    );
    let r = f();
    let _ = conn.progress_handler(EVERY, None::<fn() -> bool>);
    r.map_err(|e| if e == "interrupted" { format!("the statement ran over {SQL_MAX_VM_STEPS} virtual machine steps and was stopped") } else { e })
}

/// `database.execute`.
pub fn execute(conn: &rusqlite::Connection, sql: &str, params: Vec<sqlite::SqlValue>) -> Result<u64, String> {
    let values: Vec<rusqlite::types::Value> = params.into_iter().map(sql_value).collect();
    with_step_budget(conn, || conn.execute(sql, rusqlite::params_from_iter(values.iter())).map(|n| n as u64).map_err(|e| e.to_string()))
}

/// `database.serialize`.
pub fn serialize(conn: &rusqlite::Connection) -> Result<Vec<u8>, String> {
    conn.serialize(rusqlite::MAIN_DB).map(|d| d.to_vec()).map_err(|e| e.to_string())
}

impl HostState {
    fn open_db(&mut self, image: w::Bytes, read_only: bool) -> wasmtime::Result<Result<Resource<DatabaseRes>, String>> {
        if self.open_databases >= MAX_OPEN_DATABASES {
            return Ok(Err(format!("{MAX_OPEN_DATABASES} databases are open already (the most a call may hold)")));
        }
        // the images of a call's databases together are held to its
        // maxMemory, checked before one is copied (a span may repeat the file)
        let room = self.limiter.max_memory.saturating_sub(self.db_image_bytes);
        let len = match &image {
            w::Bytes::Inline(b) => b.len() as u64,
            w::Bytes::Span(s) => s.len as u64,
            w::Bytes::Spans(l) => l.iter().map(|s| s.len as u64).sum(),
        };
        let len = len.min(usize::MAX as u64) as usize;
        if len > room {
            return Ok(Err(format!("database images over the plugin's memory limit ({} bytes)", self.limiter.max_memory)));
        }
        let bytes = match self.resolve(image) {
            Ok(b) => b.into_owned(),
            Err(e) => return Ok(Err(e)),
        };
        if bytes.len() > room {
            return Ok(Err(format!("database images over the plugin's memory limit ({} bytes)", self.limiter.max_memory)));
        }
        match open_database_with(&bytes, read_only, self.reverse_unordered_selects, self.limiter.max_memory) {
            Ok(conn) => {
                let r = self.table.push(DatabaseRes { conn, image_len: bytes.len() })?;
                self.open_databases += 1;
                self.db_image_bytes += bytes.len();
                Ok(Ok(r))
            }
            Err(e) => Ok(Err(e)),
        }
    }
}

/// `database.query` (at most [`QUERY_CAPACITY`] bytes of results).
pub fn run_query(conn: &rusqlite::Connection, sql: &str, params: Vec<sqlite::SqlValue>) -> Result<Vec<Vec<sqlite::SqlValue>>, String> {
    with_step_budget(conn, || run_query_inner(conn, sql, params))
}

fn run_query_inner(conn: &rusqlite::Connection, sql: &str, params: Vec<sqlite::SqlValue>) -> Result<Vec<Vec<sqlite::SqlValue>>, String> {
    use rusqlite::types::ValueRef;
    let mut stmt = conn.prepare(sql).map_err(|e| e.to_string())?;
    let values: Vec<rusqlite::types::Value> = params.into_iter().map(sql_value).collect();
    let n = stmt.column_count();
    let mut rows = stmt.query(rusqlite::params_from_iter(values.iter())).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    let mut size = 0usize;
    while let Some(row) = rows.next().map_err(|e| e.to_string())? {
        let mut r = Vec::with_capacity(n);
        for i in 0..n {
            let v = row.get_ref(i).map_err(|e| e.to_string())?;
            size += 8 + match v {
                ValueRef::Text(b) | ValueRef::Blob(b) => b.len(),
                _ => 0,
            };
            // checked before the value is copied, not after the whole row
            if size > QUERY_CAPACITY {
                return Err(format!("the result is over {QUERY_CAPACITY} bytes"));
            }
            r.push(match v {
                ValueRef::Null => sqlite::SqlValue::Null,
                ValueRef::Integer(i) => sqlite::SqlValue::Integer(i),
                ValueRef::Real(f) => sqlite::SqlValue::Real(f),
                ValueRef::Text(t) => sqlite::SqlValue::Text(String::from_utf8_lossy(t).into_owned()),
                ValueRef::Blob(b) => sqlite::SqlValue::Blob(b.to_vec()),
            });
        }
        out.push(r);
    }
    Ok(out)
}

impl sqlite::HostDatabase for HostState {
    fn open(&mut self, image: w::Bytes) -> wasmtime::Result<Result<Resource<DatabaseRes>, String>> {
        self.open_db(image, true)
    }

    fn open_writable(&mut self, image: w::Bytes) -> wasmtime::Result<Result<Resource<DatabaseRes>, String>> {
        self.open_db(image, false)
    }

    fn query(&mut self, r: Resource<DatabaseRes>, sql: String, params: Vec<sqlite::SqlValue>) -> wasmtime::Result<Result<Vec<Vec<sqlite::SqlValue>>, String>> {
        let db = self.table.get(&r)?;
        Ok(run_query(&db.conn, &sql, params))
    }

    fn execute(&mut self, r: Resource<DatabaseRes>, sql: String, params: Vec<sqlite::SqlValue>) -> wasmtime::Result<Result<u64, String>> {
        let db = self.table.get(&r)?;
        Ok(execute(&db.conn, &sql, params))
    }

    fn serialize(&mut self, r: Resource<DatabaseRes>) -> wasmtime::Result<Result<Vec<u8>, String>> {
        let db = self.table.get(&r)?;
        Ok(serialize(&db.conn))
    }

    fn drop(&mut self, r: Resource<DatabaseRes>) -> wasmtime::Result<()> {
        let db = self.table.delete(r)?;
        self.open_databases = self.open_databases.saturating_sub(1);
        self.db_image_bytes = self.db_image_bytes.saturating_sub(db.image_len);
        Ok(())
    }
}

impl sqlite::Host for HostState {}

// -- xml -----------------------------------------------------------------

fn attr_key(a: &roxmltree::Attribute<'_, '_>) -> String {
    match a.namespace() {
        Some(ns) => format!("{{{ns}}}{}", a.name()),
        None => a.name().to_string(),
    }
}

/// Refuses a document whose elements nest deeper than [`XML_MAX_DEPTH`]
/// before roxmltree (which recurses once per level) sees it:
/// [`tmi_clip::mdp2::check_xml_depth`], which the built-in MDP2 reader uses
/// too.
pub(crate) fn check_xml_depth(s: &str) -> Result<(), String> {
    tmi_clip::mdp2::check_xml_depth(s, XML_MAX_DEPTH)
}

/// `xml.parse`: the elements in document order, each with its parent's
/// index. Parsed as tmi-clip parses MDP2's XML (roxmltree; attribute values
/// normalised as expat does; namespaced attribute names as `{uri}name`; a
/// document type declaration with an internal subset or an external id is
/// refused). `text` is ElementTree's `.text`: the text and CDATA before the
/// element's first child element (comments and processing instructions are
/// skipped; the text after a child, ElementTree's `.tail`, is not given).
///
/// Limits (the answer is the plugin's to hold in its memory, and roxmltree
/// recurses once per nesting level on the worker's stack): at most
/// [`XML_MAX_INPUT`] bytes, [`XML_MAX_DEPTH`] levels, [`XML_MAX_NODES`]
/// nodes. The depth is counted by [`check_xml_depth`] before the parser runs.
pub fn parse_xml(text: &[u8]) -> Result<Vec<xml::Element>, String> {
    if text.len() > XML_MAX_INPUT {
        return Err(format!("the XML is over {XML_MAX_INPUT} bytes"));
    }
    let s = std::str::from_utf8(text).map_err(|e| format!("not UTF-8: {e}"))?;
    check_xml_depth(s)?;
    let options = roxmltree::ParsingOptions { allow_dtd: true, nodes_limit: XML_MAX_NODES, ..Default::default() };
    let doc = roxmltree::Document::parse_with_options(s, options).map_err(|e| e.to_string())?;
    let root = doc.root_element();
    tmi_clip::mdp2::check_doctype(&s[..root.range().start])?;
    let mut out: Vec<xml::Element> = Vec::new();
    let mut index = std::collections::HashMap::new();
    // roxmltree keeps a namespace once; the answer copies it into every
    // element and prefixed attribute, so its size is counted as it is made
    let mut answer = 0usize;
    for node in root.descendants().filter(|n| n.is_element()) {
        let parent = node.parent_element().and_then(|p| index.get(&p.id()).copied());
        let ns = |n: Option<&str>| n.map_or(0, |s| s.len() + 2);
        answer += 16 + node.tag_name().name().len() + ns(node.tag_name().namespace());
        answer += node.attributes().map(|a| 8 + a.name().len() + ns(a.namespace()) + a.value().len()).sum::<usize>();
        let texts = node.children().take_while(|c| !c.is_element()).filter(|c| c.is_text());
        answer += texts.map(|c| c.text().unwrap_or("").len()).sum::<usize>();
        if answer > XML_MAX_ANSWER {
            return Err(format!("the parsed XML is over {XML_MAX_ANSWER} bytes"));
        }
        let mut t = String::new();
        for c in node.children() {
            if c.is_element() {
                break;
            }
            if c.is_text() {
                t.push_str(c.text().unwrap_or(""));
            }
        }
        index.insert(node.id(), out.len() as u32);
        out.push(xml::Element {
            name: node.tag_name().name().to_string(),
            namespace: node.tag_name().namespace().map(String::from),
            attributes: node.attributes().map(|a| (attr_key(&a), a.value().to_string())).collect(),
            text: t,
            parent,
        });
    }
    Ok(out)
}

impl xml::Host for HostState {
    fn parse(&mut self, text: Vec<u8>) -> wasmtime::Result<Result<Vec<xml::Element>, String>> {
        Ok(parse_xml(&text))
    }
}

// -- log -----------------------------------------------------------------

impl log::Host for HostState {
    fn emit(&mut self, level: log::Level, message: String) -> wasmtime::Result<()> {
        let cost = message.len().saturating_add(LOG_ENTRY_COST);
        if self.log_bytes.saturating_add(cost) > LOG_CAPACITY {
            self.log_truncated = true;
            return Ok(());
        }
        self.log_bytes += cost;
        self.log.push((level, message));
        Ok(())
    }
}

// -- edit (write-back; PLUGINS.md P7) ---------------------------------------

impl HostState {
    fn write_cx(&mut self) -> wasmtime::Result<&mut tmi_format::write::WriteCx> {
        self.write.as_mut().ok_or_else(|| wasmtime::Error::msg("edit is only for write-layers"))
    }
}

fn encoding(e: edit::Encoding) -> tmi_format::types::Encoding {
    use tmi_format::types::{Encoding as E, ZlibKernel as K};
    let kernel = |k: edit::ZlibKernel| match k {
        edit::ZlibKernel::ZlibV131 => K::ZlibV131,
    };
    match e {
        edit::Encoding::Raw => E::Raw,
        edit::Encoding::Zlib((k, level)) => E::Zlib(kernel(k), level),
        edit::Encoding::Deflate((k, level)) => E::Deflate(kernel(k), level),
    }
}

impl edit::HostPixels for HostState {
    fn width(&mut self, r: Resource<PixelsRes>) -> wasmtime::Result<u32> {
        Ok(self.write_cx()?.pixels(r.rep()).map_err(wasmtime::Error::msg)?.width)
    }

    fn height(&mut self, r: Resource<PixelsRes>) -> wasmtime::Result<u32> {
        Ok(self.write_cx()?.pixels(r.rep()).map_err(wasmtime::Error::msg)?.height)
    }

    fn bbox(&mut self, r: Resource<PixelsRes>) -> wasmtime::Result<Option<(u32, u32, u32, u32)>> {
        self.write_cx()?.bbox(r.rep()).map_err(wasmtime::Error::msg)
    }

    /// A region or layout the host cannot read is the plugin's fault: a trap.
    fn read(&mut self, r: Resource<PixelsRes>, x: u32, y: u32, w: u32, h: u32, layout: w::Layout) -> wasmtime::Result<Vec<u8>> {
        self.write_cx()?.read(r.rep(), x, y, w, h, &convert::layout(layout)).map_err(wasmtime::Error::msg)
    }

    fn drop(&mut self, _r: Resource<PixelsRes>) -> wasmtime::Result<()> {
        // the pixels live as long as the write (WriteCx)
        Ok(())
    }
}

impl edit::HostBlob for HostState {
    fn len(&mut self, r: Resource<BlobRes>) -> wasmtime::Result<u64> {
        Ok(self.write_cx()?.blob(r.rep()).map_err(wasmtime::Error::msg)?.0)
    }

    fn crc32(&mut self, r: Resource<BlobRes>) -> wasmtime::Result<u32> {
        Ok(self.write_cx()?.blob(r.rep()).map_err(wasmtime::Error::msg)?.1)
    }

    fn drop(&mut self, _r: Resource<BlobRes>) -> wasmtime::Result<()> {
        Ok(())
    }
}

impl edit::Host for HostState {
    fn encode_tile(
        &mut self,
        px: Resource<PixelsRes>,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
        layout: w::Layout,
        steps: Vec<edit::Encoding>,
    ) -> wasmtime::Result<Result<Resource<BlobRes>, String>> {
        let steps: Vec<_> = steps.into_iter().map(encoding).collect();
        let cx = self.write_cx()?;
        Ok(cx.encode_tile(px.rep(), x, y, w, h, &convert::layout(layout), &steps).map(Resource::new_own))
    }

    fn encode_image(&mut self, px: Resource<PixelsRes>, enc: edit::ImageEncoding) -> wasmtime::Result<Result<Resource<BlobRes>, String>> {
        let enc = match enc {
            edit::ImageEncoding::PngV1 => tmi_format::types::ImageEncoding::PngV1,
            edit::ImageEncoding::JpegKeepV1(t) => tmi_format::types::ImageEncoding::JpegKeepV1(t),
            edit::ImageEncoding::WebpV1(q) => tmi_format::types::ImageEncoding::WebpV1(q),
        };
        Ok(self.write_cx()?.encode_image(px.rep(), &enc).map(Resource::new_own))
    }

    fn composite_after_edit(&mut self) -> wasmtime::Result<Result<Resource<PixelsRes>, String>> {
        Ok(self.write_cx()?.composite_after_edit().map(Resource::new_own))
    }

    fn thumbnail(&mut self, px: Resource<PixelsRes>, max_side: u32) -> wasmtime::Result<Resource<PixelsRes>> {
        self.write_cx()?.thumbnail(px.rep(), max_side).map(Resource::new_own).map_err(wasmtime::Error::msg)
    }
}