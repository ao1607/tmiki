//! `tmi-conform plugin-import-dump <component.wasm> [--native-ora] <files>`:
//! the records a wasm plugin (or, with `--native-ora`, the ORA test plugin
//! built natively) gives for each file through `tmi_format::pipeline`, as
//! `add` and as `status` import them; one JSON line per file.
//!
//! `tmi-conform plugin-bench <ora.wasm> <bad.wasm>`: PLUGINS.md 6.4's items
//! 1-6 on this machine, as text (min / median of n runs).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tmi_format::reader::{FormatReader, ReadCx, Source};
use tmi_import::{ImportedLayer, LayerBlobs, Limits, Options, PriorDecode, Warnings};
use tmi_plugin_host::native_ora::OraNative;
use tmi_plugin_host::ora_files::{self, Merged, Sample};
use tmi_plugin_host::{PluginHost, Settings, WasmReader};
use tmi_store::json::{self, Map, Value};

fn opts<'a>(blobs: &'a LayerBlobs, render: bool) -> Options<'a> {
    Options {
        render_composite: render,
        cache: None,
        reuse_pixels: None,
        reuse_decoded: None,
        layer_blobs: blobs,
        composite_base: None,
        limits: tmi_import::limit::resolve(None, None).unwrap_or(Limits::NONE),
        verify_partial: false,
        progress: None,
    }
}

fn priors(layers: &[ImportedLayer], out: &mut HashMap<String, PriorDecode>) {
    for l in layers {
        if let (Some(sha), Some(src)) = (&l.pixel_sha256, l.source_meta.get("pixel_source")) {
            if src.truthy() {
                out.insert(l.stable_source_id.clone(), PriorDecode { pixel_source: Some(src.clone()), pixel_sha256: sha.clone(), pixel_bbox: l.pixel_bbox.clone() });
            }
        }
        priors(&l.children, out);
    }
}

fn importer_of(reader: &dyn FormatReader, component_sha256: &str) -> tmi_format::pipeline::Importer {
    tmi_format::pipeline::Importer {
        id: reader.info().id.clone(),
        version: reader.info().version.clone(),
        component_sha256: component_sha256.into(),
        abi: "tmi:format@0.1.0".into(),
    }
}

fn error_text(e: tmi_format::pipeline::ImportError) -> String {
    match e {
        tmi_format::pipeline::ImportError::Read(r) => r.to_string(),
        tmi_format::pipeline::ImportError::Decline(d) => d.0,
    }
}

pub fn dump(args: &[String]) {
    let native = args.iter().any(|a| a == "--native-ora");
    let mut rest = args.iter().filter(|a| *a != "--native-ora");
    let component = rest.next().expect("usage: plugin-import-dump <component.wasm> [--native-ora] <files>");
    let files: Vec<&String> = rest.collect();
    let reader: Box<dyn FormatReader> = if native {
        Box::new(OraNative::default())
    } else {
        let host = PluginHost::new(Settings::default()).unwrap_or_else(|e| panic!("{e}"));
        let plugin = host.load(&std::fs::read(component).unwrap()).unwrap_or_else(|e| panic!("{component}: {e}"));
        Box::new(WasmReader::new(Arc::new(plugin)))
    };
    // the component's hash in both builds' records, so they compare equal
    let importer = importer_of(reader.as_ref(), &tmi_store::oid::sha256_hex(&std::fs::read(component).unwrap()));
    let all = LayerBlobs::All;
    let none = LayerBlobs::Only(HashSet::new());
    for path in files {
        let data = std::fs::read(path).unwrap();
        let digest = tmi_store::oid::sha256_hex(&data);
        let name = std::path::Path::new(path).file_name().unwrap().to_string_lossy().into_owned();
        let mut out = Map::new();
        out.insert("file".into(), Value::Str(path.clone()));
        let mut warns = Warnings::default();
        match tmi_format::pipeline::import_plugin(reader.as_ref(), &importer, &name, &data, &digest, &opts(&all, true), &mut warns) {
            Err(e) => {
                out.insert("error".into(), Value::Str(error_text(e)));
            }
            Ok(f) => {
                out.insert("add".into(), tmi_plugin_host::dump::value(&f));
                out.insert("warnings".into(), Value::Array(warns.lines.iter().cloned().map(Value::Str).collect()));
                let mut prior = HashMap::new();
                priors(&f.layers, &mut prior);
                let o2 = Options { reuse_decoded: if prior.is_empty() { None } else { Some(&prior) }, ..opts(&none, false) };
                match tmi_format::pipeline::import_plugin(reader.as_ref(), &importer, &name, &data, &digest, &o2, &mut Warnings::default()) {
                    Err(e) => {
                        out.insert("status_error".into(), Value::Str(error_text(e)));
                    }
                    Ok(s) => {
                        out.insert("status".into(), tmi_plugin_host::dump::value(&s));
                    }
                }
            }
        }
        println!("{}", json::dumps(&Value::Object(out), &json::DumpOptions::CANONICAL));
    }
}

