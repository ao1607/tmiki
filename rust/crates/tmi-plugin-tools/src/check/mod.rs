//! `tmiki plugin test [dir|component.wasm] [--native] [--native-command <cmd>]
//! [--against old.wasm] [--update] [--fuzz N] [--dump]` (PLUGINS.md 7.1,
//! Q12). Every check
//! runs over every corpus file; every failure is listed; the exit code is 1
//! when any check failed.
//!
//! 1. native = wasm (`--native`): [`compare_readers`] (structure with the
//!    deferred layers read, the record and the warnings, in canonical CBOR)
//!    of the component and the same plugin as a development plugin, an
//!    ordinary program speaking `tmiki plugin dev`'s RPC (PLUGINS.md 7.1:
//!    `tmi_plugin_sdk::dev_main`).
//! 2. determinism: a second import with fresh instances; the deferred
//!    layers read last to first, one at a time; one instance for every call
//!    (`read-structure` twice, then the deferred layers); SQLite with
//!    `reverse_unordered_selects`; a quarter of the wasm stack; a quarter of
//!    the fuel.
//! 3. the interface's rules: the host's checks (PLUGINS.md 3.4), every
//!    recipe decoded, warning codes, an embedded composite of the canvas'
//!    size, `probe` saying `yes` to every corpus file and `no` to random
//!    bytes.
//! 4. the writer's round trip ([`roundtrip`]).
//! 5. `--fuzz N` ([`fuzz`]).
//! 6. `--against old.wasm`: what changes in the records from the older
//!    component (layers whose `pixel_sha256` changed first).
//! 7. `<file>.expect.json` beside each corpus file (`--update` writes them).

pub mod fuzz;
pub mod readers;
pub mod report;
pub mod roundtrip;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tmi_format::pipeline::Importer;
use tmi_format::reader::FormatReader;
use tmi_format::types::*;
use tmi_plugin_host::{Plugin, PluginHost, Settings, WasmReader};
use tmi_store::json::{self, Value};

use crate::cbor::{self, V};
use crate::Print;
use readers::LIMITS;
use report::Report;

pub struct Options {
    /// The component.
    pub component: PathBuf,
    /// The sample files (every file below it but `*.expect.json`).
    pub corpus: PathBuf,
    /// `--native [cmd]`: the same plugin as a program speaking the dev RPC
    /// (`tmiki plugin dev`'s), compared with the component.
    pub native: Option<tmi_plugin_host::DevCommand>,
    /// `--against old.wasm`.
    pub against: Option<PathBuf>,
    /// `--update`: write the `*.expect.json` files.
    pub update: bool,
    /// `--fuzz N`.
    pub fuzz: u64,
    /// `--dump`: each file's structure, one line per layer.
    pub dump: bool,
    /// The host's settings (the cache directory; tests use their own).
    pub settings: Settings,
}

pub const EXPECT_SUFFIX: &str = ".expect.json";

/// The corpus: every file below `dir` (sorted, by its path relative to
/// `dir` with `/`), but the `*.expect.json` files.
pub fn corpus(dir: &Path) -> Result<Vec<(String, PathBuf)>, String> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) -> std::io::Result<()> {
        for e in std::fs::read_dir(dir)? {
            let e = e?;
            let p = e.path();
            let ft = e.file_type()?;
            if ft.is_dir() {
                walk(base, &p, out)?;
            } else if ft.is_file() {
                // a link (to ~/.ssh/id_ed25519, say) is not read: the plugin
                // sees what it reads and --update writes it beside the link
                let rel = p.strip_prefix(base).unwrap_or(&p).to_string_lossy().replace('\\', "/");
                if !rel.ends_with(EXPECT_SUFFIX) {
                    out.push((rel, p));
                }
            }
        }
        Ok(())
    }
    if std::fs::symlink_metadata(dir).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(format!("{}: the corpus is a link to somewhere else; give the directory itself", dir.display()));
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out).map_err(|e| format!("{}: {e}", dir.display()))?;
    out.sort();
    Ok(out)
}

