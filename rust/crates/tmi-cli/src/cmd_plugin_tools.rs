//! `tmiki plugin new | build | test | doctor | install` (PLUGINS.md 7.1, P8):
//! the plugin author's tools, run outside any repository (the work is
//! `tmi_plugin_tools`'s; `plugin dev` is cmd_plugin_dev.rs). Python has their parsers and refuses them, as it
//! does every `plugin` command.

use std::path::{Path, PathBuf};

use crate::args::{self, flag, opt, positional};
use crate::out::Out;
use crate::CmdResult;
use clap::Command;
use tmi_plugin_tools::{check, manifest::Manifest};
use tmi_repo::trust::{self, DirAction};
use tmi_repo::RepoError;

fn err(e: String) -> RepoError {
    RepoError::runtime(e)
}

/// `tmiki plugin new <id> [--lang rust] [--writer]`: plugins are written in
/// Rust (PLUGINS.md Q14); `--lang` of another language says so.
pub fn new(argv: &[String], out: &mut Out) -> CmdResult {
    let cmd = Command::new("new").arg(positional("id", false)).arg(opt("lang", None, Some("lang"))).arg(flag("writer", None, Some("writer")));
    let m = args::parse(cmd, argv, &["id"])?;
    let Some(id) = args::get_str(&m, "id") else { return Err(RepoError::forward("plugin new without an id (argparse's error)")) };
    if let Some(lang) = args::get_str(&m, "lang") {
        tmi_plugin_tools::check_lang(&lang).map_err(err)?;
    }
    let o = tmi_plugin_tools::new::Options { id, writer: args::get_flag(&m, "writer") };
    let cwd = std::env::current_dir().map_err(|e| err(e.to_string()))?;
    let dir = tmi_plugin_tools::new::run(&cwd, &o, &mut |l| out.println(l)).map_err(err)?;
    // tmi wrote this directory itself: building it is not asking about somebody else's code
    if let Ok(digest) = Manifest::read(&dir).and_then(|m| tmi_plugin_tools::build::consent_digest(&m)) {
        let _ = trust::grant_dir(&dir, DirAction::Build, &digest, "plugin new");
    }
    Ok(0)
}

/// `tmiki plugin build [dir]`.
pub fn build(argv: &[String], out: &mut Out) -> CmdResult {
    let m = args::parse(Command::new("build").arg(positional("dir", true)), argv, &["dir"])?;
    let dirs = args::get_many(&m, "dir");
    if dirs.len() > 1 {
        return Err(RepoError::forward("plugin build with more than one directory (argparse's error)"));
    }
    let dir = dirs.first().map_or_else(|| PathBuf::from("."), PathBuf::from);
    // cargo runs the directory's build scripts and macros: its author's code (PLUGINS.md 11)
    // the consent covers every file cargo may run (build.rs, .cargo/config.toml, ...), not the manifest alone
    let manifest = Manifest::read(&dir).map_err(err)?;
    let before = tmi_plugin_tools::build::consent_digest(&manifest).map_err(err)?;
    trust::require_dir_consent(&dir, DirAction::Build, &before, tmi_plugin_tools::manifest::FILE)?;
    tmi_plugin_tools::build::run(&dir, tmi_plugin_host::Settings::default(), &mut |l| out.println(l)).map_err(err)?;
    // what the allowed build itself changed (a new Cargo.lock) is not asked about again
    if let Ok(after) = tmi_plugin_tools::build::consent_digest(&manifest) {
        let _ = trust::grant_dir(&dir, DirAction::Build, &after, "plugin build");
    }
    // the development plugin was just made by the build the user allowed: no second question
    if let Some(text) = &manifest.dev_command {
        if let Ok(cmd) = tmi_plugin_host::dev::parse_command(text) {
            let cmd = cmd.in_dir(&manifest.dir);
            let _ = trust::grant_dir(&manifest.dir, DirAction::DevCommand, &dev_content(&cmd), &cmd.display());
        }
    }
    Ok(0)
}

