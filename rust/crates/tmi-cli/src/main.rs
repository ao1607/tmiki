//! `tmiki`, the command line: every command runs here.
//!
//! A command line it cannot serve (a rare argument form, a corrupted
//! record) stops with "tmiki: error: not supported: <why>" before it writes
//! anything; argument errors exit 2 as argparse's did.
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
mod help_text;
mod logfilter;
mod gates;
mod interrupt;
mod out;
mod progress;
mod transport_errors;
mod writes;

use out::Out;
use std::ffi::OsString;
use tmi_repo::RepoError;

pub type CmdResult = Result<i32, RepoError>;

/// What a command line this executable does not take on prints.
fn unsupported(out: &mut Out, why: &str) -> i32 {
    if why.contains("argparse") || why.starts_with("argument error") || why.starts_with("invalid choice") {
        out.eprintln(format!("tmiki: error: these arguments are not accepted ({why}); see 'tmiki -h'"));
        return 2;
    }
    out.eprintln(format!("tmiki: error: not supported: {why}"));
    1
}

/// Run the command `argv` (without the program name): its exit code, the
/// output held in `out`.
fn run(argv: &[String], out: &mut Out) -> i32 {
    // `plugin dev <program> ...`: the program's own arguments (and the tmiki
    // command after `--`) are not argparse's (P8; `-h` still is)
    if let [plugin, dev, program, ..] = argv {
        if plugin == "plugin" && dev == "dev" && program != "-h" && program != "--help" {
            return match cmd_plugin_dev::dev(&argv[2..], out) {
                Ok(code) => code,
                Err(e) => {
                    out.eprintln(format!("tmiki: error: {}", e.message().unwrap_or_default()));
                    1
                }
            };
        }
    }
    // argparse's refusals, before anything runs (stage 4b)
    if let Some(text) = argparse_errors::for_argv(argv) {
        out.eprintln(text.strip_suffix('\n').unwrap_or(&text));
        return 2;
    }
    let Some(command) = argv.first() else {
        return unsupported(out, "no command");
    };
    let rest = &argv[1..];
    if matches!(
        command.as_str(),
        "add" | "commit" | "status" | "mv" | "diff" | "merge" | "restore" | "stash" | "pull" | "clone" | "fetch" | "push" | "plugin"
    ) {
        // deep stacks for nested rayon joins (tmi-render)
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
        "init" => cmd_transport::init(rest, out),
        "clone" => cmd_transport::clone(rest, out),
        "fetch" => cmd_transport::fetch(rest, out),
        "push" => cmd_transport::push(rest, out),
        "pull" => cmd_transport::pull(rest, out),
        "diff" => cmd_diff::diff(rest, out),
        "merge" => cmd_merge::merge(rest, out),
        "plugin" => cmd_plugin::plugin(rest, out),
        _ => return unsupported(out, &format!("the command {command}")),
    };
    match result {
        Ok(code) => code,
        Err(RepoError::Forward(why)) if tmi_repo::fsio::mutated() => {
            out.eprintln(format!("tmiki: error: not supported: {why} (the command had already started writing)"));
            1
        }
        Err(RepoError::Forward(why)) => unsupported(out, &why),
        Err(e) => {
            out.eprintln(format!("tmiki: error: {}", e.message().unwrap_or_default()));
            1
        }
    }
}

fn main() {
    let argv: Vec<OsString> = std::env::args_os().skip(1).collect();
    let Some(args) = argv.iter().map(|a| a.clone().into_string().ok()).collect::<Option<Vec<String>>>() else {
        eprintln!("tmiki: error: not supported: an argument that is not valid Unicode");
        std::process::exit(1);
    };
    interrupt::install();
    // a question on the terminal holds the progress spinner (issue #48)
    tmi_repo::trust::set_prompt_hooks(progress::pause_active, progress::resume_active);
    let mut out = Out::default();
    let code = run(&args, &mut out);
    // index.lock (issue #35) is the command's alone: gone before it ends
    tmi_repo::lockfile::release_all();
    out.flush();
    std::process::exit(code);
}