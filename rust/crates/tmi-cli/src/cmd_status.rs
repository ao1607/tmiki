//! `tmiki status` (cli.py `cmd_status`, `_print_short_status`), merges in
//! progress included; files the stat cache does not vouch for are hashed
//! (tmi_repo::status). A modified file in the long format goes to Python,
//! which imports it to describe its layer changes.

use crate::args::{self, flag, opt_const};
use crate::cmd_log::fwd;
use crate::out::Out;
use crate::progress::Progress;
use crate::CmdResult;
use clap::Command;
use std::collections::BTreeMap;
use tmi_repo::repo::{prefix_chars, Repo};
use tmi_import::Warnings;
use tmi_repo::status::StatusItem;
use tmi_repo::worktree_status::status_items;
use tmi_store::json::Value;

fn status_cmd() -> Command {
    Command::new("status")
        .arg(flag("short", Some('s'), Some("short")))
        .arg(flag("branch", Some('b'), Some("branch")))
        .arg(opt_const("porcelain", None, Some("porcelain"), "v1"))
}

pub fn status(argv: &[String], out: &mut Out) -> CmdResult {
    let m = args::parse(status_cmd(), argv, &[])?;
    let porcelain = args::get_str(&m, "porcelain");
    args::check_choice(&porcelain, &["v1"])?;
    let short = args::get_flag(&m, "short") || porcelain.is_some();
    let repo = Repo::open()?;
    let branch = repo.current_branch()?.unwrap_or_else(|| "(detached)".into());
    let head = repo.current_commit()?;
    if short {
        if args::get_flag(&m, "branch") {
            out.println(format!("## {branch}"));
        }
        let mut conflicts = repo.pending_conflict_paths()?;
        conflicts.sort();
        conflicts.dedup();
        for path in &conflicts {
            out.println(format!("UU {}", esc(path)));
        }
        let items: Vec<StatusItem> =
            status_items(&repo, false, &mut Warnings::default(), None)?.into_iter().filter(|i| !conflicts.contains(&i.path)).collect();
        let head_entries = repo.entries_for_commit(head.as_deref())?;
        let mut by_path: BTreeMap<String, Vec<StatusItem>> = BTreeMap::new();
        for item in items {
            by_path.entry(item.path.clone()).or_default().push(item);
        }
        for (path, states) in by_path {
            let has = |s: &str| states.iter().any(|i| i.state == s);
            if has("untracked") {
                out.println(format!("?? {}", esc(&path)));
                continue;
            }
            // setdefault(path, {})[state] = item: the last item of a state
            let x = match states.iter().rev().find(|i| i.state == "staged") {
                None => " ",
                Some(i) if i.detail == "delete" => "D",
                Some(_) if !head_entries.contains_key(&path) => "A",
                Some(_) => "M",
            };
            let y = if has("modified") {
                "M"
            } else if has("deleted") {
                "D"
            } else {
                " "
            };
            out.println(format!("{x}{y} {}", esc(&path)));
        }
        return Ok(0);
    }
    out.println(format!("On branch {branch}"));
    out.println(format!("HEAD {}", head.as_deref().map(|h| prefix_chars(h, 12)).unwrap_or_else(|| "(no commits)".into())));
    let merge_head = repo.pending_merge_head()?;
    let mut conflict_paths: Vec<String> = Vec::new();
    if let Some(mh) = &merge_head {
        out.println(format!("You are in the middle of a merge (MERGE_HEAD {}).", prefix_chars(mh, 12)));
        let conflicts = repo.pending_conflict_paths()?;
        conflict_paths = conflicts.clone();
        if conflicts.is_empty() {
            out.println("All conflicts resolved; stage your changes and run 'tmiki commit'.");
        } else {
            out.println("Unresolved conflicts (resolve, then 'tmiki add <file>' and 'tmiki commit'; or 'tmiki merge --abort'):");
            for path in &conflicts {
                out.println(format!("  conflict {}", esc(path)));
                let choices = repo.conflict_choices(path)?;
                let text = |v: Option<&Value>| -> tmi_repo::Result<String> {
                    match v {
                        None => Ok("None".into()),
                        Some(x) => x.py_str().ok_or_else(|| fwd("container conflict field")),
                    }
                };
                for (item, taken, _) in &choices {
                    let layer = match item.get("path_name") {
                        Some(v) if v.truthy() => Some(v),
                        _ => item.get("name"),
                    };
                    let reason = text(item.get("reason"))?;
                    let mut line = match layer {
                        Some(l) if l.truthy() => format!("    {}: {reason}", text(Some(l))?),
                        _ => format!("    {reason}"),
                    };
                    if let Some(t) = taken.as_ref().filter(|t| t.truthy()) {
                        line += &format!(" -> took {}", text(Some(t))?);
                    }
                    out.println(line);
                }
                let taken_count = choices.iter().filter(|(_, t, _)| t.as_ref().is_some_and(Value::truthy)).count();
                let edited = choices.first().is_some_and(|(_, _, e)| *e);
                if taken_count > 0 && edited {
                    out.println("    (working file edited since then)");
                } else if edited {
                    out.println("    (working file edited since the merge)");
                }
                if !choices.is_empty() && taken_count == choices.len() {
                    out.println(format!("    every conflict has a side; check the file, then 'tmiki add {path}'"));
                }
            }
        }
    }
    let mut warns = Warnings::default();
    let progress = Progress::new();
    let report = |msg: &str| progress.report(msg);
    let items = status_items(&repo, true, &mut warns, Some(&report));
    for line in warns.lines {
        out.eprintln(line);
    }
    progress.done();
    let items: Vec<StatusItem> = items?
        .into_iter()
        .filter(|i| !conflict_paths.contains(&i.path))
        .collect();
    if items.is_empty() {
        if merge_head.is_some() {
            out.println(if conflict_paths.is_empty() { "no unstaged changes" } else { "no other changes" });
            return Ok(0);
        }
        out.println("nothing to commit, working tree clean");
        return Ok(0);
    }
    for item in items {
        let detail = if item.detail.is_empty() { String::new() } else { format!("  {}", item.detail) };
        out.println(format!("{:10} {}{detail}", item.state, item.path));
    }
    Ok(0)
}

/// Paths and details come from the repository or the working tree: control
/// characters are escaped (tmi_repo::textsafe).
fn esc(text: &str) -> String {
    tmi_repo::textsafe::escape(text, false)
}
