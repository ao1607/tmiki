//! The test plugins of `rust/plugins` as components: built once per
//! process with `cargo build --release --target wasm32-wasip2` in that
//! workspace (its own target directory, so this never waits on the cargo
//! that runs the tests). Needs `rustup target add wasm32-wasip2`.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// `rust/plugins`.
pub fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins")
}

fn build() -> Result<PathBuf, String> {
    let dir = workspace();
    // TMI_PLUGINS_TARGET_DIR: elsewhere than rust/plugins/target
    let target = std::env::var_os("TMI_PLUGINS_TARGET_DIR").filter(|v| !v.is_empty()).map_or_else(|| dir.join("target"), PathBuf::from);
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let out = std::process::Command::new(cargo)
        .current_dir(&dir)
        .args(["build", "--release", "--target", "wasm32-wasip2", "--workspace", "--target-dir"])
        .arg(&target)
        .env_remove("CARGO_TARGET_DIR")
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .output()
        .map_err(|e| format!("running cargo: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "building rust/plugins for wasm32-wasip2 failed (is the target installed? `rustup target add wasm32-wasip2`):\n{}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(target.join("wasm32-wasip2").join("release"))
}

/// The ORA plugin as a development plugin (`ora-dev`, a native debug build
/// of the same code on tmi-plugin-sdk; `tmiki plugin dev`), built once per
/// process into `<target>/native`.
pub fn ora_dev() -> PathBuf {
    static EXE: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    EXE.get_or_init(|| {
        let dir = workspace();
        let target = std::env::var_os("TMI_PLUGINS_TARGET_DIR").filter(|v| !v.is_empty()).map_or_else(|| dir.join("target"), PathBuf::from).join("native");
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let out = std::process::Command::new(cargo)
            .current_dir(&dir)
            .args(["build", "-p", "tmi-plugin-ora", "--features", "dev", "--bin", "ora-dev", "--target-dir"])
            .arg(&target)
            .env_remove("CARGO_TARGET_DIR")
            .env_remove("RUSTFLAGS")
            .env_remove("CARGO_ENCODED_RUSTFLAGS")
            .output()
            .map_err(|e| format!("running cargo: {e}"))?;
        if !out.status.success() {
            return Err(format!("building ora-dev failed:\n{}", String::from_utf8_lossy(&out.stderr)));
        }
        Ok(target.join("debug").join(format!("ora-dev{}", std::env::consts::EXE_SUFFIX)))
    })
    .clone()
    .unwrap_or_else(|e| panic!("{e}"))
}

/// `bad-dev`, the development plugin that misbehaves on purpose
/// (`rust/plugins/bad-dev`; hardening_tests.rs), built like [`ora_dev`].
pub fn bad_dev() -> PathBuf {
    static EXE: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    EXE.get_or_init(|| {
        let dir = workspace();
        let target = std::env::var_os("TMI_PLUGINS_TARGET_DIR").filter(|v| !v.is_empty()).map_or_else(|| dir.join("target"), PathBuf::from).join("native");
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let out = std::process::Command::new(cargo)
            .current_dir(&dir)
            .args(["build", "-p", "tmi-plugin-bad-dev", "--bin", "bad-dev", "--target-dir"])
            .arg(&target)
            .env_remove("CARGO_TARGET_DIR")
            .env_remove("RUSTFLAGS")
            .env_remove("CARGO_ENCODED_RUSTFLAGS")
            .output()
            .map_err(|e| format!("running cargo: {e}"))?;
        if !out.status.success() {
            return Err(format!("building bad-dev failed:\n{}", String::from_utf8_lossy(&out.stderr)));
        }
        Ok(target.join("debug").join(format!("bad-dev{}", std::env::consts::EXE_SUFFIX)))
    })
    .clone()
    .unwrap_or_else(|e| panic!("{e}"))
}

/// The component of crate `name` (`tmi-plugin-ora`, ...).
pub fn component(name: &str) -> Vec<u8> {
    static DIR: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    let dir = DIR.get_or_init(build).as_ref().unwrap_or_else(|e| panic!("{e}"));
    let path = dir.join(format!("{}.wasm", name.replace('-', "_")));
    std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}
