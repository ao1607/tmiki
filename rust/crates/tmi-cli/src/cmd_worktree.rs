//! `tmiki switch`, `checkout`, `restore`, `reset`, `rm` (cli.py `cmd_switch`,
//! `cmd_checkout`, `cmd_restore`, `cmd_reset`, `cmd_rm`). Layer restores,
//! `--source WORKTREE` and taking one layer's side go to Python (they read
//! or write pixels). `mv` stages the moved file through tmi-import (3b-2).

use crate::args::{self, flag, opt, positional};
use crate::cmd_log::fwd;
use crate::out::Out;
use crate::progress::{with_progress, Progress};
use crate::writes;
use crate::CmdResult;
use clap::Command;
use tmi_repo::paths::{path_str, split_pathspec};
use tmi_repo::repo::prefix_chars;
use std::path::Path;
use tmi_repo::{Repo, RepoError, Result};

fn exclusive(m: &clap::ArgMatches, ids: &[&str]) -> Result<()> {
    if ids.iter().filter(|id| args::get_flag(m, id)).count() > 1 {
        return Err(fwd("mutually exclusive options (argparse's error)"));
    }
    Ok(())
}

pub fn switch(argv: &[String], out: &mut Out) -> CmdResult {
    let cmd = Command::new("switch")
        .arg(flag("create", Some('c'), Some("create")))
        .arg(positional("name", false))
        .arg(positional("start", false))
        .arg(flag("discard", None, Some("discard")));
    let m = args::parse(cmd, argv, &["name", "start"])?;
    let Some(name) = args::get_str(&m, "name") else { return Err(fwd("switch without a name")) };
    let start = args::get_str(&m, "start");
    let discard = args::get_flag(&m, "discard");
    let repo = Repo::open()?;
    writes::begin_locked(&repo)?;
    writes::no_plugins()?;
    let backups_before = repo.read_backups()?.len();
    let mut warnings = Vec::new();
    let commit = if args::get_flag(&m, "create") {
        if repo.read_ref(&format!("refs/heads/{name}"))?.is_some() {
            return Err(RepoError::runtime(format!("branch already exists: {name}")));
        }
        let created = repo.create_branch(&name, start.as_deref(), &mut warnings);
        writes::warnings(out, &mut warnings);
        created?;
        match with_progress(&format!("switching to {name}"), || repo.switch(&name, discard, &mut || out.flush_pending())) {
            Ok(c) => c,
            Err(e) => {
                repo.delete_branch(&name, true)?;
                return Err(e);
            }
        }
    } else {
        if start.is_some() {
            return Err(RepoError::runtime("a start revision is only allowed with -c/--create"));
        }
        with_progress(&format!("switching to {name}"), || repo.switch(&name, discard, &mut || out.flush_pending()))?
    };
    writes::report_backups(out, &repo, backups_before)?;
    out.println(format!("Switched to branch {name}"));
    if let Some(c) = commit.filter(|c| !c.is_empty()) {
        out.println(format!("HEAD is now {}", prefix_chars(&c, 12)));
    }
    writes::remind_reopen(out, &repo);
    Ok(0)
}

pub fn checkout(argv: &[String], out: &mut Out) -> CmdResult {
    let cmd = Command::new("checkout")
        .arg(opt("branch", Some('b'), Some("branch")))
        .arg(flag("discard", None, Some("discard")))
        .arg(positional("args", true));
    let m = args::parse(cmd, argv, &["args"])?;
    let values = args::get_many(&m, "args");
    let branch = args::get_str(&m, "branch");
    let discard = args::get_flag(&m, "discard");
    let repo = Repo::open()?;
    writes::begin_locked(&repo)?;
    let backups_before = repo.read_backups()?.len();
    let result = checkout_inner(&repo, branch, values, discard, out);
    // try: ... finally: _report_backups(...); _remind_reopen(...)
    if result.as_ref().is_err_and(RepoError::is_forward) && !tmi_repo::fsio::mutated() {
        return result;
    }
    writes::report_backups(out, &repo, backups_before)?;
    writes::remind_reopen(out, &repo);
    result
}

