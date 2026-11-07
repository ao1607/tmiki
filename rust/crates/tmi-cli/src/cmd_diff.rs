//! `tmiki diff` (cli.py `cmd_diff`, `_diff_sides_from_args`,
//! `_print_diff_summary`, `_prune_diff_dirs`; stage 4a): every mode
//! (`--name-only`, `--name-status`, `--stat`, side by side, `--overlay`,
//! `--per-layer`, `--html`) between the working tree, the index and
//! commits, through tmi-diffmerge. The files it writes (preview PNGs, the
//! report) are Python's, byte for byte; nothing in the repository changes,
//! so a case it does not reproduce can still go to Python at any point.

use crate::args::{self, flag, opt, positional};
use crate::gates::fwd;
use crate::out::Out;
use crate::progress::Progress;
use crate::CmdResult;
use clap::{Arg, Command};
use std::path::PathBuf;
use tmi_diffmerge::diff::{self, DiffRun, DiffSide, FileResult, Mode, OutDir, Side};
use tmi_diffmerge::{html, localtime};
use tmi_repo::layer_changes::format_change;
use tmi_repo::{Repo, RepoError, Result};
use tmi_store::json::{Int, Value};

fn diff_cmd() -> Command {
    Command::new("diff")
        .arg(flag("side_by_side", None, Some("side-by-side")))
        .arg(flag("overlay", None, Some("overlay")))
        .arg(flag("per_layer", None, Some("per-layer")))
        .arg(flag("html", None, Some("html")))
        .arg(Arg::new("staged").long("staged").alias("cached").action(clap::ArgAction::SetTrue).num_args(0))
        .arg(flag("no_open", None, Some("no-open")))
        .arg(flag("name_only", None, Some("name-only")))
        .arg(flag("name_status", None, Some("name-status")))
        .arg(flag("stat", None, Some("stat")))
        .arg(opt("out", None, Some("out")))
        .arg(positional("args", true))
}

/// cli.py `_diff_sides_from_args(repo, args)`.
fn sides_from_args(repo: &Repo, values: &[String], staged: bool, out: &mut Out) -> Result<(DiffSide, DiffSide, Vec<String>)> {
    let mut revs: Vec<String> = Vec::new();
    let mut paths: Vec<String> = Vec::new();
    for (i, value) in values.iter().enumerate() {
        let mut warnings = Vec::new();
        let resolved = if revs.len() < 2 {
            match repo.resolve_rev(Some(value), &mut warnings) {
                Ok(r) => r,
                Err(RepoError::Runtime(_)) => None,
                Err(e) => return Err(e),
            }
        } else {
            None
        };
        crate::writes::warnings(out, &mut warnings);
        if resolved.is_some() && !repo.pathspec_exists_or_tracked(value)? {
            revs.push(value.clone());
        } else {
            paths = values[i..].to_vec();
            break;
        }
    }
    let mut commit_side = |rev: &str| -> Result<DiffSide> {
        let mut warnings = Vec::new();
        let r = repo.resolve_rev(Some(rev), &mut warnings);
        crate::writes::warnings(out, &mut warnings);
        match r? {
            Some(oid) => Ok(DiffSide { side: Side::Commit(oid), label: rev.to_string() }),
            None => Err(RepoError::runtime(format!("unknown revision: {rev}"))),
        }
    };
    if staged {
        if revs.len() > 1 {
            return Err(RepoError::runtime("--staged takes at most one revision"));
        }
        let head = revs.first().map(String::as_str).unwrap_or("HEAD");
        return Ok((commit_side(head)?, DiffSide::index(), paths));
    }
    if revs.len() == 2 {
        return Ok((commit_side(&revs[0])?, commit_side(&revs[1])?, paths));
    }
    if revs.len() == 1 {
        return Ok((commit_side(&revs[0])?, DiffSide::worktree(), paths));
    }
    if repo.current_commit()?.is_none() && repo.index_entries()?.is_empty() {
        return Err(RepoError::runtime("no commit to diff against"));
    }
    Ok((DiffSide::index(), DiffSide::worktree(), paths))
}

/// cli.py `_has_staged_changes(repo, paths)`.
fn has_staged_changes(repo: &Repo, paths: &[String]) -> Result<bool> {
    let staged = repo.index_entries()?;
    if staged.is_empty() || paths.is_empty() {
        return Ok(!staged.is_empty());
    }
    for spec in paths {
        match repo.pathspec_relpath(spec, &|p| staged.contains_key(p)) {
            Ok(rel) if staged.contains_key(&rel) => return Ok(true),
            Ok(_) => {}
            Err(RepoError::Forward(w)) => return Err(RepoError::Forward(w)),
            Err(_) => return Ok(false),
        }
    }
    Ok(false)
}

