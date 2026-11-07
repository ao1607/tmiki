//! `new` -> `build` -> `test` for both templates, reader and writer
//! (PLUGINS.md P8: what the CI workflow runs on three systems). Needs cargo
//! and the wasm32-wasip2 target (as tmi-plugin-host's tests do). Plugins are
//! written in Rust only (Q14): `--lang` of another language is refused.

use std::path::PathBuf;

use tmi_plugin_host::Settings;
use tmi_plugin_tools::check::{self, report::Outcome};
use tmi_plugin_tools::{build, manifest::Manifest, new};

fn temp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("tmi-plugin-templates-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn new_build_test(writer: bool) {
    let tag = format!("rust-{}", if writer { "w" } else { "r" });
    let parent = temp(&tag);
    let id = format!("t-{tag}");
    let settings = Settings { cache_dir: Some(parent.join("cache")), ..Settings::default() };
    let dir = new::run(&parent, &new::Options { id: id.clone(), writer }, &mut |_| {}).unwrap();
    let mut said = Vec::new();
    let built = build::run(&dir, settings.clone(), &mut |l| said.push(l)).unwrap_or_else(|e| panic!("{tag}: {e}"));
    assert_eq!((built.id.as_str(), built.version.as_str()), (id.as_str(), "0.1.0"));
    assert!(said.iter().any(|l| l.starts_with("built target/tmi-plugin/")), "{said:?}");
    let m = Manifest::read(&dir).unwrap();
    // --native: the manifest's [dev] command, the development plugin build made
    let native = tmi_plugin_host::dev::parse_command(m.dev_command.as_deref().expect("[dev] command")).unwrap().in_dir(&dir);
    let o = check::Options { component: m.output, corpus: m.corpus, native: Some(native), against: None, update: true, fuzz: 200, dump: false, settings };
    let r = check::run(&o, &mut |_| {}).unwrap();
    let lines = r.lines().join("\n");
    assert_eq!(r.failures(), 0, "{tag}:\n{lines}");
    assert_eq!(r.warnings(), 0, "{tag}:\n{lines}");
    assert!(r.lines().iter().any(|l| l == "ok    1 native = wasm"), "{tag}:\n{lines}");
    let round_trip = r.entries.iter().any(|e| e.check.starts_with("4 round trip: test patterns") && e.outcome == Outcome::Ok);
    assert_eq!(round_trip, writer, "{tag}:\n{lines}");
    // the expect file it wrote holds on the next run
    let o = check::Options { update: false, fuzz: 0, ..o };
    let r = check::run(&o, &mut |_| {}).unwrap();
    assert!(r.lines().iter().any(|l| l == "ok    7 the records are as in *.expect.json"), "{}", r.lines().join("\n"));
    let _ = std::fs::remove_dir_all(&parent);
}

#[test]
fn rust_templates() {
    new_build_test(false);
    new_build_test(true);
}

#[test]
fn other_languages_are_refused_with_the_reason() {
    assert_eq!(tmi_plugin_tools::check_lang("rust"), Ok(()));
    for l in ["python", "js", "go", "c"] {
        let e = tmi_plugin_tools::check_lang(l).unwrap_err();
        assert!(e.starts_with(&format!("tmi format plugins are written in Rust: there is no {l} template")), "{e}");
    }
}