//! `tmiki init`, `clone`, `fetch`, `push` and `pull` (cli.py `cmd_init`,
//! `cmd_clone`, `cmd_fetch`, `cmd_push`, `cmd_pull`) on tmi-transport
//! (RUST_MIGRATION.md 4b).

use crate::args::{self, flag, positional};
use crate::cmd_log::fwd;
use crate::out::Out;
use crate::progress::Progress;
use crate::transport_errors::reword;
use crate::writes;
use crate::CmdResult;
use clap::Command;
use std::path::Path;
use tmi_repo::repo::prefix_chars;
use tmi_repo::Repo;
use tmi_transport::ops;

pub fn init(argv: &[String], out: &mut Out) -> CmdResult {
    let cmd = Command::new("init").arg(positional("path", false)).arg(flag("bare", None, Some("bare")));
    let m = args::parse(cmd, argv, &["path"])?;
    let path = args::get_str(&m, "path").unwrap_or_else(|| ".".into());
    writes::begin()?;
    let bare = args::get_flag(&m, "bare");
    // a .tmi already there (an unpacked folder) does not become trusted
    let fresh = !Path::new(&path).join(if bare { "HEAD" } else { tmi_repo::repo::TMI_DIR }).exists();
    let repo = ops::create(Path::new(&path), bare)?;
    if fresh {
        tmi_repo::trust::grant_hooks(&repo.repo_dir)?;
    }
    out.println(format!("Initialized empty tmiki repository in {}", repo.repo_dir.display()));
    Ok(0)
}

fn print_fetched(out: &mut Out, remote: &str, fetched: &ops::Fetched) {
    out.println(format!("fetched {} objects from {remote}", fetched.objects));
    let mut refs = fetched.refs.clone();
    refs.sort();
    for (branch, commit) in refs {
        let shown = if commit.is_empty() { "(empty)".to_string() } else { prefix_chars(&commit, 12) };
        out.println(format!("  {remote}/{branch} {shown}"));
    }
    for tag in &fetched.new_tags {
        out.println(format!("  new tag {tag}"));
    }
}

pub fn fetch(argv: &[String], out: &mut Out) -> CmdResult {
    let cmd = Command::new("fetch").arg(flag("all", None, Some("all"))).arg(positional("remote", false));
    let m = args::parse(cmd, argv, &["remote"])?;
    let remote = args::get_str(&m, "remote").unwrap_or_else(|| "origin".into());
    let repo = Repo::open()?;
    writes::begin()?;
    out.flush_pending();
    let progress = Progress::new();
    let report = |msg: &str| progress.report(msg);
    if args::get_flag(&m, "all") {
        let config = repo.read_config()?;
        let mut names: Vec<String> = match config.get("remotes") {
            None => Vec::new(),
            Some(tmi_store::json::Value::Object(m)) => m.keys().cloned().collect(),
            Some(_) => return Err(fwd("remotes is not a dict")),
        };
        names.sort();
        if names.is_empty() {
            progress.done();
            out.println("no remotes");
            return Ok(0);
        }
        for name in names {
            let fetched = ops::fetch(&repo, &name, &report).map_err(|e| reword(e, ops::remote_url(&repo, &name).ok().as_deref()));
            let fetched = match fetched {
                Ok(f) => f,
                Err(e) => {
                    progress.done();
                    return Err(e);
                }
            };
            progress.clear();
            print_fetched(out, &name, &fetched);
        }
        progress.done();
        return Ok(0);
    }
    let fetched = ops::fetch(&repo, &remote, &report).map_err(|e| reword(e, ops::remote_url(&repo, &remote).ok().as_deref()));
    progress.done();
    print_fetched(out, &remote, &fetched?);
    Ok(0)
}

pub fn push(argv: &[String], out: &mut Out) -> CmdResult {
    let cmd = Command::new("push")
        .arg(positional("remote", false))
        .arg(positional("branch", false))
        .arg(flag("force", Some('f'), Some("force")))
        .arg(flag("force_with_lease", None, Some("force-with-lease")))
        .arg(flag("tags", None, Some("tags")));
    let m = args::parse(cmd, argv, &["remote", "branch"])?;
    let force = args::get_flag(&m, "force");
    let lease = args::get_flag(&m, "force_with_lease");
    if force && lease {
        return Err(fwd("mutually exclusive options (argparse's error)"));
    }
    let remote = args::get_str(&m, "remote").unwrap_or_else(|| "origin".into());
    let branch = args::get_str(&m, "branch");
    let tags = args::get_flag(&m, "tags");
    let repo = Repo::open()?;
    if tags && branch.as_deref().is_some_and(|b| !b.is_empty()) {
        return Err(tmi_repo::RepoError::runtime("--tags pushes every tag; it cannot be combined with a branch or tag name"));
    }
    if tags || ops::is_tag_push(&repo, branch.as_deref())? {
        return push_tags(&repo, &remote, if tags { None } else { branch.clone() }, force, lease, out);
    }
    writes::begin()?;
    repo.prepare_identity()?;
    out.flush_pending();
    let progress = Progress::new();
    let report = |msg: &str| progress.report(msg);
    let pushed = ops::push(&repo, &remote, branch.as_deref(), force, lease, &report, &mut || out.flush_pending())
        .map_err(|e| reword(e, ops::remote_url(&repo, &remote).ok().as_deref()));
    progress.done();
    let pushed = pushed?;
    let forced = if pushed.forced { " (forced update)" } else { "" };
    out.println(format!("pushed {} {} to {remote}{forced}", pushed.branch, prefix_chars(&pushed.commit, 12)));
    out.println(format!("copied {} objects", pushed.objects));
    Ok(0)
}

