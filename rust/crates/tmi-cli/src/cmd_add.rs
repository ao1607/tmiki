//! `tmiki add` (cli.py `cmd_add`, `_stage_matching_paths`,
//! `_make_path_filter`, `_is_bulk_add_spec`), stage 3b-2. The imports run
//! in tmi-import; a layer stage (`path:layer`), `.tmiignore` layer
//! patterns, a merge in progress and files tmi-import declines go to
//! Python.

use crate::args::{self, flag, positional};
use crate::gates::fwd;
use crate::out::Out;
use crate::progress::Progress;
use crate::CmdResult;
use clap::Command;
use std::collections::HashSet;
use std::path::Path;
use tmi_repo::stage::{pathspec_full_path, pathspec_relpath, read_file_digest, relpath, Stager};
use tmi_repo::status::untracked_candidates;
use tmi_repo::{py, Repo, RepoError};
use tmi_store::json::Value;

fn add_cmd() -> Command {
    Command::new("add")
        .arg(flag("all", Some('A'), Some("all")))
        .arg(flag("update", Some('u'), Some("update")))
        .arg(flag("force", Some('f'), Some("force")))
        .arg(flag("renormalize", None, Some("renormalize")))
        .arg(positional("specs", true))
}

/// Files a bulk add could not stage: (path, why).
pub type Failures = Vec<(String, String)>;

/// `stager.stage_file(...)` in a bulk add: a file whose format plugin is
/// not available or cannot read it, or that is over the pixel limit, goes to `failures` (Ok(None)) and the
/// rest go on, as cli.py's `_stage_matching_paths` does with any file it
/// cannot add (issues #37, #49). Any other error stops the add.
pub fn stage_or_note(
    stager: &mut Stager,
    rel: &str,
    spec: &str,
    force: bool,
    source: Option<(Vec<u8>, String)>,
    failures: &mut Failures,
) -> Result<Option<bool>, RepoError> {
    match stager.stage_file(spec, force, source) {
        Ok((_rel, changed)) => Ok(Some(changed)),
        Err(e) if stager.plugin_refused => {
            failures.push((rel.to_string(), e.to_string()));
            Ok(None)
        }
        Err(e) => Err(e),
    }
}

/// cli.py `_report_bulk_add`'s tail: the files that could not be added, on
/// stderr; 1 if there are any (as Git's `add` exits when it refuses a
/// path). A plugin's refusals say once, at the end, how such files can be
/// kept as opaque blobs instead.
pub fn report_failures(out: &mut Out, failures: &Failures) -> i32 {
    if failures.is_empty() {
        return 0;
    }
    let count = failures.len();
    out.eprintln(format!("tmiki: error: {count} {} could not be added:", if count == 1 { "file" } else { "files" }));
    let mut hint = false;
    for (rel, reason) in failures {
        let reason = reason.split_whitespace().collect::<Vec<_>>().join(" ");
        let reason = reason.strip_prefix(&format!("{rel}: ")).unwrap_or(&reason);
        let reason = match reason.strip_suffix(tmi_repo::formats::ON_ERROR_HINT) {
            Some(r) => {
                hint = true;
                r
            }
            None => reason,
        };
        out.eprintln(format!("  {rel}: {reason}"));
    }
    if hint {
        out.eprintln(format!(
            "tmiki: hint: with on-error=binary in {}, a file the plugin cannot read is kept as an opaque blob instead",
            tmi_repo::attributes::FILE
        ));
    }
    1
}

