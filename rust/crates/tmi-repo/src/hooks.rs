//! `.tmi/hooks/<name>` scripts run at fixed points.
//!
//! Run with the same environment plus `TMI_REPO`,
//! `TMI_HOOK` and the event's variables (on Windows with the variable
//! names upper-cased, as `os.environ.copy()` hands them on), stdin,
//! stdout and stderr inherited. A `.py` hook (a script the user wrote) runs
//! with the Python on PATH (`py` / `python` on Windows, `python3` /
//! `python` elsewhere); tmiki itself needs no Python. A `.ps1` runs with
//! `powershell.exe -ExecutionPolicy Bypass -File`, anything else directly.
//!
//! Only the hooks of a repository this user trusts run (`trust.rs`:
//! made here by `init` / `clone`, or allowed once): a `.tmi` that came in an
//! unpacked folder runs nothing unasked. The commit message goes to the
//! hook as `TMI_COMMIT_MSG_FILE` (the exact text, `.tmi/COMMIT_EDITMSG`) and
//! `TMI_COMMIT_MSG` (without control characters).

use crate::error::{RepoError, Result};
use std::path::{Path, PathBuf};

pub const SUPPORTED_HOOKS: [&str; 6] = ["pre-commit", "post-commit", "pre-push", "post-merge", "pre-switch", "post-switch"];

const WIN_HOOK_SUFFIXES: [&str; 5] = [".py", ".bat", ".cmd", ".ps1", ".exe"];

/// The interpreter for a user's `.py` hook: the first of the usual names
/// found on PATH.
fn python_for_hooks() -> PathBuf {
    let names: &[&str] = if cfg!(windows) { &["py.exe", "python.exe"] } else { &["python3", "python"] };
    std::env::var_os("PATH").and_then(|path| first_on_path(&path, names)).unwrap_or_else(|| PathBuf::from("python"))
}

/// The first of `names` in the directories of `path` (a PATH value).
fn first_on_path(path: &std::ffi::OsStr, names: &[&str]) -> Option<PathBuf> {
    for name in names {
        // a relative entry ("." or "") would find a program in the working
        // tree, which a clone fills
        for dir in std::env::split_paths(path).filter(|d| d.is_absolute()) {
            let c = dir.join(name);
            if c.is_file() {
                return Some(c);
            }
        }
    }
    None
}

/// `hook_path(repo_dir, name)`.
pub fn hook_path(repo_dir: &Path, name: &str) -> PathBuf {
    let base = repo_dir.join("hooks").join(name);
    if cfg!(windows) {
        for suffix in WIN_HOOK_SUFFIXES {
            let candidate = repo_dir.join("hooks").join(format!("{name}{suffix}"));
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    base
}

/// `is_installed(repo_dir, name)`.
pub fn is_installed(repo_dir: &Path, name: &str) -> bool {
    let path = hook_path(repo_dir, name);
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        return path.metadata().map(|m| m.permissions().mode() & 0o100 != 0).unwrap_or(false);
    }
    #[allow(unreachable_code)]
    true
}

/// `list_installed(repo_dir)`.
pub fn list_installed(repo_dir: &Path) -> Vec<&'static str> {
    SUPPORTED_HOOKS.iter().copied().filter(|n| is_installed(repo_dir, n)).collect()
}

/// Whether any hook is installed (a command that would run one).
pub fn any_installed(repo_dir: &Path, names: &[&str]) -> bool {
    names.iter().any(|n| is_installed(repo_dir, n))
}

/// `text` without control characters (C0 but TAB and LF, DEL, C1).
fn strip_controls(text: &str) -> String {
    text.chars().filter(|&c| c == '\t' || c == '\n' || !(c < ' ' || ('\u{7f}'..='\u{9f}').contains(&c))).collect()
}

fn suffix_lower(path: &Path) -> String {
    // PurePath.suffix: from the last '.' of the name, unless the name
    // starts with it or ends with it.
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    match name.rfind('.') {
        Some(i) if i > 0 && i + 1 < name.len() => name[i..].to_lowercase(),
        _ => String::new(),
    }
}