/// `tmi-conform ora-samples <dir>`: the hand-made ORA files of the tests,
/// written out (for `plugin-import-dump` and the conformance scenario `plugins`).
pub fn samples(args: &[String]) {
    let dir = std::path::Path::new(&args[0]);
    std::fs::create_dir_all(dir).unwrap();
    for deflate in [false, true] {
        for (merged, m) in [(Merged::None, "render"), (Merged::Canvas, "merged"), (Merged::WrongSize, "wrongsize")] {
            for defer in [false, true] {
                let name = format!("sample-{}-{m}{}.ora", if deflate { "deflate" } else { "stored" }, if defer { "-deferred" } else { "" });
                std::fs::write(dir.join(name), ora_files::sample(Sample { deflate, merged, defer })).unwrap();
            }
        }
    }
    std::fs::write(dir.join("many-100.ora"), ora_files::many_layers(100, 64, true, false)).unwrap();
    std::fs::write(dir.join("many-100-deferred.ora"), ora_files::many_layers(100, 64, true, true)).unwrap();
    std::fs::write(dir.join("many-1000.ora"), ora_files::many_layers(1000, 32, false, false)).unwrap();
}

// -- measurements ------------------------------------------------------------

fn measure(n: usize, mut f: impl FnMut()) -> (Duration, Duration) {
    let mut v: Vec<Duration> = (0..n)
        .map(|_| {
            let t = Instant::now();
            f();
            t.elapsed()
        })
        .collect();
    v.sort();
    (v[0], v[n / 2])
}

fn fmt(d: Duration) -> String {
    let us = d.as_secs_f64() * 1e6;
    if us < 1000.0 {
        format!("{us:.1} µs")
    } else if us < 1e6 {
        format!("{:.2} ms", us / 1000.0)
    } else {
        format!("{:.3} s", us / 1e6)
    }
}

fn line(what: &str, (min, med): (Duration, Duration), n: usize) {
    println!("{what:<62} {:>11} {:>11}  (n={n})", fmt(min), fmt(med));
}