/// `add --renormalize [specs]` (PLUGINS.md 4.3): tracked files (all, or
/// those `specs` name) imported again even when their bytes are unchanged:
/// after `.tmiattributes` or a plugin changed.
fn renormalize(stager: &mut Stager, repo: &Repo, specs: &[String], failures: &mut Failures) -> Result<(Vec<String>, bool), RepoError> {
    let root = repo.require_worktree()?.to_path_buf();
    let index_entries = stager.index_entries()?;
    let known: HashSet<String> = stager.head_entries.keys().chain(index_entries.keys()).cloned().collect();
    let filters = specs.iter().map(|s| path_filter(repo, s, &known)).collect::<Result<Vec<_>, _>>()?;
    let mut tracked: Vec<String> = known.iter().cloned().collect();
    tracked.sort();
    let mut staged = Vec::new();
    let mut matched_any = false;
    for rel in &tracked {
        let mut hit = filters.is_empty();
        for f in &filters {
            hit = hit || f.matches(rel)?;
        }
        let entry = index_entries.get(rel).or_else(|| stager.head_entries.get(rel));
        if !hit || entry.is_some_and(|e| e.get("deleted").is_some_and(Value::truthy)) {
            continue;
        }
        matched_any = true;
        let full = root.join(rel);
        if !full.is_file() {
            continue;
        }
        if stage_or_note(stager, rel, &full.to_string_lossy(), true, None, failures)? == Some(true) {
            staged.push(rel.clone());
        }
    }
    Ok((staged, matched_any))
}

/// Flush what Python would have written before an error, then the error.
fn settle<T>(stager: &mut Stager, out: &mut Out, result: Result<T, RepoError>) -> Result<T, RepoError> {
    match result {
        Err(RepoError::Forward(w)) => Err(RepoError::Forward(w)),
        Err(e) => {
            emit_warnings(stager, out);
            stager.finish()?;
            Err(e)
        }
        Ok(v) => Ok(v),
    }
}

pub fn emit_warnings(stager: &mut Stager, out: &mut Out) {
    for line in stager.warns.lines.drain(..) {
        match line.strip_prefix(tmi_import::warn::STDOUT_MARK) {
            Some(text) => out.println(text),
            None => out.eprintln(line),
        }
    }
}

pub fn add(argv: &[String], out: &mut Out) -> CmdResult {
    let m = args::parse(add_cmd(), argv, &["specs"])?;
    let specs = args::get_many(&m, "specs");
    let (all, update, force) = (args::get_flag(&m, "all"), args::get_flag(&m, "update"), args::get_flag(&m, "force"));
    let repo = Repo::open()?;
    crate::writes::begin_locked(&repo)?;
    if specs.is_empty() && !(all || update || args::get_flag(&m, "renormalize")) {
        return Err(RepoError::runtime("add requires pathspecs, or -A/--all"));
    }
    // A symbolic link or junction named on the command line: Python says it
    // is not tracked (issue #44).
    for spec in &specs {
        if !spec.contains(['*', '?', '[']) && repo.pathspec_link(&tmi_repo::paths::split_pathspec(spec)?.0)?.is_some() {
            return Err(fwd("a symbolic link or junction named on add"));
        }
    }
    let progress = Progress::new();
    let report = |msg: &str| progress.report(msg);
    let mut stager = Stager::new(&repo)?;
    stager.progress = Some(&report);
    if args::get_flag(&m, "renormalize") {
        crate::gates::no_merge_state(&repo)?;
        let mut failures = Failures::new();
        let result = renormalize(&mut stager, &repo, &specs, &mut failures);
        let (staged, matched_any) = settle(&mut stager, out, result)?;
        emit_warnings(&mut stager, out);
        stager.finish()?;
        if !specs.is_empty() && !matched_any {
            return Err(RepoError::runtime("pathspec did not match any files"));
        }
        for rel in staged {
            out.println(format!("staged {rel}"));
        }
        progress.done();
        return Ok(report_failures(out, &failures));
    }
    let bulk = all || update || {
        let mut any = false;
        for s in &specs {
            if is_bulk_spec(&repo, s)? {
                any = true;
            }
        }
        any
    };
    if bulk {
        let mut failures = Failures::new();
        let result = stage_matching_paths(&mut stager, &repo, &specs, all || !(all || update), &mut failures);
        let (staged, matched_any) = settle(&mut stager, out, result)?;
        emit_warnings(&mut stager, out);
        if !specs.is_empty() && !matched_any {
            stager.finish()?;
            return Err(RepoError::runtime("pathspec did not match any files"));
        }
        stager.finish()?;
        for rel in &staged {
            out.println(format!("staged {rel}"));
        }
        for w in tmi_repo::formats::removed_component_warnings(&repo, &staged)? {
            out.eprintln(w);
        }
        progress.done();
        return Ok(report_failures(out, &failures));
    }
    let pending: Vec<String> = repo.pending_conflict_paths()?;
    for spec in &specs {
        let untouched = if pending.is_empty() { None } else { untouched_conflicts(&repo, spec, &pending)? };
        let result = match plugin_layer_spec(&repo, &stager, spec)? {
            // `file:layer` of a plugin format: the executable's (PLUGINS.md P7)
            Some((rel, full, layer, id)) => tmi_diffmerge::stage_layer::stage_plugin_layer(&mut stager, &rel, &full, &layer, &id),
            None => match builtin_layer_spec(&repo, spec)? {
                // `file:layer` of a built-in format
                Some((rel, file_part, full, layer)) => {
                    tmi_diffmerge::stage_layer::stage_builtin_layer(&mut stager, &rel, &file_part, &full, &layer, force)
                }
                None => stager.stage_file(spec, force, None),
            },
        };
        let (rel, changed) = settle(&mut stager, out, result)?;
        emit_warnings(&mut stager, out);
        if let Some(items) = untouched.filter(|i| !i.is_empty()) {
            if items.iter().any(|c| c.get("stable_source_id").is_some_and(Value::truthy)) {
                let mut names = Vec::new();
                for c in &items {
                    let v = ["path_name", "name", "reason"].iter().filter_map(|k| c.get(*k)).find(|v| v.truthy());
                    names.push(match v {
                        Some(v) => v.py_str().ok_or_else(|| fwd("str() of a container"))?,
                        None => "None".to_string(),
                    });
                }
                out.eprintln(format!(
                    "warning: {rel} was staged exactly as the merge left it, so the conflicting layers keep our version: {} (their side is dropped)",
                    names.join(", ")
                ));
            } else {
                // A conflict of the whole file (no layers): issue #43.
                out.eprintln(format!(
                    "warning: {rel} was staged exactly as the merge left it, so it keeps our version (their side is dropped)"
                ));
            }
        }
        if changed {
            out.println(format!("staged {rel}"));
            if tmi_repo::attributes::component_id(&rel).is_some() {
                // the index as staged so far, for the warning to read
                stager.finish()?;
                for w in tmi_repo::formats::removed_component_warnings(&repo, std::slice::from_ref(&rel))? {
                    out.eprintln(w);
                }
            }
        }
    }
    stager.finish()?;
    progress.done();
    Ok(0)
}

