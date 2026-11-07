//! tmi's tools for plugin authors (PLUGINS.md 7.1, P8): what `tmiki plugin
//! new | build | test | doctor | install` do, outside any repository.
//!
//! - [`new`]: a plugin's directory from the Rust template (on
//!   tmi-plugin-sdk), with tmi's WIT, PLUGIN_SPEC.md and a sample file the
//!   template reads. Plugins are written in Rust only ([`check_lang`]).
//! - [`build`]: runs cargo (tmi installs and downloads
//!   nothing), then checks the component with the host (its imports against
//!   PLUGINS.md 3.2's list, the link, `info`), drops the custom sections a
//!   component does not need and says its SHA-256.
//! - [`check`]: `tmiki plugin test`, every item of 7.1: native vs wasm (the
//!   comparison; the native side needs the dev RPC), determinism, the
//!   interface's rules, the round trip of the writer, fuzzing, an older
//!   component and the `*.expect.json` files.
//! - [`doctor`]: what a build needs and what is installed.
//! - [`install`]: the user's own store of components
//!   (`~/.tmi/plugins/<id>/<version>.wasm`), which `tmiki plugin add <id>`
//!   reads.

pub mod build;
pub mod cbor;
pub mod check;
pub mod doctor;
pub mod install;
pub mod manifest;
pub mod new;
pub mod sample;
pub mod strip;
pub mod template;
pub mod values;

/// The interface tmi speaks, as `new` writes it into `wit/` (the version
/// is the one in its `package` line).
pub const WIT: &str = include_str!("../../tmi-format/wit/tmi-format.wit");

/// The specification for plugin authors and for an AI writing a plugin
/// (PLUGINS.md 7.2), as `new` writes it.
pub const PLUGIN_SPEC: &str = include_str!("../PLUGIN_SPEC.md");

/// The version of the `package tmi:format@...;` line of [`WIT`].
pub fn wit_version() -> &'static str {
    WIT.lines()
        .find_map(|l| l.trim().strip_prefix("package tmi:format@"))
        .and_then(|v| v.strip_suffix(';'))
        .unwrap_or("0.1.0")
}

/// Whether `id` is a plugin id (`[a-z0-9-]{1,40}`, PLUGINS.md 2.1).
pub fn valid_id(id: &str) -> bool {
    (1..=40).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// `--lang` of `plugin new` and `lang` of `tmi-plugin.toml`: tmi's format
/// plugins are written in Rust (PLUGINS.md Q14, 2026-10-26: Rust's
/// components start in microseconds and weigh a few hundred KB; Python's
/// took milliseconds and megabytes), so `rust` is the one language; any
/// other is refused with that sentence.
pub fn check_lang(lang: &str) -> Result<(), String> {
    if lang == "rust" {
        return Ok(());
    }
    Err(format!(
        "tmi format plugins are written in Rust: there is no {lang} template, and 'tmiki plugin build' builds Rust plugins only \
         ('tmiki plugin new <id>' writes one; PLUGIN_SPEC.md is written for an AI to write it from)"
    ))
}

/// A line printer: stdout lines of a command (the CLI buffers them).
pub type Print<'a> = &'a mut dyn FnMut(String);
