//! tmi-conform: the Rust half of tools/conformance.py (RUST_MIGRATION.md 3a).
//!
//!   tmi-conform rehash <repo_dir>     every object re-read, re-hashed and
//!                                     re-written (JSON re-serialised, zlib
//!                                     re-compressed, large blobs re-chunked)
//!                                     by tmi-store; a JSON report on stdout
//!   tmi-conform large-oids <files>    write_large_blob's oid of each file
//!   tmi-conform floats                stdin: one f64 bit pattern (hex) per
//!                                     line; stdout: repr, json, round(x, 6)
//!                                     and f"{x:.1f}" of each
//!   tmi-conform import-dump <files>   tmi-import's result per file (one
//!                                     JSON line each), import_parity.py
//!   tmi-conform raster-dump | writeback-dump   PNG / JPEG decode and
//!                                     write-back against Pillow (raster_dump.rs)
//!   tmi-conform plugin-import-dump <component.wasm> [--native-ora] <files>
//!                                     a wasm plugin's records (plugin_bench.rs)
//!   tmi-conform plugin-bench --ora <ora.wasm> --bad <bad.wasm> [--python <wasm>]
//!                [--items 1,2,..] [--repeat F] [--json <out>]   PLUGINS.md 6.4
//!                                     items 1-6 (tools/bench_plugins.py)
//!   tmi-conform ora-samples <dir>     the tests' hand-made .ora files
//!   tmi-conform ora-make <out> <layers> <side> <canvas> [--deferred] [--stored]

mod import_dump;
mod plugin_bench;
mod raster_dump;

use std::collections::{BTreeMap, HashSet};
use std::io::{BufRead, Write};
use std::path::Path;
use tmi_store::json::{self, DumpOptions, Value};
use tmi_store::store::{zlib_level_for, CHUNKED_MIN_SIZE};
use tmi_store::{object_id, zlib, ObjectStore};

fn rehash(repo_dir: &Path) -> Value {
    let store = ObjectStore::new(repo_dir.join("objects"));
    let oids = store.iter_object_ids();
    let mut by_type: BTreeMap<String, i64> = BTreeMap::new();
    let mut mismatches: Vec<String> = Vec::new();
    let (mut json_checked, mut files_equal, mut large_checked) = (0i64, 0i64, 0i64);
    let mut pieces: HashSet<String> = HashSet::new();
    let mut large_candidates: Vec<String> = Vec::new();
    for oid in &oids {
        let raw = std::fs::read(store.path_for(oid)).unwrap();
        let (object_type, payload) = match store.read(oid) {
            Ok(v) => v,
            Err(e) => {
                mismatches.push(format!("{oid}: read failed: {e}"));
                continue;
            }
        };
        *by_type.entry(object_type.clone()).or_default() += 1;
        if object_id(&object_type, &payload) != *oid {
            mismatches.push(format!("{oid}: rehash differs"));
        }
        // The file bytes: zlib.compress(type + "\0" + payload, level)
        let mut data = object_type.as_bytes().to_vec();
        data.push(0);
        data.extend_from_slice(&payload);
        if zlib::compress(&data, zlib_level_for(&data)) == raw {
            files_equal += 1;
        } else {
            mismatches.push(format!("{oid}: re-compressed bytes differ"));
        }
        if matches!(object_type.as_str(), "commit" | "tree" | "tag" | "chunked") {
            match json::loads_bytes(&payload) {
                Ok(v) => {
                    json_checked += 1;
                    if json::canonical_bytes(&v) != payload {
                        mismatches.push(format!("{oid}: canonical JSON differs"));
                    }
                    if object_type == "chunked" {
                        for part in v.get("parts").and_then(Value::as_array).into_iter().flatten() {
                            if let Some(p) = part.as_str() {
                                pieces.insert(p.to_string());
                            }
                        }
                        if let Some(g) = v.get("glue").and_then(Value::as_str) {
                            pieces.insert(g.to_string());
                        }
                        large_candidates.push(oid.clone());
                    }
                }
                Err(e) => mismatches.push(format!("{oid}: JSON does not parse: {e}")),
            }
        } else if object_type == "blob" && payload.len() >= CHUNKED_MIN_SIZE {
            large_candidates.push(oid.clone());
        }
    }
    // Every large blob tmi stored whole or chunked, rebuilt from its bytes.
    for oid in large_candidates.iter().filter(|o| !pieces.contains(*o)) {
        let bytes = match store.read_blob(oid) {
            Ok(b) => b,
            Err(e) => {
                mismatches.push(format!("{oid}: read_blob failed: {e}"));
                continue;
            }
        };
        large_checked += 1;
        match store.large_blob_oids(&[&bytes]) {
            Ok(v) if v[0] == *oid => {}
            Ok(v) => mismatches.push(format!("{oid}: write_large_blob gives {}", v[0])),
            Err(e) => mismatches.push(format!("{oid}: {e}")),
        }
    }
    // The JSON files tmi rewrites: indent=2, sorted keys, "\n".
    let mut files_checked = Vec::new();
    for name in ["index", "config", "stat_cache", "stash.json"] {
        let path = repo_dir.join(name);
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let reserialised = json::loads(&text).map(|v| json::dumps(&v, &DumpOptions::INDENTED_SORTED) + "\n");
        // `tmiki init` writes the config literally until the first write_config
        let initial = name == "config" && text == "{\"version\":1,\"remotes\":{}}\n";
        if initial || reserialised.as_deref() == Ok(text.as_str()) {
            files_checked.push(Value::Str(name.into()));
        } else {
            mismatches.push(format!("{name}: re-serialised JSON differs"));
        }
    }
    let mut report = json::Map::new();
    report.insert("objects".into(), Value::Int(json::Int::Small(oids.len() as i64)));
    report.insert(
        "by_type".into(),
        Value::Object(by_type.into_iter().map(|(k, v)| (k, Value::Int(json::Int::Small(v)))).collect()),
    );
    report.insert("json_reserialised".into(), Value::Int(json::Int::Small(json_checked)));
    report.insert("files_byte_equal".into(), Value::Int(json::Int::Small(files_equal)));
    report.insert("large_blobs_rechunked".into(), Value::Int(json::Int::Small(large_checked)));
    report.insert("json_files".into(), Value::Array(files_checked));
    report.insert("mismatches".into(), Value::Array(mismatches.into_iter().map(Value::Str).collect()));
    Value::Object(report)
}

