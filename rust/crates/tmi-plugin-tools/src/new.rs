//! `tmiki plugin new <id> [--lang rust] [--writer]` (PLUGINS.md 7.1): a
//! plugin's directory. Plugins are written in Rust (Q14).
//!
//! ```text
//! <id>/
//!   wit/tmi-format.wit     tmi's interface, as this tmi speaks it
//!   Cargo.toml, src/lib.rs the template on tmi-plugin-sdk: a reader of the
//!                          made-up "tmi sample" format, and with --writer its
//!                          writer; src/bin/dev.rs, its development plugin
//!   tests/corpus/          sample.<id without dashes>, which the template reads
//!   SPEC.md                format notes (the sample format's, to replace)
//!   AGENTS.md              how to work here, for an AI (PLUGINS.md 7.2)
//!   PLUGIN_SPEC.md         the interface's specification
//!   tmi-plugin.toml        how build and test treat the directory
//!   sdk/                   tmi-plugin-sdk and tmi-plugin-dev, copied
//! ```
//!
//! The template builds and passes `tmiki plugin test` (`--native` too) as it
//! is. It is written on the SDK, so that the same code is the
//! component and a development plugin (`tmiki plugin dev`). The directory is
//! outside tmi's repository, so the SDK is not a dependency on a path into
//! it nor on a registry: `new` writes the SDK's sources of this tmi into the
//! directory ([`sdk_files`]), the versions that speak this tmi's RPC and
//! WIT. A plugin made with an older tmi keeps its copy; `tmiki plugin new` of
//! a newer one writes the new copy to take over.

use std::path::{Path, PathBuf};

use crate::template::{render, Vars};
use crate::Print;

const RUST_CARGO: &str = include_str!("../templates/rust/Cargo.toml.in");
const RUST_LIB: &str = include_str!("../templates/rust/lib.rs.in");
const RUST_DEV: &str = include_str!("../templates/rust/dev.rs.in");
const MANIFEST: &str = include_str!("../templates/tmi-plugin.toml.in");
const SPEC: &str = include_str!("../templates/SPEC.md.in");
const AGENTS: &str = include_str!("../templates/AGENTS.md.in");

pub struct Options {
    pub id: String,
    pub writer: bool,
}

/// The template's extension: the id without its dashes, at most 16
/// characters, as a plugin's extensions must be (`[a-z0-9]{1,16}`).
fn sample_ext(id: &str) -> String {
    id.chars().filter(|c| *c != '-').take(16).collect()
}

fn vars(o: &Options) -> Vars {
    let world = if o.writer { "format-plugin" } else { "format-reader" };
    let format_name = format!("{} sample", o.id);
    let values = [
        ("id", o.id.clone()),
        ("ext", sample_ext(&o.id)),
        ("world", world.to_string()),
        ("abi", crate::wit_version().to_string()),
        ("format_name", format_name),
        ("can_write", o.writer.to_string()),
        ("write_capability", if o.writer { "t::WriteCapability::Pixels" } else { "t::WriteCapability::None" }.to_string()),
        ("writer_flag", if o.writer { " --writer" } else { "" }.to_string()),
        ("writer_note", if o.writer { "。writer は編集したレイヤーの PNG を作り直す" } else { "" }.to_string()),
        ("dev_example", format!("target/debug/{}-dev", o.id)),
        ("crate_name", format!("tmi_plugin_{}", o.id.replace('-', "_"))),
        ("output", format!("target/tmi-plugin/{}.wasm", o.id)),
    ];
    let flags = [("writer", o.writer)];
    Vars { values: values.into_iter().collect(), flags: flags.into_iter().collect() }
}

