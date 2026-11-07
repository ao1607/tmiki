//! Listings of refs: `tmiki branch`, `tmiki tag` (cli.py `cmd_branch`,
//! `cmd_tag`) and `tmiki remote [-v]`. Creating and deleting is Python's.

use crate::args::{self, flag, opt, opt_const, positional};
use crate::cmd_log::{fwd, str_field};
use crate::out::Out;
use crate::CmdResult;
use clap::{ArgAction, Command};
use tmi_repo::py::{ascii_isdigit, first_line, fnmatch, splitlines};
use tmi_repo::repo::{prefix_chars, Repo, DEFAULT_BRANCH};
use tmi_repo::{RepoError, Result};
use tmi_store::json::Value;

fn branch_cmd() -> Command {
    Command::new("branch")
        .arg(flag("all", Some('a'), Some("all")))
        .arg(flag("remotes", Some('r'), Some("remotes")))
        .arg(clap::Arg::new("verbose").short('v').long("verbose").action(ArgAction::Count))
        .arg(opt_const("merged", None, Some("merged"), "HEAD"))
        .arg(opt_const("no_merged", None, Some("no-merged"), "HEAD"))
        .arg(opt_const("contains", None, Some("contains"), "HEAD"))
        .arg(opt("sort", None, Some("sort")))
        .arg(flag("delete", Some('d'), Some("delete")))
        .arg(flag("force_delete", Some('D'), None))
        .arg(positional("name", false))
        .arg(positional("start", false))
}

/// cli.py `_commit_sort_date`.
fn commit_sort_date(repo: &Repo, oid: Option<&str>) -> Result<String> {
    let Some(oid) = oid.filter(|o| !o.is_empty()) else { return Ok(String::new()) };
    match repo.read_commit(oid) {
        Ok(c) => match c.get("timestamp") {
            None => Ok(String::new()),
            Some(v) => v.py_str().ok_or_else(|| fwd("container timestamp")),
        },
        Err(RepoError::Forward(w)) => Err(RepoError::Forward(w)),
        Err(_) => Ok(String::new()),
    }
}

fn resolve_opt(repo: &Repo, rev: Option<&str>, out: &mut Out) -> Result<Option<String>> {
    let Some(rev) = rev.filter(|r| !r.is_empty()) else { return Ok(None) };
    let mut warnings = Vec::new();
    let r = repo.resolve_rev(Some(rev), &mut warnings);
    for w in warnings {
        out.eprintln(w);
    }
    r
}

pub fn branch(argv: &[String], out: &mut Out) -> CmdResult {
    let m = args::parse(branch_cmd(), argv, &["name", "start"])?;
    let (delete, force_delete) = (args::get_flag(&m, "delete"), args::get_flag(&m, "force_delete"));
    let name_arg = args::get_str(&m, "name");
    if delete || force_delete || name_arg.as_deref().is_some_and(|n| !n.is_empty()) {
        return crate::cmd_refs_write::branch(delete, force_delete, name_arg, args::get_str(&m, "start"), out);
    }
    let (all, remotes) = (args::get_flag(&m, "all"), args::get_flag(&m, "remotes"));
    let verbose = m.get_count("verbose") > 0;
    let sort = args::get_str(&m, "sort").unwrap_or_else(|| "refname".into());
    let repo = Repo::open()?;
    let current = repo.current_branch()?;
    // (name, commit, is_current)
    let mut rows: Vec<(String, Option<String>, bool)> = Vec::new();
    if !remotes {
        for (name, commit) in repo.list_head_refs()? {
            let is_current = current.as_deref() == Some(name.as_str());
            rows.push((name, commit, is_current));
        }
    }
    if all || remotes {
        for (r, value) in repo.all_refs_sorted()? {
            let Some(short) = r.strip_prefix("refs/remotes/") else { continue };
            let commit = repo.object_as_commit(Some(&value))?;
            let name = if remotes && !all { short.to_string() } else { format!("remotes/{short}") };
            rows.push((name, commit, false));
        }
    }
    let contains = resolve_opt(&repo, args::get_str(&m, "contains").as_deref(), out)?;
    let merged_into = resolve_opt(&repo, args::get_str(&m, "merged").as_deref(), out)?;
    let no_merged_into = resolve_opt(&repo, args::get_str(&m, "no_merged").as_deref(), out)?;
    let mut filtered = Vec::new();
    for (name, commit, is_current) in rows {
        let Some(c) = commit else { continue };
        if let Some(x) = &contains {
            if !repo.is_ancestor(x, &c)? {
                continue;
            }
        }
        if let Some(x) = &merged_into {
            if !repo.is_ancestor(&c, x)? {
                continue;
            }
        }
        if let Some(x) = &no_merged_into {
            if repo.is_ancestor(&c, x)? {
                continue;
            }
        }
        filtered.push((name, c, is_current));
    }
    let reverse = sort.starts_with('-');
    let key = if reverse { &sort[1..] } else { sort.as_str() };
    if key == "committerdate" || key == "authordate" {
        let mut keyed = Vec::new();
        for row in filtered {
            let k = commit_sort_date(&repo, Some(&row.1))?;
            keyed.push((k, row));
        }
        if reverse {
            keyed.sort_by(|a, b| b.0.cmp(&a.0));
        } else {
            keyed.sort_by(|a, b| a.0.cmp(&b.0));
        }
        filtered = keyed.into_iter().map(|(_, r)| r).collect();
    } else if reverse {
        filtered.sort_by(|a, b| b.0.cmp(&a.0));
    } else {
        filtered.sort_by(|a, b| a.0.cmp(&b.0));
    }
    if filtered.is_empty() {
        out.println(format!("* {DEFAULT_BRANCH} (no commits)"));
        return Ok(0);
    }
    for (name, commit, is_current) in filtered {
        let marker = if is_current { "*" } else { " " };
        if verbose {
            let c = repo.read_commit(&commit)?;
            let subject = tmi_repo::textsafe::escape(first_line(str_field(&c, "message")?), false);
            let name = tmi_repo::textsafe::escape(&name, false);
            out.println(format!("{marker} {name:24} {} {subject}", prefix_chars(&commit, 12)));
        } else {
            let name = tmi_repo::textsafe::escape(&name, false);
            out.println(format!("{marker} {name:20} {}", prefix_chars(&commit, 12)));
        }
    }
    Ok(0)
}

