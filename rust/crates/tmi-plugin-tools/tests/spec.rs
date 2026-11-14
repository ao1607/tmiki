//! PLUGIN_SPEC.md holds tmiki's WIT (1.1) and the OpenRaster plugin of
//! `rust/plugins/ora` (10: every file, verbatim) as they are in the
//! repository, and speaks of no other language than Rust (PLUGINS.md Q14).

use std::path::Path;

fn spec() -> String {
    tmi_plugin_tools::PLUGIN_SPEC.replace("\r\n", "\n")
}

#[test]
fn plugin_spec_holds_the_wit_as_it_is() {
    let wit = tmi_plugin_tools::WIT.replace("\r\n", "\n");
    assert!(spec().contains(&format!("```wit\n{wit}```")), "PLUGIN_SPEC.md's WIT is not tmi-format/wit/tmi-format.wit");
}

#[test]
fn plugin_spec_10_is_the_rust_ora_as_it_is() {
    let spec = spec();
    let ten = &spec[spec.find("## 10. ").unwrap()..spec.find("## 11. ").unwrap()];
    let ora = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/ora");
    let mut files = vec!["Cargo.toml".to_string()];
    let mut walk = vec![ora.join("src")];
    while let Some(dir) = walk.pop() {
        for e in std::fs::read_dir(&dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                walk.push(p);
            } else if p.extension().is_some_and(|x| x == "rs") {
                files.push(p.strip_prefix(&ora).unwrap().to_string_lossy().replace('\\', "/"));
            }
        }
    }
    assert!(files.len() >= 7, "{files:?}");
    for rel in files {
        let mut body = std::fs::read_to_string(ora.join(&rel)).unwrap().replace("\r\n", "\n");
        if !body.ends_with('\n') {
            body.push('\n');
        }
        let lang = if rel.ends_with(".toml") { "toml" } else { "rust" };
        assert!(
            ten.contains(&format!("### `{rel}`")) && ten.contains(&format!("```{lang}\n{body}```")),
            "PLUGIN_SPEC.md 10 does not hold rust/plugins/ora/{rel} as it is"
        );
    }
}

#[test]
fn plugin_spec_speaks_of_rust_only() {
    // (the WIT names CPython's zlib.compress, the kernel tmiki matches)
    let spec = spec().replace(&tmi_plugin_tools::WIT.replace("\r\n", "\n"), "");
    for word in ["componentize", "Python", "python", "TinyGo", "jco", "ComponentizeJS"] {
        assert!(!spec.contains(word), "PLUGIN_SPEC.md mentions {word}");
    }
}
