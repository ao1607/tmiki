//! `tmi-plugin.toml`: how `build` and `test` treat a plugin's directory
//! (PLUGINS.md 7.1). Only the part of TOML the file uses is read: `[table]`
//! headers, `key = "string"` (or a bare word) and `#` comments.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const FILE: &str = "tmi-plugin.toml";

#[derive(Clone, Debug)]
pub struct Manifest {
    /// The directory the file is in (paths are relative to it).
    pub dir: PathBuf,
    pub id: String,
    /// `format-reader` or `format-plugin`.
    pub world: String,
    /// The component `build` writes.
    pub output: PathBuf,
    /// The sample files `test` reads.
    pub corpus: PathBuf,
    /// `dev.command`: the plugin as a development plugin (`tmiki plugin
    /// test --native` without a command runs it, in `dir`).
    pub dev_command: Option<String>,
}

/// `text` as `table.key` -> value.
pub fn parse(text: &str) -> Result<BTreeMap<String, String>, String> {
    let mut out = BTreeMap::new();
    let mut table = String::new();
    for (n, raw) in text.lines().enumerate() {
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            table = name.trim().to_string();
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(format!("{FILE} line {}: expected key = \"value\"", n + 1));
        };
        let value = value.trim();
        let value = match value.strip_prefix('"').and_then(|v| v.strip_suffix('"')) {
            Some(v) => v.replace("\\\\", "\\").replace("\\\"", "\""),
            None => value.to_string(),
        };
        let key = if table.is_empty() { key.trim().to_string() } else { format!("{table}.{}", key.trim()) };
        out.insert(key, value);
    }
    Ok(out)
}

fn strip_comment(line: &str) -> &str {
    let mut quoted = false;
    for (i, c) in line.char_indices() {
        match c {
            '"' => quoted = !quoted,
            '#' if !quoted => return &line[..i],
            _ => {}
        }
    }
    line
}

impl Manifest {
    /// The manifest of the plugin directory `dir`.
    pub fn read(dir: &Path) -> Result<Manifest, String> {
        let path = dir.join(FILE);
        let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e} (a plugin directory has a {FILE}; 'tmiki plugin new' writes one)", path.display()))?;
        let keys = parse(&text)?;
        let get = |k: &str| keys.get(k).cloned();
        let need = |k: &str| get(k).ok_or_else(|| format!("{}: no {k}", path.display()));
        let id = need("plugin.id")?;
        if !crate::valid_id(&id) {
            return Err(format!("{}: the id {id:?} is not [a-z0-9-]{{1,40}}", path.display()));
        }
        // rust (the default; PLUGINS.md Q14: plugins are written in Rust)
        if let Some(lang) = get("plugin.lang") {
            crate::check_lang(&lang).map_err(|e| format!("{}: lang = {lang:?}: {e}", path.display()))?;
        }
        let world = get("plugin.world").unwrap_or_else(|| "format-reader".into());
        if world != "format-reader" && world != "format-plugin" {
            return Err(format!("{}: world {world:?} is neither format-reader nor format-plugin", path.display()));
        }
        // the manifest is the directory's author's: what it names stays inside the directory
        let inside = |key: &str, default: String| -> Result<PathBuf, String> {
            let rel = get(key).unwrap_or(default);
            let p = Path::new(&rel);
            let ok = !rel.is_empty()
                && !rel.contains(['\0', ':'])
                && p.components().all(|c| matches!(c, std::path::Component::Normal(_) | std::path::Component::CurDir));
            if !ok {
                return Err(format!("{}: {key} = {:?} must be a relative path inside the plugin directory (no .., no absolute path, no drive or stream)", path.display(), rel));
            }
            Ok(dir.join(p))
        };
        Ok(Manifest {
            dir: dir.to_path_buf(),
            output: inside("build.output", format!("target/tmi-plugin/{id}.wasm"))?,
            corpus: inside("test.corpus", "tests/corpus".into())?,
            dev_command: get("dev.command"),
            id,
            world,
        })
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_subset() {
        let m = super::parse("# c\n[plugin]\nid = \"kra\" # x\nlang = rust\n[build]\noutput = \"a#b\"\n").unwrap();
        assert_eq!(m["plugin.id"], "kra");
        assert_eq!(m["plugin.lang"], "rust");
        assert_eq!(m["build.output"], "a#b");
        assert!(super::parse("[x]\nnot a pair\n").is_err());
    }

    #[test]
    fn paths_stay_inside_the_directory() {
        let dir = std::env::temp_dir().join(format!("tmi-plugin-manifest-paths-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let read = |extra: &str| {
            std::fs::write(dir.join(super::FILE), format!("[plugin]\nid = \"x\"\n{extra}")).unwrap();
            super::Manifest::read(&dir)
        };
        assert!(read("[build]\noutput = \"out/x.wasm\"\n[test]\ncorpus = \"c\"\n").is_ok());
        for bad in ["../x.wasm", "/abs/x.wasm", "C:/x.wasm", "C:x.wasm", "a/../../x", "x.wasm:stream", "\\\\\\\\host\\\\share\\\\x", ""] {
            let e = read(&format!("[build]\noutput = \"{bad}\"\n")).unwrap_err();
            assert!(e.contains("inside the plugin directory"), "{bad}: {e}");
            let e = read(&format!("[test]\ncorpus = \"{bad}\"\n")).unwrap_err();
            assert!(e.contains("inside the plugin directory"), "{bad}: {e}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// PLUGINS.md Q14: a directory of a Python plugin (P8's `lang =
    /// "python"`) is refused with the sentence; `lang` may be left out.
    #[test]
    fn lang_is_rust_or_left_out() {
        let dir = std::env::temp_dir().join(format!("tmi-plugin-manifest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let write = |lang: &str| std::fs::write(dir.join(super::FILE), format!("[plugin]\nid = \"x\"\n{lang}")).unwrap();
        write("lang = \"python\"\n");
        let e = super::Manifest::read(&dir).unwrap_err();
        assert!(e.ends_with("lang = \"python\": tmi format plugins are written in Rust: there is no python template, and 'tmiki plugin build' builds Rust plugins only ('tmiki plugin new <id>' writes one; PLUGIN_SPEC.md is written for an AI to write it from)"), "{e}");
        write("lang = \"rust\"\n");
        assert_eq!(super::Manifest::read(&dir).unwrap().id, "x");
        write("");
        assert_eq!(super::Manifest::read(&dir).unwrap().world, "format-reader");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