/// The files of the plugin directory (relative path, bytes).
pub fn files(o: &Options) -> Vec<(String, Vec<u8>)> {
    let v = vars(o);
    let mut out: Vec<(String, Vec<u8>)> = vec![
        ("wit/tmi-format.wit".into(), crate::WIT.as_bytes().to_vec()),
        ("PLUGIN_SPEC.md".into(), crate::PLUGIN_SPEC.as_bytes().to_vec()),
        ("SPEC.md".into(), render(SPEC, &v).into_bytes()),
        ("AGENTS.md".into(), render(AGENTS, &v).into_bytes()),
        ("tmi-plugin.toml".into(), render(MANIFEST, &v).into_bytes()),
        (format!("tests/corpus/sample.{}", sample_ext(&o.id)), crate::sample::corpus_file()),
    ];
    out.push(("Cargo.toml".into(), render(RUST_CARGO, &v).into_bytes()));
    out.push(("src/lib.rs".into(), render(RUST_LIB, &v).into_bytes()));
    out.push(("src/bin/dev.rs".into(), render(RUST_DEV, &v).into_bytes()));
    out.push((".gitignore".into(), b"/target/\nCargo.lock\n".to_vec()));
    out.extend(sdk_files());
    // the same bytes on every system (a checkout may have turned the
    // templates' line ends into CRLF)
    for (rel, bytes) in out.iter_mut() {
        if !rel.starts_with("tests/") && !rel.ends_with(".cbor") {
            *bytes = String::from_utf8_lossy(bytes).replace("\r\n", "\n").into_bytes();
        }
    }
    out
}

/// The SDK the template is written on, as this tmi has it:
/// `rust/plugins/sdk` and `rust/crates/tmi-plugin-dev` under `sdk/` (their
/// paths to each other and to the WIT made the directory's own).
pub fn sdk_files() -> Vec<(String, Vec<u8>)> {
    let header = format!("# written by tmiki plugin new (tmi {}): tmi's own copy, do not edit\n", env!("CARGO_PKG_VERSION"));
    let mut out: Vec<(String, Vec<u8>)> = Vec::new();
    let mut text = |rel: &str, body: &str, rewrites: &[(&str, &str)]| {
        let mut s = body.replace("\r\n", "\n");
        for (from, to) in rewrites {
            assert!(s.contains(from), "{rel}: {from:?} is not in the SDK's source");
            s = s.replace(from, to);
        }
        if rel.ends_with("Cargo.toml") {
            s = format!("{header}{s}");
        }
        out.push((rel.to_string(), s.into_bytes()));
    };
    text("sdk/tmi-plugin-sdk/Cargo.toml", SDK_CARGO, &[("path = \"../../crates/tmi-plugin-dev\"", "path = \"../tmi-plugin-dev\"")]);
    text("sdk/tmi-plugin-sdk/src/lib.rs", SDK_LIB, &[("path: \"../../crates/tmi-format/wit\"", "path: \"../../wit\"")]);
    text("sdk/tmi-plugin-sdk/src/__guest.rs", SDK_GUEST, &[]);
    text("sdk/tmi-plugin-sdk/src/native.rs", SDK_NATIVE, &[]);
    text("sdk/tmi-plugin-sdk/src/native_sys.rs", SDK_NATIVE_SYS, &[]);
    text("sdk/tmi-plugin-sdk/src/wasm_sys.rs", SDK_WASM_SYS, &[]);
    text("sdk/tmi-plugin-sdk/src/wire.rs", SDK_WIRE, &[("../../crates/tmi-plugin-dev/testdata/structure.cbor", "../tmi-plugin-dev/testdata/structure.cbor")]);
    text("sdk/tmi-plugin-dev/Cargo.toml", DEV_CARGO, &[]);
    text("sdk/tmi-plugin-dev/src/lib.rs", DEV_LIB, &[]);
    text("sdk/tmi-plugin-dev/src/cbor.rs", DEV_CBOR, &[]);
    text("sdk/tmi-plugin-dev/src/frame.rs", DEV_FRAME, &[]);
    text("sdk/tmi-plugin-dev/src/msg.rs", DEV_MSG, &[]);
    // the SDK is MIT (tmiki itself is AGPL-3.0): its copies carry the notice
    text("sdk/LICENSE-MIT", SDK_LICENSE, &[]);
    out.push(("sdk/tmi-plugin-dev/testdata/structure.cbor".into(), DEV_FIXTURE.to_vec()));
    out
}

const SDK_LICENSE: &str = include_str!("../../../plugins/sdk/LICENSE-MIT");