fn load(settings: Settings, bytes: &[u8]) -> Result<Arc<Plugin>, String> {
    let host = PluginHost::new(settings).map_err(|e| e.to_string())?;
    host.load(bytes).map(Arc::new).map_err(|e| e.to_string())
}

fn same(what: &str, a: &V, b: &V) -> Result<(), String> {
    match cbor::first_difference(a, b) {
        None => Ok(()),
        Some(d) => Err(format!("{what} differ at {d}")),
    }
}

/// The base name of a corpus file (what the plugin sees as the file's name).
fn base_name(rel: &str) -> &str {
    rel.rsplit('/').next().unwrap_or(rel)
}

/// PLUGINS.md 7.1 item 1: `a` (the component) and `b` (the same plugin
/// built natively) give the same structure (deferred layers read), the same
/// record and the same warnings for each file, compared in canonical CBOR.
/// `importer` is used for both, so the records name the same component.
pub fn compare_readers(report: &mut Report, a: &dyn FormatReader, b: &dyn FormatReader, importer: &Importer, files: &[(String, Vec<u8>)]) {
    const CHECK: &str = "1 native = wasm";
    if a.info() != b.info() {
        report.fail(CHECK, None, format!("info differs: {:?} vs {:?}", a.info(), b.info()));
    }
    for (rel, data) in files {
        let name = base_name(rel);
        let sa = readers::resolved_structure(a, name, data).map(|s| crate::values::structure(&s)).map_err(V::text);
        let sb = readers::resolved_structure(b, name, data).map(|s| crate::values::structure(&s)).map_err(V::text);
        let ia = readers::import(a, importer, name, data, LIMITS).map(|i| i.value()).map_err(V::text);
        let ib = readers::import(b, importer, name, data, LIMITS).map(|i| i.value()).map_err(V::text);
        let pack = |s: Result<V, V>, i: Result<V, V>| {
            let f = |r: Result<V, V>| match r {
                Ok(v) => V::case("ok", v),
                Err(e) => V::case("error", e),
            };
            cbor::map([("structure", f(s)), ("import", f(i))])
        };
        let r = same("the wasm and native answers", &pack(sa, ia), &pack(sb, ib));
        report.check(CHECK, Some(rel), r);
    }
}

fn record_value(i: &readers::Imported) -> V {
    i.value()
}

/// The `*.expect.json` text of a record: the record as `tmi-conform
/// import-dump` shapes it, without `importer` (its component hash changes
/// with every build), and the warnings; canonical JSON, one key per line.
fn expect_text(i: &readers::Imported) -> String {
    let mut v = tmi_plugin_host::dump::value(&i.file);
    if let Value::Object(m) = &mut v {
        m.shift_remove("importer");
    }
    let mut top = json::Map::new();
    top.insert("record".into(), v);
    top.insert("warnings".into(), Value::Array(i.warnings.iter().map(|w| Value::Str(w.clone())).collect()));
    let opts = json::DumpOptions { indent: Some(1), ..json::DumpOptions::CANONICAL };
    json::dumps(&Value::Object(top), &opts) + "\n"
}