/// A `file:layer` spec of a built-in layered or image extension (repo.py
/// `split_pathspec`): (rel, the file part, the working file, the layer).
fn builtin_layer_spec(repo: &Repo, spec: &str) -> Result<Option<(String, String, std::path::PathBuf, String)>, RepoError> {
    let (file_part, layer) = tmi_repo::paths::split_pathspec(spec)?;
    let Some(layer) = layer else { return Ok(None) };
    let full = repo.pathspec_full_path(&file_part)?;
    if !full.exists() {
        return Err(RepoError::NotFound(format!("no such file to add: {}", full.display())));
    }
    let rel = repo.relpath(&full)?;
    Ok(Some((rel, file_part, full, layer)))
}

/// A `file:layer` spec whose file `.tmiattributes` maps to a format plugin:
/// (rel, the working file, the layer selector, the plugin's id).
fn plugin_layer_spec(repo: &Repo, stager: &Stager, spec: &str) -> Result<Option<(String, std::path::PathBuf, String, String)>, RepoError> {
    let (file_part, layer) = tmi_repo::paths::split_layer_spec(repo, spec)?;
    let Some(layer) = layer else { return Ok(None) };
    let full = repo.pathspec_full_path(&file_part)?;
    let rel = match repo.relpath(&full) {
        Ok(r) => r,
        Err(RepoError::Forward(w)) => return Err(RepoError::Forward(w)),
        Err(_) => return Ok(None),
    };
    let Some(id) = stager.ctx.formats.mapping(&rel).plugin_id().map(str::to_string) else { return Ok(None) };
    if !full.is_file() {
        return Err(RepoError::NotFound(format!("no such file to add: {}", full.display())));
    }
    Ok(Some((rel, full, layer, id)))
}

