//! `tmi`, the Rust front door (RUST_MIGRATION.md 3a, 3b-1).
//!
//! The commands it reproduces exactly run here: the read-only ones (log,
//! show, listings, config get, reflog, status) and the writes that need no
//! format import (branch / tag / remote / config changes, switch, checkout,
//! restore of whole files, reset, rm, stash apply / pop / drop / clear, gc,
//! recover). Everything else, and every case one of them does not
//! reproduce, runs `python -m tmi` with the same argv, stdin, stdout,
//! environment and exit code. A write command decides that before it
//! writes anything.

mod args;
mod argparse_errors;
mod argparse_table;
mod casefold_table;
mod cmd_add;
mod cmd_commit;
mod cmd_config;
mod cmd_config_write;
mod cmd_diff;
mod cmd_layer;
mod cmd_merge;
mod cmd_log;
mod cmd_lslayers;
mod cmd_maint;
mod cmd_meta;
mod cmd_plugin;
mod cmd_plugin_dev;
mod cmd_plugin_tools;
mod cmd_refs;
mod cmd_refs_write;
mod cmd_reflog;
mod cmd_show;
mod cmd_stash;
mod cmd_status;
mod cmd_transport;
mod cmd_worktree;
mod forward;
mod help_text;
mod logfilter;
mod gates;
mod interrupt;
mod out;
mod progress;
mod pyenv;
mod transport_errors;
mod writes;

use out::Out;
use std::ffi::OsString;
use tmi_repo::RepoError;

/// The result of a native command.
pub enum Outcome {
    /// Exit code; the output is in the `Out` buffer.
    Done(i32),
    /// Run the command in Python instead.
    Forward(String),
}

pub type CmdResult = Result<i32, RepoError>;

/// Run `argv[1..]` natively if this binary serves it.
fn run_native(argv: &[String], out: &mut Out) -> Outcome {
    // `plugin dev <program> ...`: the program's own arguments (and the tmi
    // command after `--`) are not argparse's (P8; `-h` still is)
    if let [plugin, dev, program, ..] = argv {
        if plugin == "plugin" && dev == "dev" && program != "-h" && program != "--help" {
            return match cmd_plugin_dev::dev(&argv[2..], out) {
                Ok(code) => Outcome::Done(code),
                Err(e) => {
                    out.eprintln(format!("tmiki: error: {}", e.message().unwrap_or_default()));
                    Outcome::Done(1)
                }
            };
        }
    }
    // argparse refuses the command line before anything runs (stage 4b)
    if let Some(text) = argparse_errors::for_argv(argv) {
        out.eprintln(text.strip_suffix('\n').unwrap_or(&text));
        return Outcome::Done(2);
    }
    let Some(command) = argv.first() else {
        return Outcome::Forward("no command".into());
    };
    let rest = &argv[1..];
    if matches!(
        command.as_str(),
        "add" | "commit" | "status" | "mv" | "diff" | "merge" | "restore" | "stash" | "pull" | "clone" | "fetch" | "push" | "plugin"
    ) {
        // tmi_accel's pool: deep stacks for nested rayon joins (tmi-render).
        let _ = rayon::ThreadPoolBuilder::new().stack_size(32 << 20).build_global();
    }
    let result = match command.as_str() {
        "-h" | "--help" | "--version" => cmd_meta::top(argv, out),
        _ if rest.iter().any(|a| a == "-h" || a == "--help") => cmd_meta::help(argv, out),
        "log" => cmd_log::log(rest, out),
        "show" => cmd_show::show(rest, out),
        "branch" => cmd_refs::branch(rest, out),
        "tag" => cmd_refs::tag(rest, out),
        "remote" => cmd_refs::remote(rest, out),
        "config" => cmd_config::config(rest, out),
        "recover" | "reflog" => cmd_reflog::recover(rest, out),
        "status" => cmd_status::status(rest, out),
        "switch" => cmd_worktree::switch(rest, out),
        "checkout" => cmd_worktree::checkout(rest, out),
        "restore" => cmd_worktree::restore(rest, out),
        "reset" => cmd_worktree::reset(rest, out),
        "rm" => cmd_worktree::rm(rest, out),
        "mv" => cmd_worktree::mv(rest, out),
        "stash" => cmd_stash::stash(rest, out),
        "gc" => cmd_maint::gc(rest, out),
        "hook" => cmd_maint::hook(rest, out),
        "ls-layers" => cmd_lslayers::ls_layers(rest, out),
        "add" => cmd_add::add(rest, out),
        "commit" => cmd_commit::commit(rest, out),
        // stage 4b: transport
        "init" => cmd_transport::init(rest, out),
        "clone" => cmd_transport::clone(rest, out),
        "fetch" => cmd_transport::fetch(rest, out),
        "push" => cmd_transport::push(rest, out),
        "pull" => cmd_transport::pull(rest, out),
        "diff" => cmd_diff::diff(rest, out),
        "merge" => cmd_merge::merge(rest, out),
        // PLUGINS.md P6: the executable's own (Python refuses it)
        "plugin" => cmd_plugin::plugin(rest, out),
        _ => return Outcome::Forward(format!("{command} is not native")),
    };
    match result {
        Ok(code) => Outcome::Done(code),
        Err(RepoError::Forward(why)) if tmi_repo::fsio::mutated() => {
            // Too late to hand over: the repository has changed.
            out.eprintln(format!("tmiki: error: {why} (the native command had already started writing)"));
            Outcome::Done(1)
        }
        Err(RepoError::Forward(why)) => Outcome::Forward(why),
        Err(e) => {
            // tmi.cli.main: print(f"tmiki: error: {exc}", file=sys.stderr); return 1
            out.eprintln(format!("tmiki: error: {}", e.message().unwrap_or_default()));
            Outcome::Done(1)
        }
    }
}

fn main() {
    let argv: Vec<OsString> = std::env::args_os().collect();
    if std::env::var_os(forward::GUARD).is_some() {
        eprintln!(
            "tmiki: error: the Python fallback started the tmi executable again ({} is set); \
             point TMI_PYTHON at a Python with tmi installed",
            forward::GUARD
        );
        std::process::exit(1);
    }
    let native_off = std::env::var("TMI_NATIVE").is_ok_and(|v| v.trim() == "0");
    let utf8: Option<Vec<String>> = argv[1..].iter().map(|a| a.clone().into_string().ok()).collect();
    if let (false, Some(args)) = (native_off, utf8) {
        tmi_repo::hooks::set_python(pyenv::python_for_hooks());
        interrupt::install();
        // a question on the terminal holds the progress spinner (issue #48)
        tmi_repo::trust::set_prompt_hooks(progress::pause_active, progress::resume_active);
        let mut out = Out::default();
        let outcome = run_native(&args, &mut out);
        // index.lock (issue #35) is the native command's alone: gone before
        // the command ends or goes to Python, which takes its own.
        tmi_repo::lockfile::release_all();
        match outcome {
            Outcome::Done(code) if out.writable() || tmi_repo::fsio::mutated() => {
                forward::log_invocation(&format!("native\t{}", args.first().map_or("", String::as_str)));
                out.flush();
                std::process::exit(code);
            }
            Outcome::Done(_) => forward::log_reason("output needs Python's encoder"),
            Outcome::Forward(why) => forward::log_reason(&why),
        }
    }
    forward::forward(&argv);
}