fn checkout_inner(repo: &Repo, branch: Option<String>, values: Vec<String>, discard: bool, out: &mut Out) -> CmdResult {
    let mut warnings = Vec::new();
    if let Some(branch) = branch.filter(|b| !b.is_empty()) {
        if values.len() > 1 {
            return Err(RepoError::runtime("checkout -b accepts at most one start revision"));
        }
        writes::no_plugins()?;
        let start = values.first().map(String::as_str);
        let created = repo.create_branch(&branch, start, &mut warnings);
        writes::warnings(out, &mut warnings);
        created?;
        let commit = match with_progress(&format!("switching to {branch}"), || repo.switch(&branch, discard, &mut || out.flush_pending())) {
            Ok(c) => c,
            Err(e) => {
                repo.delete_branch(&branch, true)?;
                return Err(e);
            }
        };
        out.println(format!("Switched to a new branch {branch}"));
        if let Some(c) = commit.filter(|c| !c.is_empty()) {
            out.println(format!("HEAD is now {}", prefix_chars(&c, 12)));
        }
        return Ok(0);
    }
    if values.is_empty() {
        return Err(RepoError::runtime("checkout requires a branch, revision, or pathspec"));
    }
    if values.len() > 1 {
        let source = &values[0];
        let progress = Progress::new();
        let mut layers = crate::cmd_layer::LayerWrites::new(&repo)?;
        for spec in &values[1..] {
            progress.report(&format!("restoring {spec}"));
            // `file:layer`: that layer of the commit exported as a PNG
            // beside the file (repo.py `restore` without --in-place)
            let r = if split_pathspec(spec)?.1.is_some() {
                let r = layers.restore_layer(spec, Some(source), false, None, &mut warnings);
                layers.warnings(out);
                r
            } else {
                repo.restore_file(spec, Some(source), None, &format!("checkout {source}"), &mut warnings)
            };
            writes::warnings(out, &mut warnings);
            let path = r?;
            out.println(format!("restored {}", path.display()));
        }
        progress.done();
        return Ok(0);
    }
    let target = &values[0];
    if repo.read_ref(&format!("refs/heads/{target}"))?.is_some() {
        writes::no_plugins()?;
        let commit = with_progress(&format!("switching to {target}"), || repo.switch(target, discard, &mut || out.flush_pending()))?;
        out.println(format!("Switched to branch {target}"));
        if let Some(c) = commit.filter(|c| !c.is_empty()) {
            out.println(format!("HEAD is now {}", prefix_chars(&c, 12)));
        }
        return Ok(0);
    }
    let is_rev = match repo.resolve_rev(Some(target), &mut warnings) {
        Ok(_) => true,
        Err(RepoError::Runtime(_)) => false,
        Err(e) => return Err(e),
    };
    writes::warnings(out, &mut warnings);
    if !is_rev && repo.pathspec_exists_or_tracked(target)? {
        if split_pathspec(target)?.1.is_some() {
            let mut layers = crate::cmd_layer::LayerWrites::new(&repo)?;
            let r = with_progress(&format!("restoring {target}"), || layers.restore_layer(target, None, false, None, &mut warnings));
            layers.warnings(out);
            writes::warnings(out, &mut warnings);
            out.println(format!("restored {}", r?.display()));
            return Ok(0);
        }
        let r = with_progress(&format!("restoring {target}"), || repo.restore_file(target, None, None, "checkout", &mut warnings));
        writes::warnings(out, &mut warnings);
        out.println(format!("restored {}", r?.display()));
        return Ok(0);
    }
    let r = with_progress(&format!("checking out {target}"), || repo.checkout_detached(target, discard, &mut warnings));
    writes::warnings(out, &mut warnings);
    out.println(format!("HEAD is now at {}", prefix_chars(&r?, 12)));
    Ok(0)
}

