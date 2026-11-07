//! A plugin that misbehaves on purpose (PLUGINS.md P5). What it does is
//! chosen by the file it reads: the first line, `BAD:<what>`. The host's
//! tests (`tmi-plugin-host/src/tests.rs`) expect a defined error for each:
//!
//! | what | does | the host's verdict |
//! |---|---|---|
//! | `loop` | loops forever | out of fuel (or past the time limit) |
//! | `hog` | allocates until memory runs out | over the memory limit |
//! | `deep` | recurses forever | stack overflow |
//! | `panic` | panics | a trap with the message |
//! | `huge` | a 17 MiB `context` | the host's check (16 MiB) |
//! | `layers` | 100,001 layers | the host's check (100,000) |
//! | `span` | a span past the end of the file | the host's check |
//! | `tree` | a layer whose parent comes after it | the host's check |
//! | `tiles` | one layer of 100,000 tiles (a measurement, not a fault) | (a good structure) |
//! | `world` | reads clocks, random, env, args, files; returns them | (a good structure; the values are fixed) |
//! | `sqlite` | uses the host's SQLite, tries ATTACH and VACUUM INTO | (a good structure; the refusals are in it) |
//! | `xml` | uses the host's XML parser | (a good structure) |
//! | `log` | `log.emit`, stdout and stderr | (a good structure) |
//! | `chunk` | a tile in its own codec (`decode-chunk` inverts the bytes) | (a good structure) |
//! | `chunk-big` | `decode-chunk` returns more than asked | the host's check |
//! | `defer-loop` | defers a layer whose `read-layer` loops | out of fuel, from `read-layer` |
//! | `fmt-lz4-lie` | 64 one-pixel tiles whose lz4 announces 4 GiB | the host's check, before it reserves anything |
//! | `fmt-raw-wrap` | raw pixels of 2^31 x 2^31 with no bytes (2^64 wraps to 0) | the host's check |
//! | `sleep` | `std::thread::sleep` for an hour, and a blocking read of stdin | (a good structure at once: the clocks stand still and nothing waits) |
//! | `flood-stdout` | 3 MiB to stdout and to stderr (the pipes hold 1 MiB), flushed | (a good structure; the writes fail at once, `BrokenPipe`) |
//! | `sqlite-mem` | `CREATE TABLE AS` of gigabytes, in the database and in a temporary table, a value over the length limit | (a good structure; the refusals are in it) |
//! | `sqlite-pragma` | sets `temp_store` (FILE), the heap limits and `max_page_count`; sorts more than the cache holds | (a good structure; the refusals are in it, no file is written) |
//! | `sqlite-spin` | a three-way product of 100,000 rows | (a good structure; the statement was stopped) |
//! | `sqlite-many` | opens databases without closing them | (a good structure; the ninth is refused) |
//! | `xml-deep` | `xml.parse` of `<a>` nested N deep (N is the second line) | (a good structure; an `err` over 256 levels) |
//! | `xml-wide` | `xml.parse` of two million empty elements | (a good structure; an `err`) |
//! | `log-flood` | a million empty `log.emit` lines | (a good structure; the log is cut) |
//! | anything else | `not-this-format` | the plugin's own error |

#[cfg(target_arch = "wasm32")]
mod wasm {
    wit_bindgen::generate!({
        path: "../../crates/tmi-format/wit",
        world: "format-reader",
    });

    use exports::tmi::format::reader::Guest;
    use tmi::format::source::SourceFile;
    use tmi::format::types::*;
    use tmi::format::{log, sqlite, xml};

    struct Bad;

    fn what(src: &SourceFile) -> String {
        let head = src.read(0, 64);
        let line = head.split(|&b| b == b'\n').next().unwrap_or(&[]);
        String::from_utf8_lossy(line).strip_prefix("BAD:").unwrap_or("").to_string()
    }

    fn layer(i: usize, parent: Option<u32>, pixels: LayerPixels) -> Layer {
        Layer {
            id: format!("l{i}"),
            stable_id: format!("l{i}"),
            parent,
            name: format!("layer {i}"),
            kind: LayerKind::Raster,
            visible: true,
            opacity: 1.0,
            blend: BlendMode::Normal,
            clip_to_below: false,
            offset_x: 0,
            offset_y: 0,
            pixels,
            meta: vec![],
            semantic: vec![],
            write: WriteCapability::None,
        }
    }