fn floats() {
    let stdin = std::io::stdin();
    let mut out = std::io::BufWriter::new(std::io::stdout().lock());
    for line in stdin.lock().lines() {
        let line = line.unwrap();
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let x = f64::from_bits(u64::from_str_radix(line, 16).unwrap());
        let rounded = json::py_round(x, 6);
        let fmt1 = if x.is_finite() { format!("{x:.1}") } else { "-".into() };
        writeln!(out, "{} {} {} {}", json::float_repr(x), json::json_float(x), json::float_repr(rounded), fmt1).unwrap();
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("rehash") => {
            let report = rehash(Path::new(&args[2]));
            println!("{}", json::dumps(&report, &DumpOptions::INDENTED_SORTED));
        }
        Some("large-oids") => {
            let store = ObjectStore::new(std::env::temp_dir().join("tmi-conform-unused"));
            for file in &args[2..] {
                let data = std::fs::read(file).unwrap();
                println!("{} {file}", store.large_blob_oids(&[&data]).unwrap()[0]);
            }
        }
        Some("floats") => floats(),
        Some("import-dump") => import_dump::run(&args[2..]),
        Some("plugin-import-dump") => plugin_bench::dump(&args[2..]),
        Some("plugin-bench") => plugin_bench::bench(&args[2..]),
        Some("ora-samples") => plugin_bench::samples(&args[2..]),
        Some("ora-make") => plugin_bench::ora_make(&args[2..]),
        Some("raster-dump") => raster_dump::raster(&args[2..]),
        Some("writeback-dump") => raster_dump::writeback(&args[2..]),
        Some("sniff") => {
            // tools/sniff_parity.py: one path per stdin line -> "image" /
            // "opaque" / "declined", the verdict of tmi-import on the bytes
            // under a name with the extension `.bin` (PLUGINS.md P0e)
            use std::io::BufRead;
            use tmi_format::import_file;
use tmi_import::{LayerBlobs, Limits, Options, Warnings};
            let none = LayerBlobs::Only(Default::default());
            let opts = Options {
                render_composite: false,
                cache: None,
                reuse_pixels: None,
                reuse_decoded: None,
                layer_blobs: &none,
                composite_base: None,
                limits: tmi_import::limit::resolve(None, None).unwrap_or(Limits::NONE),
                verify_partial: false,
                progress: None,
            };
            for line in std::io::stdin().lock().lines() {
                let path = line.unwrap();
                let data = std::fs::read(&path).unwrap();
                let digest = tmi_store::oid::sha256_hex(&data);
                let verdict = match import_file("sniffed.bin", &data, &digest, &opts, &mut Warnings::default()) {
                    Ok(f) if f.source_format == "binary" => "opaque",
                    Ok(_) => "image",
                    Err(_) => "declined",
                };
                println!("{verdict}\t{path}");
            }
        }
        _ => {
            eprintln!("usage: tmiki-conform rehash <repo_dir> | large-oids <files...> | floats");
            std::process::exit(2);
        }
    }
}
