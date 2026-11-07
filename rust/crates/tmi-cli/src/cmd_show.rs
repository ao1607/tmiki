//! `tmiki show` (cli.py `cmd_show`): a commit's metadata and changed paths,
//! or `rev:path`'s stored bytes. `--thumbnail` writes a PNG: Python.

use crate::args::{self, flag, opt, positional};
use crate::cmd_log::{commit_decorations, fwd, print_log_commit, LogFormat};
use crate::out::Out;
use crate::CmdResult;
use clap::Command;
use std::path::Path;
use tmi_repo::py;
use tmi_repo::repo::{prefix_chars, Repo};
use tmi_repo::{RepoError, Result};
use tmi_store::json::Value;

fn show_cmd() -> Command {
    Command::new("show")
        .arg(flag("stat", None, Some("stat")))
        .arg(flag("name_only", None, Some("name-only")))
        .arg(flag("name_status", None, Some("name-status")))
        .arg(opt("abbrev", None, Some("abbrev")))
        .arg(opt("date", None, Some("date")))
        .arg(flag("thumbnail", None, Some("thumbnail")))
        .arg(opt("output", Some('o'), Some("output")))
        .arg(positional("object", false))
}

/// repo.py `pathspec_relpath(pathspec, known_paths)`.
pub fn pathspec_relpath(repo: &Repo, pathspec: &str, known: &dyn Fn(&str) -> bool) -> Result<String> {
    let root = repo.require_worktree()?.to_path_buf();
    let path = Path::new(pathspec);
    if path.is_absolute() {
        let full = py::realpath(path);
        return py::relative_posix(&full, &root)
            .map(|r| if r.is_empty() { ".".to_string() } else { r })
            .ok_or_else(|| fwd("absolute path outside the working tree"));
    }
    let cwd = std::env::current_dir().map_err(|e| fwd(&e.to_string()))?;
    let mut candidates: Vec<String> = Vec::new();
    for candidate in [py::realpath(&cwd.join(path)), py::realpath(&root.join(path))] {
        if let Some(rel) = py::relative_posix(&candidate, &root) {
            let rel = if rel.is_empty() { ".".to_string() } else { rel };
            if !candidates.contains(&rel) {
                candidates.push(rel);
            }
        }
    }
    if let Some(hit) = candidates.iter().find(|c| known(c)) {
        return Ok(hit.clone());
    }
    candidates.into_iter().next().ok_or_else(|| fwd("pathspec outside the working tree"))
}

pub fn show(argv: &[String], out: &mut Out) -> CmdResult {
    let m = args::parse(show_cmd(), argv, &["object"])?;
    let date = args::get_str(&m, "date").unwrap_or_else(|| "iso".into());
    args::check_choice(&Some(date.clone()), &["iso", "local", "short"])?;
    let abbrev = args::get_int(&m, "abbrev")?.unwrap_or(12);
    if args::get_flag(&m, "thumbnail") {
        return Err(fwd("show --thumbnail writes a PNG"));
    }
    let repo = Repo::open()?;
    let target = args::get_str(&m, "object").filter(|t| !t.is_empty()).unwrap_or_else(|| "HEAD".into());
    let mut warnings = Vec::new();
    if target.contains(':') && !target.starts_with(':') {
        let (rev, path) = target.split_once(':').unwrap();
        let resolved = repo.resolve_rev(Some(if rev.is_empty() { "HEAD" } else { rev }), &mut warnings);
        for w in warnings.drain(..) {
            out.eprintln(w);
        }
        let Some(commit) = resolved? else {
            return Err(RepoError::runtime(format!("unknown revision: {rev}")));
        };
        let entries = repo.entries_for_commit(Some(&commit))?;
        let rel = pathspec_relpath(&repo, path, &|p| entries.contains_key(p))?;
        let entry = match entries.get(&rel) {
            Some(e) if e.truthy() => e,
            _ => {
                return Err(RepoError::runtime(format!("path not found in {}: {rel}", prefix_chars(&commit, 12))));
            }
        };
        let blob_ref = match entry.get("source_blob_ref") {
            Some(Value::Str(r)) => r.clone(),
            _ => return Err(fwd("entry without a source_blob_ref")),
        };
        out.write_bytes(repo.store.read_blob(&blob_ref)?);
        return Ok(0);
    }
    let resolved = repo.resolve_rev(Some(&target), &mut warnings);
    for w in warnings.drain(..) {
        out.eprintln(w);
    }
    let Some(oid) = resolved? else {
        return Err(RepoError::runtime(format!("unknown revision: {target}")));
    };
    let commit = repo.read_commit(&oid)?;
    let (stat, name_only) = (args::get_flag(&m, "stat"), args::get_flag(&m, "name_only"));
    let f = LogFormat {
        format: None,
        pretty: None,
        oneline: false,
        parents: true,
        abbrev,
        abbrev_commit: false,
        date,
        stat,
        name_only,
        name_status: args::get_flag(&m, "name_status") || (!stat && !name_only),
    };
    let decorations = commit_decorations(&repo, false)?.remove(&oid).unwrap_or_default();
    print_log_commit(&repo, &oid, &commit, &f, &decorations, &[], &mut crate::cmd_log::Prefixes::none(), out)?;
    Ok(0)
}