    fn fill(w: u32, h: u32) -> LayerPixels {
        LayerPixels::Ready(Bitmap {
            size: Some((w, h)),
            source: PixelSource::Tiles(Tiled {
                tiles: vec![Tile { x: 0, y: 0, width: w, height: h, content: TileContent::Fill(Rgba8 { r: 1, g: 2, b: 3, a: 255 }) }],
                overlap: Overlap::Replace,
            }),
        })
    }

    fn structure(layers: Vec<Layer>, meta: Vec<(String, MetaValue)>) -> Structure {
        Structure {
            format: "Bad".into(),
            canvas: Canvas { width: 4, height: 4, dpi: None, profile: ColorProfile::None },
            layers,
            composite: Composite::Render(Compositor::GenericV1),
            file_meta: meta.into_iter().map(|(key, value)| MetaEntry { key, value }).collect(),
            context: vec![],
            warnings: vec![],
        }
    }

    fn text(k: &str, v: impl ToString) -> (String, MetaValue) {
        (k.to_string(), MetaValue::Text(v.to_string()))
    }

    /// Recursion on the wasm stack itself (no locals in linear memory, so
    /// it is the wasm stack that overflows, not Rust's shadow stack).
    #[inline(never)]
    fn deep(n: u64, k: u64) -> u64 {
        if n == k {
            return 0;
        }
        deep(n + 1, k).wrapping_mul(3).wrapping_add(n)
    }

    fn spin() -> ! {
        let mut x = 0u64;
        loop {
            x = std::hint::black_box(x.wrapping_add(1));
        }
    }

    fn world() -> Structure {
        use std::time::{SystemTime, UNIX_EPOCH};
        let wall = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(u128::MAX);
        let mono = std::time::Instant::now();
        let mut random = [0u8; 8];
        let _ = getrandom(&mut random);
        let file = std::fs::read("C:/Windows/win.ini").map(|b| b.len().to_string()).unwrap_or_else(|e| format!("{:?}", e.kind()));
        let root = std::fs::read_dir("/").map(|d| d.count().to_string()).unwrap_or_else(|e| format!("{:?}", e.kind()));
        let map: std::collections::HashMap<u32, u32> = (0..16).map(|i| (i, i)).collect();
        let order: Vec<String> = map.keys().map(|k| k.to_string()).collect();
        structure(
            vec![layer(0, None, fill(2, 2))],
            vec![
                text("wall_ns", wall),
                text("monotonic_elapsed_ns", mono.elapsed().as_nanos()),
                text("random", random.iter().map(|b| format!("{b:02x}")).collect::<String>()),
                text("env", std::env::vars().count()),
                text("args", std::env::args().count()),
                text("file", file),
                text("root_dir", root),
                text("hashmap_order", order.join(",")),
            ],
        )
    }

    /// The random keys Rust's std draws for a `HashMap` (wasi:random).
    fn getrandom(buf: &mut [u8]) -> Result<(), ()> {
        let n = std::collections::hash_map::RandomState::new();
        use std::hash::{BuildHasher, Hasher};
        let mut h = n.build_hasher();
        h.write_u8(0);
        buf.copy_from_slice(&h.finish().to_le_bytes());
        Ok(())
    }

    /// Two rows read without ORDER BY: their order is SQLite's to choose.
    fn unordered() -> Structure {
        let db = sqlite::Database::open_writable(&Bytes::Inline(vec![])).expect("an empty database");
        db.execute("CREATE TABLE t (b TEXT)", &[]).expect("create");
        db.execute("INSERT INTO t VALUES ('one'), ('two')", &[]).expect("insert");
        let rows = db.query("SELECT b FROM t", &[]).expect("select");
        let order: Vec<String> = rows
            .iter()
            .map(|r| match r.first() {
                Some(sqlite::SqlValue::Text(s)) => s.clone(),
                _ => "?".into(),
            })
            .collect();
        structure(vec![layer(0, None, fill(1, 1))], vec![text("order", order.join(","))])
    }