/// `tmiki plugin test [dir|component.wasm] [--corpus DIR] [--native]
/// [--native-command CMD] [--against OLD] [--update] [--fuzz N] [--dump]`.
pub fn test(argv: &[String], out: &mut Out) -> CmdResult {
    let cmd = Command::new("test")
        .arg(positional("path", true))
        .arg(opt("corpus", None, Some("corpus")))
        .arg(flag("native", None, Some("native")))
        .arg(opt("native_command", None, Some("native-command")))
        .arg(opt("against", None, Some("against")))
        .arg(flag("update", None, Some("update")))
        .arg(opt("fuzz", None, Some("fuzz")))
        .arg(flag("dump", None, Some("dump")));
    let m = args::parse(cmd, argv, &["path"])?;
    let paths = args::get_many(&m, "path");
    if paths.len() > 1 {
        return Err(RepoError::forward("plugin test with more than one path (argparse's error)"));
    }
    let fuzz = match args::get_int(&m, "fuzz")? {
        Some(n) if n < 0 => return Err(err(format!("--fuzz {n}: a number of files, 0 or more"))),
        Some(n) => n as u64,
        None => 0,
    };
    let path = paths.first().map_or_else(|| PathBuf::from("."), PathBuf::from);
    let manifest = if path.is_dir() { Some(Manifest::read(&path).map_err(err)?) } else { None };
    let (component, corpus) = if let Some(manifest) = &manifest {
        (manifest.output.clone(), manifest.corpus.clone())
    } else {
        // a component on its own: the corpus is named
        let Some(corpus) = args::get_str(&m, "corpus") else {
            return Err(err(format!(
                "{}: a component on its own needs --corpus <dir> (in a plugin directory, tmi-plugin.toml names it)",
                path.display()
            )));
        };
        (path.clone(), PathBuf::from(corpus))
    };
    if let (Some(manifest), None) = (&manifest, args::get_str(&m, "corpus")) {
        // the directory's own corpus stays in it, also through a linked tests/
        let inside = match (std::fs::canonicalize(&manifest.dir), std::fs::canonicalize(&corpus)) {
            (Ok(d), Ok(c)) => c.starts_with(d),
            _ => true, // a missing corpus is reported where it is read
        };
        if !inside {
            return Err(err(format!("{}: the corpus leads outside the plugin directory (a link); give one with --corpus", corpus.display())));
        }
    }
    let corpus = args::get_str(&m, "corpus").map_or(corpus, PathBuf::from);
    let o = check::Options {
        component,
        corpus,
        native: native_command(args::get_flag(&m, "native"), args::get_str(&m, "native_command"), manifest.as_ref())?,
        against: args::get_str(&m, "against").map(PathBuf::from),
        update: args::get_flag(&m, "update"),
        fuzz,
        dump: args::get_flag(&m, "dump"),
        settings: tmi_plugin_host::Settings::default(),
    };
    let report = check::run(&o, &mut |l| out.println(l)).map_err(err)?;
    for line in report.lines() {
        out.println(line);
    }
    Ok(if report.failures() > 0 { 1 } else { 0 })
}

/// `--native-command CMD`: that command (words, or a JSON array); `--native`:
/// the plugin directory's `dev.command`, run in the directory (PLUGINS.md
/// Q12: a flag, so `--native .` keeps `.` as the path).
fn native_command(native: bool, given: Option<String>, manifest: Option<&Manifest>) -> Result<Option<tmi_plugin_host::DevCommand>, RepoError> {
    if let Some(given) = given {
        return tmi_plugin_host::dev::parse_command(&given).map(Some).map_err(|e| err(format!("--native-command: {e}")));
    }
    if !native {
        return Ok(None);
    }
    let Some(m) = manifest else {
        return Err(err("--native needs a plugin directory whose tmi-plugin.toml has [dev] command (or give one: --native-command <command>)".into()));
    };
    let Some(text) = &m.dev_command else {
        return Err(err(format!("--native: {} has no [dev] command; give one: --native-command <command>", m.dir.join(tmi_plugin_tools::manifest::FILE).display())));
    };
    let cmd = tmi_plugin_host::dev::parse_command(text).map_err(|e| err(format!("{}: dev.command: {e}", tmi_plugin_tools::manifest::FILE)))?;
    let cmd = cmd.in_dir(&m.dir);
    // the manifest's command is its author's program (it may be an executable that came
    // with the directory): run only after a yes for this directory and this content
    trust::require_dir_consent(&m.dir, DirAction::DevCommand, &dev_content(&cmd), &cmd.display())?;
    Ok(Some(cmd))
}

/// What a consent to run `cmd` is about: the command line and, when the
/// program is a file (a bundled executable, `target/debug/x-dev`), its bytes' hash.
fn dev_content(cmd: &tmi_plugin_host::DevCommand) -> Vec<u8> {
    let mut content = cmd.display().into_bytes();
    content.push(0);
    match std::fs::read(program_file(&cmd.program_path())) {
        Ok(bytes) => content.extend_from_slice(tmi_store::oid::sha256_hex(&bytes).as_bytes()),
        // not there yet: a file that appears later is asked about then
        Err(_) => content.extend_from_slice(b"no file"),
    }
    content
}

/// The file Windows starts for `program`: `x-dev` with no extension runs
/// `x-dev.exe` (std's `Command`), so that is the file whose bytes count.
fn program_file(program: &std::ffi::OsStr) -> PathBuf {
    let p = PathBuf::from(program);
    if cfg!(windows) && p.extension().is_none() {
        let exe = p.with_extension("exe");
        if exe.is_file() {
            return exe;
        }
    }
    p
}

