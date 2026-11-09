//! When add / commit / status go to Python instead (stage 3b-2): a merge
//! in progress and plugins that the command would run. Hooks run natively
//! (tmi_repo::hooks, stage 3b-1); TMI_PROGRESS=1's timed lines too
//! (progress.rs, issue #26).

use std::path::{Path, PathBuf};
use tmi_repo::{Repo, RepoError};

pub fn fwd(why: &str) -> RepoError {
    RepoError::forward(why)
}

/// A merge in progress or conflict records: Python resolves and reports them.
pub fn no_merge_state(repo: &Repo) -> Result<(), RepoError> {
    if repo.pending_merge_head()?.is_some() {
        return Err(fwd("merge in progress"));
    }
    if let Ok(mut dir) = std::fs::read_dir(repo.repo_dir.join("conflicts")) {
        if dir.next().is_some() {
            return Err(fwd("conflict records"));
        }
    }
    Ok(())
}

fn exe(stem: &str) -> String {
    if cfg!(windows) {
        format!("{stem}.exe")
    } else {
        stem.to_string()
    }
}

/// The Python the forward would run (forward.rs), as a path.
fn python_path() -> Option<PathBuf> {
    if let Some(py) = std::env::var_os("TMI_PYTHON").filter(|v| !v.is_empty()) {
        let p = PathBuf::from(py);
        if p.components().count() > 1 {
            return Some(p);
        }
        return on_path(&p.to_string_lossy());
    }
    if let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(PathBuf::from)) {
        for name in if cfg!(windows) { &["python", "tmiki-py"][..] } else { &["python3", "python", "tmiki-py"][..] } {
            let c = dir.join(exe(name));
            if c.is_file() {
                return Some(c);
            }
        }
    }
    on_path(if cfg!(windows) { "python" } else { "python3" })
}

fn on_path(name: &str) -> Option<PathBuf> {
    let file = if cfg!(windows) && !name.to_lowercase().ends_with(".exe") { exe(name) } else { name.to_string() };
    std::env::split_paths(&std::env::var_os("PATH")?).map(|d| d.join(&file)).find(|p| p.is_file())
}

fn push_glob(out: &mut Vec<PathBuf>, dir: &Path, prefix: &str, tail: &[&str]) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            if e.file_name().to_string_lossy().to_lowercase().starts_with(prefix) {
                let mut p = e.path();
                for t in tail {
                    p.push(t);
                }
                out.push(p);
            }
        }
    }
}

/// The site-packages directories the forwarded Python would load
/// distributions from (venv, base install with system site, user site,
/// PYTHONPATH). None when none can be found.
fn site_dirs() -> Option<Vec<PathBuf>> {
    let py = python_path()?;
    let bin = py.parent()?.to_path_buf();
    let mut prefixes = vec![bin.clone()];
    if let Some(p) = bin.parent() {
        prefixes.push(p.to_path_buf());
    }
    let mut dirs = Vec::new();
    let mut system = true;
    for prefix in prefixes.clone() {
        let cfg = prefix.join("pyvenv.cfg");
        if let Ok(text) = std::fs::read_to_string(&cfg) {
            system = false;
            for line in text.lines() {
                let (k, v) = line.split_once('=').unwrap_or((line, ""));
                match k.trim().to_lowercase().as_str() {
                    "home" => {
                        let home = PathBuf::from(v.trim());
                        prefixes.push(home.clone());
                        if let Some(p) = home.parent() {
                            prefixes.push(p.to_path_buf());
                        }
                    }
                    "include-system-site-packages" => system = v.trim().eq_ignore_ascii_case("true"),
                    _ => {}
                }
            }
        }
    }
    for prefix in &prefixes {
        dirs.push(prefix.join("Lib").join("site-packages"));
        push_glob(&mut dirs, &prefix.join("lib"), "python3", &["site-packages"]);
    }
    if system {
        if let Some(appdata) = std::env::var_os("APPDATA") {
            push_glob(&mut dirs, &PathBuf::from(appdata).join("Python"), "python3", &["site-packages"]);
        }
        if let Some(home) = std::env::var_os("HOME") {
            push_glob(&mut dirs, &PathBuf::from(home).join(".local").join("lib"), "python3", &["site-packages"]);
        }
    }
    if let Some(pp) = std::env::var_os("PYTHONPATH") {
        dirs.extend(std::env::split_paths(&pp));
    }
    let dirs: Vec<PathBuf> = dirs.into_iter().filter(|d| d.is_dir()).collect();
    (!dirs.is_empty()).then_some(dirs)
}

/// Whether a distribution registers a `tmi.plugins` entry point (commit
/// emits plugin events). Unknown counts as yes.
pub fn plugins_installed() -> bool {
    if !crate::pyenv::fallback() {
        return false;
    }
    let Some(dirs) = site_dirs() else { return true };
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_lowercase();
            if !(name.ends_with(".dist-info") || name.ends_with(".egg-info")) {
                continue;
            }
            if let Ok(text) = std::fs::read_to_string(e.path().join("entry_points.txt")) {
                if text.lines().any(|l| l.trim() == "[tmi.plugins]") {
                    return true;
                }
            }
        }
    }
    false
}
