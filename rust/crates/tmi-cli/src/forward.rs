//! Handing a command to Python: `python -m tmi <same argv>` (through a
//! `-c` bootstrap that says so in tmi's words when that Python has no tmi).
//!
//! Which Python: `TMI_PYTHON` if set; else the `python` next to this
//! executable (a venv's Scripts / bin), else the `tmiki-py` launcher pip put
//! next to it (the same install's console script); else `python` /
//! `python3` on PATH. The child inherits stdin, stdout, stderr and the
//! environment plus `TMI_FORWARDED=1`, which `tmi.cli.main` removes; if this
//! executable ever starts with it set, the "Python" was this executable
//! again, and it stops instead of looping.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Command;

pub const GUARD: &str = "TMI_FORWARDED";

/// Variables a native command hands the Python it forwards to (stage 4b:
/// `TMI_PULL_FETCHED`, a pull whose fetch already ran here).
static EXTRA_ENV: std::sync::Mutex<Vec<(String, String)>> = std::sync::Mutex::new(Vec::new());

pub fn set_env(key: &str, value: &str) {
    EXTRA_ENV.lock().unwrap().push((key.to_string(), value.to_string()));
}

/// `TMI_DEBUG_FORWARD=1` prints why a command went to Python.
pub fn log_reason(why: &str) {
    if std::env::var("TMI_DEBUG_FORWARD").is_ok_and(|v| v == "1") {
        eprintln!("tmiki: forwarding to Python: {why}");
    }
    log_invocation(&format!("forward\t{why}"));
}

/// `TMI_FORWARD_LOG=<file>` appends "native\t<command>" or
/// "forward\t<reason>" per run (how much of a test suite ran natively).
pub fn log_invocation(line: &str) {
    if let Some(path) = std::env::var_os("TMI_FORWARD_LOG").filter(|p| !p.is_empty()) {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(f, "{line}");
        }
    }
}

fn exe_name(stem: &str) -> String {
    if cfg!(windows) {
        format!("{stem}.exe")
    } else {
        stem.to_string()
    }
}

/// `python -m tmi`, saying in tmi's words when that Python has no tmi
/// package (issue #29: Python's own "No module named tmi" said nothing about
/// what to do). `run_module(alter_sys=True)` sets `sys.argv[0]` and
/// `__main__` as `-m` does; `-c` puts the working directory first on
/// `sys.path` as `-m` does.
const BOOTSTRAP: &str = "import sys\n\
from importlib.util import find_spec\n\
try:\n    found = find_spec('tmi.cli') is not None\n\
except ImportError:\n    found = False\n\
if not found:\n    \
sys.stderr.write('tmiki: error: this command runs in the Python part of tmi, which ' + sys.executable + ' cannot import; '\n        \
'install it from the tmi source (see README: pip install -e .), or set TMI_PYTHON to a Python that has it\\n')\n    \
raise SystemExit(1)\n\
from runpy import run_module\n\
run_module('tmi', run_name='__main__', alter_sys=True)\n";

/// The program and the arguments that precede the user's argv.
fn python_command() -> (PathBuf, Vec<OsString>) {
    let module = vec![OsString::from("-c"), OsString::from(BOOTSTRAP)];
    if let Some(py) = std::env::var_os("TMI_PYTHON").filter(|v| !v.is_empty()) {
        return (PathBuf::from(py), module);
    }
    if let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(PathBuf::from)) {
        let names: &[&str] = if cfg!(windows) { &["python"] } else { &["python3", "python"] };
        for name in names {
            let candidate = dir.join(exe_name(name));
            if candidate.is_file() {
                return (candidate, module);
            }
        }
        let launcher = dir.join(exe_name("tmiki-py"));
        if launcher.is_file() {
            return (launcher, Vec::new());
        }
    }
    let fallback = if cfg!(windows) { "python" } else { "python3" };
    (PathBuf::from(fallback), module)
}

/// Python could not be started at all.
fn cannot_run(program: &std::path::Path, err: &std::io::Error) {
    eprintln!(
        "tmiki: error: this command runs in Python, but {} could not be started ({err}); install Python and the \
         Python part of tmi (see README), or set TMI_PYTHON to the Python to use",
        program.display()
    );
}

/// Standing alone (no `TMI_PYTHON`): what this executable does not serve is
/// an error, not a hand-over. Argument errors exit 2 as argparse's do.
fn unsupported(why: &str) -> ! {
    if why.contains("argparse") || why.starts_with("argument error") || why.starts_with("invalid choice") {
        eprintln!("tmiki: error: these arguments are not accepted ({why}); see 'tmiki -h'");
        std::process::exit(2);
    }
    eprintln!("tmiki: error: not supported: {why}");
    std::process::exit(1);
}

pub fn forward_or_fail(argv: &[OsString], why: &str) -> ! {
    if !crate::pyenv::fallback() {
        unsupported(why);
    }
    forward(argv)
}

pub fn forward(argv: &[OsString]) -> ! {
    if !crate::pyenv::fallback() {
        unsupported("this command line");
    }
    let (program, prefix) = python_command();
    let mut cmd = Command::new(&program);
    cmd.args(&prefix).args(&argv[1..]).env(GUARD, "1");
    for (k, v) in EXTRA_ENV.lock().unwrap().iter() {
        cmd.env(k, v);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = cmd.exec();
        cannot_run(&program, &err);
        std::process::exit(1);
    }
    #[cfg(not(unix))]
    {
        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(err) => {
                cannot_run(&program, &err);
                std::process::exit(1);
            }
        };
        // Ctrl+C reaches the child too; this process waits for it.
        #[cfg(windows)]
        // SAFETY: installs the "ignore Ctrl+C" handler for this process.
        unsafe {
            windows_sys::Win32::System::Console::SetConsoleCtrlHandler(None, 1);
        }
        let code = match child.wait() {
            Ok(status) => status.code().unwrap_or(1),
            Err(_) => 1,
        };
        std::process::exit(code);
    }
}