/// `run(repo_dir, name, extra_env=..., fatal=...)`: the hook's exit code (0
/// when it is not installed). `before` runs right before the hook starts
/// (the caller writes what Python would already have written).
pub fn run(
    repo_dir: &Path,
    name: &str,
    extra_env: &[(&str, &str)],
    fatal: bool,
    before: &mut dyn FnMut(),
) -> Result<i32> {
    if !SUPPORTED_HOOKS.contains(&name) {
        return Err(RepoError::runtime(format!("unknown hook: {name}")));
    }
    if !is_installed(repo_dir, name) {
        return Ok(0);
    }
    let path = hook_path(repo_dir, name);
    // a .tmi that came in an unpacked folder runs nothing unasked
    if !crate::trust::require_hook_consent(repo_dir, &path, fatal)? {
        return Ok(1);
    }
    let tmi_repo = if repo_dir.file_name().is_some_and(|n| n == ".tmi") {
        repo_dir.parent().unwrap_or(repo_dir).to_path_buf()
    } else {
        repo_dir.to_path_buf()
    };
    let mut env: Vec<(String, String)> = Vec::new();
    let set = |k: &str, v: &str, env: &mut Vec<(String, String)>| {
        let key = if cfg!(windows) { k.to_uppercase() } else { k.to_string() };
        if let Some(slot) = env.iter_mut().find(|(ek, _)| *ek == key) {
            slot.1 = v.to_string();
        } else {
            env.push((key, v.to_string()));
        }
    };
    for (k, v) in std::env::vars_os() {
        let (Some(k), Some(v)) = (k.to_str(), v.to_str()) else {
            return Err(RepoError::forward("non-UTF-8 environment"));
        };
        set(k, v, &mut env);
    }
    set("TMI_REPO", &tmi_repo.to_string_lossy(), &mut env);
    set("TMI_HOOK", name, &mut env);
    for (k, v) in extra_env {
        if *k == "TMI_COMMIT_MSG" {
            // The message may be someone else's (`commit --amend` of a pulled
            // commit): the exact text in a file, the variable without
            // control characters (a hook reads it as untrusted text).
            let file = repo_dir.join("COMMIT_EDITMSG");
            crate::fsio::write_bytes(&file, v.as_bytes())?;
            set("TMI_COMMIT_MSG_FILE", &file.to_string_lossy(), &mut env);
            set(k, &strip_controls(v), &mut env);
            continue;
        }
        set(k, v, &mut env);
    }
    let suffix = suffix_lower(&path);
    let mut cmd = match suffix.as_str() {
        ".py" => {
            let mut c = std::process::Command::new(python_for_hooks());
            c.arg(&path);
            c
        }
        ".ps1" => {
            let mut c = std::process::Command::new("powershell.exe");
            c.args(["-ExecutionPolicy", "Bypass", "-File"]).arg(&path);
            c
        }
        _ => std::process::Command::new(&path),
    };
    cmd.env_clear().envs(env);
    before();
    crate::fsio::mark_mutated(); // a hook may change anything
    // The hook may run tmiki itself (`tmiki add` in a pre-commit hook): give up
    // index.lock while it runs and take it back after (issue #35).
    let held = crate::lockfile::suspend();
    let ran = cmd.status();
    crate::lockfile::resume(held)?;
    match ran {
        Ok(status) => {
            let code = status.code().unwrap_or(1);
            if code != 0 && fatal {
                return Err(RepoError::runtime(format!("hook {name} aborted with exit code {code}")));
            }
            Ok(code)
        }
        Err(e) => {
            if fatal {
                return Err(RepoError::runtime(format!("hook {name} could not be executed: {}", os_error_str(&e))));
            }
            Ok(1)
        }
    }
}

/// `str(OSError)` for an error from starting a process.
pub fn os_error_str(e: &std::io::Error) -> String {
    #[cfg(windows)]
    if let Some(code) = e.raw_os_error() {
        return format!("[WinError {code}] {}", win_message(code as u32));
    }
    #[cfg(unix)]
    if let Some(code) = e.raw_os_error() {
        let text = e.to_string();
        let msg = text.split(" (os error").next().unwrap_or(&text).to_string();
        return format!("[Errno {code}] {msg}");
    }
    e.to_string()
}

/// CPython's `PyErr_SetFromWindowsErr` text: FormatMessageW, trailing
/// whitespace and dots removed.
#[cfg(windows)]
fn win_message(code: u32) -> String {
    use windows_sys::Win32::System::Diagnostics::Debug::{
        FormatMessageW, FORMAT_MESSAGE_FROM_SYSTEM, FORMAT_MESSAGE_IGNORE_INSERTS,
    };
    let mut buf = vec![0u16; 2048];
    // SAFETY: a plain buffer of the given length.
    let n = unsafe {
        FormatMessageW(
            FORMAT_MESSAGE_FROM_SYSTEM | FORMAT_MESSAGE_IGNORE_INSERTS,
            std::ptr::null(),
            code,
            0,
            buf.as_mut_ptr(),
            buf.len() as u32,
            std::ptr::null(),
        )
    } as usize;
    let mut s: Vec<u16> = buf[..n].to_vec();
    while s.last().is_some_and(|&c| c <= b' ' as u16 || c == b'.' as u16) {
        s.pop();
    }
    if s.is_empty() {
        return format!("Windows Error 0x{code:x}");
    }
    String::from_utf16_lossy(&s)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A PATH with a relative entry: the interpreter for a .py hook was
    /// looked up there, i.e. in the working tree (cargo runs the tests in
    /// the crate's directory, where `src/lib.rs` stands in for a planted
    /// `py.exe`).
    #[test]
    fn a_python_planted_in_the_working_tree_is_not_the_hooks_interpreter() {
        assert!(std::path::Path::new("src").join("lib.rs").is_file());
        let path = std::env::join_paths(["src"]).unwrap();
        assert_eq!(first_on_path(&path, &["lib.rs"]), None);
        let abs = std::env::join_paths([std::fs::canonicalize("src").unwrap()]).unwrap();
        assert!(first_on_path(&abs, &["lib.rs"]).is_some());
    }

    #[test]
    fn the_commit_message_variable_has_no_control_characters() {
        assert_eq!(strip_controls("fix\x1b[2J & go\r\n\tnext\u{9b}"), "fix[2J & go\n\tnext");
    }
}
