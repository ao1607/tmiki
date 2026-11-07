//! The host functions and WASI keep a plugin inside its limits (PLUGINS.md
//! 3.2, 5): what a plugin asks of the host that fuel and the epoch cannot
//! stop (they cannot interrupt a host function), each with a bad plugin
//! (`rust/plugins/bad`) that asks for it.
//!
//! Before these limits: the sleep waited for real (an hour), SQLite took
//! gigabytes and wrote `etilqs_*` files into the temp directory, a document
//! nested 50,000 deep overflowed the worker's stack and ended the process, and
//! `log.emit("")` cost 32 bytes of host memory a call.

use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use crate::{test_plugins, Plugin, PluginHost, Settings};

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("tmi-plugin-host-limits-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A host whose plugins may hold 64 MiB: what a hostile plugin takes from the
/// host is measured against that.
fn small_plugin() -> Arc<Plugin> {
    static BAD: OnceLock<Arc<Plugin>> = OnceLock::new();
    BAD.get_or_init(|| bad_plugin(64 << 20, "small")).clone()
}

/// The same with 512 MiB, for what needs room (a sort of 10 MB).
fn roomy_plugin() -> Arc<Plugin> {
    static BAD: OnceLock<Arc<Plugin>> = OnceLock::new();
    BAD.get_or_init(|| bad_plugin(512 << 20, "roomy")).clone()
}

fn bad_plugin(max_memory: usize, tag: &str) -> Arc<Plugin> {
    let host = PluginHost::new(Settings { fuel: Some(5_000_000_000), max_memory, cache_dir: Some(temp_dir(tag)), ..Settings::default() }).unwrap();
    Arc::new(host.load(&test_plugins::component("tmi-plugin-bad")).unwrap())
}

fn meta(s: &tmi_format::types::Structure, key: &str) -> String {
    match s.file_meta.iter().find(|e| e.key == key).map(|e| &e.value) {
        Some(tmi_format::types::MetaValue::Text(t)) => t.clone(),
        other => panic!("{key}: {other:?}"),
    }
}

fn run(what: &str) -> (tmi_format::types::Structure, Duration) {
    run_on(&small_plugin(), what)
}

fn run_on(p: &Plugin, what: &str) -> (tmi_format::types::Structure, Duration) {
    let t = Instant::now();
    let s = p.read_structure("x.bad", format!("BAD:{what}\n").as_bytes()).unwrap_or_else(|e| panic!("{what}: {e:?}"));
    (s, t.elapsed())
}

fn run_with(what: &str, n: u64) -> tmi_format::types::Structure {
    small_plugin().read_structure("x.bad", format!("BAD:{what}\n{n}\n").as_bytes()).unwrap_or_else(|e| panic!("{what}: {e:?}"))
}

// -- WASI: nothing waits ------------------------------------------------------

#[test]
fn a_sleep_of_an_hour_is_over_at_once() {
    let (s, took) = run("sleep");
    assert!(took < Duration::from_secs(20), "{took:?}");
    // the monotonic clock stood still through the sleep, and stdin is empty
    assert_eq!(meta(&s, "slept_ns"), "0");
    assert_eq!(meta(&s, "stdin"), "0");
}

#[test]
fn blocking_writes_past_the_pipes_end_at_once() {
    // stdout and stderr are memory pipes of 1 MiB; the blocking writes and
    // flushes of wasi:io/streams past that must not wait for room
    let t = Instant::now();
    match small_plugin().read_structure("x.bad", b"BAD:flood-stdout\n") {
        Ok(s) => println!("flood-stdout: stdout {}, stderr {}", meta(&s, "stdout"), meta(&s, "stderr")),
        Err(e) => println!("flood-stdout: {e:?}"),
    }
    assert!(t.elapsed() < Duration::from_secs(20), "{:?}", t.elapsed());
}

// -- sqlite: memory and files ---------------------------------------------------

#[test]
fn sqlite_memory_is_held_to_the_plugins_budget() {
    // 4,000 rows of 1 MB, into the database and into a temporary table: 4 GB each
    // (the process-wide high-water mark also sees the other tests' SQLite, a few MB)
    unsafe { rusqlite::ffi::sqlite3_memory_highwater(1) };
    let (s, _) = run("sqlite-mem");
    let peak = unsafe { rusqlite::ffi::sqlite3_memory_highwater(1) };
    // 64 MiB over 16: 4 MiB of growth for the database, 4 MiB for the temporary tables
    assert!(peak < 48 << 20, "SQLite's heap peaked at {peak} bytes");
    for key in ["ctas", "temp_ctas"] {
        assert!(meta(&s, key).starts_with("err ") && meta(&s, key).contains("full"), "{key}: {}", meta(&s, key));
    }
    assert!(meta(&s, "too_big").starts_with("err ") && meta(&s, "too_big").contains("too big"), "{}", meta(&s, "too_big"));
    assert_eq!(meta(&s, "still_works"), "42");
    assert_eq!(meta(&s, "temp_store"), "2", "temporary tables are in memory");
}

/// The files SQLite writes for temporary data go into the directory named by
/// `TMP` / `TEMP` (Windows) or `TMPDIR` (elsewhere); this test points them at
/// a directory of its own and lists what appears there.
struct TempWatch {
    dir: std::path::PathBuf,
    stop: Arc<std::sync::atomic::AtomicBool>,
    seen: Arc<std::sync::Mutex<std::collections::BTreeSet<String>>>,
    thread: Option<std::thread::JoinHandle<()>>,
    saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
}

impl TempWatch {
    fn new(tag: &str) -> TempWatch {
        let dir = temp_dir(tag);
        let saved: Vec<_> = ["TMP", "TEMP", "TMPDIR"].into_iter().map(|k| (k, std::env::var_os(k))).collect();
        for (k, _) in &saved {
            std::env::set_var(k, &dir);
        }
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let seen = Arc::new(std::sync::Mutex::new(std::collections::BTreeSet::new()));
        let thread = {
            let (dir, stop, seen) = (dir.clone(), stop.clone(), seen.clone());
            std::thread::spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    if let Ok(rd) = std::fs::read_dir(&dir) {
                        for e in rd.flatten() {
                            // (other tests of the process make their own files here too)
                            let name = e.file_name().to_string_lossy().into_owned();
                            if name.starts_with("etilqs_") {
                                seen.lock().unwrap().insert(name);
                            }
                        }
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
            })
        };
        TempWatch { dir, stop, seen, thread: Some(thread), saved }
    }

    /// The names that appeared in the directory so far.
    fn finish(mut self) -> Vec<String> {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        self.thread.take().unwrap().join().unwrap();
        for (k, v) in &self.saved {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
        let mut names: Vec<String> = self.seen.lock().unwrap().iter().cloned().collect();
        if let Ok(rd) = std::fs::read_dir(&self.dir) {
            names.extend(rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.starts_with("etilqs_")));
        }
        names.sort();
        names.dedup();
        names
    }
}

