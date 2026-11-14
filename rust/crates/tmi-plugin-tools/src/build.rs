//! `tmiki plugin build [dir]` (PLUGINS.md 7.1): cargo makes the component,
//! then tmiki checks and finishes it. Plugins are written in Rust (Q14).
//!
//! 1. `cargo build --release --target wasm32-wasip2` (its own
//!    `--target-dir <dir>/target`).
//! 2. The custom sections a component does not need go ([`crate::strip`]).
//! 3. The host loads it as `tmiki plugin add` will: compiled (the compile
//!    validates it, and the pooling slots sized for Rust components refuse
//!    a larger one), its imports against the list of PLUGINS.md 3.2, its
//!    exports against the interface, `info` called and checked. The id
//!    must be the manifest's; the world must match `can-write`.
//! 4. The component is written to the manifest's `output`, with its size
//!    and SHA-256.
//! 5. With a `[dev] command`, what it runs is made too ([`build_dev`]).
//!
//! tmiki runs the tools that are installed and installs or downloads nothing
//! (`tmiki plugin doctor` says what is missing). component-init (an
//! initialised snapshot) is not done: it made the Rust ORA's instances
//! slower (6.2, P9).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use tmi_plugin_host::{PluginHost, Settings};

use crate::manifest::Manifest;
use crate::{doctor, Print};

pub struct Built {
    pub path: PathBuf,
    pub sha256: String,
    pub id: String,
    pub version: String,
}

fn run_tool(mut cmd: Command, what: &str) -> Result<(), String> {
    // the tool's own output goes to stderr (tmiki's stdout is its result)
    cmd.stdout(Stdio::from(std::io::stderr())).stderr(Stdio::inherit());
    let status = cmd.status().map_err(|e| format!("running {what}: {e}"))?;
    if !status.success() {
        return Err(format!("{what} failed ({status})"));
    }
    Ok(())
}