/// cli.py `_push_tags`: `push --tags` and `push <remote> <tag>` (issue #40).
fn push_tags(repo: &Repo, remote: &str, tag: Option<String>, force: bool, lease: bool, out: &mut Out) -> CmdResult {
    if lease {
        return Err(tmi_repo::RepoError::runtime("--force-with-lease does not apply to tags; --force replaces a tag on the remote"));
    }
    writes::begin()?;
    repo.prepare_identity()?;
    out.flush_pending();
    let progress = Progress::new();
    let report = |msg: &str| progress.report(msg);
    let tags = tag.map(|t| vec![t]);
    let pushed = ops::push_tags(repo, remote, tags.as_deref(), force, &report, &mut || out.flush_pending())
        .map_err(|e| reword(e, ops::remote_url(repo, remote).ok().as_deref()));
    progress.done();
    let pushed = pushed?;
    for (tag, value) in &pushed.pushed {
        out.println(format!("pushed tag {tag} {} to {remote}", prefix_chars(value, 12)));
    }
    if pushed.pushed.is_empty() {
        out.println(format!("tags are up to date on {remote}"));
    } else {
        out.println(format!("copied {} objects", pushed.objects));
    }
    Ok(0)
}

pub fn pull(argv: &[String], out: &mut Out) -> CmdResult {
    let cmd = Command::new("pull").arg(positional("remote", false)).arg(positional("branch", false));
    let m = args::parse(cmd, argv, &["remote", "branch"])?;
    let remote = args::get_str(&m, "remote").unwrap_or_else(|| "origin".into());
    let branch = args::get_str(&m, "branch");
    let repo = Repo::open()?;
    writes::begin_locked(&repo)?;
    repo.prepare_identity()?;
    let backups_before = repo.read_backups()?.len();
    let progress = Progress::new();
    let report = |msg: &str| progress.report(msg);
    let mut warnings = Vec::new();
    let mut fetched = false;
    let mut import_warnings: Vec<String> = Vec::new();
    // (mode, commit, conflicts with the conflicts directory)
    let pulled: tmi_repo::Result<(String, String, Option<(Vec<String>, String)>)> = (|| {
        let out_ref = &mut *out;
        let pulled =
            ops::pull(&repo, &remote, branch.as_deref(), &report, &mut warnings, &mut fetched, &mut || out_ref.flush_pending())
                .map_err(|e| reword(e, ops::remote_url(&repo, &remote).ok().as_deref()))?;
        let ops::Pulled::Merge { incoming, operation, message } = pulled else {
            return Ok((pulled.mode().to_string(), pulled.commit().to_string(), None));
        };
        // repo.merge(incoming, message, operation=...) (stage 4a's merge)
        use tmi_diffmerge::merge::{self, MergeOutcome};
        let mut ctx = merge::new_ctx(&repo)?;
        ctx.progress = Some(&report);
        let mut again = Vec::new(); // resolve_rev's warnings came out above
        let outcome = merge::merge_as(&mut ctx, &incoming, Some(&message), Some(&operation), &mut again, &mut || {
            out_ref.flush_pending()
        });
        import_warnings.extend(ctx.warns.lines.drain(..));
        Ok(match outcome? {
            MergeOutcome::UpToDate => ("already-up-to-date".into(), repo.current_commit()?.unwrap_or_default(), None),
            MergeOutcome::FastForward(c) => ("fast-forward".into(), c, None),
            MergeOutcome::Merged(c) => ("merge".into(), c, None),
            MergeOutcome::Conflict(paths, _, dir) => ("conflict".into(), String::new(), Some((paths, dir))),
        })
    })();
    let _ = fetched;
    for line in import_warnings {
        out.eprintln(line);
    }
    progress.done();
    writes::warnings(out, &mut warnings);
    let (mode, commit, conflicts) = pulled?;
    out.println(format!("pull {mode}"));
    if !commit.is_empty() {
        out.println(format!("HEAD is now {}", prefix_chars(&commit, 12)));
    }
    writes::report_backups(out, &repo, backups_before)?;
    if let Some((paths, dir)) = conflicts {
        for path in paths {
            out.println(format!("conflict {path}"));
        }
        out.println(format!("conflict material written to {dir}"));
        writes::remind_reopen(out, &repo);
        return Ok(1);
    }
    writes::remind_reopen(out, &repo);
    Ok(0)
}

pub fn clone(argv: &[String], out: &mut Out) -> CmdResult {
    let cmd = Command::new("clone").arg(positional("url", false)).arg(positional("directory", false));
    let m = args::parse(cmd, argv, &["url", "directory"])?;
    let Some(url) = args::get_str(&m, "url") else { return Err(fwd("clone without a url (argparse's error)")) };
    let directory = args::get_str(&m, "directory");
    writes::begin()?;
    let progress = Progress::new();
    let report = |msg: &str| progress.report(msg);
    let cloned = ops::clone(&url, directory.as_deref(), &report, &mut || progress.done()).map_err(|e| reword(e, Some(url.as_str())));
    let cloned = match cloned {
        Ok(c) => c,
        Err(e) => {
            if !e.is_forward() {
                progress.done();
            }
            return Err(e);
        }
    };
    tmi_repo::trust::grant_hooks(&cloned.target.join(tmi_repo::repo::TMI_DIR))?;
    out.println(format!("cloned {} into {}", cloned.stored_url, cloned.target.display()));
    out.println(format!("copied {} objects", cloned.objects));
    // PLUGINS.md 4.3: the plugins compiled now, not on the first add
    crate::cmd_plugin::prepare_after_clone(&cloned.target, out);
    Ok(0)
}