/// cli.py `_diff_retain_count(repo)`.
fn retain_count(repo: &Repo) -> Result<i64> {
    let config = match repo.read_config() {
        Ok(c) => c,
        Err(RepoError::Forward(w)) => return Err(RepoError::Forward(w)),
        Err(_) => return Ok(20),
    };
    let raw = match tmi_repo::config::get_value(&config, "diff.retain") {
        Ok(v) => v.cloned(),
        Err(RepoError::Forward(w)) => return Err(RepoError::Forward(w)),
        Err(_) => return Ok(20),
    };
    let value = match raw {
        None | Some(Value::Null) => return Ok(20),
        Some(Value::Int(Int::Small(n))) => n,
        Some(Value::Bool(b)) => b as i64,
        Some(Value::Float(x)) if x.is_finite() => x.trunc() as i64,
        Some(Value::Str(s)) => {
            let t = s.trim_matches([' ', '\t', '\n', '\r']);
            let digits = t.strip_prefix(['-', '+']).unwrap_or(t);
            if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) || digits.len() > 18 {
                return Err(fwd("diff.retain that int() may read differently"));
            }
            t.parse().map_err(|_| fwd("diff.retain"))?
        }
        Some(Value::Array(_)) | Some(Value::Object(_)) => return Ok(20),
        Some(_) => return Err(fwd("diff.retain of another type")),
    };
    Ok(value.max(1))
}

/// cli.py `_prune_diff_dirs(root, keep)`.
fn prune_diff_dirs(root: &std::path::Path, keep: i64) {
    let Ok(dir) = std::fs::read_dir(root) else { return };
    let mut dirs: Vec<(String, PathBuf)> = dir
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let b = name.as_bytes();
            let ok = b.len() == 15 && b[8] == b'-' && b.iter().enumerate().all(|(i, c)| i == 8 || c.is_ascii_digit());
            (ok && e.path().is_dir()).then(|| (name, e.path()))
        })
        .collect();
    dirs.sort_by(|a, b| b.0.cmp(&a.0));
    for (_, stale) in dirs.into_iter().skip(keep as usize) {
        let _ = std::fs::remove_dir_all(stale);
    }
}

fn layer_count_text(fr: &FileResult) -> String {
    let mut parts = Vec::new();
    for state in ["changed", "added", "deleted"] {
        let n = fr.layers.iter().filter(|l| l.state == state).count();
        if n > 0 {
            parts.push(format!("{n} {} {state}", if n == 1 { "layer" } else { "layers" }));
        }
    }
    parts.join(", ")
}