    fn sql() -> Structure {
        let mut out = Vec::new();
        let db = match sqlite::Database::open_writable(&Bytes::Inline(vec![])) {
            Ok(db) => db,
            Err(e) => return structure(vec![layer(0, None, fill(1, 1))], vec![text("open", e)]),
        };
        let r = |res: Result<u64, String>| match res {
            Ok(n) => format!("ok {n}"),
            Err(e) => format!("err {e}"),
        };
        out.push(text("create", r(db.execute("CREATE TABLE t (a INTEGER, b TEXT)", &[]))));
        out.push(text("insert", r(db.execute("INSERT INTO t VALUES (?, ?), (2, 'two')", &[sqlite::SqlValue::Integer(1), sqlite::SqlValue::Text("one".into())]))));
        let rows = match db.query("SELECT a, b FROM t ORDER BY a", &[]) {
            Ok(rows) => rows
                .iter()
                .map(|r| {
                    r.iter()
                        .map(|v| match v {
                            sqlite::SqlValue::Integer(i) => i.to_string(),
                            sqlite::SqlValue::Text(s) => s.clone(),
                            _ => "?".into(),
                        })
                        .collect::<Vec<_>>()
                        .join(":")
                })
                .collect::<Vec<_>>()
                .join(","),
            Err(e) => format!("err {e}"),
        };
        out.push(text("rows", rows));
        out.push(text("attach", r(db.execute("ATTACH DATABASE 'tmi-attach-test.db' AS x", &[]))));
        out.push(text("vacuum_into", r(db.execute("VACUUM INTO 'tmi-vacuum-test.db'", &[]))));
        out.push(text("version", match db.query("SELECT sqlite_version()", &[]) {
            Ok(rows) => match rows.first().and_then(|r| r.first()) {
                Some(sqlite::SqlValue::Text(s)) => s.clone(),
                _ => "?".into(),
            },
            Err(e) => format!("err {e}"),
        }));
        out.push(text("serialized", match db.serialize() {
            Ok(b) => format!("{} {}", b.len(), b.starts_with(b"SQLite format 3\0")),
            Err(e) => format!("err {e}"),
        }));
        structure(vec![layer(0, None, fill(1, 1))], out)
    }

    fn xml_doc() -> Structure {
        let doc = b"<?xml version='1.0'?><a x='1'>hi<b xmlns:n='urn:n' n:y='2'>in</b>tail<c/></a>";
        let v = match xml::parse(doc) {
            Ok(els) => els
                .iter()
                .map(|e| {
                    let attrs = e.attributes.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&");
                    format!("{}[{}]({})<{}>", e.name, attrs, e.text, e.parent.map_or(-1, |p| p as i64))
                })
                .collect::<Vec<_>>()
                .join(" "),
            Err(e) => format!("err {e}"),
        };
        let dtd = match xml::parse(b"<!DOCTYPE a [<!ENTITY e 'x'>]><a>&e;</a>") {
            Ok(_) => "accepted".to_string(),
            Err(e) => format!("err {e}"),
        };
        structure(vec![layer(0, None, fill(1, 1))], vec![text("elements", v), text("dtd", dtd)])
    }

    /// The number on the file's second line.
    fn number(src: &SourceFile) -> u64 {
        let head = src.read(0, 64);
        let text = String::from_utf8_lossy(&head);
        text.lines().nth(1).and_then(|l| l.trim().parse().ok()).unwrap_or(0)
    }

    /// An hour's sleep and a read of stdin: both wait in the host (wasi:io/poll),
    /// out of reach of fuel and the epoch, unless the host answers at once.
    fn sleeping() -> Structure {
        let started = std::time::Instant::now();
        std::thread::sleep(std::time::Duration::from_secs(3600));
        let mut buf = [0u8; 16];
        let read = std::io::Read::read(&mut std::io::stdin(), &mut buf).map(|n| n.to_string()).unwrap_or_else(|e| format!("{:?}", e.kind()));
        structure(vec![layer(0, None, fill(1, 1))], vec![text("slept_ns", started.elapsed().as_nanos()), text("stdin", read)])
    }

    fn res(r: Result<u64, String>) -> String {
        match r {
            Ok(n) => format!("ok {n}"),
            Err(e) => format!("err {e}"),
        }
    }

    fn first_text(rows: Result<Vec<Vec<sqlite::SqlValue>>, String>) -> String {
        match rows {
            Ok(rows) => match rows.first().and_then(|r| r.first()) {
                Some(sqlite::SqlValue::Text(s)) => s.clone(),
                Some(sqlite::SqlValue::Integer(i)) => i.to_string(),
                _ => "?".into(),
            },
            Err(e) => format!("err {e}"),
        }
    }

    const MANY_ROWS: &str = "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x<4000) SELECT zeroblob(1000000) AS b FROM c";