/// Both tests below change the process's temp directory: one at a time.
fn temp_env_lock() -> std::sync::MutexGuard<'static, ()> {
    static L: std::sync::Mutex<()> = std::sync::Mutex::new(());
    L.lock().unwrap_or_else(|e| e.into_inner())
}

/// Sorts and fills 10 MB through `conn`.
fn big_sort(conn: &rusqlite::Connection) {
    conn.execute_batch("CREATE TABLE t AS WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x<100000) SELECT randomblob(64) AS b, x FROM c").unwrap();
    let n: i64 = conn.query_row("SELECT count(*) FROM (SELECT b FROM t ORDER BY b)", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 100_000);
}

#[test]
fn the_watch_sees_the_temp_files_sqlite_writes_when_it_may() {
    // the control of the next test: a connection with the file-backed
    // temp_store writes etilqs_* files there
    let _l = temp_env_lock();
    let watch = TempWatch::new("control");
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.pragma_update(None, "temp_store", "FILE").unwrap();
    conn.pragma_update(None, "cache_size", 100).unwrap();
    big_sort(&conn);
    drop(conn);
    let seen = watch.finish();
    assert!(seen.iter().any(|n| n.starts_with("etilqs_")), "{seen:?}");
}

#[test]
fn a_plugin_cannot_move_sqlites_temporary_data_to_files() {
    let _l = temp_env_lock();
    let plugin = roomy_plugin(); // (its cache directory is made under the real temp directory)
    let watch = TempWatch::new("plugin");
    let (s, _) = run_on(&plugin, "sqlite-pragma");
    let seen = watch.finish();
    assert!(seen.is_empty(), "SQLite temp files appeared in the temp directory: {seen:?}");
    for key in [
        "set_temp_store_file",
        "set_temp_store_default",
        "set_temp_store_memory",
        "set_hard_heap_limit",
        "set_soft_heap_limit",
        "set_max_page_count",
        "set_temp_max_page_count",
        "set_page_size",
        "set_temp_directory",
    ] {
        assert!(meta(&s, key).starts_with("err "), "{key}: {}", meta(&s, key));
    }
    assert_eq!(meta(&s, "temp_store"), "2");
    // the work itself is done: in memory
    assert_eq!(meta(&s, "sort"), "150000");
    assert_eq!(meta(&s, "distinct"), "150000");
}

