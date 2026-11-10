//! `tmiki commit` (cli.py `cmd_commit`): `-m`, `-a` (repo.py
//! `stage_all_modified` through tmi-import), `--amend`, `--allow-empty`
//! (stage 3b-2), running the pre- and post-commit hooks (3b-1).

use crate::args::{self, flag, opt};
use crate::cmd_add::emit_warnings;
use crate::gates::fwd;
use crate::out::Out;
use crate::progress::Progress;
use crate::CmdResult;
use clap::Command;
use tmi_repo::repo::prefix_chars;
use tmi_repo::stage::Stager;
use tmi_repo::{commit, py, wfs, Repo, RepoError};
use tmi_store::json::Value;

fn commit_cmd() -> Command {
    Command::new("commit")
        .arg(opt("message", Some('m'), Some("message")))
        .arg(flag("stage_all", Some('a'), Some("all")))
        .arg(flag("amend", None, Some("amend")))
        .arg(flag("allow_empty", None, Some("allow-empty")))
}

/// What could only be decided by Python later is decided now, while the
/// command can still go to Python whole: the commit time's form and the
/// config files the identity comes from (their errors are raised in order
/// later, as Python raises them).
fn preflight(repo: &Repo) -> Result<(), RepoError> {
    wfs::object_timestamp()?;
    match wfs::resolve_identity(repo) {
        Err(RepoError::Forward(w)) => Err(RepoError::Forward(w)),
        _ => Ok(()),
    }
}

pub fn commit(argv: &[String], out: &mut Out) -> CmdResult {
    let m = args::parse(commit_cmd(), argv, &[])?;
    let message = args::get_str(&m, "message");
    let (stage_all, amend, allow_empty) =
        (args::get_flag(&m, "stage_all"), args::get_flag(&m, "amend"), args::get_flag(&m, "allow_empty"));
    let repo = Repo::open()?;
    if amend && repo.pending_merge_head()?.is_some() {
        return Err(RepoError::runtime("cannot amend during a merge; finish the merge with 'tmiki commit' or discard it"));
    }
    crate::writes::begin_locked(&repo)?;
    preflight(&repo)?;
    let progress = Progress::new();
    let report = |msg: &str| progress.report(msg);
    if stage_all {
        let mut stager = Stager::new(&repo)?;
        stager.progress = Some(&report);
        let result = stager.stage_all_modified();
        let staged = match result {
            Err(RepoError::Forward(w)) => return Err(RepoError::Forward(w)),
            Err(e) => {
                emit_warnings(&mut stager, out);
                stager.finish()?;
                return Err(e);
            }
            Ok(s) => s,
        };
        emit_warnings(&mut stager, out);
        stager.finish()?;
        for path in &staged {
            out.println(format!("staged {path}"));
        }
        // a plugin's component that .tmiattributes still needs (issue #50)
        for w in tmi_repo::formats::removed_component_warnings(&repo, &staged)? {
            out.eprintln(w);
        }
    }
    let (oid, label) = if amend {
        (commit::commit_amend(&repo, message.as_deref(), &mut || out.flush_pending())?, "amend")
    } else {
        let Some(msg) = message.as_deref().filter(|m| !m.is_empty()) else {
            return Err(RepoError::runtime("commit requires -m <message>"));
        };
        (commit::commit(&repo, msg, allow_empty, &mut || out.flush_pending())?, "")
    };
    progress.done();
    let branch = repo.current_branch()?.unwrap_or_else(|| "detached".into());
    let display = match message.as_deref().filter(|m| !m.is_empty()) {
        Some(msg) => py::first_line(msg).to_string(),
        None => match repo.read_commit(&oid)?.get("message") {
            None => String::new(),
            Some(Value::Str(s)) => py::first_line(s).to_string(),
            Some(_) => return Err(fwd("commit message is not a str")),
        },
    };
    let suffix = if label.is_empty() { String::new() } else { format!(" [{label}]") };
    out.println(format!("[{branch} {}]{suffix} {display}", prefix_chars(&oid, 12)));
    Ok(0)
}
