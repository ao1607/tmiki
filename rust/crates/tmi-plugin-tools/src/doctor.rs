//! `tmiki plugin doctor` (PLUGINS.md 7.1): what `build` needs (cargo and the
//! `wasm32-wasip2` target: plugins are written in Rust, Q14) and what is
//! installed. tmi installs nothing; it says how.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::Print;

/// A program on PATH (on Windows with the PATHEXT extensions).
pub fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let exts: Vec<String> = if cfg!(windows) {
        let pe = std::env::var("PATHEXT").unwrap_or_else(|_| ".EXE;.CMD;.BAT".into());
        std::iter::once(String::new()).chain(pe.split(';').filter(|e| !e.is_empty()).map(str::to_string)).collect()
    } else {
        vec![String::new()]
    };
    for dir in std::env::split_paths(&path) {
        for ext in &exts {
            let p = dir.join(format!("{name}{ext}"));
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

/// The first line a program prints for `args`, if it runs.
fn version(program: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new(program).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = if out.stdout.is_empty() { out.stderr } else { out.stdout };
    String::from_utf8_lossy(&text).lines().next().map(|l| l.trim().to_string())
}

/// cargo: `CARGO` (as cargo sets it for what it runs), else PATH.
pub fn cargo() -> Option<PathBuf> {
    std::env::var_os("CARGO").map(PathBuf::from).filter(|p| p.is_file()).or_else(|| which("cargo"))
}

/// Whether rustc has the standard library for `wasm32-wasip2`
/// (`<sysroot>/lib/rustlib/wasm32-wasip2`: rustup or not).
pub fn wasip2_target() -> Option<bool> {
    let rustc = std::env::var_os("RUSTC").map(PathBuf::from).or_else(|| which("rustc"))?;
    let out = Command::new(rustc).args(["--print", "sysroot"]).output().ok()?;
    let sysroot = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Some(Path::new(&sysroot).join("lib").join("rustlib").join("wasm32-wasip2").is_dir())
}

pub struct Tool {
    pub name: &'static str,
    /// Whether `build` needs it (else: optional).
    pub needed: bool,
    /// What is installed (a version line), or None.
    pub found: Option<String>,
    pub how: &'static str,
}

pub fn tools() -> Vec<Tool> {
    let cargo_v = cargo().and_then(|c| version(&c, &["--version"]).map(|v| format!("{v} ({})", c.display())));
    let target = match wasip2_target() {
        Some(true) => Some("installed".to_string()),
        _ => None,
    };
    let wasm_tools = which("wasm-tools").and_then(|w| version(&w, &["--version"]).map(|v| format!("{v} ({})", w.display())));
    vec![
        Tool { name: "cargo", needed: true, found: cargo_v, how: "https://rustup.rs" },
        Tool { name: "wasm32-wasip2 target", needed: true, found: target, how: "rustup target add wasm32-wasip2" },
        Tool {
            name: "wasm-tools",
            needed: false,
            found: wasm_tools,
            how: "optional, to look inside components: cargo install wasm-tools (build checks and strips components itself)",
        },
    ]
}

/// The missing tools `build` needs (their names and how to get them).
pub fn missing() -> Vec<(&'static str, &'static str)> {
    tools().into_iter().filter(|t| t.needed && t.found.is_none()).map(|t| (t.name, t.how)).collect()
}

/// Prints the table; `in_plugin_dir`: run for a plugin directory, whose
/// build's missing tools make the exit code 1.
pub fn run(in_plugin_dir: bool, print: Print) -> i32 {
    let mut code = 0;
    for t in tools() {
        let need = if t.needed { "needed" } else { "optional" };
        match &t.found {
            Some(v) => print(format!("ok       {:<22} {need:<8} {v}", t.name)),
            None => {
                print(format!("missing  {:<22} {need:<8} {}", t.name, t.how));
                if in_plugin_dir && t.needed {
                    code = 1;
                }
            }
        }
    }
    if in_plugin_dir {
        print(if code == 0 { "tmiki plugin build has what a plugin needs".to_string() } else { "the plugin's build needs what is missing above".to_string() });
    }
    code
}