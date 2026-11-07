//! Stage 4a of RUST_MIGRATION.md: `diff` (every mode and the HTML report),
//! `merge` (fast-forward, three-way, layer-aware, conflicts, abort), the
//! layer write-back of `restore` and stash push, as tmi/diff.py,
//! tmi/diff_html.py, tmi/merge.py and the merge / conflict / layer-restore
//! paths of tmi/repo.py do them, byte for byte.

pub mod diff;
pub mod html;
pub mod html_text;
pub mod localtime;
pub mod merge;
pub mod pix;
pub mod prof;
pub mod restore;
pub mod stage_layer;
pub mod stash;
pub mod writeback;
