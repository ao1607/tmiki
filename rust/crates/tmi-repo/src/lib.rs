//! tmi's repository (tmi/repo.py and its helpers): refs and HEAD,
//! revisions, commits and trees, the index, the stat cache, config, ignore
//! patterns, the reflog and hooks, and the writes that need no format
//! import (RUST_MIGRATION.md 3a, 3b-1).

pub mod clock;
pub mod config;
pub mod diskspace;
pub mod error;
pub mod fsio;
pub mod gc;
pub mod hooks;
pub mod ignore;
pub mod lockfile;
pub mod paths;
pub mod py;
pub mod reflog;
pub mod recpath;
pub mod refs_write;
pub mod repo;
pub mod stash;
pub mod textsafe;
pub mod status;
pub mod worktree;
pub mod write;
// stage 3b-2: add, commit and status with modified files
pub mod commit;
pub mod layer_changes;
// stage 4b
pub mod layer_ignore;
pub mod pyeq;
pub mod stage;
pub mod wfs;
pub mod worktree_status;
// PLUGINS.md P6: .tmiattributes, format plugins and their trust
pub mod attributes;
pub mod formats;
pub mod trust;
// PLUGINS.md P9: what a plugin said about a file, kept (6.2)
pub mod structure_cache;

pub use error::{RepoError, Result};
pub use repo::Repo;