pub fn diff(argv: &[String], out: &mut Out) -> CmdResult {
    let m = args::parse(diff_cmd(), argv, &["args"])?;
    let f = |id: &str| args::get_flag(&m, id);
    if ["side_by_side", "overlay", "per_layer", "html"].iter().filter(|id| f(id)).count() > 1 {
        return Err(fwd("mutually exclusive options (argparse's error)"));
    }
    let (name_only, name_status, stat) = (f("name_only"), f("name_status"), f("stat"));
    let summary_only = name_only || name_status || stat;
    let mode = if summary_only {
        if stat {
            Mode::Summary
        } else {
            Mode::Names
        }
    } else if f("html") {
        Mode::Html
    } else if f("overlay") {
        Mode::Overlay
    } else if f("per_layer") {
        Mode::PerLayer
    } else {
        Mode::SideBySide
    };
    let repo = Repo::open()?;
    let out_arg = args::get_str(&m, "out");
    let auto_out = out_arg.is_none();
    if auto_out && !summary_only {
        // Output Python must encode (a legacy code page) may still send the
        // command to Python at the end; with a directory named by the clock
        // that would leave two of them, so such a run goes now.
        crate::writes::begin()?;
    }
    let out_dir = match &out_arg {
        Some(text) => OutDir { path: PathBuf::from(text), text: tmi_repo::paths::path_str(text)? },
        None => {
            let p = repo.repo_dir.join("diff").join(diff::auto_dir_name(&localtime::now()));
            OutDir { text: p.display().to_string(), path: p }
        }
    };
    let retain = if auto_out && !summary_only { Some(retain_count(&repo)?) } else { None };
    let values = args::get_many(&m, "args");
    let (left, right, paths) = sides_from_args(&repo, &values, f("staged"), out)?;
    let progress = Progress::new();
    let report = |msg: &str| progress.report(msg);
    let mut run = DiffRun { repo: &repo, mode, out: &out_dir, warns: Default::default(), progress: Some(&report) };
    let result = diff::diff_sides(&mut run, &left, &right, &paths);
    for line in run.warns.lines.drain(..) {
        out.eprintln(line);
    }
    let results = result?;
    let mut index_html: Option<String> = None;
    if !results.is_empty() && !summary_only {
        if let Some(keep) = retain {
            prune_diff_dirs(&repo.repo_dir.join("diff"), keep);
        }
        if mode == Mode::Html {
            progress.report("writing HTML report");
            let t = localtime::now();
            let ts = format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", t.year, t.month, t.day, t.hour, t.minute, t.second);
            let page = tmi_diffmerge::prof::time("html: render", || html::render_report(&results, &ts));
            let page = if cfg!(windows) { page.replace('\n', "\r\n") } else { page };
            std::fs::create_dir_all(&out_dir.path).map_err(|e| RepoError::runtime(format!("{}: {e}", out_dir.path.display())))?;
            let target = out_dir.path.join("index.html");
            std::fs::write(&target, page.as_bytes()).map_err(|e| RepoError::runtime(format!("{}: {e}", target.display())))?;
            index_html = Some(out_dir.join_text("index.html"));
        }
    }
    progress.done();
    tmi_diffmerge::prof::report();
    if results.is_empty() {
        if !(name_only || name_status) {
            out.println("no differences");
        }
        if matches!(left.side, Side::Index) && matches!(right.side, Side::Worktree) && has_staged_changes(&repo, &paths)? {
            out.eprintln("tmiki: hint: staged changes are not shown; use 'tmiki diff --staged' to see them");
        }
        if auto_out && out_dir.path.is_dir() && std::fs::read_dir(&out_dir.path).map(|mut d| d.next().is_none()).unwrap_or(false) {
            let _ = std::fs::remove_dir(&out_dir.path);
        }
        return Ok(0);
    }
    if summary_only {
        if name_only {
            for r in &results {
                out.println(r.path.clone());
            }
            return Ok(0);
        }
        if name_status {
            for r in &results {
                let letter = match r.state {
                    "deleted" => "D",
                    "added" => "A",
                    _ => "M",
                };
                out.println(format!("{letter}\t{}", r.path));
            }
            return Ok(0);
        }
        for r in &results {
            let detail = if r.binary {
                "binary".to_string()
            } else if r.state == "deleted" {
                "deleted".to_string()
            } else if r.raster {
                if r.state == "added" { "new image".into() } else { "pixels changed".into() }
            } else {
                let d = layer_count_text(r);
                if r.state == "added" {
                    if d.is_empty() { "new file".into() } else { format!("new file, {d}") }
                } else if d.is_empty() {
                    "modified, no layer changes".into()
                } else {
                    d
                }
            };
            let notes = if r.notes.is_empty() { String::new() } else { format!(" ({})", r.notes.join("; ")) };
            out.println(format!(" {} | {detail}{notes}", r.path));
        }
        let noun = if results.len() == 1 { "file" } else { "files" };
        out.println(format!(" {} {noun} changed", results.len()));
        return Ok(0);
    }
    for r in &results {
        out.println(format!("diff {}", r.path));
        for note in &r.notes {
            out.println(format!("  {note}"));
        }
        if r.binary {
            out.println("  binary file changed");
        }
        if let Some(b) = &r.bbox {
            out.println(format!("  composite bbox={}", html::bbox_text(b)));
        }
        if r.state == "deleted" {
            out.println("  deleted");
        }
        for layer in &r.layers {
            let suffix = layer.bbox.as_ref().map(|b| format!(" bbox={}", html::bbox_text(b))).unwrap_or_default();
            let what = if r.raster { "pixels".to_string() } else { format!("layer {}", layer.name) };
            out.println(format!("  {} {what}{suffix}", layer.state));
            for change in &layer.prop_changes {
                out.println(format!("    {}", format_change(change)));
            }
        }
        for output in &r.outputs {
            out.println(format!("  wrote {output}"));
        }
    }
    if let Some(index) = index_html {
        out.println(format!("wrote {index}"));
        if !f("no_open") {
            localtime::open_in_default_browser(&out_dir.path.join("index.html"));
        }
    }
    Ok(0)
}