fn temp(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("tmi-plugin-bench-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn host(s: Settings) -> PluginHost {
    PluginHost::new(s).unwrap_or_else(|e| panic!("{e}"))
}

fn import(reader: &dyn FormatReader, name: &str, data: &[u8]) {
    let all = LayerBlobs::All;
    tmi_format::pipeline::import_plugin(reader, &importer_of(reader, "bench"), name, data, "d", &opts(&all, true), &mut Warnings::default()).unwrap();
}

/// `tmi-conform plugin-bench`: PLUGINS.md 6.4's items 1-6 (tools/bench_plugins.py
/// runs it and adds items 7-10 with the executable).
///
///   plugin-bench <ora.wasm> <bad.wasm>                  (the P5 form)
///   plugin-bench --ora <ora.wasm> --bad <bad.wasm>
///                [--items 1,2,...] [--repeat F] [--json <out.jsonl>]
///
/// Every measurement is n runs, min and median; `--repeat` multiplies n.
/// `--json` writes one JSON object per measurement (item, lang, case, n,
/// min_s / median_s, or value and unit) for the script's tables.
pub fn bench(args: &[String]) {
    let mut ora_path = None;
    let mut bad_path = None;
    let mut items: Vec<u32> = (1..=6).collect();
    let mut repeat = 1.0f64;
    let mut json_path = None;
    let mut positional = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = || it.next().unwrap_or_else(|| panic!("{a} needs a value")).clone();
        match a.as_str() {
            "--ora" => ora_path = Some(val()),
            "--bad" => bad_path = Some(val()),
            "--items" => items = val().split(',').map(|s| s.trim().parse().expect("--items 1,2,...")).collect(),
            "--repeat" => repeat = val().parse().expect("--repeat <factor>"),
            "--json" => json_path = Some(val()),
            _ => positional.push(a.clone()),
        }
    }
    let mut pos = positional.into_iter();
    let ora_path = ora_path.or_else(|| pos.next()).expect("usage: plugin-bench --ora <ora.wasm> --bad <bad.wasm> ");
    let bad_path = bad_path.or_else(|| pos.next()).expect("plugin-bench: --bad <bad.wasm>");
    let ora = std::fs::read(&ora_path).unwrap();
    let bad = std::fs::read(&bad_path).unwrap();
    let _ = rayon::ThreadPoolBuilder::new().stack_size(32 << 20).build_global();
    let mut b = Bench { repeat, json: json_path.map(|p| std::io::BufWriter::new(std::fs::File::create(p).unwrap())), item: 0 };
    // one entry, the Rust ORA (P8 and P9-A had the Python ORA too, which
    // PLUGINS.md Q14 took out with Python plugins)
    let langs: Vec<(&str, &[u8])> = vec![("rust", &ora)];
    println!("components: ora {} bytes, bad {} bytes", ora.len(), bad.len());
    b.item = 0;
    for (lang, bytes) in &langs {
        b.value(lang, "component bytes", bytes.len() as f64, "bytes");
    }
    b.value("-", "available_parallelism", std::thread::available_parallelism().map_or(1, |n| n.get()) as f64, "threads");
    println!("{:<62} {:>11} {:>11}", "", "min", "median");
    for item in items {
        b.item = item;
        match item {
            1 => item1(&mut b, &langs),
            2 => item2(&mut b, &langs),
            3 => item3(&mut b, &langs, &bad),
            4 => item4(&mut b, &langs),
            5 => item5(&mut b, &langs),
            6 => item6(&mut b, &langs),
            _ => eprintln!("plugin-bench: item {item} is not measured here (tools/bench_plugins.py does 7-10)"),
        }
    }
}

struct Bench {
    repeat: f64,
    json: Option<std::io::BufWriter<std::fs::File>>,
    item: u32,
}

impl Bench {
    /// n scaled by `--repeat` (at least 3, so a median means something).
    fn n(&self, n: usize) -> usize {
        ((n as f64 * self.repeat).round() as usize).max(3)
    }

    fn emit(&mut self, lang: &str, case: &str, fields: Vec<(&str, Value)>) {
        if let Some(w) = &mut self.json {
            use std::io::Write;
            let mut m = Map::new();
            m.insert("item".into(), Value::Int(json::Int::Small(self.item as i64)));
            m.insert("lang".into(), Value::Str(lang.into()));
            m.insert("case".into(), Value::Str(case.into()));
            for (k, v) in fields {
                m.insert(k.into(), v);
            }
            writeln!(w, "{}", json::dumps(&Value::Object(m), &json::DumpOptions::CANONICAL)).unwrap();
            w.flush().unwrap();
        }
    }

