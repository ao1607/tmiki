//! tmi/hooks.py: `.tmi/hooks/<name>` scripts run at fixed points.
//!
//! Run as Python runs them: the same environment plus `TMI_REPO`,
//! `TMI_HOOK` and the event's variables (on Windows with the variable
//! names upper-cased, as `os.environ.copy()` hands them on), stdin,
//! stdout and stderr inherited. A `.py` hook runs with the Python that
//! `python -m tmi` would use (see [`set_python`]), a `.ps1` with
//! `powershell.exe -ExecutionPolicy Bypass -File`, anything else directly.

use crate::error::{RepoError, Result};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub const SUPPORTED_HOOKS: [&str; 6] = ["pre-commit", "post-commit", "pre-push", "post-merge", "pre-switch", "post-switch"];

const WIN_HOOK_SUFFIXES: [&str; 5] = [".py", ".bat", ".cmd", ".ps1", ".exe"];

static PYTHON: OnceLock<PathBuf> = OnceLock::new();

/// The interpreter for `.py` hooks (Python's `sys.executable`).
pub fn set_python(python: PathBuf) {
    let _ = PYTHON.set(python);
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
        set(k, v, &mut env);
    }
    let suffix = suffix_lower(&path);
    let mut cmd = match suffix.as_str() {
        ".py" => {
            let python = PYTHON.get().cloned().unwrap_or_else(|| PathBuf::from("python"));
            let mut c = std::process::Command::new(python);
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
    // The hook may run tmi itself (`tmiki add` in a pre-commit hook): give up
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
