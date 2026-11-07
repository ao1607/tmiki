//! `tmiki plugin test`'s checks against the test plugins of `rust/plugins`
//! (built for wasm32-wasip2 by tmi-plugin-host's test helper): the ORA
//! plugin passes them all, its native build compares equal to its
//! component, and plugins that break a rule fail the check that names it.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tmi_format::reader::FormatReader;
use tmi_plugin_host::native_ora::OraNative;
use tmi_plugin_host::ora_files::{self, Merged, Sample};
use tmi_plugin_host::{test_plugins, DevCommand, PluginHost, Settings, WasmReader};
use tmi_plugin_tools::check::{self, report::Outcome, report::Report};

fn temp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("tmi-plugin-tools-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn settings(tag: &str) -> Settings {
    Settings { cache_dir: Some(temp(&format!("cache-{tag}"))), ..Settings::default() }
}

fn component(dir: &Path, crate_name: &str) -> PathBuf {
    let p = dir.join(format!("{crate_name}.wasm"));
    std::fs::write(&p, test_plugins::component(crate_name)).unwrap();
    p
}

fn options(component: PathBuf, corpus: PathBuf, tag: &str) -> check::Options {
    check::Options { component, corpus, native: None, against: None, update: false, fuzz: 0, dump: false, settings: settings(tag) }
}

fn run(o: &check::Options) -> Report {
    check::run(o, &mut |_| {}).unwrap()
}

fn failures(r: &Report) -> Vec<String> {
    r.entries
        .iter()
        .filter_map(|e| match &e.outcome {
            Outcome::Fail(m) => Some(format!("{}: {:?}: {m}", e.check, e.file)),
            _ => None,
        })
        .collect()
}

fn ora_corpus(dir: &Path) {
    for (name, s) in [
        ("stored.ora", Sample { deflate: false, merged: Merged::None, defer: false }),
        ("deflate-merged.ora", Sample { deflate: true, merged: Merged::Canvas, defer: false }),
        ("deferred.ora", Sample { deflate: true, merged: Merged::None, defer: true }),
    ] {
        std::fs::write(dir.join(name), ora_files::sample(s)).unwrap();
    }
    std::fs::create_dir_all(dir.join("more")).unwrap();
    std::fs::write(dir.join("more/many.ora"), ora_files::many_layers(20, 8, true, true)).unwrap();
}

#[test]
fn the_ora_plugin_passes_every_check() {
    let dir = temp("ora");
    let corpus = dir.join("corpus");
    std::fs::create_dir_all(&corpus).unwrap();
    ora_corpus(&corpus);
    let wasm = component(&dir, "tmi-plugin-ora");
    let mut o = options(wasm.clone(), corpus.clone(), "ora");
    o.update = true;
    o.fuzz = 300;
    o.dump = true;
    o.against = Some(wasm);
    let r = run(&o);
    assert_eq!(failures(&r), Vec::<String>::new(), "{}", r.lines().join("\n"));
    let lines = r.lines().join("\n");
    for want in [
        "ok    2 determinism: deferred layers last to first, one at a time (2 files)",
        "ok    2 determinism: one instance for every call (4 files)",
        "ok    4 round trip: test patterns (4 files)",
        "ok    5 fuzz",
        "the same records for every file",
        "layer ora:0  raster",
    ] {
        assert!(lines.contains(want), "{want}:\n{lines}");
    }
    assert!(corpus.join("more/many.ora.expect.json").is_file());
    // the expect files hold: a second run compares them
    o.update = false;
    o.fuzz = 0;
    o.against = None;
    let r = run(&o);
    assert_eq!(failures(&r), Vec::<String>::new());
    assert!(r.lines().iter().any(|l| l == "ok    7 the records are as in *.expect.json (4 files)"), "{}", r.lines().join("\n"));
    // a changed record fails 7, with where
    let p = corpus.join("stored.ora.expect.json");
    let text = std::fs::read_to_string(&p).unwrap().replace("\"背景\"", "\"other\"");
    std::fs::write(&p, text).unwrap();
    let r = run(&o);
    let f = failures(&r);
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(f[0].contains("name: \"other\" vs \"背景\""), "{f:?}");
}