    /// Gigabytes asked of the host's SQLite, in the database and in a
    /// temporary table; a single value over the length limit.
    fn sqlite_memory() -> Structure {
        let db = sqlite::Database::open_writable(&Bytes::Inline(vec![])).expect("an empty database");
        let mut out = vec![
            text("ctas", res(db.execute(&format!("CREATE TABLE t AS {MANY_ROWS}"), &[]))),
            text("temp_ctas", res(db.execute(&format!("CREATE TEMP TABLE u AS {MANY_ROWS}"), &[]))),
            text("too_big", first_text(db.query("SELECT length(zeroblob(1000000000))", &[]))),
            text("still_works", first_text(db.query("SELECT 40 + 2", &[]))),
        ];
        out.push(text("temp_store", first_text(db.query("PRAGMA temp_store", &[]))));
        structure(vec![layer(0, None, fill(1, 1))], out)
    }

    /// Pragmas that would undo the host's limits, and a sort that does not fit
    /// the page cache (which SQLite takes to a file under `temp_store=FILE`).
    fn sqlite_pragmas() -> Structure {
        let db = sqlite::Database::open_writable(&Bytes::Inline(vec![])).expect("an empty database");
        let mut out = Vec::new();
        for (key, sql) in [
            ("set_temp_store_file", "PRAGMA temp_store=FILE"),
            ("set_temp_store_default", "PRAGMA temp_store=0"),
            ("set_temp_store_memory", "PRAGMA temp_store=MEMORY"),
            ("set_hard_heap_limit", "PRAGMA hard_heap_limit=1"),
            ("set_soft_heap_limit", "PRAGMA soft_heap_limit=1"),
            ("set_max_page_count", "PRAGMA max_page_count=1073741823"),
            ("set_temp_max_page_count", "PRAGMA temp.max_page_count=1073741823"),
            ("set_page_size", "PRAGMA page_size=65536"),
            ("set_temp_directory", "PRAGMA temp_store_directory='C:/'"),
        ] {
            out.push(text(key, res(db.execute(sql, &[]))));
        }
        out.push(text("temp_store", first_text(db.query("PRAGMA temp_store", &[]))));
        // 150,000 rows of 64 random bytes: about 10 MB, over the 2 MB page cache
        let fill = "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x<150000) SELECT randomblob(64) AS b, x FROM c";
        out.push(text("fill", res(db.execute(&format!("CREATE TABLE t AS {fill}"), &[]))));
        out.push(text("sort", first_text(db.query("SELECT count(*) FROM (SELECT b FROM t ORDER BY b)", &[]))));
        out.push(text("distinct", first_text(db.query("SELECT count(*) FROM (SELECT DISTINCT b FROM t)", &[]))));
        out.push(text("temp_table", res(db.execute("CREATE TEMP TABLE v AS SELECT * FROM t", &[]))));
        structure(vec![layer(0, None, fill_px())], out)
    }

    fn fill_px() -> LayerPixels {
        fill(1, 1)
    }

    /// 100,000³ rows: no statement of a format reader should run that long.
    fn sqlite_spin() -> Structure {
        let db = sqlite::Database::open_writable(&Bytes::Inline(vec![])).expect("an empty database");
        let r = first_text(db.query(
            "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x<100000) SELECT count(*) FROM c a, c b, c d",
            &[],
        ));
        structure(vec![layer(0, None, fill(1, 1))], vec![text("spin", r), text("after", first_text(db.query("SELECT 1", &[])))])
    }

    fn sqlite_many() -> Structure {
        let mut held = Vec::new();
        let mut verdicts = Vec::new();
        for _ in 0..10 {
            match sqlite::Database::open_writable(&Bytes::Inline(vec![])) {
                Ok(db) => {
                    verdicts.push("ok".to_string());
                    held.push(db);
                }
                Err(e) => verdicts.push(format!("err {e}")),
            }
        }
        let first_err = verdicts.iter().position(|v| v.starts_with("err")).map_or(-1, |p| p as i64);
        let opened = held.len();
        // closing one makes room for another
        held.pop();
        let again = sqlite::Database::open_writable(&Bytes::Inline(vec![])).map(|_| "ok".to_string()).unwrap_or_else(|e| format!("err {e}"));
        structure(vec![layer(0, None, fill(1, 1))], vec![text("opened", opened), text("first_refused", first_err), text("after_close", again)])
    }

