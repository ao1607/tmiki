//! `tmiki gc` and `tmiki hook list / run` (cli.py `cmd_gc`, `cmd_hook_list`,
//! `cmd_hook_run`).

use crate::args::{self, opt, positional};
use crate::cmd_log::fwd;
use crate::out::Out;
use crate::writes;
use crate::CmdResult;
use clap::Command;
use tmi_repo::hooks;
use tmi_repo::repo::py_repr_ascii;
use tmi_repo::{Repo, RepoError};

/// cli.py `_parse_prune_age(spec)`.
fn parse_prune_age(spec: &str) -> Result<i64, RepoError> {
    if !spec.is_ascii() {
        return Err(fwd("non-ASCII --prune"));
    }
    let spec = tmi_repo::repo::py_strip(spec).to_lowercase();
    if spec == "now" {
        return Ok(0);
    }
    let plain = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let days = if let Some(num) = spec.strip_suffix('d') {
        if plain(num) {
            num.parse::<i64>().ok()
        } else if num.trim().is_empty() || num.bytes().all(|b| b.is_ascii_alphabetic()) {
            None // int() raises
        } else {
            return Err(fwd("--prune text int() may read"));
        }
    } else if plain(&spec) {
        spec.parse::<i64>().ok()
    } else {
        None
    };
    match days {
        Some(d) if d >= 0 => Ok(d),
        _ => Err(RepoError::runtime(format!(
            "--prune expects 'now' or non-negative '<N>d' (got {})",
            py_repr_ascii(&spec)?
        ))),
    }
}

pub fn gc(argv: &[String], out: &mut Out) -> CmdResult {
    let m = args::parse(Command::new("gc").arg(opt("prune", None, Some("prune"))), argv, &[])?;
    let prune_text = args::get_str(&m, "prune").unwrap_or_else(|| "90d".into());
    let repo = Repo::open()?;
    let age = parse_prune_age(&prune_text)?;
    writes::begin()?;
    let r = crate::progress::with_progress("collecting garbage", || repo.gc(age))?;
    out.println(format!("gc: kept {} objects, removed {}", r.kept, r.removed));
    if r.scratch_bytes_removed > 0 {
        out.println(format!(
            "gc: removed {:.1} MB of diff reports, caches and temporary files",
            r.scratch_bytes_removed as f64 / 1e6
        ));
    }
    if r.reflog_entries_removed > 0 {
        out.println(format!("gc: pruned {} reflog entries older than {prune_text}", r.reflog_entries_removed));
    }
    Ok(0)
}

pub fn hook(argv: &[String], out: &mut Out) -> CmdResult {
    let (sub, rest) = match argv.first() {
        None => ("list".to_string(), &argv[..0]),
        Some(s) if !s.starts_with('-') => (s.clone(), &argv[1..]),
        Some(_) => return Err(fwd("hook options before the subcommand")),
    };
    match sub.as_str() {
        "list" => {
            args::parse(Command::new("list"), rest, &[])?;
            let repo = Repo::open()?;
            let installed = hooks::list_installed(&repo.repo_dir);
            for name in hooks::SUPPORTED_HOOKS {
                let on = installed.contains(&name);
                let marker = if on { "*" } else { " " };
                let path = if on { hooks::hook_path(&repo.repo_dir, name).display().to_string() } else { "(none)".into() };
                out.println(format!("{marker} {name:14} {path}"));
            }
            Ok(0)
        }
        "run" => {
            let m = args::parse(Command::new("run").arg(positional("name", false)), rest, &["name"])?;
            let Some(name) = args::get_str(&m, "name").filter(|n| hooks::SUPPORTED_HOOKS.contains(&n.as_str())) else {
                return Err(fwd("hook run: argparse's choices error"));
            };
            let repo = Repo::open()?;
            writes::begin()?;
            let code = hooks::run(&repo.repo_dir, &name, &[], false, &mut || out.flush_pending())?;
            out.println(format!("hook {name} exited with {code}"));
            Ok(code)
        }
        _ => Err(fwd("unknown hook subcommand")),
    }
}
