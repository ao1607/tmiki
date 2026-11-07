//! What the Python that `python -m tmi` would run sees: its interpreter
//! (for `.py` hooks, Python's `sys.executable`), the directories on its
//! `sys.path` that hold packages, whether any `tmi.plugins` entry point is
//! installed, and the versions `tmiki --version` prints.
//!
//! Anything not read with certainty returns None and the command goes to
//! Python.

use std::path::{Path, PathBuf};

fn exe_name(stem: &str) -> String {
    if cfg!(windows) {
        format!("{stem}.exe")
    } else {
        stem.to_string()
    }
}

/// The interpreter `forward` would start (`TMI_PYTHON`, else the one next
/// to this executable), if known.
pub fn python() -> Option<PathBuf> {
    if let Some(py) = std::env::var_os("TMI_PYTHON").filter(|v| !v.is_empty()) {
        let p = PathBuf::from(py);
        if p.is_absolute() {
            return Some(p);
        }
        return which(&p.to_string_lossy());
    }
    let dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    let names: &[&str] = if cfg!(windows) { &["python"] } else { &["python3", "python"] };
    names.iter().map(|n| dir.join(exe_name(n))).find(|c| c.is_file())
}

/// The interpreter for `.py` hooks: [`python`], else `py` / `python` on PATH.
pub fn python_for_hooks() -> PathBuf {
    if let Some(p) = python() {
        return p;
    }
    let names: &[&str] = if cfg!(windows) { &["py", "python"] } else { &["python3", "python"] };
    names.iter().find_map(|n| which(n)).unwrap_or_else(|| PathBuf::from("python"))
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let c = dir.join(exe_name(name));
        if c.is_file() {
            return Some(c);
        }
    }
    None
}

/// The parts of `sys.path` that can hold packages, in order: the cwd
/// (`python -m`), `PYTHONPATH`, the environment's site-packages (and the
/// base's / the user's when they are on the path). None when the layout is
/// not one recognised here.
pub fn package_dirs() -> Option<Vec<PathBuf>> {
    let python = python()?;
    let bindir = python.parent()?.to_path_buf();
    let mut dirs = vec![std::env::current_dir().ok()?];
    if let Some(pp) = std::env::var_os("PYTHONPATH").filter(|v| !v.is_empty()) {
        dirs.extend(std::env::split_paths(&pp).filter(|p| !p.as_os_str().is_empty()));
    }
    if std::env::var_os("PYTHONHOME").is_some() || std::env::var_os("PYTHONNOUSERSITE").is_some() {
        return None;
    }
    // A venv: pyvenv.cfg in the directory above Scripts / bin (or beside).
    let (prefix, cfg) = [bindir.parent().map(Path::to_path_buf), Some(bindir.clone())]
        .into_iter()
        .flatten()
        .find_map(|d| {
            let c = d.join("pyvenv.cfg");
            std::fs::read_to_string(&c).ok().map(|t| (d, Some(t)))
        })
        .unwrap_or((bindir.clone(), None));
    let site = |p: &Path| -> Vec<PathBuf> {
        if cfg!(windows) {
            vec![p.join("Lib").join("site-packages")]
        } else {
            std::fs::read_dir(p.join("lib"))
                .map(|d| {
                    d.flatten()
                        .filter(|e| e.file_name().to_string_lossy().starts_with("python3"))
                        .map(|e| e.path().join("site-packages"))
                        .collect()
                })
                .unwrap_or_default()
        }
    };
    match cfg {
        Some(text) => {
            let value = |key: &str| {
                text.lines().find_map(|l| {
                    let (k, v) = l.split_once('=')?;
                    (k.trim() == key).then(|| v.trim().to_string())
                })
            };
            if value("include-system-site-packages").is_some_and(|v| v.eq_ignore_ascii_case("true")) {
                // site.py with a venv that sees the base install (stage 4b):
                // the user site (enabled then, added first), the venv's,
                // then the base prefix's. Windows only: elsewhere the base
                // install's directories depend on the distribution.
                if !cfg!(windows) {
                    return None;
                }
                let home = PathBuf::from(value("home")?);
                let version = value("version").or_else(|| value("version_info"))?;
                let nodot: String = version.split('.').take(2).collect();
                if let Some(appdata) = std::env::var_os("APPDATA") {
                    dirs.push(PathBuf::from(appdata).join("Python").join(format!("Python{nodot}")).join("site-packages"));
                }
                dirs.extend(site(&prefix));
                dirs.extend(site(&home));
                return Some(dirs);
            }
            dirs.extend(site(&prefix));
        }
        None => {
            if !cfg!(windows) {
                return None;
            }
            // A plain installation: its site-packages, then the user's
            // (enabled by default), which comes first on sys.path.
            let version = python_version()?;
            let nodot: String = version.split('.').take(2).collect();
            if let Some(appdata) = std::env::var_os("APPDATA") {
                dirs.push(PathBuf::from(appdata).join("Python").join(format!("Python{nodot}")).join("site-packages"));
            }
            dirs.extend(site(&prefix));
        }
    }
    Some(dirs)
}