    fn xml_nested(depth: u64) -> Structure {
        let mut doc = String::new();
        for _ in 0..depth {
            doc.push_str("<a>");
        }
        for _ in 0..depth {
            doc.push_str("</a>");
        }
        let r = match xml::parse(doc.as_bytes()) {
            Ok(els) => format!("ok {}", els.len()),
            Err(e) => format!("err {e}"),
        };
        structure(vec![layer(0, None, fill(1, 1))], vec![text("parse", r)])
    }

    fn xml_wide() -> Structure {
        let mut doc = String::from("<a>");
        for _ in 0..2_000_000 {
            doc.push_str("<b/>");
        }
        doc.push_str("</a>");
        let r = match xml::parse(doc.as_bytes()) {
            Ok(els) => format!("ok {}", els.len()),
            Err(e) => format!("err {e}"),
        };
        structure(vec![layer(0, None, fill(1, 1))], vec![text("parse", r)])
    }

    impl Guest for Bad {
        fn info() -> PluginInfo {
            PluginInfo {
                id: "bad".into(),
                version: "0.1.0".into(),
                formats: vec![FormatInfo { name: "Bad".into(), extensions: vec![".bad".into()], mime: None }],
                probe_bytes: 64,
                can_write: false,
            }
        }

        fn probe(_name: String, head: Vec<u8>, _size: u64) -> Confidence {
            if head.starts_with(b"BAD:") {
                Confidence::Yes
            } else {
                Confidence::No
            }
        }

