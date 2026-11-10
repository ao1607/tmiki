//! What every write command shares: the point before its first write, and
//! the notices printed after rewriting working files.

use crate::out::Out;
use tmi_repo::{Repo, Result};

/// Called before a command's first write: whatever must be refused is
/// refused before this.
pub fn begin() -> Result<()> {
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