/// `tmiki plugin doctor [dir]`.
pub fn doctor(argv: &[String], out: &mut Out) -> CmdResult {
    let m = args::parse(Command::new("doctor").arg(positional("dir", true)), argv, &["dir"])?;
    let dirs = args::get_many(&m, "dir");
    let dir = dirs.first().map_or_else(|| PathBuf::from("."), PathBuf::from);
    let in_plugin_dir = if dir.join(tmi_plugin_tools::manifest::FILE).is_file() {
        Manifest::read(&dir).map_err(err)?;
        true
    } else if !dirs.is_empty() {
        return Err(err(format!("{}: no {}", dir.display(), tmi_plugin_tools::manifest::FILE)));
    } else {
        false
    };
    Ok(tmi_plugin_tools::doctor::run(in_plugin_dir, &mut |l| out.println(l)))
}

/// `tmiki plugin install <file.wasm>`: into the user's store, nothing else.
pub fn install(argv: &[String], out: &mut Out) -> CmdResult {
    let m = args::parse(Command::new("install").arg(positional("file", false)), argv, &["file"])?;
    let Some(file) = args::get_str(&m, "file") else { return Err(RepoError::forward("plugin install without a file (argparse's error)")) };
    let bytes = std::fs::read(&file).map_err(|e| err(format!("{file}: {e}")))?;
    let host = tmi_plugin_host::PluginHost::new(tmi_plugin_host::Settings::default()).map_err(|e| err(e.to_string()))?;
    let plugin = host.load(&bytes).map_err(|e| err(format!("{file}: {e}")))?;
    let home = tmi_plugin_tools::install::home().map_err(err)?;
    let (path, replaced) = tmi_plugin_tools::install::install(&home, &plugin.info.id, &plugin.info.version, &bytes).map_err(err)?;
    out.println(format!(
        "installed plugin {} {} (sha256 {}...) as {}{}",
        plugin.info.id,
        plugin.info.version,
        &plugin.sha256[..12],
        path.display(),
        if replaced { ", replacing a different component of the same version" } else { "" }
    ));
    out.println(format!("in a repository, 'tmiki plugin add {}' puts it there", plugin.info.id));
    Ok(0)
}

/// `tmiki plugin add`'s argument: a file, else an installed plugin
/// (`<id>` or `<id>@<version>`, PLUGINS.md 7.1).
pub fn component_path(spec: &str) -> Result<PathBuf, RepoError> {
    // a bare name is an installed plugin even when a file of that name is
    // here (a repository's file `ora` must not stand in for plugin ora)
    let file_like = spec.contains(['/', '\\']) || spec.to_ascii_lowercase().ends_with(".wasm");
    if file_like || (Path::new(spec).exists() && !tmi_plugin_tools::valid_id(spec.split('@').next().unwrap_or(spec))) {
        return Ok(PathBuf::from(spec));
    }
    let id = spec.split('@').next().unwrap_or(spec);
    if !tmi_plugin_tools::valid_id(id) {
        return Err(err(format!("{spec}: no such file")));
    }
    let home = tmi_plugin_tools::install::home().map_err(err)?;
    tmi_plugin_tools::install::find(&home, spec).map_err(err)
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_file_named_like_a_plugin_does_not_stand_in_for_the_installed_one() {
        // a repository's file `<id>` in the current directory, and no such plugin installed
        let id = format!("zz-planted-{}", std::process::id());
        std::fs::write(&id, b"\0asm").unwrap();
        let got = super::component_path(&id);
        let _ = std::fs::remove_file(&id);
        assert!(got.as_ref().map_or(true, |p| p != std::path::Path::new(&id)), "{got:?}");
        // a path or a .wasm name is a file
        assert_eq!(super::component_path("./x.wasm").unwrap(), std::path::PathBuf::from("./x.wasm"));
        assert_eq!(super::component_path("x.wasm").unwrap(), std::path::PathBuf::from("x.wasm"));
    }

    #[test]
    fn the_dev_command_consent_covers_the_exe_windows_runs() {
        let dir = std::env::temp_dir().join(format!("tmi-dev-exe-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("target/debug")).unwrap();
        let cmd = tmi_plugin_host::dev::parse_command("target/debug/x-dev").unwrap().in_dir(&dir);
        let exe = if cfg!(windows) { "target/debug/x-dev.exe" } else { "target/debug/x-dev" };
        std::fs::write(dir.join(exe), b"granted").unwrap();
        let granted = super::dev_content(&cmd);
        std::fs::write(dir.join(exe), b"swapped").unwrap();
        assert_ne!(super::dev_content(&cmd), granted, "a swapped program must be asked about again");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