        fn read_structure(src: &SourceFile) -> Result<Structure, ReadError> {
            match what(src).as_str() {
                "loop" => spin(),
                "hog" => {
                    let mut keep: Vec<Vec<u8>> = Vec::new();
                    loop {
                        keep.push(std::hint::black_box(vec![1u8; 16 << 20]));
                    }
                }
                "deep" => {
                    let n = deep(0, src.size() | 1 << 63);
                    Ok(structure(vec![], vec![text("n", n)]))
                }
                "panic" => panic!("deliberate panic in read-structure"),
                "huge" => {
                    let mut s = structure(vec![], vec![]);
                    s.context = vec![7u8; 17 << 20];
                    Ok(s)
                }
                "layers" => Ok(structure((0..100_001).map(|i| layer(i, None, LayerPixels::None)).collect(), vec![])),
                "span" => {
                    let size = src.size();
                    let image = EncodedImage { data: Bytes::Span(Span { offset: size - 2, len: 10 }), pre: vec![], codec: ImageCodec::Png, frame: 0 };
                    let bitmap = Bitmap { size: None, source: PixelSource::Image(PlacedImage { x: 0, y: 0, image }) };
                    Ok(structure(vec![layer(0, None, LayerPixels::Ready(bitmap))], vec![]))
                }
                "tiles" => {
                    // 100,000 1×1 tiles in one layer (PLUGINS.md 6.4 item 3)
                    let tiles = (0..100_000)
                        .map(|i| Tile { x: (i % 400) as i32, y: (i / 400) as i32, width: 1, height: 1, content: TileContent::Fill(Rgba8 { r: i as u8, g: 0, b: 0, a: 255 }) })
                        .collect();
                    let bitmap = Bitmap { size: Some((400, 250)), source: PixelSource::Tiles(Tiled { tiles, overlap: Overlap::Replace }) };
                    Ok(structure(vec![layer(0, None, LayerPixels::Ready(bitmap))], vec![]))
                }
                "fmt-lz4-lie" => {
                    let raster = Raster {
                        data: Bytes::Inline(vec![0x10, 1]),
                        codecs: vec![Codec::Lz4Block(u32::MAX)],
                        layout: Layout { groups: vec![Group { channels: vec![Channel::R, Channel::G, Channel::B, Channel::A], depth: Depth::Eight }], row_padding: 0, tint: None, premultiplied: false },
                    };
                    let tiles = (0..64).map(|_| Tile { x: 0, y: 0, width: 1, height: 1, content: TileContent::Raster(raster.clone()) }).collect();
                    let bitmap = Bitmap { size: Some((1, 1)), source: PixelSource::Tiles(Tiled { tiles, overlap: Overlap::Replace }) };
                    Ok(structure(vec![layer(0, None, LayerPixels::Ready(bitmap))], vec![]))
                }
                "fmt-raw-wrap" => {
                    let raw = RawPixels { x: 0, y: 0, width: 1 << 31, height: 1 << 31, rgba: vec![] };
                    let bitmap = Bitmap { size: None, source: PixelSource::Raw(raw) };
                    Ok(structure(vec![layer(0, None, LayerPixels::Ready(bitmap))], vec![]))
                }
                "tree" => Ok(structure(vec![layer(0, Some(1), LayerPixels::None), layer(1, None, LayerPixels::None)], vec![])),
                "world" => Ok(world()),
                "sqlite" => Ok(sql()),
                // P8 (tmiki plugin test): answers that hang on the order of an
                // unordered SELECT, and on calls before this one in the instance
                "unordered" => Ok(unordered()),
                "stateful" => {
                    static CALLS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
                    let n = CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    Ok(structure(vec![layer(0, None, fill(1, 1))], vec![text("calls before", n)]))
                }
                "xml" => Ok(xml_doc()),
                "sleep" => Ok(sleeping()),
                "flood-stdout" => {
                    // more than the 1 MiB pipe holds, then a flush: the blocking
                    // writes of wasi:io/streams must end, with an error or a trap
                    use std::io::Write;
                    let mut out = std::io::stdout();
                    let wrote = out.write_all(&vec![b'x'; 3 << 20]).and_then(|_| out.flush());
                    let mut err = std::io::stderr();
                    let wrote_err = err.write_all(&vec![b'y'; 3 << 20]).and_then(|_| err.flush());
                    Ok(structure(vec![layer(0, None, fill(1, 1))], vec![text("stdout", format!("{:?}", wrote.map_err(|e| e.kind()))), text("stderr", format!("{:?}", wrote_err.map_err(|e| e.kind())))]))
                }
                "sqlite-mem" => Ok(sqlite_memory()),
                "sqlite-pragma" => Ok(sqlite_pragmas()),
                "sqlite-spin" => Ok(sqlite_spin()),
                "sqlite-many" => Ok(sqlite_many()),
                "xml-deep" => Ok(xml_nested(number(src))),
                "xml-wide" => Ok(xml_wide()),
                "log-flood" => {
                    for _ in 0..1_000_000 {
                        log::emit(log::Level::Info, "");
                    }
                    Ok(structure(vec![layer(0, None, fill(1, 1))], vec![]))
                }
                "log" => {
                    log::emit(log::Level::Info, "hello from log.emit");
                    log::emit(log::Level::Debug, "a debug line");
                    println!("hello from stdout");
                    eprintln!("hello from stderr");
                    Ok(structure(vec![layer(0, None, fill(3, 3))], vec![]))
                }
                w @ ("chunk" | "chunk-big") => {
                    let codec = if w == "chunk" { 7 } else { 8 };
                    let raster = Raster {
                        data: Bytes::Inline(vec![0x00, 0x10, 0x20, 0xff, 0x40, 0x50, 0x60, 0x80]),
                        codecs: vec![Codec::Plugin(codec)],
                        layout: Layout { groups: vec![Group { channels: vec![Channel::R, Channel::G, Channel::B, Channel::A], depth: Depth::Eight }], row_padding: 0, tint: None, premultiplied: false },
                    };
                    let bitmap = Bitmap {
                        size: Some((2, 1)),
                        source: PixelSource::Tiles(Tiled { tiles: vec![Tile { x: 0, y: 0, width: 2, height: 1, content: TileContent::Raster(raster) }], overlap: Overlap::Replace }),
                    };
                    Ok(structure(vec![layer(0, None, LayerPixels::Ready(bitmap))], vec![]))
                }
                "defer-loop" => {
                    let mut s = structure(vec![layer(0, None, fill(2, 2)), layer(1, None, LayerPixels::Deferred(1))], vec![]);
                    s.context = b"loop".to_vec();
                    Ok(s)
                }
                other => Err(ReadError::NotThisFormat(format!("BAD:{other} is not a behaviour"))),
            }
        }

        fn read_layer(_src: &SourceFile, context: Vec<u8>, _token: u32) -> Result<Bitmap, ReadError> {
            if context == b"loop" {
                spin();
            }
            Err(ReadError::Unsupported("no deferred layers".into()))
        }

        fn decode_chunk(codec_id: u32, input: Vec<u8>, out_len: u32) -> Result<Vec<u8>, ReadError> {
            match codec_id {
                7 => Ok(input.iter().map(|b| !b).collect()),
                8 => Ok(vec![0; out_len as usize + 1]),
                _ => Err(ReadError::Unsupported(format!("codec {codec_id}"))),
            }
        }
    }

    export!(Bad);
}
