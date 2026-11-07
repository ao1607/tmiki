//! repo.py's status pieces shared by the status commands: `StatusItem`
//! and the untracked-file walk. `status_items` itself is
//! worktree_status.rs (stage 3b-2), one implementation for status and the
//! dirty-tree checks of switch / checkout / stash apply.

use crate::error::{RepoError, Result};
use crate::ignore::{self, IgnorePattern};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusItem {
    pub state: &'static str,
    pub path: String,
    pub detail: String,
}


const WALK_PRUNE_DIRS: [&str; 3] = [".tmi", ".git", "__pycache__"];

/// repo.py `iter_supported_files(root, patterns)`, sorted as Python sorts
/// the Paths, as posix paths relative to root.
pub fn untracked_candidates(root: &Path, patterns: &[IgnorePattern]) -> Result<Vec<String>> {
    let mut out: Vec<(Vec<String>, String)> = Vec::new();
    let mut stack: Vec<(Vec<String>, PathBuf)> = vec![(Vec::new(), root.to_path_buf())];
    while let Some((parts, dir)) = stack.pop() {
        let entries = std::fs::read_dir(&dir).map_err(|e| RepoError::forward(format!("{}: {e}", dir.display())))?;
        for entry in entries {
            let entry = entry.map_err(|e| RepoError::forward(e.to_string()))?;
            let name = entry.file_name().into_string().map_err(|_| RepoError::forward("non-UTF-8 file name"))?;
            let ft = entry.file_type().map_err(|e| RepoError::forward(e.to_string()))?;
            if ft.is_symlink() {
                return Err(RepoError::forward("symlink or junction in the working tree"));
            }
            let mut child = parts.clone();
            child.push(name.clone());
            if ft.is_dir() {
                if !WALK_PRUNE_DIRS.contains(&name.as_str()) {
                    stack.push((child, entry.path()));
                }
                continue;
            }
            if !ft.is_file() {
                continue;
            }
            let rel = child.join("/");
            if ignore::is_file_ignored(&rel, patterns)? {
                continue;
            }
            out.push((child, rel));
        }
    }
    // Path.__lt__ on Windows: the lowercased str split at the separator
    if cfg!(windows) {
        out.sort_by_cached_key(|(parts, _)| parts.iter().map(|p| p.to_lowercase()).collect::<Vec<_>>());
    } else {
        out.sort_by(|a, b| a.0.cmp(&b.0));
    }
    Ok(out.into_iter().map(|(_, rel)| rel).collect())
}