/// cli.py `cmd_add`'s check before staging a conflicted file
/// (`repo.conflict_left_untouched(rel)`): its conflicts when the working
/// file is still what the merge left there.
fn untouched_conflicts(repo: &Repo, spec: &str, pending: &[String]) -> Result<Option<Vec<Value>>, RepoError> {
    let (file_part, _) = tmi_repo::paths::split_pathspec(spec)?;
    let rel = match repo.pathspec_relpath(&file_part, &|p| pending.iter().any(|x| x == p)) {
        Ok(r) => r,
        Err(RepoError::Forward(w)) => return Err(RepoError::Forward(w)),
        Err(_) => return Ok(None),
    };
    if !pending.contains(&rel) {
        return Ok(None);
    }
    let Some(meta) = repo.read_conflict_meta(&rel)?.filter(Value::truthy) else { return Ok(None) };
    let Some(want) = meta.get("worktree_digest").filter(|v| v.truthy()) else { return Ok(None) };
    // `restore --ours/--theirs <file>` chose a side for the whole file
    // (issue #43): staging it is no surprise.
    if matches!(meta.get("taken"), Some(Value::Object(m)) if m.contains_key("*")) {
        return Ok(None);
    }
    let full = repo.abspath(&rel)?;
    if !full.is_file() || !matches!(want, Value::Str(w) if *w == tmi_repo::fsio::sha256_file(&full)?) {
        return Ok(None);
    }
    Ok(Some(match meta.get("conflicts") {
        Some(Value::Array(items)) => items.clone(),
        Some(v) if v.truthy() => return Err(fwd("conflicts is not a list")),
        _ => Vec::new(),
    }))
}

/// `_is_bulk_add_spec(repo, spec)`.
fn is_bulk_spec(repo: &Repo, spec: &str) -> Result<bool, RepoError> {
    if !Path::new(spec).exists() && looks_like_layer_spec(spec) {
        return Ok(false);
    }
    if spec.contains(['*', '?', '[']) {
        return Ok(true);
    }
    let full = pathspec_full_path(repo, spec)?;
    Ok(full.is_dir())
}

/// `split_pathspec`'s regex (the layer part is what Stager forwards).
fn looks_like_layer_spec(spec: &str) -> bool {
    let lower = spec.to_lowercase();
    spec.match_indices(':').any(|(i, _)| {
        i + 1 < spec.len()
            && lower.get(..i).is_some_and(|h| [".png", ".jpg", ".jpeg", ".mdp", ".clip"].iter().any(|e| h.len() > e.len() && h.ends_with(e)))
    })
}

enum Filter {
    Glob(String),
    Prefix(String),
    Exact(String),
}

impl Filter {
    fn matches(&self, rel: &str) -> Result<bool, RepoError> {
        Ok(match self {
            Filter::Glob(p) => py::fnmatch(rel, p).map_err(|_| fwd("glob Python's re rejects"))?,
            Filter::Prefix(p) => p.is_empty() || rel == p || rel.starts_with(&format!("{p}/")),
            Filter::Exact(r) => rel == r,
        })
    }
}

/// `_make_path_filter(repo, spec, known_paths)`.
fn path_filter(repo: &Repo, spec: &str, known: &HashSet<String>) -> Result<Filter, RepoError> {
    if spec.contains(['*', '?', '[']) {
        return Ok(Filter::Glob(spec.replace('\\', "/")));
    }
    let full = pathspec_full_path(repo, spec)?;
    if full.is_dir() {
        let root = py::realpath(repo.require_worktree()?);
        let rel_dir = if py::realpath(&full) == root { String::new() } else { relpath(repo, &full)? };
        return Ok(Filter::Prefix(rel_dir.trim_end_matches('/').to_string()));
    }
    Ok(Filter::Exact(pathspec_relpath(repo, spec, known)?))
}

