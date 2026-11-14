//! Compiled components on disk (PLUGINS.md 6.2).
//!
//! `<dir>/<key>/<component sha256>.cwasm`, where `<key>` is the SHA-256 of
//! wasmtime's `precompile_compatibility_hash` (its version and compiler
//! flags) and of [`crate::engine::fingerprint`] (the settings tmiki chose).
//! A file is written to a temporary name and renamed into place, so a reader
//! never sees half of one. A `.cwasm` is native code wasmtime runs as it is,
//! so a file is used only when it is sealed: `<sha256>.cwasm.seal` holds
//! HMAC-SHA256 (key: `plugin-cache.key`, 32 random bytes in the user's own
//! `tmiki` directory, never in the cache directory) of the key, the
//! component's sha256 and the file's bytes. A cache directory somebody else
//! can write (`TMI_PLUGIN_CACHE` at a shared place) can replace a file but
//! not make tmiki run it: the seal does not match, the component is compiled
//! again and the file written over. The bytes that were sealed are the
//! bytes that are loaded (read once, checked, deserialized from memory: no
//! window between the check and the use). The default directory is the
//! user's own (`%LOCALAPPDATA%` on Windows, `$XDG_CACHE_HOME` or
//! `~/.cache` elsewhere).

use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use wasmtime::component::Component;
use wasmtime::Engine;

/// The cache directory tmiki uses by default (`TMI_PLUGIN_CACHE` overrides it;
/// an empty value turns the cache off).
pub fn default_dir() -> Option<PathBuf> {
    if let Some(v) = std::env::var_os("TMI_PLUGIN_CACHE") {
        return if v.is_empty() { None } else { Some(PathBuf::from(v)) };
    }
    let base = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_CACHE_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
    };
    base.map(|b| b.join("tmi").join("plugin-cache"))
}

/// Where the user's sealing key lives: `<local data>/tmi/plugin-cache.key`,
/// whatever `TMI_PLUGIN_CACHE` says. `TMI_PLUGIN_CACHE_KEY` names another
/// file (the tests' own).
pub fn key_path() -> Option<PathBuf> {
    if let Some(v) = std::env::var_os("TMI_PLUGIN_CACHE_KEY").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(v));
    }
    let base = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_CACHE_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
    };
    base.map(|b| b.join("tmi").join("plugin-cache.key"))
}

/// The sealing key: read, or made (32 random bytes, owner-only where the OS
/// has modes) on first use.
fn seal_key() -> std::io::Result<Vec<u8>> {
    use std::io::Write;
    let path = key_path().ok_or_else(|| std::io::Error::other("no directory for the cache key"))?;
    let read = |p: &Path| match std::fs::read(p) {
        Ok(k) if k.len() == 32 => Ok(k),
        Ok(_) => Err(std::io::Error::other("the cache key file is damaged")),
        Err(e) => Err(e),
    };
    if let Ok(k) = read(&path) {
        return Ok(k);
    }
    let mut k = [0u8; 32];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut k).map_err(|_| std::io::Error::other("no random bytes for the cache key"))?;
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    match o.open(&path) {
        Ok(mut f) => {
            f.write_all(&k)?;
            Ok(k.to_vec())
        }
        // another process made it first
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => read(&path),
        Err(e) => Err(e),
    }
}

/// What the seal covers: the directory key, the component's sha256, the bytes.
fn seal_message(dir_key: &str, sha256: &str, bytes: &[u8]) -> Vec<u8> {
    [dir_key.as_bytes(), b"\0", sha256.as_bytes(), b"\0", bytes].concat()
}

fn seal_of(key: &[u8], dir_key: &str, sha256: &str, bytes: &[u8]) -> ring::hmac::Tag {
    ring::hmac::sign(&ring::hmac::Key::new(ring::hmac::HMAC_SHA256, key), &seal_message(dir_key, sha256, bytes))
}

/// A `Hasher` that keeps every byte it is given (the compatibility hash is
/// an `impl Hash`; tmiki wants a stable digest of it, not a SipHash).
struct Collect(Vec<u8>);

impl Hasher for Collect {
    fn finish(&self) -> u64 {
        0
    }
    fn write(&mut self, bytes: &[u8]) {
        self.0.extend_from_slice(bytes);
    }
}

