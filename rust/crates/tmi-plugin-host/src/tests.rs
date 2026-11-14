//! The P5 checks (PLUGINS.md 10): the ORA component gives the records its
//! native build gives, each bad plugin ends in its defined error, and the
//! host's world is the same on every call.

use std::collections::HashSet;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use tmi_format::pipeline::{self, ImportError};
use tmi_format::reader::FormatReader;
use tmi_format::types::ReadError;
use tmi_import::{ImportedFile, ImportedLayer, LayerBlobs, Limits, Options, PriorDecode, Warnings};
use tmi_store::json::Value;

use crate::native_ora::OraNative;
use crate::ora_files::{self, Merged, Sample};
use crate::{test_plugins, Plugin, PluginError, PluginHost, Settings, WasmReader};

fn temp_dir(tag: &str) -> std::path::PathBuf {
    // the cache seals with a key of its own: the tests' own file, not the user's
    static KEY: std::sync::Once = std::sync::Once::new();
    KEY.call_once(|| std::env::set_var("TMI_PLUGIN_CACHE_KEY", std::env::temp_dir().join(format!("tmi-plugin-host-cache-key-{}", std::process::id()))));
    let d = std::env::temp_dir().join(format!("tmi-plugin-host-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// The tests' host: small limits so the bad plugins end quickly.
fn test_settings() -> Settings {
    Settings { fuel: Some(500_000_000), max_memory: 256 << 20, cache_dir: Some(temp_dir("cache")), ..Settings::default() }
}

fn host() -> &'static PluginHost {
    static HOST: OnceLock<PluginHost> = OnceLock::new();
    HOST.get_or_init(|| PluginHost::new(test_settings()).unwrap())
}

pub(crate) fn plugin(name: &str) -> Arc<Plugin> {
    static ORA: OnceLock<Arc<Plugin>> = OnceLock::new();
    static BAD: OnceLock<Arc<Plugin>> = OnceLock::new();
    let cell = match name {
        "tmi-plugin-ora" => &ORA,
        "tmi-plugin-bad" => &BAD,
        _ => panic!("{name}"),
    };
    cell.get_or_init(|| Arc::new(host().load(&test_plugins::component(name)).unwrap())).clone()
}

// -- records ---------------------------------------------------------------

fn dump(f: &ImportedFile) -> String {
    crate::dump::text(f)
}

/// The record's `importer` for `reader`: the same component hash for the
/// wasm build and its native twin, so their records can be compared.
pub(crate) fn importer(reader: &dyn FormatReader) -> pipeline::Importer {
    pipeline::Importer {
        id: reader.info().id.clone(),
        version: reader.info().version.clone(),
        component_sha256: "test-component".into(),
        abi: "tmi:format@0.1.0".into(),
    }
}

#[test]
fn a_plugin_record_has_its_importer_and_the_host_made_keys() {
    let wasm = WasmReader::new(plugin("tmi-plugin-ora"));
    let real = wasm.plugin().importer();
    assert_eq!((real.id.as_str(), real.abi.as_str()), ("ora", "tmi:format@0.1.0"));
    assert_eq!(real.component_sha256, tmi_store::oid::sha256_hex(&test_plugins::component("tmi-plugin-ora")));
    for defer in [false, true] {
        let data = ora_files::sample(Sample { deflate: true, merged: Merged::None, defer });
        let (add, status, _) = import_both(&wasm, "doc.ora", &data).unwrap();
        assert!(add.contains(r#""importer":{"abi":"tmi:format@0.1.0","component_sha256":"test-component","id":"ora","version":"#), "{add}");
        for key in ["\"pixel_source\":", "\"semantic_digest\":", "\"semantic_digest_version\":1", "\"write_capability\":\"metadata-only\""] {
            assert!(add.contains(key), "{key}: {add}");
        }
        // the status import reuses every prior decode (the same pixel_source)
        assert!(status.contains("\"pixel_source\":"), "{status}");
    }
}

fn priors(layers: &[ImportedLayer], out: &mut std::collections::HashMap<String, PriorDecode>) {
    for l in layers {
        if let (Some(sha), Some(src)) = (&l.pixel_sha256, l.source_meta.get("pixel_source")) {
            if src.truthy() {
                out.insert(l.stable_source_id.clone(), PriorDecode { pixel_source: Some(src.clone()), pixel_sha256: sha.clone(), pixel_bbox: l.pixel_bbox.clone() });
            }
        }
        priors(&l.children, out);
    }
}

/// `add` (composite, every blob) then `status` (no composite, no blobs) of
/// `data` through `reader`: both dumps and the warnings.
pub(crate) fn import_both(reader: &dyn FormatReader, name: &str, data: &[u8]) -> Result<(String, String, Vec<String>), ImportError> {
    let all = LayerBlobs::All;
    let none = LayerBlobs::Only(HashSet::new());
    let opts = Options {
        render_composite: true,
        cache: None,
        reuse_pixels: None,
        reuse_decoded: None,
        layer_blobs: &all,
        composite_base: None,
        limits: Limits::NONE,
        verify_partial: false,
        progress: None,
    };
    let mut warns = Warnings::default();
    let add = pipeline::import_plugin(reader, &importer(reader), name, data, "digest", &opts, &mut warns)?;
    let mut prior = std::collections::HashMap::new();
    priors(&add.layers, &mut prior);
    let opts2 = Options { render_composite: false, reuse_decoded: if prior.is_empty() { None } else { Some(&prior) }, layer_blobs: &none, ..opts };
    let status = pipeline::import_plugin(reader, &importer(reader), name, data, "digest", &opts2, &mut Warnings::default())?;
    Ok((dump(&add), dump(&status), warns.lines))
}

pub(crate) fn samples() -> Vec<Sample> {
    let mut out = Vec::new();
    for deflate in [false, true] {
        for merged in [Merged::None, Merged::Canvas, Merged::WrongSize] {
            for defer in [false, true] {
                out.push(Sample { deflate, merged, defer });
            }
        }
    }
    out
}

#[test]
fn the_ora_component_makes_the_records_its_native_build_makes() {
    let wasm = WasmReader::new(plugin("tmi-plugin-ora"));
    let native = OraNative::default();
    assert_eq!(wasm.info(), native.info());
    let mut by_defer = std::collections::HashMap::new();
    for s in samples() {
        let data = ora_files::sample(s);
        let w = import_both(&wasm, "doc.ora", &data).unwrap_or_else(|e| panic!("{s:?}: {e:?}"));
        let n = import_both(&native, "doc.ora", &data).unwrap();
        assert_eq!(w, n, "{s:?}");
        assert!(w.0.contains(r#""source_format":"plugin:ora""#), "{}", w.0);
        assert!(w.0.contains("背景"));
        // the structure's warnings are printed (P6), then the pipeline's own
        assert_eq!(w.2[0], "tmiki: warning: doc.ora: unknown composite-op svg:xor; drawn as normal (blend)", "{:?}", w.2);
        if s.merged == Merged::WrongSize {
            assert_eq!(w.2.len(), 2, "{:?}", w.2);
            assert!(w.2[1].contains("drawing it with generic-v1"), "{:?}", w.2);
        } else {
            assert_eq!(w.2.len(), 1, "{:?}", w.2);
        }
        // deferring the layers does not change the records
        if let Some(other) = by_defer.insert((s.deflate, s.merged), w.clone()) {
            assert_eq!(other.0, w.0, "{s:?}");
        }
    }
}

#[test]
fn the_records_hold_what_stack_xml_says() {
    let data = ora_files::sample(Sample { deflate: true, merged: Merged::None, defer: false });
    let wasm = WasmReader::new(plugin("tmi-plugin-ora"));
    let opts_blobs = LayerBlobs::All;
    let opts = Options {
        render_composite: true,
        cache: None,
        reuse_pixels: None,
        reuse_decoded: None,
        layer_blobs: &opts_blobs,
        composite_base: None,
        limits: Limits::NONE,
        verify_partial: false,
        progress: None,
    };
    let f = pipeline::import_plugin(&wasm, &importer(&wasm), "doc.ora", &data, "d", &opts, &mut Warnings::default()).unwrap();
    let names: Vec<(&str, &str, &str)> = f.layers.iter().map(|l| (l.layer_id.as_str(), l.name.as_str(), l.blend_mode.as_str())).collect();
    // bottom to top
    assert_eq!(names, [("ora:0", "背景", "normal"), ("ora:1", "through", "pass_through"), ("ora:2", "group", "screen"), ("ora:3", "top", "multiply")]);
    assert_eq!(f.layers[1].children[0].blend_mode, "other");
    assert_eq!(f.layers[2].children.len(), 2);
    assert!(!f.layers[2].children[1].visible, "the hidden layer is the top child");
    assert_eq!((f.layers[3].offset_x, f.layers[3].offset_y, f.layers[3].opacity), (10, -4, 0.5));
    assert!(matches!(f.canvas_meta.get("dpi"), Some(Value::Float(d)) if *d == 144.0));
    // drawn by generic-v1, with its tag
    let png = &f.composite_png;
    assert!(png.windows(13).any(|w| w == b"tmi-composite"), "the composite is tagged");
}

#[test]
fn many_deferred_layers_decode_in_parallel_as_the_native_build_does() {
    let data = ora_files::many_layers(200, 16, true, true);
    let wasm = WasmReader::new(plugin("tmi-plugin-ora"));
    let w = import_both(&wasm, "many.ora", &data).unwrap();
    let n = import_both(&OraNative::default(), "many.ora", &data).unwrap();
    assert_eq!(w, n);
    assert_eq!(w.0.matches("\"layer_id\"").count(), 200);
}

#[test]
fn a_fresh_instance_per_call_gives_the_same_answer() {
    let p = plugin("tmi-plugin-ora");
    let data = ora_files::sample(Sample { deflate: true, merged: Merged::Canvas, defer: true });
    let a = p.read_structure("doc.ora", &data).unwrap();
    let b = p.read_structure("doc.ora", &data).unwrap();
    assert_eq!(a, b);
    let la = p.read_layer("doc.ora", &data, &a.context, 2).unwrap();
    let lb = p.read_layer("doc.ora", &data, &a.context, 2).unwrap();
    assert_eq!(la, lb);
    assert_eq!(p.probe("doc.ora", &data[..64], data.len() as u64).unwrap(), tmi_format::types::Confidence::Yes);
    assert_eq!(p.probe("x.ora", b"PK\x03\x04 not an ora", 20).unwrap(), tmi_format::types::Confidence::No);
}

// -- bad plugins -----------------------------------------------------------

fn bad(what: &str) -> Vec<u8> {
    format!("BAD:{what}\n").into_bytes()
}

fn read(what: &str) -> Result<tmi_format::types::Structure, PluginError> {
    plugin("tmi-plugin-bad").read_structure("x.bad", &bad(what))
}

/// The pipeline's verdict on `what` (the host's 3.4 checks run there).
fn through_pipeline(what: &str) -> Result<(String, String, Vec<String>), ImportError> {
    import_both(&WasmReader::new(plugin("tmi-plugin-bad")), "x.bad", &bad(what))
}

fn corrupt_message(r: Result<(String, String, Vec<String>), ImportError>) -> String {
    match r {
        Err(ImportError::Read(ReadError::Corrupt(m))) => m,
        other => panic!("expected a corrupt structure, got {other:?}"),
    }
}

#[test]
fn an_endless_loop_runs_out_of_fuel() {
    assert_eq!(read("loop").unwrap_err(), PluginError::Fuel { limit: 500_000_000 });
}

#[test]
fn an_endless_loop_without_fuel_meets_the_time_limit() {
    let h = PluginHost::new(Settings { fuel: None, timeout: Some(Duration::from_millis(300)), cache_dir: None, workers: 1, ..Settings::default() }).unwrap();
    let p = h.load(&test_plugins::component("tmi-plugin-bad")).unwrap();
    let t = std::time::Instant::now();
    assert_eq!(p.read_structure("x.bad", &bad("loop")).unwrap_err(), PluginError::Timeout { limit: Duration::from_millis(300) });
    let took = t.elapsed();
    assert!(took >= Duration::from_millis(300) && took < Duration::from_secs(5), "{took:?}");
}

#[test]
fn a_memory_hog_meets_the_memory_limit() {
    assert_eq!(read("hog").unwrap_err(), PluginError::Memory { limit: 256 << 20 });
}

#[test]
fn endless_recursion_overflows_the_stack() {
    assert_eq!(read("deep").unwrap_err(), PluginError::StackOverflow);
}

#[test]
fn a_panic_is_a_trap() {
    // issue #51: the plugin's panic message, in one line, no wasm backtrace
    let e = read("panic").unwrap_err();
    assert_eq!(e, PluginError::Trap("panic: deliberate panic in read-structure".into()));
    assert_eq!(e.to_string(), "crashed (panic: deliberate panic in read-structure)");
}

#[test]
fn panic_messages_are_found_in_stderr() {
    let now = "\nthread '<unnamed>' panicked at src/lib.rs:241:28:\ndeliberate panic\nnote: run with `RUST_BACKTRACE=1` environment variable to display a backtrace\n";
    assert_eq!(crate::plugin::panic_message(now).as_deref(), Some("deliberate panic"));
    let old = "thread 'main' panicked at 'it broke', src/lib.rs:3:5\nnote: run with ...\n";
    assert_eq!(crate::plugin::panic_message(old).as_deref(), Some("it broke"));
    assert_eq!(crate::plugin::panic_message("some log line\n"), None);
}

#[test]
fn what_breaks_the_rules_is_refused_by_the_host_check() {
    assert_eq!(corrupt_message(through_pipeline("huge")), "context over 16 MiB");
    assert_eq!(corrupt_message(through_pipeline("layers")), "100001 layers (at most 100000)");
    assert!(corrupt_message(through_pipeline("span")).starts_with("span 7+10 is outside the 9-byte file"));
    assert_eq!(corrupt_message(through_pipeline("tree")), "layer 0 names layer 1 as its parent, which is not before it");
    // the plugin's own error passes through as it is
    assert!(matches!(through_pipeline("nothing"), Err(ImportError::Read(ReadError::NotThisFormat(_)))));
}

#[test]
fn a_component_that_imports_sockets_is_refused_when_loaded() {
    match host().load(&test_plugins::component("tmi-plugin-bad-sockets")) {
        Err(PluginError::Imports(list)) => {
            assert!(list.iter().any(|i| i.starts_with("wasi:sockets/")), "{list:?}");
            assert!(list.iter().all(|i| !i.starts_with("tmi:format/")), "{list:?}");
        }
        Err(e) => panic!("{e:?}"),
        Ok(_) => panic!("loaded"),
    }
}

#[test]
fn bytes_that_are_not_a_component_are_refused() {
    assert!(matches!(host().load(b"\0asm\x01\0\0\0"), Err(PluginError::Compile(_))));
    assert!(matches!(host().load(b"not wasm"), Err(PluginError::Compile(_))));
}

/// A component (binary format) that instantiates `module` `n` times.
fn component_instantiating(module: &[u8], n: u8) -> Vec<u8> {
    assert!(module.len() < 128 && n < 42);
    let mut c = b"\0asm\x0d\x00\x01\x00".to_vec();
    c.push(0x01); // core module section
    c.push(module.len() as u8);
    c.extend_from_slice(module);
    c.push(0x02); // core instance section: n × instantiate module 0, no args
    c.push(1 + 3 * n);
    c.push(n);
    for _ in 0..n {
        c.extend_from_slice(&[0x00, 0x00, 0x00]);
    }
    c
}

/// PLUGINS.md Q14, 6.2: the pooling slots are sized for Rust components
/// (the test plugins, loaded by every other test here, and the templates,
/// `tmi-plugin-tools`' tests); a component over them, as one from another
/// language is, is refused with a sentence that says so.
#[test]
fn a_component_over_the_pooling_slots_is_refused_with_a_sentence() {
    let empty = b"\0asm\x01\0\0\0";
    let n = crate::engine::CORE_INSTANCES_PER_COMPONENT as u8;
    // exactly the slots: compiles (and then fails for not being a plugin)
    match host().load(&component_instantiating(empty, n)) {
        Err(PluginError::Link(_)) => {}
        other => panic!("{:?}", other.err()),
    }
    match host().load(&component_instantiating(empty, n + 22)) {
        Err(e @ PluginError::Compile(_)) => assert_eq!(
            e.to_string(),
            format!(
                "not a usable component: it has {} core instances and a plugin may have {n}: tmiki runs format plugins written in Rust \
                 ('tmiki plugin new' writes one on tmi-plugin-sdk); a component made from another language is larger than that",
                n + 22
            )
        ),
        other => panic!("{:?}", other.err()),
    }
    // memories: a module with one memory, instantiated over the slots
    let with_memory = b"\0asm\x01\0\0\0\x05\x03\x01\x00\x01";
    let m = crate::engine::MEMORIES_PER_COMPONENT as u8 + 1;
    match host().load(&component_instantiating(with_memory, m)) {
        Err(PluginError::Compile(msg)) => assert!(msg.starts_with(&format!("it has {m} memories and a plugin may have {}: tmiki runs", m - 1)), "{msg}"),
        other => panic!("{:?}", other.err()),
    }
}

#[test]
fn a_deferred_layer_that_loops_fails_the_import() {
    match through_pipeline("defer-loop") {
        Err(ImportError::Read(ReadError::Plugin(m))) => assert_eq!(m, "plugin bad 0.1.0: ran out of fuel (500000000 per call; plugin.fuel)"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_plugin_codec_runs_in_the_plugin_and_its_output_is_checked() {
    let (add, _, _) = through_pipeline("chunk").unwrap();
    // [00 10 20 ff 40 50 60 80] inverted: (ff, ef, df, 00) and (bf, af, 9f, 7f)
    let crop = tmi_import::crop::LayerCrop::from_full(&[0xff, 0xef, 0xdf, 0x00, 0xbf, 0xaf, 0x9f, 0x7f], 2, 1);
    let digest = tmi_import::crop::digest_and_encode(&crop, "l0", &Options {
        render_composite: false,
        cache: None,
        reuse_pixels: None,
        reuse_decoded: None,
        layer_blobs: &LayerBlobs::Only(HashSet::new()),
        composite_base: None,
        limits: Limits::NONE,
        verify_partial: false,
        progress: None,
    })
    .unwrap()
    .0;
    assert!(add.contains(&digest), "{add}");
    assert!(corrupt_message(through_pipeline("chunk-big")).starts_with("plugin codec 8 gave 9 bytes"));
}

fn meta(s: &tmi_format::types::Structure, key: &str) -> String {
    match s.file_meta.iter().find(|e| e.key == key).map(|e| &e.value) {
        Some(tmi_format::types::MetaValue::Text(t)) => t.clone(),
        other => panic!("{key}: {other:?}"),
    }
}

#[test]
fn the_plugins_world_is_fixed() {
    let a = read("world").unwrap();
    let b = read("world").unwrap();
    assert_eq!(a, b, "two fresh instances see the same world");
    assert_eq!(meta(&a, "wall_ns"), "0");
    assert_eq!(meta(&a, "monotonic_elapsed_ns"), "0");
    assert_eq!(meta(&a, "env"), "0");
    assert_eq!(meta(&a, "args"), "0");
    assert!(meta(&a, "file").parse::<u64>().is_err(), "{}", meta(&a, "file"));
    assert!(meta(&a, "root_dir").parse::<u64>().is_err(), "{}", meta(&a, "root_dir"));
}

#[test]
fn the_host_sqlite_works_and_reaches_no_file() {
    let s = read("sqlite").unwrap();
    assert_eq!(meta(&s, "create"), "ok 0");
    assert_eq!(meta(&s, "insert"), "ok 2");
    assert_eq!(meta(&s, "rows"), "1:one,2:two");
    assert!(meta(&s, "attach").starts_with("err "), "{}", meta(&s, "attach"));
    assert!(meta(&s, "vacuum_into").starts_with("err "), "{}", meta(&s, "vacuum_into"));
    assert_eq!(meta(&s, "version"), "3.50.4");
    assert!(meta(&s, "serialized").ends_with(" true"), "{}", meta(&s, "serialized"));
    for f in ["tmi-attach-test.db", "tmi-vacuum-test.db"] {
        assert!(!std::path::Path::new(f).exists(), "{f} was written");
    }
}

#[test]
fn the_host_xml_follows_elementtree() {
    let s = read("xml").unwrap();
    assert_eq!(meta(&s, "elements"), "a[x=1](hi)<-1> b[{urn:n}y=2](in)<0> c[]()<0>");
    assert!(meta(&s, "dtd").starts_with("err "), "{}", meta(&s, "dtd"));
}

#[test]
fn log_and_stdout_do_not_change_the_answer() {
    assert!(through_pipeline("log").is_ok());
}

// -- the writer's WIT --------------------------------------------------------

/// The whole `format-plugin` world goes through wasmtime's bindgen with the
/// writer's pixels as a list of handles (P7 made it `list<pixels>`: the
/// Rust guest code wit-bindgen 0.63 writes for `list<borrow<pixels>>` does
/// not compile).
#[allow(dead_code)]
mod format_plugin_world {
    pub struct Pixels;
    pub struct Blob;
    wasmtime::component::bindgen!({
        path: "../tmi-format/wit",
        world: "format-plugin",
        with: {
            "tmi:format/source.source-file": crate::host::SourceFileRes,
            "tmi:format/sqlite.database": crate::host::DatabaseRes,
            "tmi:format/edit.pixels": Pixels,
            "tmi:format/edit.blob": Blob,
        },
    });

    #[test]
    fn write_layers_takes_a_list_of_pixels() {
        // the generated export: (source-file, list<layer-edit>, list<pixels>)
        fn _call(i: &exports::tmi::format::writer::Guest, store: &mut wasmtime::Store<()>) {
            let _ = i.call_write_layers(store, wasmtime::component::Resource::new_borrow(0), &[], &[wasmtime::component::Resource::<Pixels>::new_own(1)]);
        }
    }
}

/// `temp_dir` for hardening_tests.rs.
pub(crate) fn temp_dir_for_hardening(tag: &str) -> std::path::PathBuf {
    temp_dir(tag)
}

// -- the cache -------------------------------------------------------------

#[test]
fn the_compiled_component_is_cached_and_a_damaged_file_is_replaced() {
    let dir = temp_dir("cache-test");
    let h = PluginHost::new(Settings { cache_dir: Some(dir.clone()), workers: 1, ..Settings::default() }).unwrap();
    let bytes = test_plugins::component("tmi-plugin-ora");
    let first = h.load(&bytes).unwrap();
    assert_eq!(first.cache, crate::cache::Outcome::Compiled);
    let second = h.load(&bytes).unwrap();
    assert_eq!(second.cache, crate::cache::Outcome::Loaded);
    let file = dir.join(crate::cache::key(h.engine(), &crate::engine::fingerprint(h.settings()))).join(format!("{}.cwasm", first.sha256));
    assert!(file.is_file());
    drop((first, second));
    std::fs::write(&file, b"damaged").unwrap();
    let third = h.load(&bytes).unwrap();
    assert!(matches!(third.cache, crate::cache::Outcome::Replaced(_)), "{:?}", third.cache);
    // another engine configuration has another key
    let other = PluginHost::new(Settings { cache_dir: Some(dir.clone()), workers: 1, fuel: None, ..Settings::default() }).unwrap();
    assert_eq!(other.load(&bytes).unwrap().cache, crate::cache::Outcome::Compiled);
}

// -- writing back (P7) -------------------------------------------------------

mod writing {
    use super::*;
    use tmi_format::write::{self, Edit, WriteFailure};
    use tmi_import::Rgba;

    fn new_top() -> Rgba {
        Rgba { width: 20, height: 12, data: ora_files::pattern(20, 12, 77) }
    }

    fn write_with(reader: &impl tmi_format::reader::FormatWriter, data: &[u8], edits: Vec<Edit>) -> Result<Vec<u8>, WriteFailure> {
        let importer = importer(reader);
        write::write_checked(reader, &importer, "doc.ora", data, edits, Limits::NONE, &mut Warnings::default())
    }

    fn edit_top() -> Vec<Edit> {
        vec![Edit { stable_id: "ora:3".into(), pixels: new_top() }]
    }

    fn record(reader: &dyn FormatReader, data: &[u8]) -> ImportedFile {
        let all = LayerBlobs::All;
        let opts = Options {
            render_composite: true,
            cache: None,
            reuse_pixels: None,
            reuse_decoded: None,
            layer_blobs: &all,
            composite_base: None,
            limits: Limits::NONE,
            verify_partial: false,
            progress: None,
        };
        pipeline::import_plugin(reader, &importer(reader), "doc.ora", data, "d", &opts, &mut Warnings::default()).unwrap()
    }

    fn contains(hay: &[u8], needle: &[u8]) -> bool {
        hay.windows(needle.len()).any(|w| w == needle)
    }

    #[test]
    fn the_ora_writer_writes_a_layer_that_reads_back_and_wasm_and_native_agree() {
        let wasm = WasmReader::new(plugin("tmi-plugin-ora"));
        assert!(wasm.plugin().can_write());
        let native = OraNative::default();
        for deflate in [false, true] {
            for merged in [Merged::None, Merged::Canvas] {
                for defer in [false, true] {
                    let s = Sample { deflate, merged, defer };
                    let data = ora_files::sample(s);
                    let w = write_with(&wasm, &data, edit_top()).unwrap_or_else(|e| panic!("{s:?}: {e}"));
                    let n = write_with(&native, &data, edit_top()).unwrap();
                    assert!(w == n, "{s:?}: the wasm and native writers wrote other bytes");
                    // idempotent: the same pixels into what it wrote
                    let again = write_with(&wasm, &w, edit_top()).unwrap();
                    assert!(again == w, "{s:?}: writing the same pixels again changed the bytes");
                    // the edited layer holds the new pixels; the others are as they were
                    let (before, after) = (record(&wasm, &data), record(&wasm, &w));
                    let top = after.layers.iter().find(|l| l.stable_source_id == "ora:3").unwrap();
                    assert_eq!(top.pixel_sha256.as_ref().and_then(|v| v.as_str()), Some(write::pixels_digest(&new_top()).as_str()));
                    assert_eq!(before.layers[0].pixel_sha256.as_ref().and_then(|v| v.as_str()), after.layers[0].pixel_sha256.as_ref().and_then(|v| v.as_str()));
                    // untouched members are copied byte for byte (here the
                    // background's PNG, which is the first member after stack.xml)
                    let bg = ora_files::png(64, 48, &ora_files::pattern(64, 48, 5));
                    if !deflate {
                        assert!(contains(&w, &bg), "{s:?}: the background's bytes moved");
                    }
                    if merged == Merged::Canvas {
                        // the composite is rebuilt from the edited document
                        assert_ne!(before.composite_png, after.composite_png, "{s:?}");
                    }
                    assert!(w.starts_with(b"PK\x03\x04") && &w[30..38] == b"mimetype");
                }
            }
        }
    }

    #[test]
    fn a_writer_that_breaks_the_rules_is_stopped_by_the_host() {
        let wasm = WasmReader::new(plugin("tmi-plugin-ora"));
        let native = OraNative::default();
        let s = Sample { deflate: true, merged: Merged::Canvas, defer: false };
        for (fault, want) in [
            ("other-layer", "changed although it was not edited"),
            ("ignore-edit", "does not hold the new pixels"),
            ("bad-span", "is outside the"),
        ] {
            let data = ora_files::sample_with(s, &format!(r#" tmi-test-write-fault="{fault}""#));
            for (who, r) in [("wasm", write_with(&wasm, &data, edit_top())), ("native", write_with(&native, &data, edit_top()))] {
                match r {
                    Err(e @ (WriteFailure::Check(_) | WriteFailure::Plan(_))) => assert!(e.to_string().contains(want), "{fault} {who}: {e}"),
                    other => panic!("{fault} {who}: {other:?}"),
                }
            }
        }
    }

    #[test]
    fn a_layer_without_writable_pixels_is_refused_before_the_writer_runs() {
        let wasm = WasmReader::new(plugin("tmi-plugin-ora"));
        let data = ora_files::sample(Sample { deflate: true, merged: Merged::None, defer: false });
        let folder = vec![Edit { stable_id: "ora:1".into(), pixels: new_top() }];
        match write_with(&wasm, &data, folder) {
            Err(WriteFailure::Layer(m)) => assert!(m.contains("cannot write the pixels of layer through"), "{m}"),
            other => panic!("{other:?}"),
        }
        match write_with(&wasm, &data, vec![Edit { stable_id: "nope".into(), pixels: new_top() }]) {
            Err(WriteFailure::Layer(m)) => assert!(m.contains("has no layer nope"), "{m}"),
            other => panic!("{other:?}"),
        }
    }
}