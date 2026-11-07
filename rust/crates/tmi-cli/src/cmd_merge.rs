//! `tmiki merge` (cli.py `cmd_merge`; stage 4a): fast-forward, three-way and
//! layer-aware merges, the conflict state, `--abort`, through
//! tmi-diffmerge. Installed `tmi.plugins` (the post-merge event) goes to
//! Python.

use crate::args::{self, flag, opt, positional};
use crate::gates::fwd;
use crate::out::Out;
use crate::progress::Progress;
use crate::writes;
use crate::CmdResult;
use clap::Command;
use tmi_diffmerge::merge::{self, MergeOutcome};
use tmi_repo::repo::prefix_chars;
use tmi_repo::{Repo, RepoError};
use tmi_store::json::Value;

fn merge_cmd() -> Command {
    Command::new("merge")
        .arg(positional("rev", false))
        .arg(opt("message", Some('m'), Some("message")))
        .arg(flag("abort", None, Some("abort")))
}

pub fn merge(argv: &[String], out: &mut Out) -> CmdResult {
    let m = args::parse(merge_cmd(), argv, &["rev"])?;
    let rev = args::get_str(&m, "rev");
    let message = args::get_str(&m, "message");
    let repo = Repo::open()?;
    writes::begin_locked(&repo)?;
    if args::get_flag(&m, "abort") {
        let before = repo.read_backups()?.len();
        let progress = Progress::new();
        progress.report("aborting merge");
        let aborted = merge::merge_abort(&repo);
        progress.done();
        match aborted {
            Ok(_) => {}
            Err(e) => {
                if !e.is_forward() {
                    writes::report_backups(out, &repo, before)?;
                }
                return Err(e);
            }
        }
        out.println("merge aborted; working tree is back at HEAD");
        writes::report_backups(out, &repo, before)?;
        writes::remind_reopen(out, &repo);
        return Ok(0);
    }
    let Some(rev) = rev else {
        return Err(RepoError::runtime("merge requires a revision (or --abort)"));
    };
    writes::no_plugins()?;
    let progress = Progress::new();
    let report = |msg: &str| progress.report(msg);
    let before = repo.read_backups()?.len();
    let mut ctx = merge::new_ctx(&repo)?;
    ctx.progress = Some(&report);
    let mut warnings = Vec::new();
    let result = merge::merge(&mut ctx, &rev, message.as_deref(), &mut warnings, &mut || out.flush_pending());
    writes::warnings(out, &mut warnings);
    for line in ctx.warns.lines.drain(..) {
        out.eprintln(line);
    }
    progress.done();
    let outcome = result?;
    writes::report_backups(out, &repo, before)?;
    match &outcome {
        MergeOutcome::UpToDate => out.println("Already up to date."),
        MergeOutcome::FastForward(c) => out.println(format!("Fast-forward to {}", prefix_chars(c, 12))),
        MergeOutcome::Merged(c) => out.println(format!("Merged {rev}; HEAD is now {}", prefix_chars(c, 12))),
        MergeOutcome::Conflict(..) => {}
    }
    if let MergeOutcome::Conflict(paths, details, dir) = &outcome {
        out.println(format!("Automatic merge of {rev} stopped on conflicts:"));
        for (path, items) in paths.iter().zip(details) {
            out.println(format!("  conflict {path}"));
            for item in items {
                let layer = [item.get("path_name"), item.get("name")].into_iter().flatten().find(|v| v.truthy());
                let reason = item.get("reason").and_then(Value::py_str).unwrap_or_default();
                match layer {
                    Some(l) => out.println(format!("    {}: {reason}", l.py_str().ok_or_else(|| fwd("layer name"))?)),
                    None => out.println(format!("    {reason}")),
                }
            }
        }
        // cli.py cmd_merge: no <file>:<layer> advice when no conflict is a
        // layer's (a format without layers, add / delete; issue #43).
        let layered = details.iter().flatten().any(|item| item.get("stable_source_id").is_some_and(Value::truthy));
        if layered {
            out.println("Layers that did not conflict are already merged into the working files;");
            out.println("each conflicting layer still holds our version. To resolve:");
            out.println("  tmiki restore --theirs <file>:<layer>   take their version of one layer");
        } else {
            out.println("Each conflicting file still holds our version. To resolve:");
        }
        out.println("  tmiki restore --ours|--theirs <file>    take one side of the whole file");
        out.println("  (or edit the file in your drawing app), then 'tmiki add <file>' and 'tmiki commit'");
        out.println("  tmiki merge --abort                     give up the merge");
        out.println(format!("Both sides of each conflicting {}: {dir}", if layered { "layer" } else { "file" }));
        writes::remind_reopen(out, &repo);
        return Ok(1);
    }
    writes::remind_reopen(out, &repo);
    Ok(0)
}