pub fn restore(argv: &[String], out: &mut Out) -> CmdResult {
    let cmd = Command::new("restore")
        .arg(flag("staged", Some('S'), Some("staged")))
        .arg(opt("source", None, Some("source")))
        .arg(flag("in_place", None, Some("in-place")))
        .arg(opt("output", None, Some("output")))
        .arg(flag("ours", None, Some("ours")))
        .arg(flag("theirs", None, Some("theirs")))
        .arg(positional("specs", true));
    let m = args::parse(cmd, argv, &["specs"])?;
    exclusive(&m, &["ours", "theirs"])?;
    let specs = args::get_many(&m, "specs");
    if specs.is_empty() {
        return Err(fwd("restore without specs (argparse's error)"));
    }
    let source = args::get_str(&m, "source");
    let in_place = args::get_flag(&m, "in_place");
    let output = args::get_str(&m, "output");
    let repo = Repo::open()?;
    repo.lock_index()?; // held until the command ends (issue #35)
    let truthy = |v: &Option<String>| v.as_deref().is_some_and(|s| !s.is_empty());
    // Path(args.output): "" is "." (truthy)
    let has_output = output.is_some();
    if args::get_flag(&m, "staged") {
        if truthy(&source) || in_place || has_output {
            return Err(RepoError::runtime("--staged cannot be combined with --source, --in-place, or --output"));
        }
        writes::begin()?;
        let merging = repo.pending_merge_head()?.is_some();
        for spec in &specs {
            let (rel, was_staged) = repo.unstage(spec)?;
            if merging && was_staged && repo.pending_conflict_paths()?.contains(&rel) {
                out.println(format!(
                    "unstaged {rel}; its merge conflict is unresolved again (tmiki restore --ours|--theirs {rel}[:<layer>], then 'tmiki add {rel}')"
                ));
            } else if was_staged {
                out.println(format!("unstaged {rel}"));
            } else {
                out.println(format!("{rel} was not staged"));
            }
        }
        return Ok(0);
    }
    let (ours, theirs) = (args::get_flag(&m, "ours"), args::get_flag(&m, "theirs"));
    if ours || theirs {
        if truthy(&source) || in_place || has_output {
            return Err(RepoError::runtime("--ours/--theirs cannot be combined with --source, --in-place, or --output"));
        }
        let side = if ours { "ours" } else { "theirs" };
        writes::begin()?;
        let before = repo.read_backups()?.len();
        let mut layers = crate::cmd_layer::LayerWrites::new(&repo)?;
        let progress = Progress::new();
        for spec in &specs {
            progress.report(&format!("restoring {spec}"));
            if tmi_repo::paths::split_layer_spec(&repo, spec)?.1.is_some() {
                // stage 4a: that layer's pixels written into the file
                let r = layers.resolve_conflict_layer(spec, side);
                layers.warnings(out);
                r?;
            } else {
                repo.resolve_conflict_file(spec, side)?;
            }
            out.println(format!("took {side} for {spec}; stage it with 'tmiki add' when done"));
        }
        progress.done();
        writes::report_backups(out, &repo, before)?;
        writes::remind_reopen(out, &repo);
        return Ok(0);
    }
    if has_output && specs.len() != 1 {
        return Err(RepoError::runtime("--output can only be used with one restore spec"));
    }
    if has_output && in_place {
        return Err(RepoError::runtime("--output cannot be used with --in-place"));
    }
    let shown_output = match &output {
        Some(o) => Some(path_str(o)?),
        None => None,
    };
    writes::begin()?;
    let before = repo.read_backups()?.len();
    let mut warnings = Vec::new();
    let mut layers = crate::cmd_layer::LayerWrites::new(&repo)?;
    let progress = Progress::new();
    for spec in &specs {
        progress.report(&format!("restoring {spec}"));
        let r = if source.as_deref() == Some("WORKTREE") {
            // stage 4a: an export from the working file
            let r = layers.export_from_worktree(spec, in_place, output.as_deref());
            layers.warnings(out);
            r
        } else if tmi_repo::paths::split_layer_spec(&repo, spec)?.1.is_some() {
            // stage 4a: a layer exported, or written back with --in-place
            let r = layers.restore_layer(spec, source.as_deref(), in_place, output.as_deref(), &mut warnings);
            layers.warnings(out);
            r
        } else {
            repo.restore_file(spec, source.as_deref(), output.as_deref(), "restore", &mut warnings)
        };
        writes::warnings(out, &mut warnings);
        let path = r?;
        match &shown_output {
            Some(text) => out.println(format!("restored {text}")),
            None => out.println(format!("restored {}", path.display())),
        }
    }
    progress.done();
    writes::report_backups(out, &repo, before)?;
    writes::remind_reopen(out, &repo);
    Ok(0)
}

