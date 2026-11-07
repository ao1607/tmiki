//! `tmiki plugin dev <program> [<arg> ...] [-- <command> ...]` (PLUGINS.md
//! 7.1, 5; P8): a development plugin, an executable that speaks the plugin
//! interface on its stdin and stdout (`tmi_plugin_sdk::dev_main`; the wire
//! is `tmi_plugin_dev`'s).
//!
//! Without `--`: starts it, reads its `info`, says what it is and whether
//! `.tmiattributes` maps files to it. With `-- <command>`: runs `tmi
//! <command>` with `TMI_PLUGIN_DEV` naming the program, so that command
//! reads (and writes back) the files `.tmiattributes` maps to the plugin's
//! id with it instead of `.tmiplugins/<id>.wasm`. The records it makes carry
//! `importer.component_sha256 = "dev"`; `commit` refuses them.
//!
//! The front door hands `plugin dev <program> ...` here before argparse's
//! table (`main.rs`): the program's own options (`python -m ...`) are not
//! tmi's. Python has the parser for `-h` only and refuses to run it.

use crate::out::Out;
use crate::CmdResult;
use tmi_plugin_host::{DevCommand, DevOptions, DevProcessBackend, PluginBackend};
use tmi_repo::formats::{self, Formats};
use tmi_repo::{Repo, RepoError};

pub fn dev(argv: &[String], out: &mut Out) -> CmdResult {
    let (words, command) = match argv.iter().position(|a| a == "--") {
        Some(i) => (&argv[..i], Some(&argv[i + 1..])),
        None => (argv, None),
    };
    let Some((program, args)) = words.split_first() else {
        return Err(RepoError::runtime("plugin dev: name the development plugin's program before --"));
    };
    let cmd = DevCommand::new(program, args);
    match command {
        None => describe(cmd, out),
        Some([]) => Err(RepoError::runtime("plugin dev: nothing after --; name a tmi command to run with the plugin, e.g. -- add a.ora")),
        Some(rest) => run(&cmd, rest),
    }
}

/// `tmi <rest>` with `TMI_PLUGIN_DEV` naming `cmd`; its exit code.
fn run(cmd: &DevCommand, rest: &[String]) -> CmdResult {
    let exe = std::env::current_exe().map_err(|e| RepoError::runtime(format!("plugin dev: the tmi executable: {e}")))?;
    let status = std::process::Command::new(exe)
        .args(rest)
        .env(formats::DEV_ENV, tmi_plugin_host::dev::env_value(std::slice::from_ref(cmd)))
        .status()
        .map_err(|e| RepoError::runtime(format!("plugin dev: running tmiki: {e}")))?;
    Ok(status.code().unwrap_or(1))
}

fn describe(cmd: DevCommand, out: &mut Out) -> CmdResult {
    let repo = Repo::open().ok();
    let timeout = match &repo {
        Some(r) => formats::settings(r)?.timeout,
        None => DevOptions::default().timeout,
    };
    let shown = cmd.display();
    let backend = DevProcessBackend::start(cmd.clone(), DevOptions { timeout, ..DevOptions::default() })
        .map_err(|e| RepoError::runtime(format!("plugin dev: {shown}: {e}")))?;
    let info = backend.info().clone();
    let what = if backend.can_write() { "reads and writes layers" } else { "reads only" };
    let formats: Vec<String> = info.formats.iter().map(|f| format!("{}: {}", f.name, f.extensions.join(" "))).collect();
    out.println(format!("development plugin {} {} ({}; {what}), {}", info.id, info.version, formats.join(", "), backend.abi()));
    if let Some(repo) = &repo {
        let mapping = Formats::for_repo(repo);
        for f in &info.formats {
            for ext in &f.extensions {
                let pattern = format!("*.{}", ext.trim_start_matches('.').to_ascii_lowercase());
                let probe = format!("file{}", if ext.starts_with('.') { ext.clone() } else { format!(".{ext}") });
                if mapping.mapping(&probe).plugin_id() == Some(info.id.as_str()) {
                    out.println(format!("{}: {pattern} files are read with it", tmi_repo::attributes::FILE));
                } else {
                    out.println(format!(
                        "{}: no {pattern} files are mapped to plugin '{}'; to read them with it, add the line: {pattern} format=plugin:{}",
                        tmi_repo::attributes::FILE,
                        info.id,
                        info.id
                    ));
                }
            }
        }
    }
    out.println("its records carry component_sha256 \"dev\": tmiki commit refuses them");
    out.println(format!("run a command with it: tmiki plugin dev {shown} -- add <file>"));
    out.println(format!("or for every command of this shell: {}={}", formats::DEV_ENV, tmi_plugin_host::dev::env_value(&[cmd])));
    Ok(0)
}