fn warning_code_ok(code: &str) -> bool {
    (1..=40).contains(&code.len()) && code.bytes().next().is_some_and(|b| b.is_ascii_lowercase()) && code.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// The embedded composite's size, when the structure has one.
fn composite_size(reader: &dyn FormatReader, data: &[u8], s: &Structure) -> Option<Result<(usize, usize), String>> {
    let Composite::Image(b) = &s.composite else { return None };
    let cx = tmi_format::reader::ReadCx::new(None);
    let env = tmi_format::recipe::DecodeEnv { source: data, reader: Some(reader), cx: &cx };
    Some(tmi_format::recipe::decode_bitmap(&env, b).map(|d| (d.width, d.height)).map_err(|e| e.to_string()))
}

/// Random byte strings `probe` must say `no` to (fixed seeds).
fn random_heads(probe_bytes: usize) -> Vec<Vec<u8>> {
    let mut out = vec![Vec::new(), vec![0u8; probe_bytes.max(1)], vec![0xffu8; probe_bytes.max(1)]];
    let mut x: u64 = 0x2545_F491_4F6C_DD1D;
    for i in 0..16 {
        let n = (probe_bytes.max(16) * (i + 1)) / 16;
        out.push(
            (0..n)
                .map(|_| {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    x as u8
                })
                .collect(),
        );
    }
    out
}

/// Runs every check; the report (the CLI prints its lines; exit 1 when it
/// has a failure).
pub fn run(o: &Options, print: Print) -> Result<Report, String> {
    let mut report = Report::default();
    let bytes = std::fs::read(&o.component).map_err(|e| format!("{}: {e} (tmiki plugin build makes it)", o.component.display()))?;
    let plugin = match load(o.settings.clone(), &bytes) {
        Ok(p) => p,
        Err(e) => {
            report.fail("0 load", None, format!("{}: {e}", o.component.display()));
            return Ok(report);
        }
    };
    let importer = plugin.importer();
    let info = plugin.info.clone();
    print(format!(
        "plugin {} {} ({}, sha256 {}..., {})",
        info.id,
        info.version,
        if plugin.can_write() { "reads and writes" } else { "reads" },
        &importer.component_sha256[..12],
        importer.abi
    ));
    let wasm = WasmReader::new(plugin.clone());
    let listed = corpus(&o.corpus)?;
    if listed.is_empty() {
        report.fail("0 corpus", None, format!("{} holds no sample file", o.corpus.display()));
        return Ok(report);
    }
    let mut files = Vec::new();
    for (rel, path) in &listed {
        files.push((rel.clone(), std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?));
    }
    print(format!("corpus: {} in {}", plural(files.len(), "file"), shown(&o.corpus)));

    // 1: the dev plugin answers as the component does (one importer for both:
    // the records then name the same component)
    match &o.native {
        None => report.skip("1 native = wasm", None, "no --native"),
        Some(cmd) => {
            let opts = tmi_plugin_host::DevOptions { timeout: o.settings.timeout.or(Some(tmi_plugin_host::dev::DEFAULT_TIMEOUT)), ..Default::default() };
            match tmi_plugin_host::dev::open(cmd.clone(), opts) {
                Ok(native) => {
                    print(format!("native: {} (tmiki plugin dev)", cmd.display()));
                    compare_readers(&mut report, &wasm, &native, &importer, &files);
                }
                Err(e) => report.fail("1 native = wasm", None, format!("{}: {e}", cmd.display())),
            }
        }
    }

    // the variants of 2 (each its own host)
    let defaults = o.settings.clone();
    let variant = |f: &dyn Fn(&mut Settings)| {
        let mut s = defaults.clone();
        f(&mut s);
        load(s, &bytes)
    };
    let sqlite = variant(&|s| s.sqlite_reverse_unordered_selects = true);
    let stack = variant(&|s| s.max_wasm_stack /= 4);
    let fuel = variant(&|s| s.fuel = s.fuel.map(|f| f / 4));

    for (rel, data) in &files {
        let name = base_name(rel);
        let file = Some(rel.as_str());
        let first = match readers::import(&wasm, &importer, name, data, LIMITS) {
            Ok(i) => {
                report.ok("3 the record: every recipe decoded, the host's checks (PLUGINS.md 3.4)", file);
                i
            }
            Err(e) => {
                report.fail("3 the record: every recipe decoded, the host's checks (PLUGINS.md 3.4)", file, e);
                continue;
            }
        };
        let base = record_value(&first);
        let structure = match readers::resolved_structure(&wasm, name, data) {
            Ok(s) => s,
            Err(e) => {
                report.fail("2 determinism: read-structure", file, e);
                continue;
            }
        };
        if o.dump {
            report.notes.push(format!("{rel}:"));
            report.notes.extend(crate::values::describe(&structure).into_iter().map(|l| format!("  {l}")));
        }

        // 2
        let again = readers::import(&wasm, &importer, name, data, LIMITS).map(|i| record_value(&i));
        report.check("2 determinism: a second import, fresh instances", file, again.and_then(|v| same("the records", &base, &v)));
        let raw = wasm.read_structure(&tmi_format::reader::Source::new(name, data), &tmi_format::reader::ReadCx::new(None));
        if readers::defers(raw.as_ref().unwrap_or(&structure)) {
            let reversed = readers::Reversed { inner: &wasm };
            let r = readers::import(&reversed, &importer, name, data, LIMITS).map(|i| record_value(&i));
            report.check("2 determinism: deferred layers last to first, one at a time", file, r.and_then(|v| same("the records", &base, &v)));
        } else {
            report.skip("2 determinism: deferred layers last to first, one at a time", file, "no deferred layer");
        }
        let reuse = readers::one_instance(&plugin, name, data).and_then(|(first_call, resolved)| {
            let fresh = crate::values::structure(&structure);
            same("read-structure in one instance and a fresh one", &crate::values::structure(&resolved), &fresh)?;
            let raw_first = raw.as_ref().map(crate::values::structure).map_err(|e| e.to_string())?;
            same("the first and second read-structure of one instance", &crate::values::structure(&first_call), &raw_first)
        });
        report.check("2 determinism: one instance for every call", file, reuse);
        for (check, host) in [
            ("2 determinism: SQLite with reverse_unordered_selects", &sqlite),
            ("2 determinism: a quarter of the wasm stack", &stack),
            ("2 determinism: a quarter of the fuel", &fuel),
        ] {
            let r = match host {
                Ok(p) => readers::import(&WasmReader::new(p.clone()), &importer, name, data, LIMITS).and_then(|i| same("the records", &base, &record_value(&i))),
                Err(e) => Err(format!("the component does not load so: {e}")),
            };
            report.check(check, file, r);
        }

        // 3
        let dups = readers::duplicate_ids(&structure);
        report.check("3 stable ids unique", file, if dups.is_empty() { Ok(()) } else { Err(format!("repeated: {}", dups.join(", "))) });
        let bad: Vec<String> = structure.warnings.iter().filter(|w| !warning_code_ok(&w.code)).map(|w| format!("{:?}", w.code)).collect();
        report.check(
            "3 warning codes ([a-z][a-z0-9-]*, at most 40)",
            file,
            if bad.is_empty() { Ok(()) } else { Err(format!("codes {}", bad.join(", "))) },
        );
        match composite_size(&wasm, data, &structure) {
            None => {}
            Some(Ok((w, h))) if (w as u64, h as u64) == (u64::from(structure.canvas.width), u64::from(structure.canvas.height)) => {
                report.ok("3 the embedded composite is the canvas' size", file)
            }
            Some(Ok((w, h))) => report.fail(
                "3 the embedded composite is the canvas' size",
                file,
                format!("{w}x{h} on a {}x{} canvas (tmi draws generic-v1 instead)", structure.canvas.width, structure.canvas.height),
            ),
            Some(Err(e)) => report.fail("3 the embedded composite is the canvas' size", file, format!("it does not decode: {e}")),
        }
        let head = &data[..data.len().min(info.probe_bytes as usize)];
        match plugin.probe(name, head, data.len() as u64) {
            Ok(Confidence::Yes) => report.ok("3 probe says yes to the corpus", file),
            Ok(Confidence::Maybe) => report.warn("3 probe says yes to the corpus", file, "maybe (a corpus file should be a yes)"),
            Ok(Confidence::No) => report.fail("3 probe says yes to the corpus", file, "no"),
            Err(e) => report.fail("3 probe says yes to the corpus", file, e.to_string()),
        }

        // 4
        if plugin.can_write() {
            roundtrip::run(&mut report, &wasm, &importer, name, data, &first);
        }

        // 7
        let expect_path = listed.iter().find(|(r, _)| r == rel).map(|(_, p)| PathBuf::from(format!("{}{EXPECT_SUFFIX}", p.display()))).expect("listed");
        let text = expect_text(&first);
        if o.update {
            std::fs::write(&expect_path, &text).map_err(|e| format!("{}: {e}", expect_path.display()))?;
            report.notes.push(format!("wrote {}", shown(&expect_path)));
        } else {
            match std::fs::read_to_string(&expect_path) {
                Ok(old) if old.replace("\r\n", "\n") == text => report.ok("7 the records are as in *.expect.json", file),
                Ok(old) => {
                    let why = match json::loads(&old) {
                        Ok(v) => cbor::first_difference(&cbor::from_json(&v), &cbor::from_json(&json::loads(&text).expect("our own JSON"))).unwrap_or_else(|| "the text".into()),
                        Err(_) => "it is not JSON".into(),
                    };
                    report.fail("7 the records are as in *.expect.json", file, format!("differs at {why} (tmiki plugin test --update writes the new records)"));
                }
                Err(_) => report.skip("7 the records are as in *.expect.json", file, "no .expect.json (tmiki plugin test --update writes it)"),
            }
        }
    }
    if !plugin.can_write() {
        report.skip("4 round trip: the same pixels", None, "the plugin has no writer");
    }

    // 3: random bytes
    let mut said: Vec<String> = Vec::new();
    for (i, head) in random_heads(info.probe_bytes as usize).iter().enumerate() {
        match plugin.probe("random.bin", head, head.len() as u64) {
            Ok(Confidence::No) => {}
            Ok(c) => said.push(format!("{c:?} to random bytes #{i} ({} bytes)", head.len()).to_lowercase()),
            Err(e) => said.push(format!("random bytes #{i}: {e}")),
        }
    }
    report.check("3 probe says no to random bytes", None, if said.is_empty() { Ok(()) } else { Err(said.join("; ")) });

    // 5
    if o.fuzz > 0 {
        let pairs: Vec<(String, Vec<u8>)> = files.iter().map(|(r, d)| (base_name(r).to_string(), d.clone())).collect();
        fuzz::run(&mut report, &wasm, &importer, &pairs, o.fuzz);
    }

    // 6
    if let Some(old_path) = &o.against {
        against(&mut report, &wasm, &importer, old_path, &o.settings, &files)?;
    }
    Ok(report)
}

/// PLUGINS.md 7.1 item 6: the records of `old` next to the new ones.
fn against(report: &mut Report, new: &WasmReader, importer: &Importer, old_path: &Path, settings: &Settings, files: &[(String, Vec<u8>)]) -> Result<(), String> {
    let old_bytes = std::fs::read(old_path).map_err(|e| format!("{}: {e}", old_path.display()))?;
    let old = load(settings.clone(), &old_bytes).map_err(|e| format!("{}: {e}", old_path.display()))?;
    let old_importer = old.importer();
    let old_reader = WasmReader::new(old.clone());
    report.notes.push(format!("against {} {} ({}):", old.info.id, old.info.version, old_path.display()));
    let mut changed_files = 0;
    for (rel, data) in files {
        let name = base_name(rel);
        let n = readers::import(new, importer, name, data, LIMITS);
        let o = readers::import(&old_reader, &old_importer, name, data, LIMITS);
        let (n, o) = match (n, o) {
            (Ok(n), Ok(o)) => (n, o),
            (Err(e), Ok(_)) => {
                report.notes.push(format!("  {rel}: the old version reads it, the new one does not: {e}"));
                changed_files += 1;
                continue;
            }
            (Ok(_), Err(e)) => {
                report.notes.push(format!("  {rel}: the new version reads it, the old one did not: {e}"));
                changed_files += 1;
                continue;
            }
            (Err(_), Err(_)) => continue,
        };
        let mut lines = Vec::new();
        let (mut a, mut b) = (Vec::new(), Vec::new());
        flatten(&o.file.layers, &mut a);
        flatten(&n.file.layers, &mut b);
        for lb in &b {
            match a.iter().find(|la| la.stable_source_id == lb.stable_source_id) {
                None => lines.push(format!("    + layer {:?} ({})", lb.name, lb.stable_source_id)),
                Some(la) if la.pixel_sha256.as_ref().map(sha_text) != lb.pixel_sha256.as_ref().map(sha_text) => {
                    lines.insert(0, format!("    ! pixels changed: layer {:?} ({})", lb.name, lb.stable_source_id))
                }
                Some(_) => {}
            }
        }
        for la in &a {
            if !b.iter().any(|lb| lb.stable_source_id == la.stable_source_id) {
                lines.push(format!("    - layer {:?} ({})", la.name, la.stable_source_id));
            }
        }
        let strip = |v: &V| match v {
            V::Map(m) => V::Map(
                m.iter()
                    .map(|(k, v)| match (k, v) {
                        (V::Text(t), V::Map(r)) if t == "record" => (k.clone(), V::Map(r.iter().filter(|(k, _)| *k != V::text("importer")).cloned().collect())),
                        _ => (k.clone(), v.clone()),
                    })
                    .collect(),
            ),
            other => other.clone(),
        };
        if let Some(d) = cbor::first_difference(&strip(&o.value()), &strip(&n.value())) {
            if lines.is_empty() {
                lines.push(format!("    first difference: {d}"));
            } else {
                lines.push(format!("    first difference in the record: {d}"));
            }
        }
        if !lines.is_empty() {
            changed_files += 1;
            report.notes.push(format!("  {rel}:"));
            report.notes.extend(lines);
        }
    }
    if changed_files == 0 {
        report.notes.push("  the same records for every file".into());
    }
    report.ok("6 against an older component", None);
    Ok(())
}

/// A path as the user gave it, with `/` (no leading `./`).
pub fn shown(p: &Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    s.strip_prefix("./").map(str::to_string).unwrap_or(s)
}

fn plural(n: usize, what: &str) -> String {
    if n == 1 {
        format!("1 {what}")
    } else {
        format!("{n} {what}s")
    }
}

fn sha_text(v: &Value) -> String {
    json::dumps(v, &json::DumpOptions::CANONICAL)
}

fn flatten<'a>(layers: &'a [tmi_import::ImportedLayer], out: &mut Vec<&'a tmi_import::ImportedLayer>) {
    for l in layers {
        out.push(l);
        flatten(&l.children, out);
    }
}