/// The directory name for `engine` built with `fingerprint`.
pub fn key(engine: &Engine, fingerprint: &str) -> String {
    let mut c = Collect(Vec::new());
    engine.precompile_compatibility_hash().hash(&mut c);
    let mut d = ring::digest::Context::new(&ring::digest::SHA256);
    d.update(&c.0);
    d.update(b"\0");
    d.update(fingerprint.as_bytes());
    tmi_store::oid::hex(d.finish().as_ref())
}

/// How a component was made ready.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Read from the cache.
    Loaded,
    /// Compiled, and written to the cache.
    Compiled,
    /// Compiled; there is no cache directory.
    CompiledUncached,
    /// Compiled; writing the cache failed (the message says why). The
    /// component works; the next load compiles again.
    CompiledNotWritten(String),
    /// The cached file did not load (another wasmtime build, a damaged
    /// file): compiled again and written over it.
    Replaced(String),
}

#[derive(Clone, Debug)]
pub struct Loaded {
    pub component: Component,
    pub outcome: Outcome,
    /// Compiling, or reading the cached file.
    pub took: Duration,
}

fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().expect("a cache file has a directory");
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(
        ".{}.{}.{:x}.tmp",
        path.file_name().unwrap().to_string_lossy(),
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos())
    ));
    std::fs::write(&tmp, bytes)?;
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            // another process wrote the same file first
            if path.is_file() {
                Ok(())
            } else {
                Err(e)
            }
        }
    }
}

/// The component `bytes` (SHA-256 `sha256`) for `engine`, from `dir` when
/// it was compiled before.
pub fn load_or_compile(engine: &Engine, fingerprint: &str, bytes: &[u8], sha256: &str, dir: Option<&Path>) -> wasmtime::Result<Loaded> {
    let t = Instant::now();
    let Some(dir) = dir else {
        let component = Component::new(engine, bytes)?;
        return Ok(Loaded { component, outcome: Outcome::CompiledUncached, took: t.elapsed() });
    };
    let dir_key = key(engine, fingerprint);
    let path = dir.join(&dir_key).join(format!("{sha256}.cwasm"));
    let seal_path = path.with_extension("cwasm.seal");
    // without a key nothing can be sealed or checked: compile, write nothing
    let sealing = seal_key();
    let mut replaced = None;
    if path.is_file() {
        match &sealing {
            Err(e) => replaced = Some(format!("the cache key is not available ({e}); not using the cached file")),
            Ok(k) => match (std::fs::read(&path), std::fs::read(&seal_path)) {
                (Ok(file), Ok(seal)) => {
                    let hkey = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, k);
                    if ring::hmac::verify(&hkey, &seal_message(&dir_key, sha256, &file), &seal).is_ok() {
                        // SAFETY: these are the bytes this module sealed with the user's own
                        // key (they came from wasmtime's `precompile_component`), checked
                        // just now and loaded from memory, not from the file again;
                        // wasmtime checks the header too.
                        match unsafe { Component::deserialize(engine, &file) } {
                            Ok(component) => return Ok(Loaded { component, outcome: Outcome::Loaded, took: t.elapsed() }),
                            Err(e) => replaced = Some(format!("{e}")),
                        }
                    } else {
                        replaced = Some("the cached file does not match its seal (it was changed, or sealed by another user); compiled again".into());
                    }
                }
                _ => replaced = Some("the cached file has no seal; compiled again".into()),
            },
        }
    }
    let compiled = engine.precompile_component(bytes)?;
    // SAFETY: the bytes `precompile_component` just made with this engine.
    let component = unsafe { Component::deserialize(engine, &compiled)? };
    let written = match &sealing {
        Ok(k) => {
            let seal = seal_of(k, &dir_key, sha256, &compiled);
            // the seal first: a file is never there without the means to check it
            write_atomically(&seal_path, seal.as_ref()).and_then(|()| write_atomically(&path, &compiled))
        }
        Err(e) => Err(std::io::Error::other(format!("the cache key is not available ({e})"))),
    };
    let outcome = match (written, replaced) {
        (Ok(()), None) => Outcome::Compiled,
        (Ok(()), Some(why)) => Outcome::Replaced(why),
        (Err(e), _) => Outcome::CompiledNotWritten(format!("{}: {e}", path.display())),
    };
    Ok(Loaded { component, outcome, took: t.elapsed() })
}
