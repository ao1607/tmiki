//! Layer-level `restore` (stage 4a): `restore [--source REV] [--in-place]
//! file:layer` and `restore --ours|--theirs file:layer` during a merge,
//! through tmi-diffmerge's write-back (cmd_worktree.rs calls these).

use crate::out::Out;
use std::path::PathBuf;
use tmi_diffmerge::restore::{self, Restored};
use tmi_diffmerge::writeback::WriteCtx;
use tmi_import::{Limits, Warnings};
use tmi_repo::{Repo, Result};

pub struct LayerWrites<'a> {
    repo: &'a Repo,
    limits: Option<Limits>,
    warns: Warnings,
}

impl<'a> LayerWrites<'a> {
    pub fn new(repo: &'a Repo) -> Result<LayerWrites<'a>> {
        Ok(LayerWrites { repo, limits: None, warns: Warnings::default() })
    }

    fn limits(&mut self) -> Result<Limits> {
        if self.limits.is_none() {
            self.limits = Some(restore::ctx_limits(self.repo)?);
        }
        Ok(self.limits.unwrap())
    }

    /// The warnings Python printed on the way (stderr).
    pub fn warnings(&mut self, out: &mut Out) {
        for line in self.warns.lines.drain(..) {
            out.eprintln(line);
        }
    }

    pub fn restore_layer(
        &mut self,
        spec: &str,
        source: Option<&str>,
        in_place: bool,
        output: Option<&str>,
        warnings: &mut Vec<String>,
    ) -> Result<PathBuf> {
        let limits = self.limits()?;
        let mut ctx = WriteCtx { limits, warns: &mut self.warns };
        Ok(match restore::restore_layer(self.repo, &mut ctx, spec, source, in_place, output, warnings)? {
            Restored::InPlace(p) | Restored::Exported(p) => p,
        })
    }

    pub fn export_from_worktree(&mut self, spec: &str, in_place: bool, output: Option<&str>) -> Result<PathBuf> {
        let limits = self.limits()?;
        let mut ctx = WriteCtx { limits, warns: &mut self.warns };
        restore::export_from_worktree(self.repo, &mut ctx, spec, in_place, output)
    }

    pub fn resolve_conflict_layer(&mut self, spec: &str, side: &str) -> Result<PathBuf> {
        let limits = self.limits()?;
        let mut ctx = WriteCtx { limits, warns: &mut self.warns };
        restore::resolve_conflict_layer(self.repo, &mut ctx, spec, side)
    }
}
