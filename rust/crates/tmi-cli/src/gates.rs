//! What a command does not take on: a merge in progress for `add
//! --renormalize`.

use tmi_repo::{Repo, RepoError};

pub fn fwd(why: &str) -> RepoError {
    RepoError::forward(why)
}

/// A merge in progress or conflict records.
pub fn no_merge_state(repo: &Repo) -> Result<(), RepoError> {
    if repo.pending_merge_head()?.is_some() {
        return Err(fwd("merge in progress"));
    }
    if let Ok(mut dir) = std::fs::read_dir(repo.repo_dir.join("conflicts")) {
        if dir.next().is_some() {
            return Err(fwd("conflict records"));
        }
    }
    Ok(())
}