fn tag_cmd() -> Command {
    Command::new("tag")
        .arg(flag("annotated", Some('a'), Some("annotated")))
        .arg(opt("message", Some('m'), Some("message")))
        .arg(flag("delete", Some('d'), Some("delete")))
        .arg(flag("list", Some('l'), Some("list")))
        .arg(opt_const("lines", Some('n'), None, "1"))
        .arg(opt("points_at", None, Some("points-at")))
        .arg(opt("contains", None, Some("contains")))
        .arg(opt("sort", None, Some("sort")))
        .arg(positional("name", false))
        .arg(positional("rev", false))
}

/// cli.py `_tag_annotation`.
fn tag_annotation(repo: &Repo, target: &str, line_count: i64) -> Result<String> {
    let (object_type, payload) = match repo.store.read(target) {
        Ok(v) => v,
        Err(tmi_store::StoreError::Other(w)) => return Err(RepoError::Forward(w)),
        Err(_) => return Ok(String::new()),
    };
    if object_type != "tag" {
        return Ok(String::new());
    }
    let tag = tmi_store::json::loads_bytes(&payload).map_err(|e| fwd(&e.to_string()))?;
    let lines = splitlines(str_field(&tag, "message")?);
    let n = line_count.max(0) as usize;
    Ok(lines.into_iter().take(n).collect::<Vec<_>>().join(" "))
}

/// cli.py `_tag_sort_date`.
fn tag_sort_date(repo: &Repo, target: &str, target_commit: Option<&str>) -> Result<String> {
    match repo.store.read(target) {
        Ok((t, payload)) if t == "tag" => {
            let tag = tmi_store::json::loads_bytes(&payload).map_err(|e| fwd(&e.to_string()))?;
            return match tag.get("timestamp") {
                None => Ok(String::new()),
                Some(v) => v.py_str().ok_or_else(|| fwd("container timestamp")),
            };
        }
        Err(tmi_store::StoreError::Other(w)) => return Err(RepoError::Forward(w)),
        _ => {}
    }
    commit_sort_date(repo, target_commit)
}