/// `_stage_matching_paths(repo, specs, include_untracked=...)`.
fn stage_matching_paths(
    stager: &mut Stager,
    repo: &Repo,
    specs: &[String],
    include_untracked: bool,
    failures: &mut Failures,
) -> Result<(Vec<String>, bool), RepoError> {
    let root = repo.require_worktree()?.to_path_buf();
    let index_entries = stager.index_entries()?;
    let known: HashSet<String> = stager.head_entries.keys().chain(index_entries.keys()).cloned().collect();
    let filters = specs.iter().map(|s| path_filter(repo, s, &known)).collect::<Result<Vec<_>, _>>()?;
    let matches = |rel: &str| -> Result<bool, RepoError> {
        if filters.is_empty() {
            return Ok(true);
        }
        for f in &filters {
            if f.matches(rel)? {
                return Ok(true);
            }
        }
        Ok(false)
    };
    let mut staged = Vec::new();
    let mut matched_any = false;
    let mut tracked: Vec<String> = known.iter().cloned().collect();
    tracked.sort();
    let mut steps = Vec::new();
    for rel in &tracked {
        if !matches(rel)? {
            continue;
        }
        matched_any = true;
        steps.push(rel.clone());
    }
    let head_entries = stager.head_entries.clone();
    let tracked_step = |rel: String| -> Result<Option<Step>, RepoError> {
        let entry = index_entries.get(&rel);
        let full = root.join(&rel);
        if !full.exists() {
            if !entry.is_some_and(|e| e.get("deleted").is_some_and(Value::truthy)) {
                return Ok(Some(Step::Delete(rel)));
            }
            return Ok(None);
        }
        let target = match entry {
            Some(e) if !e.get("deleted").is_some_and(Value::truthy) => Some(e),
            _ => head_entries.get(&rel),
        };
        let mut source = None;
        if let Some(t) = target {
            let (bytes, digest) = read_file_digest(&full)?;
            if matches!(t.get("source_digest"), Some(Value::Str(d)) if *d == digest) {
                return Ok(None);
            }
            source = Some((bytes, digest));
        }
        Ok(Some(Step::Stage { rel, full, source }))
    };
    drive(stager, steps.into_iter().map(tracked_step), &mut staged, failures)?;
    if include_untracked {
        let patterns = stager.patterns()?.to_vec();
        let mut untracked = Vec::new();
        for rel in untracked_candidates(&root, &patterns)? {
            if known.contains(&rel) || !matches(&rel)? {
                continue;
            }
            matched_any = true;
            untracked.push(rel);
        }
        let untracked_step = |rel: String| -> Result<Option<Step>, RepoError> {
            let full = root.join(&rel);
            Ok(Some(Step::Stage { rel, full, source: None }))
        };
        drive(stager, untracked.into_iter().map(untracked_step), &mut staged, failures)?;
    }
    Ok((staged, matched_any))
}

/// One path of a bulk add, in order.
enum Step {
    Delete(String),
    /// `source`: the bytes and digest, when already read.
    Stage { rel: String, full: std::path::PathBuf, source: Option<(Vec<u8>, String)> },
}

/// Runs `steps` in order. While one file is staged, the next one, when it
/// maps to a format plugin this command has already loaded, is read and
/// its structure read by the plugin on a thread of its own (PLUGINS.md
/// 6.1: the next file's `read-structure` beside this one's decode); every
/// other file is staged exactly as before.
fn drive(
    stager: &mut Stager,
    mut steps: impl Iterator<Item = Result<Option<Step>, RepoError>>,
    staged: &mut Vec<String>,
    failures: &mut Failures,
) -> Result<(), RepoError> {
    let mut next = || steps.by_ref().find(|s| !matches!(s, Ok(None)));
    let mut ahead = next();
    while let Some(step) = ahead.take() {
        ahead = next();
        if let Some(Ok(Some(Step::Stage { rel, full, source }))) = &mut ahead {
            if stager.ctx.formats.maps_to_loaded_plugin(rel) {
                if source.is_none() {
                    // read here rather than in stage_file (not twice)
                    *source = read_file_digest(full).ok();
                }
                if let Some((bytes, digest)) = source {
                    let name = tmi_repo::stage::file_name(full);
                    let shared = std::sync::Arc::new(bytes.clone());
                    stager.ctx.formats.prefetch(rel, &name, &shared, digest);
                }
            }
        }
        match step? {
            None => {}
            Some(Step::Delete(rel)) => {
                stager.stage_delete(&rel);
                staged.push(rel);
            }
            Some(Step::Stage { rel, full, source }) => {
                if stage_or_note(stager, &rel, &full.to_string_lossy(), false, source, failures)? == Some(true) {
                    staged.push(rel);
                }
            }
        }
    }
    Ok(())
}