    /// A timed measurement.
    fn time(&mut self, lang: &str, case: &str, n: usize, f: impl FnMut()) -> (Duration, Duration) {
        let r = measure(n, f);
        self.timed(lang, case, r, n);
        r
    }

    fn timed(&mut self, lang: &str, case: &str, r: (Duration, Duration), n: usize) {
        line(&format!("[{lang}] {case}"), r, n);
        self.emit(lang, case, vec![
            ("n", Value::Int(json::Int::Small(n as i64))),
            ("min_s", Value::Float(r.0.as_secs_f64())),
            ("median_s", Value::Float(r.1.as_secs_f64())),
        ]);
    }

    /// A number that is not a time (bytes, instances per second).
    fn value(&mut self, lang: &str, case: &str, v: f64, unit: &str) {
        println!("[{lang}] {case}: {v:.1} {unit}");
        self.emit(lang, case, vec![("value", Value::Float(v)), ("unit", Value::Str(unit.into()))]);
    }
}

/// 1. Compiling (no cache) and loading from the `.cwasm` cache.
fn item1(b: &mut Bench, langs: &[(&str, &[u8])]) {
    println!("-- 1. compiling and loading");
    for &(lang, bytes) in langs {
        let nocache = host(Settings { cache_dir: None, ..Settings::default() });
        let n = b.n(5);
        b.time(lang, "compile (Cranelift, no cache): whole load", n, || drop(nocache.load(bytes).unwrap()));
        let p = nocache.load(bytes).unwrap();
        b.timed(lang, "compile alone (one run)", (p.compile, p.compile), 1);
        b.timed(lang, "instantiate_pre + export check (one run)", (p.prepare, p.prepare), 1);
        drop(p);
        let dir = temp(&format!("cache-{lang}"));
        let cached = host(Settings { cache_dir: Some(dir.clone()), ..Settings::default() });
        let first = cached.load(bytes).unwrap();
        let file = std::fs::read_dir(&dir).unwrap().flatten().flat_map(|d| std::fs::read_dir(d.path()).unwrap().flatten()).find(|f| f.path().extension().is_some_and(|e| e == "cwasm"));
        if let Some(f) = file {
            b.value(lang, ".cwasm bytes", f.metadata().unwrap().len() as f64, "bytes");
        }
        drop(first);
        let n = b.n(20);
        b.time(lang, "load from the .cwasm cache (whole load)", n, || drop(cached.load(bytes).unwrap()));
        let reads: Vec<Duration> = (0..n).map(|_| cached.load(bytes).unwrap().compile).collect();
        let mut r = reads.clone();
        r.sort();
        b.timed(lang, "the .cwasm read alone (deserialize_file)", (r[0], r[r.len() / 2]), n);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// 2. A fresh instance: pooling / CoW, compiled in memory or mapped from
/// the `.cwasm` cache, and in parallel with pools of 1-12.
fn item2(b: &mut Bench, langs: &[(&str, &[u8])]) {
    println!("-- 2. a fresh instance (store + instantiate, nothing called; incl. the worker round trip)");
    for &(lang, bytes) in langs {
        for from_cache in [true, false] {
            for (pooling, cow) in [(true, true), (true, false), (false, true), (false, false)] {
                let dir = temp(&format!("inst-{lang}"));
                let h = host(Settings { cache_dir: from_cache.then(|| dir.clone()), pooling, memory_init_cow: cow, workers: 1, ..Settings::default() });
                if from_cache {
                    drop(h.load(bytes).unwrap()); // compile into the cache; the next load maps it
                }
                let p = h.load(bytes).unwrap();
                for _ in 0..20 {
                    p.instantiate_only().unwrap();
                }
                let n = b.n(500);
                let src = if from_cache { "cwasm" } else { "memory" };
                b.time(lang, &format!("instance: {src} pooling={pooling} cow={cow}"), n, || p.instantiate_only().unwrap());
                if pooling && cow && from_cache {
                    b.time(lang, "the worker round trip alone", b.n(500), || h.worker_roundtrip());
                }
                drop(p);
                drop(h);
                let _ = std::fs::remove_dir_all(&dir);
            }
        }
        // in parallel: k workers (= the pool's slots), each instantiating
        // total/k times; the best and median of the repeats
        for cow in [true, false] {
            for k in [1usize, 2, 3, 4, 6, 8, 10, 12] {
                let dir = temp(&format!("par-{lang}"));
                let h = host(Settings { cache_dir: Some(dir.clone()), memory_init_cow: cow, workers: k, ..Settings::default() });
                drop(h.load(bytes).unwrap());
                let p = Arc::new(h.load(bytes).unwrap());
                let total = 480;
                let per = total / k;
                let reps = b.n(3);
                let mut times: Vec<Duration> = (0..reps)
                    .map(|_| {
                        let t = Instant::now();
                        std::thread::scope(|s| {
                            for _ in 0..k {
                                let p = Arc::clone(&p);
                                s.spawn(move || {
                                    for _ in 0..per {
                                        p.instantiate_only().unwrap();
                                    }
                                });
                            }
                        });
                        t.elapsed()
                    })
                    .collect();
                times.sort();
                let rate = |d: Duration| (per * k) as f64 / d.as_secs_f64();
                b.value(lang, &format!("parallel cwasm pooling cow={cow} k={k}: instances per second (best)"), rate(times[0]), "per_s");
                b.value(lang, &format!("parallel cwasm pooling cow={cow} k={k}: instances per second (median)"), rate(times[times.len() / 2]), "per_s");
                drop(p);
                drop(h);
                let _ = std::fs::remove_dir_all(&dir);
            }
        }
    }
}

/// 3. One `read-structure` by the size of what it returns.
fn item3(b: &mut Bench, langs: &[(&str, &[u8])], bad: &[u8]) {
    println!("-- 3. read-structure, by size (no decoding)");
    let native = OraNative::default();
    let h = host(Settings { cache_dir: None, ..Settings::default() });
    for n in [10usize, 100, 10_000] {
        let data = ora_files::many_layers(n, 8, true, false);
        let cx = ReadCx::new(None);
        let src = Source::new("many.ora", &data);
        for &(lang, bytes) in langs {
            let p = h.load(bytes).unwrap();
            let iters = b.n(if n >= 10_000 { 9 } else { 50 });
            b.time(lang, &format!("read-structure {n} layers"), iters, || drop(p.read_structure("many.ora", &data).unwrap()));
        }
        let iters = b.n(if n >= 10_000 { 9 } else { 50 });
        b.time("native", &format!("read-structure {n} layers"), iters, || drop(native.read_structure(&src, &cx).unwrap()));
    }
    let pb = h.load(bad).unwrap();
    b.time("rust", "read-structure one layer of 100,000 tiles (bad)", b.n(9), || drop(pb.read_structure("x.bad", b"BAD:tiles\n").unwrap()));
}

fn pipeline_files() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("sample (64x48, 5 layers, mergedimage)", ora_files::sample(Sample { deflate: true, merged: Merged::Canvas, defer: false })),
        ("100 layers of 64x64", ora_files::many_layers(100, 64, true, false)),
        ("100 layers of 64x64, deferred", ora_files::many_layers(100, 64, true, true)),
        ("1000 layers of 32x32", ora_files::many_layers(1000, 32, true, false)),
    ]
}