#[test]
fn the_native_ora_compares_equal_to_its_component_and_a_difference_is_found() {
    let host = PluginHost::new(settings("native")).unwrap();
    let plugin = Arc::new(host.load(&test_plugins::component("tmi-plugin-ora")).unwrap());
    let wasm = WasmReader::new(plugin.clone());
    let native = OraNative::default();
    let importer = plugin.importer();
    let files: Vec<(String, Vec<u8>)> = vec![
        ("a.ora".into(), ora_files::sample(Sample { deflate: true, merged: Merged::Canvas, defer: true })),
        ("b.ora".into(), ora_files::many_layers(30, 8, false, true)),
        ("cut.ora".into(), ora_files::sample(Sample { deflate: false, merged: Merged::None, defer: false })[..300].to_vec()),
    ];
    let mut r = Report::default();
    check::compare_readers(&mut r, &wasm, &native, &importer, &files);
    assert_eq!(failures(&r), Vec::<String>::new());
    assert_eq!(r.entries.len(), 3);

    // a native build that names one layer differently
    struct Renamed(OraNative);
    impl FormatReader for Renamed {
        fn info(&self) -> &tmi_format::types::PluginInfo {
            self.0.info()
        }
        fn probe(&self, n: &str, h: &[u8], s: u64) -> tmi_format::types::Confidence {
            self.0.probe(n, h, s)
        }
        fn read_structure(&self, src: &tmi_format::reader::Source, cx: &tmi_format::reader::ReadCx) -> Result<tmi_format::types::Structure, tmi_format::types::ReadError> {
            let mut s = self.0.read_structure(src, cx)?;
            if let Some(l) = s.layers.last_mut() {
                l.name.push('!');
            }
            Ok(s)
        }
        fn read_layer(&self, src: &tmi_format::reader::Source, c: &[u8], t: u32, cx: &tmi_format::reader::ReadCx) -> Result<tmi_format::types::Bitmap, tmi_format::types::ReadError> {
            self.0.read_layer(src, c, t, cx)
        }
    }
    let mut r = Report::default();
    check::compare_readers(&mut r, &wasm, &Renamed(OraNative::default()), &importer, &files[..1]);
    let f = failures(&r);
    assert_eq!(f.len(), 1);
    assert!(f[0].contains(".name:") && f[0].contains("!\""), "{f:?}");
}

#[test]
fn rule_breakers_fail_the_checks_that_name_their_rule() {
    let dir = temp("bad");
    let corpus = dir.join("corpus");
    std::fs::create_dir_all(&corpus).unwrap();
    for mode in ["unordered", "stateful", "panic"] {
        std::fs::write(corpus.join(format!("{mode}.bad")), format!("BAD:{mode}\n")).unwrap();
    }
    let r = run(&options(component(&dir, "tmi-plugin-bad"), corpus, "bad"));
    let f = failures(&r);
    let has = |check: &str, file: &str| f.iter().any(|l| l.starts_with(check) && l.contains(file));
    assert!(has("2 determinism: SQLite with reverse_unordered_selects", "unordered.bad"), "{f:#?}");
    assert!(has("2 determinism: one instance for every call", "stateful.bad"), "{f:#?}");
    assert!(has("3 the record", "panic.bad") && f.iter().any(|l| l.contains("deliberate panic")), "{f:#?}");
    assert!(!has("2 determinism: SQLite", "stateful.bad"), "{f:#?}");
    assert!(!has("2 determinism: one instance", "unordered.bad"), "{f:#?}");

    // a writer that also writes another layer fails the round trip
    let dir = temp("fault");
    let corpus = dir.join("corpus");
    std::fs::create_dir_all(&corpus).unwrap();
    let s = Sample { deflate: true, merged: Merged::None, defer: false };
    std::fs::write(corpus.join("other-layer.ora"), ora_files::sample_with(s, r#" tmi-test-write-fault="other-layer""#)).unwrap();
    let r = run(&options(component(&dir, "tmi-plugin-ora"), corpus, "fault"));
    let f = failures(&r);
    assert!(f.iter().any(|l| l.starts_with("4 round trip") && l.contains("changed although it was not edited")), "{f:#?}");

    // no corpus, a component that is not one
    let dir = temp("empty");
    let corpus = dir.join("corpus");
    std::fs::create_dir_all(&corpus).unwrap();
    let r = run(&options(component(&dir, "tmi-plugin-ora"), corpus.clone(), "empty"));
    assert!(failures(&r)[0].contains("holds no sample file"));
    std::fs::write(dir.join("junk.wasm"), b"\0asm\x0d\0\x01\0").unwrap();
    let r = run(&options(dir.join("junk.wasm"), corpus, "empty"));
    assert!(failures(&r)[0].starts_with("0 load"));
}

#[test]
fn native_compares_the_development_plugin_with_the_component() {
    let dir = temp("native-cmd");
    let corpus = dir.join("corpus");
    std::fs::create_dir_all(&corpus).unwrap();
    ora_corpus(&corpus);
    let mut o = options(component(&dir, "tmi-plugin-ora"), corpus, "native-cmd");
    // ora-dev: the same ORA code on tmi-plugin-sdk, as a development plugin
    o.native = Some(DevCommand::new(test_plugins::ora_dev(), Vec::<String>::new()));
    let r = run(&o);
    assert_eq!(failures(&r), Vec::<String>::new(), "{}", r.lines().join("\n"));
    assert!(r.lines().iter().any(|l| l == "ok    1 native = wasm (4 files)"), "{}", r.lines().join("\n"));
    // a program that is not there
    o.native = Some(DevCommand::new("tmi-no-such-program-xyz", Vec::<String>::new()));
    let f = failures(&run(&o));
    assert!(f.len() == 1 && f[0].starts_with("1 native = wasm") && f[0].contains("tmi-no-such-program-xyz"), "{f:?}");
}