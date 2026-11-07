//! Creating and deleting branches and tags, `tmiki remote add / remove /
//! rename / set-url`, `tmiki recover --as` (cli.py `cmd_branch`, `cmd_tag`,
//! `cmd_remote_*`, `cmd_recover`).

use crate::args::{self, flag, positional};
use crate::cmd_log::fwd;
use crate::out::Out;
use crate::writes;
use crate::CmdResult;
use clap::Command;
use tmi_repo::repo::prefix_chars;
use tmi_repo::{reflog, Repo, RepoError};

/// `branch -d/-D <name>` and `branch <name> [<start>]`.
pub fn branch(delete: bool, force_delete: bool, name: Option<String>, start: Option<String>, out: &mut Out) -> CmdResult {
    let repo = Repo::open()?;
    if delete || force_delete {
        let Some(name) = name.filter(|n| !n.is_empty()) else {
            return Err(RepoError::runtime("branch -d requires a branch name"));
        };
        if start.as_deref().is_some_and(|s| !s.is_empty()) {
            return Err(RepoError::runtime("branch -d does not take a start revision"));
        }
        writes::begin()?;
        let target = repo.delete_branch(&name, force_delete)?;
        out.println(format!("deleted branch {name} (was {})", prefix_chars(&target, 12)));
        return Ok(0);
    }
    let name = name.unwrap_or_default();
    writes::begin()?;
    let mut warnings = Vec::new();
    let r = repo.create_branch(&name, start.as_deref(), &mut warnings);
    writes::warnings(out, &mut warnings);
    let commit = r?;
    out.println(format!("created branch {name} at {}", prefix_chars(&commit, 12)));
    Ok(0)
}

pub fn tag_delete(name: Option<String>, rev: Option<String>, out: &mut Out) -> CmdResult {
    let repo = Repo::open()?;
    let Some(name) = name.filter(|n| !n.is_empty()) else {
        return Err(RepoError::runtime("tag -d requires a tag name"));
    };
    if rev.as_deref().is_some_and(|s| !s.is_empty()) {
        return Err(RepoError::runtime("tag -d does not take a revision"));
    }
    writes::begin()?;
    let target = repo.delete_tag(&name)?;
    out.println(format!("deleted tag {name} (was {})", prefix_chars(&target, 12)));
    Ok(0)
}

pub fn tag_create(repo: &Repo, name: &str, rev: Option<String>, message: Option<String>, annotated: bool, out: &mut Out) -> CmdResult {
    let message = if annotated || message.as_deref().is_some_and(|m| !m.is_empty()) { message } else { None };
    writes::begin()?;
    let mut warnings = Vec::new();
    let r = repo.create_tag(name, rev.as_deref(), message.as_deref(), &mut warnings);
    writes::warnings(out, &mut warnings);
    let target = r?;
    out.println(format!("tagged {name} -> {}", prefix_chars(&target, 12)));
    Ok(0)
}

/// `tmiki remote <sub> ...` after the remote parser's own options.
pub fn remote_sub(sub: &str, rest: &[String], out: &mut Out) -> CmdResult {
    let (cmd, names): (Command, &[&str]) = match sub {
        "add" | "set-url" => (Command::new("remote-sub").arg(positional("name", false)).arg(positional("url", false)), &["name", "url"]),
        "remove" | "rm" => (Command::new("remote-sub").arg(positional("name", false)), &["name"]),
        "rename" => (Command::new("remote-sub").arg(positional("old", false)).arg(positional("new", false)), &["old", "new"]),
        _ => return Err(fwd("unknown remote subcommand")),
    };
    let m = args::parse(cmd, rest, names)?;
    let vals: Vec<String> = names.iter().map(|n| args::get_str(&m, n)).collect::<Option<Vec<_>>>().ok_or_else(|| fwd("missing remote argument (argparse's error)"))?;
    let repo = Repo::open()?;
    writes::begin()?;
    match sub {
        "add" => {
            repo.add_remote(&vals[0], &vals[1])?;
            out.println(format!("added remote {} {}", vals[0], vals[1]));
        }
        "remove" | "rm" => {
            repo.remove_remote(&vals[0])?;
            out.println(format!("removed remote {}", vals[0]));
        }
        "rename" => {
            repo.rename_remote(&vals[0], &vals[1])?;
            out.println(format!("renamed remote {} -> {}", vals[0], vals[1]));
        }
        _ => {
            let url = repo.set_remote_url(&vals[0], &vals[1])?;
            out.println(format!("set remote {} url to {url}", vals[0]));
        }
    }
    Ok(0)
}