/// `X.Y.Z` of the interpreter, from pyvenv.cfg or the DLL beside it.
pub fn python_version() -> Option<String> {
    let python = python()?;
    let bindir = python.parent()?;
    for d in [bindir.parent(), Some(bindir)].into_iter().flatten() {
        if let Ok(text) = std::fs::read_to_string(d.join("pyvenv.cfg")) {
            for l in text.lines() {
                if let Some((k, v)) = l.split_once('=') {
                    if k.trim() == "version" || k.trim() == "version_info" {
                        return Some(v.trim().to_string());
                    }
                }
            }
        }
    }
    if cfg!(windows) {
        for e in std::fs::read_dir(bindir).ok()?.flatten() {
            let n = e.file_name().to_string_lossy().to_lowercase();
            if let Some(rest) = n.strip_prefix("python3").and_then(|r| r.strip_suffix(".dll")) {
                if !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()) {
                    return Some(format!("3.{rest}.0"));
                }
            }
        }
    }
    None
}

/// The `.pth` files of a directory, read: None when one holds something
/// other than plain paths and the editable-install hooks known here.
fn pth_paths(dir: &Path) -> Option<Vec<PathBuf>> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else { return Some(out) };
    let mut names: Vec<PathBuf> = entries.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "pth")).collect();
    names.sort();
    for p in names {
        let text = std::fs::read_to_string(&p).ok()?;
        for line in text.lines() {
            let line = line.trim_end();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if line.starts_with("import ") || line.starts_with("import\t") {
                // setuptools' editable finders and distutils-precedence
                let n = p.file_name()?.to_string_lossy().to_string();
                if n.starts_with("__editable__") || n == "distutils-precedence.pth" {
                    continue;
                }
                // pywin32's: adds DLL directories, not package paths
                if n == "pywin32.pth" && line == "import pywin32_bootstrap" {
                    continue;
                }
                return None;
            }
            let path = if Path::new(line).is_absolute() { PathBuf::from(line) } else { dir.join(line) };
            if path.exists() {
                out.push(path);
            }
        }
    }
    Some(out)
}

/// `package_dirs()` plus what their `.pth` files add.
fn all_dirs() -> Option<Vec<PathBuf>> {
    let base = package_dirs()?;
    let mut out = base.clone();
    for d in &base {
        if d.ends_with("site-packages") {
            out.extend(pth_paths(d)?);
        }
    }
    Some(out)
}

/// Whether any installed distribution declares a `tmi.plugins` entry
/// point (Python would load and call it). None when unsure.
pub fn has_plugins() -> Option<bool> {
    for dir in all_dirs()? {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_lowercase();
            if !(name.ends_with(".dist-info") || name.ends_with(".egg-info")) {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(e.path().join("entry_points.txt")) else { continue };
            let mut in_section = false;
            for line in text.lines() {
                let t = line.trim();
                if t.starts_with('[') {
                    in_section = t == "[tmi.plugins]";
                } else if in_section && t.contains('=') && !t.starts_with('#') && !t.starts_with(';') {
                    return Some(true);
                }
            }
        }
    }
    Some(false)
}