#[cfg(test)]
mod link_tests {
    fn link(target: &std::path::Path, at: &std::path::Path, dir: bool) -> std::io::Result<()> {
        #[cfg(windows)]
        return if dir { std::os::windows::fs::symlink_dir(target, at) } else { std::os::windows::fs::symlink_file(target, at) };
        #[cfg(unix)]
        {
            let _ = dir;
            std::os::unix::fs::symlink(target, at)
        }
    }

    #[test]
    fn a_corpus_link_to_a_private_file_is_not_read() {
        let base = std::env::temp_dir().join(format!("tmi-corpus-links-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let corpus = base.join("corpus");
        std::fs::create_dir_all(&corpus).unwrap();
        let secret = base.join("id_ed25519");
        std::fs::write(&secret, b"PRIVATE KEY").unwrap();
        std::fs::write(corpus.join("a.x"), b"sample").unwrap();
        if link(&secret, &corpus.join("b.x"), false).is_err() {
            eprintln!("skipped: this account cannot make symbolic links");
            let _ = std::fs::remove_dir_all(&base);
            return;
        }
        let files: Vec<String> = super::corpus(&corpus).unwrap().into_iter().map(|(rel, _)| rel).collect();
        assert_eq!(files, ["a.x"]);
        // nor a corpus that is itself a link
        link(&corpus, &base.join("linked"), true).unwrap();
        assert!(super::corpus(&base.join("linked")).unwrap_err().contains("link"));
        let _ = std::fs::remove_dir_all(&base);
    }
}