/// Split `tmiki remote` argv at its subcommand: (remote's own options, sub, rest).
pub fn split_remote_argv(argv: &[String]) -> Option<(Vec<String>, String, Vec<String>)> {
    let pos = argv.iter().position(|a| !a.starts_with('-') || a == "-")?;
    Some((argv[..pos].to_vec(), argv[pos].clone(), argv[pos + 1..].to_vec()))
}

pub fn remote_parent_cmd() -> Command {
    Command::new("remote").arg(flag("verbose", Some('v'), Some("verbose")))
}

/// cli.py `_resolve_recover_rev(repo, rev)`.
fn resolve_recover_rev(repo: &Repo, rev: &str, warnings: &mut Vec<String>) -> tmi_repo::Result<Option<String>> {
    if let Some(at) = reflog::resolve_at_spec(&repo.repo_dir, rev)? {
        return Ok(Some(at));
    }
    match repo.resolve_rev(Some(rev), warnings) {
        Ok(Some(c)) => return Ok(Some(c)),
        Ok(None) | Err(RepoError::Runtime(_)) => {}
        Err(e) => return Err(e),
    }
    let lower = rev.to_lowercase();
    if rev.chars().count() >= 4 && lower.chars().all(|c| "0123456789abcdef".contains(c)) {
        for entry in reflog::read(&repo.repo_dir, "HEAD")? {
            for oid in [&entry.old_oid, &entry.new_oid] {
                if oid.starts_with(&lower) && oid != reflog::ZERO_OID {
                    return Ok(Some(oid.clone()));
                }
            }
        }
    }
    Ok(None)
}

/// `recover --as <branch> <rev>`.
pub fn recover_as(repo: &Repo, as_branch: &str, rev: Option<String>, out: &mut Out) -> CmdResult {
    let Some(rev) = rev.filter(|r| !r.is_empty()) else {
        return Err(RepoError::runtime("--as requires a revision (HEAD@{N}, oid prefix, or branch name)"));
    };
    let mut warnings = Vec::new();
    let r = resolve_recover_rev(repo, &rev, &mut warnings);
    writes::warnings(out, &mut warnings);
    let Some(oid) = r? else {
        return Err(RepoError::runtime(format!("could not resolve revision: {rev}")));
    };
    writes::begin()?;
    let r = repo.create_branch(as_branch, Some(&oid), &mut warnings);
    writes::warnings(out, &mut warnings);
    r?;
    out.println(format!("created branch {as_branch} at {}", prefix_chars(&oid, 12)));
    Ok(0)
}

/// `recover --file N [--output PATH]`.
pub fn recover_file(repo: &Repo, spec: &str, output: Option<String>, out: &mut Out) -> CmdResult {
    let shown = match &output {
        Some(o) => Some(tmi_repo::paths::path_str(o)?),
        None => None,
    };
    writes::begin()?;
    let (backup, path) = repo.restore_backup(spec, output.as_deref())?;
    let field = |k: &str| -> tmi_repo::Result<String> {
        match backup.get(k) {
            None => Err(fwd("backup without a field (KeyError)")),
            Some(v) => v.py_str().ok_or_else(|| fwd("container backup field")),
        }
    };
    let ts = field("timestamp")?;
    let when = prefix_chars(&ts, 19).replace('T', " ");
    let to = shown.unwrap_or_else(|| path.display().to_string());
    out.println(format!("restored {} ({}, {when}) to {to}", field("path")?, field("reason")?));
    writes::remind_reopen(out, repo);
    Ok(0)
}