#[test]
fn a_statement_that_runs_for_ever_is_stopped_by_its_step_budget() {
    let (s, took) = run("sqlite-spin");
    assert!(meta(&s, "spin").contains("virtual machine steps"), "{}", meta(&s, "spin"));
    assert!(took < Duration::from_secs(120), "{took:?}");
    // the connection is usable afterwards, with a budget of its own again
    assert_eq!(meta(&s, "after"), "1");
}

#[test]
fn a_call_holds_at_most_eight_databases() {
    let (s, _) = run("sqlite-many");
    assert_eq!(meta(&s, "opened"), "8");
    assert_eq!(meta(&s, "first_refused"), "8");
    assert_eq!(meta(&s, "after_close"), "ok");
}

#[test]
fn a_database_image_opened_by_a_plugin_may_grow_by_its_budget() {
    // the cap is the image's size plus the budget: a 6 MB image (more than the
    // 4 MiB of growth a 64 MiB plugin gets) is read, and grows by that much
    use crate::host::open_database_with;
    let src = rusqlite::Connection::open_in_memory().unwrap();
    src.execute_batch("CREATE TABLE t AS WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x<2000) SELECT zeroblob(3000) AS b FROM c").unwrap();
    let image = src.serialize(rusqlite::MAIN_DB).unwrap().to_vec();
    assert!(image.len() > 5 << 20, "{}", image.len());
    let db = open_database_with(&image, false, false, 64 << 20).unwrap();
    let n: i64 = db.query_row("SELECT count(*) FROM t", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 2000);
    db.execute_batch("CREATE TABLE u AS SELECT * FROM t LIMIT 1000").unwrap();
    assert!(db.execute_batch("CREATE TABLE w AS SELECT zeroblob(1000000) FROM t LIMIT 10").unwrap_err().to_string().contains("full"));
}

// -- xml ------------------------------------------------------------------------

#[test]
fn a_document_nested_fifty_thousand_deep_is_refused_not_parsed() {
    // (before the limit: STATUS_STACK_OVERFLOW, the whole process gone)
    let s = run_with("xml-deep", 50_000);
    let r = meta(&s, "parse");
    assert!(r.starts_with("err ") && r.contains("nested more than 256 deep"), "{r}");
    assert_eq!(meta(&run_with("xml-deep", 256), "parse"), "ok 256");
    assert!(meta(&run_with("xml-deep", 257), "parse").starts_with("err "));
}

#[test]
fn a_document_of_two_million_elements_is_refused() {
    let r = meta(&run("xml-wide").0, "parse");
    assert!(r.starts_with("err "), "{r}");
}