pub fn reset(argv: &[String], out: &mut Out) -> CmdResult {
    let cmd = Command::new("reset")
        .arg(flag("soft", None, Some("soft")))
        .arg(flag("mixed", None, Some("mixed")))
        .arg(flag("hard", None, Some("hard")))
        .arg(positional("args", true));
    let m = args::parse(cmd, argv, &["args"])?;
    exclusive(&m, &["soft", "mixed", "hard"])?;
    let mode = if args::get_flag(&m, "hard") {
        "hard"
    } else if args::get_flag(&m, "soft") {
        "soft"
    } else {
        "mixed"
    };
    let values = args::get_many(&m, "args");
    let repo = Repo::open()?;
    writes::begin_locked(&repo)?;
    let mut warnings = Vec::new();
    if values.len() > 1 {
        let (source, specs) = (&values[0], &values[1..]);
        if mode != "mixed" {
            return Err(RepoError::runtime("path reset cannot be combined with --soft or --hard"));
        }
        for spec in specs {
            let r = repo.reset_path(spec, Some(source), &mut warnings);
            writes::warnings(out, &mut warnings);
            let (rel, changed) = r?;
            out.println(if changed { format!("unstaged {rel}") } else { format!("{rel} was not staged") });
        }
        return Ok(0);
    }
    let rev = values.first().cloned();
    let before = repo.read_backups()?.len();
    let result = if mode == "hard" {
        with_progress("resetting working tree", || repo.reset(rev.as_deref(), mode, &mut warnings))
    } else {
        repo.reset(rev.as_deref(), mode, &mut warnings)
    };
    writes::warnings(out, &mut warnings);
    let commit = match result {
        Ok(c) => {
            writes::report_backups(out, &repo, before)?;
            c
        }
        Err(RepoError::Runtime(msg)) if msg.contains("unknown revision") && rev.is_some() => {
            let rev = rev.unwrap();
            let names_a_path = repo.pathspec_exists_or_tracked(&rev)? || {
                match repo.pathspec_full_path(&rev).and_then(|p| repo.relpath(&p)) {
                    Ok(rel) => repo.index_entries()?.contains_key(&rel),
                    Err(RepoError::Forward(w)) => return Err(RepoError::Forward(w)),
                    Err(_) => false,
                }
            };
            if !names_a_path {
                return Err(RepoError::Runtime(msg));
            }
            if mode != "mixed" {
                return Err(RepoError::runtime("path reset cannot be combined with --soft or --hard"));
            }
            let r = repo.reset_path(&rev, None, &mut warnings);
            writes::warnings(out, &mut warnings);
            let (rel, changed) = r?;
            out.println(if changed { format!("unstaged {rel}") } else { format!("{rel} was not staged") });
            return Ok(0);
        }
        Err(e) => return Err(e),
    };
    out.println(format!("HEAD is now {}", prefix_chars(&commit, 12)));
    writes::remind_reopen(out, &repo);
    Ok(0)
}