const SDK_CARGO: &str = include_str!("../../../plugins/sdk/Cargo.toml");
const SDK_LIB: &str = include_str!("../../../plugins/sdk/src/lib.rs");
const SDK_GUEST: &str = include_str!("../../../plugins/sdk/src/__guest.rs");
const SDK_NATIVE: &str = include_str!("../../../plugins/sdk/src/native.rs");
const SDK_NATIVE_SYS: &str = include_str!("../../../plugins/sdk/src/native_sys.rs");
const SDK_WASM_SYS: &str = include_str!("../../../plugins/sdk/src/wasm_sys.rs");
const SDK_WIRE: &str = include_str!("../../../plugins/sdk/src/wire.rs");
const DEV_CARGO: &str = include_str!("../../tmi-plugin-dev/Cargo.toml");
const DEV_LIB: &str = include_str!("../../tmi-plugin-dev/src/lib.rs");
const DEV_CBOR: &str = include_str!("../../tmi-plugin-dev/src/cbor.rs");
const DEV_FRAME: &str = include_str!("../../tmi-plugin-dev/src/frame.rs");
const DEV_MSG: &str = include_str!("../../tmi-plugin-dev/src/msg.rs");
const DEV_FIXTURE: &[u8] = include_bytes!("../../tmi-plugin-dev/testdata/structure.cbor");

/// Writes the plugin directory `<parent>/<id>`; the directory may exist
/// only when empty.
pub fn run(parent: &Path, o: &Options, print: Print) -> Result<PathBuf, String> {
    if !crate::valid_id(&o.id) {
        return Err(format!("{:?} is not a plugin id: [a-z0-9-], 1 to 40 characters", o.id));
    }
    let dir = parent.join(&o.id);
    if dir.exists() {
        let empty = std::fs::read_dir(&dir).map(|mut d| d.next().is_none()).unwrap_or(false);
        if !empty {
            return Err(format!("{} already exists and is not empty", dir.display()));
        }
    }
    for (rel, bytes) in files(o) {
        let path = dir.join(&rel);
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p).map_err(|e| format!("{}: {e}", p.display()))?;
        }
        std::fs::write(&path, bytes).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    let world = if o.writer { "format-plugin" } else { "format-reader" };
    print(format!(
        "created {}/ (rust plugin {}, tmi:format@{}, world {world})",
        o.id,
        o.id,
        crate::wit_version()
    ));
    print(format!("next: cd {} && tmiki plugin build && tmiki plugin test", o.id));
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_template_renders() {
        for writer in [false, true] {
            let fs = files(&Options { id: "demo-1".into(), writer });
            for (rel, bytes) in &fs {
                let text = String::from_utf8_lossy(bytes);
                if rel.ends_with(".wit") || rel == "PLUGIN_SPEC.md" || rel.starts_with("sdk/") {
                    continue;
                }
                assert!(!text.contains("{{"), "{rel}: {text}");
            }
            let src = fs.iter().find(|(r, _)| r == "src/lib.rs").unwrap();
            let text = String::from_utf8_lossy(&src.1);
            assert_eq!(text.contains("write_layers"), writer, "{writer}");
            assert!(!text.contains("python") && !text.contains("Python"), "{text}");
            let manifest = fs.iter().find(|(r, _)| r == "tmi-plugin.toml").unwrap();
            let keys = crate::manifest::parse(&String::from_utf8_lossy(&manifest.1)).unwrap();
            assert_eq!(keys["plugin.id"], "demo-1");
            assert_eq!(keys["plugin.world"], if writer { "format-plugin" } else { "format-reader" });
            assert!(keys.contains_key("dev.command"));
        }
    }

    /// Every source file of the SDKs is in the copy `new` writes.
    #[test]
    fn every_sdk_source_is_copied() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let names = |dir: &str, ext: &str| -> Vec<String> {
            let mut v: Vec<String> = std::fs::read_dir(root.join(dir)).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).filter(|n| n.ends_with(ext)).collect();
            v.sort();
            v
        };
        let rust: Vec<String> = sdk_files().into_iter().map(|(r, _)| r).collect();
        for n in names("plugins/sdk/src", ".rs") {
            assert!(rust.contains(&format!("sdk/tmi-plugin-sdk/src/{n}")), "{n}");
        }
        for n in names("crates/tmi-plugin-dev/src", ".rs") {
            assert!(rust.contains(&format!("sdk/tmi-plugin-dev/src/{n}")), "{n}");
        }
    }
}