/// 4. The same ORA through the whole pipeline: native and wasm.
fn item4(b: &mut Bench, langs: &[(&str, &[u8])]) {
    println!("-- 4. import through the pipeline (composite, blobs): native vs wasm");
    let native = OraNative::default();
    let h = host(Settings { cache_dir: None, ..Settings::default() });
    let readers: Vec<(&str, WasmReader)> = langs.iter().map(|&(l, bytes)| (l, WasmReader::new(Arc::new(h.load(bytes).unwrap())))).collect();
    for (what, data) in &pipeline_files() {
        b.time("native", &format!("import {what}"), b.n(9), || import(&native, "f.ora", data));
        for (lang, r) in &readers {
            let n = b.n(9);
            b.time(lang, &format!("import {what}"), n, || import(r, "f.ora", data));
        }
    }
}

/// 5. Fuel and epoch on and off: `read-structure` of 10,000 layers.
fn item5(b: &mut Bench, langs: &[(&str, &[u8])]) {
    println!("-- 5. fuel / epoch: read-structure of 10,000 layers");
    let data = ora_files::many_layers(10_000, 8, true, false);
    for &(lang, bytes) in langs {
        for (fuel, epoch) in [(true, true), (true, false), (false, true), (false, false)] {
            let h = host(Settings {
                cache_dir: None,
                fuel: if fuel { Some(20_000_000_000) } else { None },
                timeout: if epoch { Some(Duration::from_secs(120)) } else { None },
                ..Settings::default()
            });
            let p = h.load(bytes).unwrap();
            let n = b.n(9);
            b.time(lang, &format!("fuel={fuel} epoch={epoch}"), n, || drop(p.read_structure("many.ora", &data).unwrap()));
        }
    }
}

