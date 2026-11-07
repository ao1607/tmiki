//! The user's own components (PLUGINS.md 7.1): `tmiki plugin install
//! <file.wasm>` keeps one in `~/.tmi/plugins/<id>/<version>.wasm` (no
//! repository changes); `tmiki plugin add <id>[@<version>]` takes it from
//! there (the highest version when none is named).
//!
//! `TMI_PLUGIN_HOME` puts the store elsewhere (the tests' own).

use std::path::{Path, PathBuf};

/// The store: `TMI_PLUGIN_HOME`, else `~/.tmi/plugins`.
pub fn home() -> Result<PathBuf, String> {
    if let Some(h) = std::env::var_os("TMI_PLUGIN_HOME").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(h));
    }
    let user = if cfg!(windows) { std::env::var_os("USERPROFILE") } else { std::env::var_os("HOME") };
    user.filter(|v| !v.is_empty())
        .map(|u| PathBuf::from(u).join(".tmi").join("plugins"))
        .ok_or_else(|| "no home directory (set TMI_PLUGIN_HOME)".to_string())
}

/// (major, minor, patch, whether a pre-release) for ordering versions: a
/// pre-release sorts before its release.
fn order(v: &str) -> (u64, u64, u64, bool) {
    let pre = v.contains('-');
    let core = v.split(['-', '+']).next().unwrap_or("");
    let mut it = core.split('.').map(|p| p.parse::<u64>().unwrap_or(0));
    (it.next().unwrap_or(0), it.next().unwrap_or(0), it.next().unwrap_or(0), !pre)
}

/// Where `id` `version` goes.
pub fn path_of(home: &Path, id: &str, version: &str) -> PathBuf {
    home.join(id).join(format!("{version}.wasm"))
}

/// Copies `bytes` (plugin `id` `version`, already checked) into the store;
/// the path, and whether a different component was there.
pub fn install(home: &Path, id: &str, version: &str, bytes: &[u8]) -> Result<(PathBuf, bool), String> {
    // The version is the plugin's own word: only a semver ([0-9A-Za-z.+-]: no
    // separator, no `:` (an NTFS alternate data stream), no control) names a file.
    if !crate::valid_id(id) || !tmi_plugin_host::plugin::is_semver(version) {
        return Err(format!(
            "plugin {} {}: not an id and version a file may be named after",
            tmi_format::safe_text::line(id),
            tmi_format::safe_text::line(version)
        ));
    }
    let path = path_of(home, id, version);
    // what is made must be <home>/<id>/<version>.wasm, a plain file, nothing else
    let want_parent = home.join(id);
    if path.parent() != Some(want_parent.as_path())
        || path.file_name().and_then(|n| n.to_str()) != Some(format!("{version}.wasm").as_str())
        || path.components().skip(home.components().count()).count() != 2
    {
        return Err(format!("plugin {id} {}: the path {} is not directly under {}", tmi_format::safe_text::line(version), path.display(), want_parent.display()));
    }
    if let Ok(m) = std::fs::symlink_metadata(&path) {
        if !m.file_type().is_file() {
            return Err(format!("{} is not a plain file; not replacing it", path.display()));
        }
    }
    let replaced = std::fs::read(&path).is_ok_and(|old| old != bytes);
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p).map_err(|e| format!("{}: {e}", p.display()))?;
    }
    // a temporary file, then a rename: a reader never sees half a component
    let tmp = path.with_extension("wasm.tmp");
    std::fs::write(&tmp, bytes).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok((path, replaced))
}

/// The installed versions of `id`, highest first.
pub fn versions(home: &Path, id: &str) -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(home.join(id))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_str().and_then(|n| n.strip_suffix(".wasm")).map(str::to_string))
        .collect();
    out.sort_by(|a, b| order(b).cmp(&order(a)).then_with(|| b.cmp(a)));
    out
}

/// `spec` (`id` or `id@version`) as the stored component's path.
pub fn find(home: &Path, spec: &str) -> Result<PathBuf, String> {
    let (id, version) = match spec.split_once('@') {
        Some((i, v)) => (i, Some(v)),
        None => (spec, None),
    };
    if !crate::valid_id(id) {
        return Err(format!("{}: no such file, and not a plugin id ([a-z0-9-]{{1,40}})", tmi_format::safe_text::line(spec)));
    }
    let have = versions(home, id);
    let pick = match version {
        Some(v) => have.iter().find(|h| h.as_str() == v).cloned(),
        None => have.first().cloned(),
    };
    match pick {
        Some(v) => Ok(path_of(home, id, &v)),
        None if have.is_empty() => Err(format!(
            "{spec}: no such file, and no plugin {id} is installed in {} ('tmiki plugin install <file.wasm>' puts one there)",
            home.display()
        )),
        None => Err(format!("{spec}: plugin {id} is installed as {} only", have.join(", "))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_and_lookup() {
        let home = std::env::temp_dir().join(format!("tmi-plugin-tools-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        for v in ["0.2.0", "0.10.0", "0.10.0-rc1", "0.9.1"] {
            install(&home, "ora", v, v.as_bytes()).unwrap();
        }
        assert_eq!(versions(&home, "ora"), ["0.10.0", "0.10.0-rc1", "0.9.1", "0.2.0"]);
        assert_eq!(find(&home, "ora").unwrap(), path_of(&home, "ora", "0.10.0"));
        assert_eq!(find(&home, "ora@0.2.0").unwrap(), path_of(&home, "ora", "0.2.0"));
        assert!(find(&home, "ora@1.0.0").unwrap_err().contains("installed as 0.10.0"));
        assert!(find(&home, "kra").unwrap_err().contains("no plugin kra is installed"));
        let (_, replaced) = install(&home, "ora", "0.2.0", b"other").unwrap();
        assert!(replaced);
        assert!(install(&home, "ora", "../x", b"").is_err());
        for bad in ["1.0.0-a:b", "1.0.0-a\\b", "1.0.0\r\x1b[2K", "1.0.0-.x", "..", "1.0", "1.0.0+a/b", "1.0.0-a b", "1.0.0::$DATA"] {
            assert!(install(&home, "ora", bad, b"").is_err(), "{bad:?}");
        }
        assert!(find(&home, "../x@1.0.0").is_err());
        let _ = std::fs::remove_dir_all(&home);
    }
}