/// `tmiki mv` (cli.py `cmd_mv`, repo.py `move_path`): the move, then the
/// source staged as deleted and the destination staged through tmi-import
/// (stage 3b-2). If the import is one tmi-import leaves to Python, the
/// file is moved back and the whole command goes to Python.
pub fn mv(argv: &[String], out: &mut Out) -> CmdResult {
    let cmd = Command::new("mv")
        .arg(flag("force", Some('f'), Some("force")))
        .arg(positional("source", false))
        .arg(positional("dest", false));
    let m = args::parse(cmd, argv, &["source", "dest"])?;
    let (Some(source), Some(dest)) = (args::get_str(&m, "source"), args::get_str(&m, "dest")) else {
        return Err(fwd("mv without source and dest (argparse's error)"));
    };
    let force = args::get_flag(&m, "force");
    let repo = Repo::open()?;
    repo.lock_index()?;
    crate::gates::no_merge_state(&repo)?;
    let root = repo.require_worktree()?.to_path_buf();
    let head = repo.entries_for_commit(repo.current_commit()?.as_deref())?;
    let index = tmi_repo::write::entries_of(&repo.read_index()?)?;
    let src_rel = repo.pathspec_relpath(&source, &|p| head.contains_key(p) || index.contains_key(p))?;
    let src = repo.abspath(&src_rel)?;
    if !src.exists() {
        return Err(RepoError::runtime(format!("source does not exist: {src_rel}")));
    }
    let mut dst = repo.pathspec_full_path(&dest)?;
    if dst.is_dir() {
        dst = dst.join(src.file_name().ok_or_else(|| fwd("source without a name"))?);
    }
    if tmi_repo::py::relative_posix(&dst, &root).is_none() {
        return Err(RepoError::runtime(format!("destination is outside working tree: {}", dst.display())));
    }
    let dst_rel = repo.relpath(&dst)?;
    let same_file = dst.exists() && tmi_repo::write::samefile(&src, &dst).unwrap_or(false);
    if dst.exists() && !same_file {
        if !force {
            return Err(RepoError::runtime(format!("destination exists: {dst_rel}")));
        }
        if dst.is_dir() {
            return Err(RepoError::runtime(format!("destination is a directory: {dst_rel}")));
        }
    }
    if same_file && src_rel == dst_rel {
        return Err(RepoError::runtime(format!("source and destination are the same file: {src_rel}")));
    }
    if src.is_dir() {
        return Err(fwd("moving a directory"));
    }
    crate::writes::begin()?;
    let mut stager = tmi_repo::stage::Stager::new(&repo)?;
    // PLUGINS.md 4.4: refused before anything moves
    if let Some((id, u)) = stager.ctx.formats.unavailable(&repo, &dst_rel) {
        return Err(RepoError::runtime(tmi_repo::formats::not_available_message(&dst_rel, &id, &u)));
    }
    // Moved aside rather than deleted, so a hand-over can put it back.
    let displaced = if dst.exists() && !same_file {
        let aside = dst.with_file_name(format!(".{}.tmi-mv", dst.file_name().unwrap().to_string_lossy()));
        tmi_repo::fsio::replace(&dst, &aside)?;
        Some(aside)
    } else {
        None
    };
    let created = !dst.parent().is_some_and(Path::exists);
    tmi_repo::fsio::mkdir_parent(&dst)?;
    tmi_repo::fsio::replace(&src, &dst)?;
    stager.stage_delete(&src_rel);
    let staged = stager.stage_file(&dst.to_string_lossy(), true, None);
    if let Err(RepoError::Forward(why)) = staged {
        // Put everything back: the command goes to Python whole.
        std::fs::rename(&dst, &src).map_err(|e| RepoError::runtime(format!("{}: {e}", dst.display())))?;
        if let Some(aside) = &displaced {
            std::fs::rename(aside, &dst).map_err(|e| RepoError::runtime(format!("{}: {e}", aside.display())))?;
        }
        if created {
            if let Some(parent) = dst.parent() {
                repo.remove_empty_parent_dirs(parent)?;
            }
        }
        tmi_repo::fsio::unmark_mutated();
        return Err(RepoError::Forward(why));
    }
    if let Some(aside) = &displaced {
        tmi_repo::fsio::unlink(aside)?;
    }
    crate::cmd_add::emit_warnings(&mut stager, out);
    stager.finish()?;
    staged?;
    out.println(format!("renamed {src_rel} -> {dst_rel}"));
    Ok(0)
}

pub fn rm(argv: &[String], out: &mut Out) -> CmdResult {
    let cmd = Command::new("rm")
        .arg(flag("force", Some('f'), Some("force")))
        .arg(flag("cached", None, Some("cached")))
        .arg(positional("specs", true));
    let m = args::parse(cmd, argv, &["specs"])?;
    let specs = args::get_many(&m, "specs");
    if specs.is_empty() {
        return Err(fwd("rm without specs (argparse's error)"));
    }
    let cached = args::get_flag(&m, "cached");
    let force = args::get_flag(&m, "force");
    let repo = Repo::open()?;
    writes::begin_locked(&repo)?;
    for spec in &specs {
        let rel = repo.remove_path(spec, cached, force)?;
        out.println(if cached { format!("removed {rel} from index") } else { format!("removed {rel}") });
        // a plugin's component that .tmiattributes still needs (issue #50)
        for w in tmi_repo::formats::removed_component_warnings(&repo, std::slice::from_ref(&rel))? {
            out.eprintln(w);
        }
    }
    Ok(0)
}