#[test]
fn the_depth_check_reads_tags_as_xml_does() {
    use crate::host::{parse_xml, XML_MAX_DEPTH};
    let nest = |n: usize| format!("{}{}", "<a>".repeat(n), "</a>".repeat(n));
    assert_eq!(parse_xml(nest(XML_MAX_DEPTH).as_bytes()).unwrap().len(), XML_MAX_DEPTH);
    assert!(parse_xml(nest(XML_MAX_DEPTH + 1).as_bytes()).unwrap_err().contains("nested more than"));
    // depth is nesting, not the number of elements
    let wide = format!("<r>{}</r>", "<c/><c x='1'/><c x=\"/\"/>".repeat(5000));
    assert_eq!(parse_xml(wide.as_bytes()).unwrap().len(), 15_001);
    let flat = format!("<r>{}</r>", "<c></c>".repeat(5000));
    assert_eq!(parse_xml(flat.as_bytes()).unwrap().len(), 5001);
    // what hides tags: comments, CDATA, processing instructions, attribute values
    let hidden = format!(
        "<?xml version='1.0'?><!-- {0} --><r a='>>' b=\"/>\"><![CDATA[{0}]]><?pi {0}?>x</r>",
        "<a>".repeat(1000)
    );
    assert_eq!(parse_xml(hidden.as_bytes()).unwrap().len(), 1);
    // and what does not: a '>' inside a quoted value does not end the tag early
    let gt = format!("{}<z q='>'><y q=\">\"/></z>{}", "<a>".repeat(255), "</a>".repeat(255));
    assert_eq!(parse_xml(gt.as_bytes()).unwrap().len(), 255 + 2);
    let over = format!("{}<z q='>'><y><x/></y></z>{}", "<a>".repeat(255), "</a>".repeat(255));
    assert!(parse_xml(over.as_bytes()).is_err_and(|e| e.contains("nested more than")));
    // a document type declaration: bare is accepted, a subset is refused (as before)
    assert_eq!(parse_xml(b"<!DOCTYPE a><a/>").unwrap().len(), 1);
    assert!(parse_xml(b"<!DOCTYPE a [<!ENTITY e 'x'>]><a>&e;</a>").unwrap_err().contains("document type definition"));
    let sneaky = format!("<!DOCTYPE a [<!-- ' -->]>{}", "<a>".repeat(100_000));
    assert!(parse_xml(sneaky.as_bytes()).unwrap_err().contains("document type definition"));
    // not well formed: the parser's to refuse, and no walk past the end
    assert!(parse_xml(b"<a><b").is_err());
    assert!(parse_xml(b"<a><!-- ").is_err());
    assert!(parse_xml(b"</a></a><a>").is_err());
    assert!(parse_xml(&vec![b'<'; 1000]).is_err());
}

#[test]
fn the_deepest_allowed_document_fits_a_small_stack() {
    // the margin of XML_MAX_DEPTH: 256 levels parse on a 512 KiB stack, and a
    // document a million deep is refused without recursing at all
    let t = std::thread::Builder::new()
        .stack_size(512 << 10)
        .spawn(|| {
            use crate::host::{parse_xml, XML_MAX_DEPTH};
            let nest = |n: usize| format!("{}{}", "<a>".repeat(n), "</a>".repeat(n));
            assert_eq!(parse_xml(nest(XML_MAX_DEPTH).as_bytes()).unwrap().len(), XML_MAX_DEPTH);
            assert!(parse_xml(nest(1_000_000).as_bytes()).is_err());
        })
        .unwrap();
    t.join().unwrap();
}

#[test]
fn an_xml_document_over_the_input_limit_is_refused() {
    let big = vec![b' '; crate::host::XML_MAX_INPUT + 1];
    assert!(crate::host::parse_xml(&big).unwrap_err().contains("over"));
}

