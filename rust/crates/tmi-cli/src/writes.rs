//! What every native write command shares: the checks that must pass
//! before anything is written (so the command can still go to Python), and
//! the notices cli.py prints after rewriting working files.

use crate::out::{self, Encoding, Out, Stream};
use tmi_repo::{Repo, RepoError, Result};

/// Called before a command's first write. Output Python would encode other
/// than as UTF-8 is not reproduced, and once something is written the
/// command can no longer be handed to Python: so those go now.
pub fn begin() -> Result<()> {
    for stream in [Stream::Stdout, Stream::Stderr] {
        if !matches!(out::encoding(stream), Encoding::Utf8 | Encoding::Cp932) {
            return Err(RepoError::forward("output encoding other than UTF-8 or cp932 (write command)"));
        }
    }
    Ok(())
}

/// `begin()` and, for a command that reads the index in order to change it,
/// the lock on it (`repo.lock_index()`, issue #35): held until the command
/// ends (main releases it, also before a command goes to Python).
pub fn begin_locked(repo: &Repo) -> Result<()> {
    begin()?;
    repo.lock_index()?;
    Ok(())
}

/// A command that emits a plugin event (`post-switch`): Python would call
/// the installed plugins, which run only there.
pub fn no_plugins() -> Result<()> {
    match crate::pyenv::has_plugins() {
        Some(false) => Ok(()),
        Some(true) => Err(RepoError::forward("tmi.plugins are installed")),
        None => Err(RepoError::forward("could not tell whether tmi.plugins are installed")),
    }
}

pub fn warnings(out: &mut Out, warnings: &mut Vec<String>) {
    for w in warnings.drain(..) {
        out.eprintln(w);
    }
}

/// cli.py `_remind_reopen(repo.rewritten)`.
pub fn remind_reopen(out: &mut Out, repo: &Repo) {
    let shown: Vec<_> = repo.rewritten.lock().unwrap().iter().take(5).cloned().collect();
    if !shown.is_empty() {
        out.println("If any of these files is open in your drawing app, reopen it before saving:");
        for p in shown {
            out.println(format!("  {}", p.display()));
        }
    }
}

/// cli.py `_report_backups(repo, before)`.
pub fn report_backups(out: &mut Out, repo: &Repo, before: usize) -> Result<()> {
    let made = repo.read_backups()?.len() as i64 - before as i64;
    if made > 0 {
        out.println(format!("backed up {made} file(s) with uncommitted work first; see 'tmiki recover --files'"));
    }
    Ok(())
}