/// The package name of the Cargo.toml in `dir`.
fn crate_name(dir: &Path) -> Result<String, String> {
    let path = dir.join("Cargo.toml");
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let keys = crate::manifest::parse(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    keys.get("package.name").cloned().ok_or_else(|| format!("{}: no [package] name", path.display()))
}

/// The raw component: what the tool wrote.
fn compile(m: &Manifest) -> Result<Vec<u8>, String> {
    let missing = doctor::missing();
    if !missing.is_empty() {
        let list: Vec<String> = missing.iter().map(|(n, how)| format!("{n} ({how})")).collect();
        return Err(format!("building a plugin needs {}; 'tmiki plugin doctor' lists what is installed", list.join(", ")));
    }
    let name = crate_name(&m.dir)?;
    let target = m.dir.join("target");
    let mut cmd = Command::new(doctor::cargo().expect("checked above"));
    cmd.current_dir(&m.dir)
        .args(["build", "--release", "--target", "wasm32-wasip2", "--target-dir"])
        .arg(&target)
        .env_remove("CARGO_TARGET_DIR")
        .env_remove("CARGO_BUILD_TARGET")
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_ENCODED_RUSTFLAGS");
    run_tool(cmd, "cargo build --release --target wasm32-wasip2")?;
    let out = target.join("wasm32-wasip2").join("release").join(format!("{}.wasm", name.replace('-', "_")));
    std::fs::read(&out).map_err(|e| format!("{}: {e} (cargo built no component there; is the crate a cdylib?)", out.display()))
}

/// What `tmiki plugin dev` / `tmiki plugin test --native` run (the manifest's
/// `[dev] command`): the crate built natively with its feature `dev`
/// (`target/debug/<id>-dev`). Not done for a directory without
/// `[dev] command`.
fn build_dev(m: &Manifest, print: Print) -> Result<(), String> {
    if m.dev_command.is_none() {
        return Ok(());
    }
    let mut cmd = Command::new(doctor::cargo().expect("checked above"));
    cmd.current_dir(&m.dir)
        .args(["build", "--features", "dev", "--bins", "--target-dir"])
        .arg(m.dir.join("target"))
        .env_remove("CARGO_TARGET_DIR")
        .env_remove("CARGO_BUILD_TARGET")
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_ENCODED_RUSTFLAGS");
    run_tool(cmd, "cargo build --features dev")?;
    print("  development plugin: target/debug (cargo build --features dev)".into());
    Ok(())
}

/// The most files and bytes [`consent_digest`] reads.
const DIGEST_MAX_FILES: usize = 20_000;
const DIGEST_MAX_BYTES: u64 = 512 << 20;

/// What a consent to build `m.dir` is about: every file cargo could run or
/// read there (Cargo.toml, Cargo.lock, build.rs, macros, `.cargo/config.toml`,
/// `rust-toolchain.toml`, path dependencies inside it, the sources), by
/// relative path and SHA-256, so any change is asked about again. Not
/// `target/`, `.git/`, the component the build writes or the corpus's
/// `*.expect.json`. A link is recorded by where it points, not followed.
pub fn consent_digest(m: &Manifest) -> Result<Vec<u8>, String> {
    let mut entries: Vec<(String, String)> = Vec::new();
    let mut bytes = 0u64;
    let mut stack = vec![m.dir.clone()];
    while let Some(d) = stack.pop() {
        let list = std::fs::read_dir(&d).map_err(|e| format!("{}: {e}", d.display()))?;
        for e in list {
            let e = e.map_err(|e| format!("{}: {e}", d.display()))?;
            let path = e.path();
            let rel = path.strip_prefix(&m.dir).unwrap_or(&path).to_string_lossy().replace('\\', "/");
            // what tmiki writes there itself: the component, `test --update`'s expectations
            let expectation = path.starts_with(&m.corpus) && rel.ends_with(crate::check::EXPECT_SUFFIX);
            if rel == "target" || rel == ".git" || path == m.output || expectation {
                continue;
            }
            let ft = e.file_type().map_err(|e| format!("{}: {e}", path.display()))?;
            let what = if ft.is_symlink() || (!ft.is_dir() && !ft.is_file()) {
                format!("link {}", std::fs::read_link(&path).map(|t| t.display().to_string()).unwrap_or_default())
            } else if ft.is_dir() {
                stack.push(path);
                continue;
            } else {
                let data = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
                bytes += data.len() as u64;
                tmi_store::oid::sha256_hex(&data)
            };
            entries.push((rel, what));
            if entries.len() > DIGEST_MAX_FILES || bytes > DIGEST_MAX_BYTES {
                return Err(format!(
                    "{}: more than {DIGEST_MAX_FILES} files or {} MiB besides target/: too much to vet before building it",
                    m.dir.display(),
                    DIGEST_MAX_BYTES >> 20
                ));
            }
        }
    }
    entries.sort();
    let mut out = Vec::new();
    for (rel, what) in entries {
        out.extend_from_slice(format!("{rel}\0{what}\n").as_bytes());
    }
    Ok(out)
}

/// Checks a component as `plugin add` will, and says what it is.
pub fn inspect(bytes: &[u8], settings: Settings, print: Print) -> Result<tmi_plugin_host::Plugin, String> {
    let host = PluginHost::new(settings).map_err(|e| e.to_string())?;
    let plugin = host.load(bytes).map_err(|e| e.to_string())?;
    let importer = plugin.importer();
    let mut imports: Vec<String> = plugin.imports.iter().map(|i| i.split('@').next().unwrap_or(i).to_string()).collect();
    imports.sort();
    imports.dedup();
    print(format!(
        "plugin {} {}: {}, {}",
        plugin.info.id,
        plugin.info.version,
        importer.abi,
        if plugin.can_write() { "reads and writes" } else { "reads" }
    ));
    for f in &plugin.info.formats {
        print(format!("  format {:?}: {}", f.name, f.extensions.join(" ")));
    }
    print(format!("  imports: {}", imports.join(", ")));
    Ok(plugin)
}

pub fn run(dir: &Path, settings: Settings, print: Print) -> Result<Built, String> {
    let m = Manifest::read(dir)?;
    eprintln!("building plugin {} ...", m.id);
    let raw = compile(&m)?;
    let (bytes, removed) = crate::strip::strip(&raw).map_err(|e| format!("the tool's output: {e}"))?;
    let plugin = inspect(&bytes, settings, print)?;
    if plugin.info.id != m.id {
        return Err(format!("the component says it is plugin {:?}, but {} says {:?}", plugin.info.id, crate::manifest::FILE, m.id));
    }
    let writes = m.world == "format-plugin";
    if writes != plugin.can_write() {
        return Err(if writes {
            format!("{} says world format-plugin, but the component does not write (its info says can-write: {}, and a writer export is needed too)", crate::manifest::FILE, plugin.info.can_write)
        } else {
            format!("{} says world format-reader, but the component's info says can-write: true", crate::manifest::FILE)
        });
    }
    if !removed.is_empty() {
        let total: usize = removed.iter().map(|(_, n)| n).sum();
        let mut names: Vec<&str> = removed.iter().map(|(n, _)| n.as_str()).collect();
        names.sort();
        names.dedup();
        print(format!("  removed custom sections: {} ({total} bytes)", names.join(", ")));
    }
    if let Some(p) = m.output.parent() {
        std::fs::create_dir_all(p).map_err(|e| format!("{}: {e}", p.display()))?;
    }
    std::fs::write(&m.output, &bytes).map_err(|e| format!("{}: {e}", m.output.display()))?;
    let rel = m.output.strip_prefix(dir).unwrap_or(&m.output).to_string_lossy().replace('\\', "/");
    print(format!("built {rel} ({} bytes, sha256 {})", bytes.len(), plugin.sha256));
    build_dev(&m, print)?;
    Ok(Built { path: m.output.clone(), sha256: plugin.sha256.clone(), id: plugin.info.id.clone(), version: plugin.info.version.clone() })
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_consent_to_build_does_not_cover_a_changed_build_script_or_cargo_config() {
        let dir = std::env::temp_dir().join(format!("tmi-plugin-consent-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join(crate::manifest::FILE), "[plugin]\nid = \"x\"\n[build]\noutput = \"x.wasm\"\n").unwrap();
        std::fs::write(dir.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        std::fs::write(dir.join("src/lib.rs"), "").unwrap();
        let m = crate::manifest::Manifest::read(&dir).unwrap();
        let digest = || super::consent_digest(&m).unwrap();
        let granted = digest();
        // what the build makes does not count
        std::fs::create_dir_all(dir.join("target/debug")).unwrap();
        std::fs::write(dir.join("target/debug/x-dev.exe"), b"built").unwrap();
        std::fs::write(dir.join("x.wasm"), b"component").unwrap();
        assert_eq!(digest(), granted);
        // code cargo runs does
        std::fs::write(dir.join("build.rs"), "fn main() {}").unwrap();
        assert_ne!(digest(), granted);
        std::fs::remove_file(dir.join("build.rs")).unwrap();
        assert_eq!(digest(), granted);
        std::fs::create_dir_all(dir.join(".cargo")).unwrap();
        std::fs::write(dir.join(".cargo/config.toml"), "[build]\nrustc-wrapper = \"evil\"\n").unwrap();
        assert_ne!(digest(), granted);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