/// 6. Deferred layers (64×64 each) by their number and the workers K.
fn item6(b: &mut Bench, langs: &[(&str, &[u8])]) {
    println!("-- 6. deferred layers of 64x64: import by layers and workers");
    let native = OraNative::default();
    for d in [1usize, 2, 4, 8, 16, 32, 64] {
        let data = ora_files::many_layers(d, 64, true, true);
        b.time("native", &format!("deferred={d}"), b.n(9), || import(&native, "f.ora", &data));
        for &(lang, bytes) in langs {
            for k in [1usize, 2, 4, 8, 12] {
                let h = host(Settings { cache_dir: None, workers: k, ..Settings::default() });
                let r = WasmReader::new(Arc::new(h.load(bytes).unwrap()));
                let n = b.n(9);
                b.time(lang, &format!("deferred={d} k={k}"), n, || import(&r, "f.ora", &data));
            }
        }
    }
}

/// `tmi-conform ora-make <out.ora> <layers> <side> <canvas> [--deferred] [--stored]`:
/// an ORA of `layers` layers of `side`×`side` (16 distinct pictures) on a
/// `canvas`×`canvas` canvas, for item 7's `tmiki add`.
pub fn ora_make(args: &[String]) {
    let out = &args[0];
    let n: usize = args[1].parse().unwrap();
    let side: usize = args[2].parse().unwrap();
    let canvas: usize = args[3].parse().unwrap();
    let defer = args.iter().any(|a| a == "--deferred");
    let deflate = !args.iter().any(|a| a == "--stored");
    let mut xml = format!(
        "<?xml version='1.0' encoding='UTF-8'?>\n<image w=\"{canvas}\" h=\"{canvas}\"{}>\n <stack>\n",
        if defer { r#" tmi-test-defer="1""# } else { "" }
    );
    let span = canvas.saturating_sub(side).max(1);
    for i in 0..n {
        xml.push_str(&format!("  <layer name=\"l{i}\" src=\"data/{i}.png\" x=\"{}\" y=\"{}\"/>\n", (i * 37) % span, (i * 53) % span));
    }
    xml.push_str(" </stack>\n</image>\n");
    let mut members = vec![
        ora_files::Member { name: "mimetype".into(), data: b"image/openraster".to_vec(), deflate: false },
        ora_files::Member { name: "stack.xml".into(), data: xml.into_bytes(), deflate },
    ];
    let pics: Vec<Vec<u8>> = (0..16.min(n.max(1))).map(|s| ora_files::png(side, side, &ora_files::pattern(side, side, s as u32))).collect();
    for i in 0..n {
        members.push(ora_files::Member { name: format!("data/{i}.png"), data: pics[i % pics.len()].clone(), deflate });
    }
    std::fs::write(out, ora_files::zip(&members)).unwrap();
}