/// What the limits cost (PLUGINS.md 6): `cargo test --release -p tmi-plugin-host
/// -- --ignored --nocapture measure_the_limits`.
#[test]
#[ignore]
fn measure_the_limits() {
    use crate::host::{execute, open_database_with, parse_xml, run_query};
    let median = |mut v: Vec<f64>| {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        v[v.len() / 2]
    };
    // a 5 MB database image, the size of a small CLIP's SQLite chunk
    let src = rusqlite::Connection::open_in_memory().unwrap();
    src.execute_batch("CREATE TABLE t AS WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x<50000) SELECT x, randomblob(100) AS b FROM c").unwrap();
    let image = src.serialize(rusqlite::MAIN_DB).unwrap().to_vec();
    // open: bare (deserialize only) against open_database_with
    let bare = (0..200)
        .map(|_| {
            let t = Instant::now();
            let mut c = rusqlite::Connection::open_in_memory().unwrap();
            c.deserialize_read_exact(rusqlite::MAIN_DB, &image[..], image.len(), true).unwrap();
            t.elapsed().as_secs_f64() * 1e6
        })
        .collect();
    let limited = (0..200)
        .map(|_| {
            let t = Instant::now();
            let _c = open_database_with(&image, true, false, 1 << 30).unwrap();
            t.elapsed().as_secs_f64() * 1e6
        })
        .collect();
    println!("open of a {} KB image: bare {:.0} us, open_database_with {:.0} us (median of 200)", image.len() / 1024, median(bare), median(limited));
    // a full scan of 50,000 rows, 20 times: bare against run_query (step budget installed)
    let db = open_database_with(&image, true, false, 1 << 30).unwrap();
    let bare_db = {
        let mut c = rusqlite::Connection::open_in_memory().unwrap();
        c.deserialize_read_exact(rusqlite::MAIN_DB, &image[..], image.len(), true).unwrap();
        c
    };
    let scan = |f: &dyn Fn()| median((0..20).map(|_| { let t = Instant::now(); f(); t.elapsed().as_secs_f64() * 1e3 }).collect());
    let q = "SELECT sum(length(b)) FROM t WHERE x % 7 = 3 OR b LIKE 'x%'";
    let a = scan(&|| { bare_db.query_row(q, [], |r| r.get::<_, i64>(0)).unwrap(); });
    let b = scan(&|| { run_query(&db, q, vec![]).unwrap(); });
    println!("scan of 50,000 rows: bare {a:.2} ms, run_query {b:.2} ms (median of 20)");
    let _ = execute;
    // the XML check: a stack.xml of 2,000 layers, and 1 MB of it
    let layer = "<layer name=\"layer 1\" src=\"data/l1.png\" x=\"0\" y=\"0\" opacity=\"1.0\" visibility=\"visible\" composite-op=\"svg:src-over\"/>";
    for n in [2_000usize, 20_000] {
        let doc = format!("<?xml version='1.0'?><image w=\"100\" h=\"100\"><stack>{}</stack></image>", layer.repeat(n));
        let parse = median((0..20).map(|_| { let t = Instant::now(); parse_xml(doc.as_bytes()).unwrap(); t.elapsed().as_secs_f64() * 1e3 }).collect());
        let opts = roxmltree::ParsingOptions { allow_dtd: true, ..Default::default() };
        let bare = median((0..20).map(|_| { let t = Instant::now(); roxmltree::Document::parse_with_options(&doc, opts).unwrap(); t.elapsed().as_secs_f64() * 1e3 }).collect());
        let check = median((0..20).map(|_| { let t = Instant::now(); crate::host::check_xml_depth(&doc).unwrap(); t.elapsed().as_secs_f64() * 1e3 }).collect());
        println!("xml of {} KB ({n} elements): the depth check {check:.3} ms, roxmltree alone {bare:.2} ms, parse_xml (check + parse + answer) {parse:.2} ms", doc.len() / 1024);
    }
}

// -- log ----------------------------------------------------------------------

#[test]
fn log_lines_cost_their_text_and_a_fixed_overhead() {
    use crate::bindings::tmi::format::log::{Host, Level};
    use crate::host::{HostState, LOG_CAPACITY, LOG_ENTRY_COST};
    let mut s = HostState::new(None, 1 << 20);
    for _ in 0..5_000_000 {
        s.emit(Level::Info, String::new()).unwrap();
    }
    assert_eq!(s.log.len(), LOG_CAPACITY / LOG_ENTRY_COST);
    assert!(s.log_truncated);
    // a long line is cut by its length, as before
    let mut s = HostState::new(None, 1 << 20);
    s.emit(Level::Info, "x".repeat(LOG_CAPACITY - LOG_ENTRY_COST)).unwrap();
    assert_eq!(s.log.len(), 1);
    s.emit(Level::Info, String::new()).unwrap();
    assert_eq!(s.log.len(), 1);
    assert!(s.log_truncated);
}

#[test]
fn a_million_empty_log_lines_end_in_a_good_answer() {
    let (s, _) = run("log-flood");
    assert_eq!(s.layers.len(), 1);
}