fn normalize_dist(name: &str) -> String {
    let mut out = String::new();
    let mut last_sep = false;
    for c in name.chars() {
        if c == '-' || c == '_' || c == '.' {
            if !last_sep {
                out.push('_');
            }
            last_sep = true;
        } else {
            out.push(c.to_ascii_lowercase());
            last_sep = false;
        }
    }
    out
}

/// `importlib.metadata.version(name)` over the package dirs: the first
/// `<name>-*.dist-info/METADATA` (None: not found; Err: unsure).
fn dist_version(dirs: &[PathBuf], name: &str) -> Result<Option<String>, ()> {
    let want = normalize_dist(name);
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else { continue };
        let mut found: Vec<PathBuf> = Vec::new();
        for e in entries.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            let lower = n.to_lowercase();
            if lower.ends_with(".egg-info") && normalize_dist(lower.split('-').next().unwrap_or("")) == want {
                return Err(());
            }
            if let Some(stem) = lower.strip_suffix(".dist-info") {
                let dist = stem.split('-').next().unwrap_or("");
                if normalize_dist(dist) == want {
                    found.push(e.path());
                }
            }
        }
        if found.len() > 1 {
            return Err(());
        }
        if let Some(p) = found.pop() {
            let text = std::fs::read_to_string(p.join("METADATA")).map_err(|_| ())?;
            for line in text.lines() {
                if line.is_empty() {
                    break;
                }
                if let Some(v) = line.strip_prefix("Version:") {
                    return Ok(Some(v.trim().to_string()));
                }
            }
            return Err(());
        }
    }
    Ok(None)
}

/// `tmi.__version__`: the `tmi` package first found on the path.
fn tmi_version(dirs: &[PathBuf]) -> Option<String> {
    for dir in dirs {
        let init = dir.join("tmi").join("__init__.py");
        if init.is_file() {
            return parse_version(&std::fs::read_to_string(init).ok()?);
        }
        if dir.ends_with("site-packages") {
            // setuptools' editable install: __editable___tmi_*_finder.py
            for e in std::fs::read_dir(dir).ok()?.flatten() {
                let n = e.file_name().to_string_lossy().to_string();
                if n.starts_with("__editable___tmi_") && n.ends_with("_finder.py") {
                    let text = std::fs::read_to_string(e.path()).ok()?;
                    let start = text.find("MAPPING")?;
                    let rest = &text[start..];
                    let key = rest.find("'tmi':").or_else(|| rest.find("\"tmi\":"))?;
                    let after = rest[key + 6..].trim_start();
                    let quote = after.chars().next()?;
                    let end = after[1..].find(quote)?;
                    let path = after[1..1 + end].replace("\\\\", "\\");
                    return parse_version(&std::fs::read_to_string(Path::new(&path).join("__init__.py")).ok()?);
                }
            }
        }
    }
    None
}

fn parse_version(text: &str) -> Option<String> {
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("__version__ = ") {
            let rest = rest.trim();
            let q = rest.chars().next()?;
            if (q == '"' || q == '\'') && rest.ends_with(q) && rest.len() >= 2 {
                return Some(rest[1..rest.len() - 1].to_string());
            }
            return None;
        }
    }
    None
}

/// What `find_spec("tmi_accel")` finds on the path.
fn accel_found(dirs: &[PathBuf]) -> bool {
    dirs.iter().any(|d| {
        d.join("tmi_accel").is_dir()
            || d.join("tmi_accel.py").is_file()
            || std::fs::read_dir(d).is_ok_and(|mut it| {
                it.any(|e| {
                    e.is_ok_and(|e| {
                        let n = e.file_name().to_string_lossy().to_lowercase();
                        n.starts_with("tmi_accel.") && (n.ends_with(".pyd") || n.ends_with(".so"))
                    })
                })
            })
    })
}

/// cli.py `_version_text()`, when every part is certain.
pub fn version_text() -> Option<String> {
    let dirs = all_dirs()?;
    let tmi = tmi_version(&dirs)?;
    let accel = if accel_found(&dirs) {
        match dist_version(&dirs, "tmi-accel").ok()? {
            Some(v) => v,
            None => "installed (version unknown)".to_string(),
        }
    } else {
        "not installed (tmi needs it; see Install in the README)".to_string()
    };
    Some(format!("tmiki {tmi}\ntmi_accel {accel}"))
}