pub fn tag(argv: &[String], out: &mut Out) -> CmdResult {
    let m = args::parse(tag_cmd(), argv, &["name", "rev"])?;
    if args::get_flag(&m, "delete") {
        return crate::cmd_refs_write::tag_delete(args::get_str(&m, "name"), args::get_str(&m, "rev"), out);
    }
    let mut name = args::get_str(&m, "name");
    let annotated = args::get_flag(&m, "annotated");
    let message = args::get_str(&m, "message");
    let repo = Repo::open()?;
    if annotated && name.as_deref().is_some_and(|n| !n.is_empty()) && message.is_none() {
        return Err(RepoError::runtime("annotated tag requires -m <message>"));
    }
    let mut lines: Option<i64> = None;
    if let Some(text) = args::get_str(&m, "lines") {
        match ascii_isdigit(&text) {
            None => return Err(fwd("tag -n with non-ASCII text")),
            Some(false) => {
                if name.is_none() {
                    name = Some(text);
                }
                lines = Some(1);
            }
            Some(true) => lines = Some(text.parse().map_err(|_| fwd("huge -n"))?),
        }
    }
    let points_at = args::get_str(&m, "points_at");
    let contains_arg = args::get_str(&m, "contains");
    let listing = args::get_flag(&m, "list")
        || name.as_deref().is_none_or(str::is_empty)
        || points_at.as_deref().is_some_and(|s| !s.is_empty())
        || contains_arg.as_deref().is_some_and(|s| !s.is_empty())
        || lines.is_some_and(|n| n != 0);
    if !listing {
        let name = name.unwrap_or_default();
        return crate::cmd_refs_write::tag_create(&repo, &name, args::get_str(&m, "rev"), message, annotated, out);
    }
    if args::get_str(&m, "rev").is_some_and(|r| !r.is_empty()) {
        return Err(RepoError::runtime("tag listing accepts at most one pattern"));
    }
    let pattern = name.filter(|n| !n.is_empty());
    let points_at = resolve_opt(&repo, points_at.as_deref(), out)?;
    let contains = resolve_opt(&repo, contains_arg.as_deref(), out)?;
    let mut rows: Vec<(String, String, String)> = Vec::new();
    for (tag_name, target) in repo.list_tags()? {
        if let Some(p) = &pattern {
            if !fnmatch(&tag_name, p).map_err(|_| fwd("bad glob"))? {
                continue;
            }
        }
        let target_commit = repo.object_as_commit(Some(&target))?;
        if let Some(pa) = &points_at {
            if target_commit.as_ref() != Some(pa) {
                continue;
            }
        }
        if let Some(c) = &contains {
            match &target_commit {
                Some(tc) if repo.is_ancestor(c, tc)? => {}
                _ => continue,
            }
        }
        let date = tag_sort_date(&repo, &target, target_commit.as_deref())?;
        rows.push((tag_name, target, date));
    }
    let sort = args::get_str(&m, "sort").unwrap_or_else(|| "refname".into());
    let reverse = sort.starts_with('-');
    let key = if reverse { &sort[1..] } else { sort.as_str() };
    let by_date = key == "creatordate";
    rows.sort_by(|a, b| {
        let (x, y) = if by_date { (&a.2, &b.2) } else { (&a.0, &b.0) };
        if reverse {
            y.cmp(x)
        } else {
            x.cmp(y)
        }
    });
    for (tag_name, target, _) in rows {
        let short = prefix_chars(&target, 12);
        let tag_name = tmi_repo::textsafe::escape(&tag_name, false);
        match lines.filter(|n| *n != 0) {
            Some(n) => {
                let annotation = tmi_repo::textsafe::escape(&tag_annotation(&repo, &target, n)?, false);
                let suffix = if annotation.is_empty() { String::new() } else { format!(" {annotation}") };
                out.println(format!("{tag_name:20} {short}{suffix}"));
            }
            None => out.println(format!("{tag_name:20} {short}")),
        }
    }
    Ok(0)
}

pub fn remote(argv: &[String], out: &mut Out) -> CmdResult {
    if let Some((own, sub, rest)) = crate::cmd_refs_write::split_remote_argv(argv) {
        args::parse(crate::cmd_refs_write::remote_parent_cmd(), &own, &[])?;
        return crate::cmd_refs_write::remote_sub(&sub, &rest, out);
    }
    let cmd = Command::new("remote").arg(flag("verbose", Some('v'), Some("verbose")));
    let m = args::parse(cmd, argv, &[])?;
    let verbose = args::get_flag(&m, "verbose");
    let repo = Repo::open()?;
    let config = repo.read_config()?;
    let remotes = match config.get("remotes") {
        None => return Ok(0),
        Some(Value::Object(map)) => map.clone(),
        Some(_) => return Err(fwd("remotes is not a dict")),
    };
    let mut names: Vec<&String> = remotes.keys().collect();
    names.sort();
    for name in names {
        let url = match remotes.get(name.as_str()) {
            Some(Value::Object(v)) => match v.get("url") {
                None => String::new(),
                Some(u) => u.py_str().ok_or_else(|| fwd("container url"))?,
            },
            _ => return Err(fwd("remote entry is not a dict")),
        };
        if verbose {
            out.println(format!("{name}\t{url} (fetch)"));
            out.println(format!("{name}\t{url} (push)"));
        } else {
            out.println(format!("{name}\t{url}"));
        }
    }
    Ok(0)
}
