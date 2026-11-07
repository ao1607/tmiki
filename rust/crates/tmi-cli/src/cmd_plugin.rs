//! `tmiki plugin add | list | prepare | reimport | trust | remove` (PLUGINS.md 4.6,
//! 7.1; P6). The executable's own: Python has the parser (for `-h` and
//! argparse's errors) and refuses to run them (format plugins run here
//! only).

use crate::args::{self, flag, positional};
use crate::out::Out;
use crate::CmdResult;
use clap::Command;
use tmi_repo::attributes::{self, Attributes};
use tmi_repo::formats::{self, Formats};
use tmi_repo::stage::Stager;
use tmi_repo::{trust, Repo, RepoError};
use tmi_store::json::Value;

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(12)]
}

pub fn plugin(argv: &[String], out: &mut Out) -> CmdResult {
    match argv.first().map(String::as_str) {
        None | Some("list") => list(argv.get(1..).unwrap_or(&[]), out),
        Some("add") => add(&argv[1..], out),
        Some("prepare") => prepare(&argv[1..], out),
        Some("reimport") => reimport(&argv[1..], out),
        Some("trust") => trust_cmd(&argv[1..], out),
        Some("remove") => remove(&argv[1..], out),
        // P8: the plugin author's tools (cmd_plugin_dev.rs: dev; cmd_plugin_tools.rs: the rest)
        Some("dev") => crate::cmd_plugin_dev::dev(&argv[1..], out),
        Some("new") => crate::cmd_plugin_tools::new(&argv[1..], out),
        Some("build") => crate::cmd_plugin_tools::build(&argv[1..], out),
        Some("test") => crate::cmd_plugin_tools::test(&argv[1..], out),
        Some("doctor") => crate::cmd_plugin_tools::doctor(&argv[1..], out),
        Some("install") => crate::cmd_plugin_tools::install(&argv[1..], out),
        Some(other) => Err(RepoError::forward(format!("plugin {other} (argparse's error)"))),
    }
}

/// (major, minor, patch) of a semver version, pre-release ignored.
fn semver(v: &str) -> (u64, u64, u64) {
    let core = v.split(['-', '+']).next().unwrap_or("");
    let mut it = core.split('.').map(|p| p.parse::<u64>().unwrap_or(0));
    (it.next().unwrap_or(0), it.next().unwrap_or(0), it.next().unwrap_or(0))
}

fn load_component(repo: &Repo, bytes: &[u8], what: &str) -> Result<tmi_plugin_host::Plugin, RepoError> {
    let host = formats::host(repo).map_err(RepoError::runtime)?;
    host.load(bytes).map_err(|e| RepoError::runtime(format!("{what}: {e}")))
}

/// The `.tmiattributes` line for `pattern` and plugin `id`, or an error when
/// the pattern could write more than one line or more than one rule: a
/// newline, any whitespace or control, `#`, `=`, a quote, `!` or a leading
/// `/` or `.`-only pattern. Patterns tmi makes are `*.<[a-z0-9]+>`; the ones a
/// person types may use the glob characters `* ? [ ]` and `/`.
fn attribute_line(pattern: &str, id: &str) -> Result<String, RepoError> {
    let bad = pattern.is_empty()
        || pattern.len() > 200
        || pattern.chars().any(|c| c.is_whitespace() || c.is_control() || matches!(c, '#' | '=' | '"' | '\'' | '\\' | '!' | '`'))
        || pattern.starts_with('/');
    if bad {
        return Err(RepoError::runtime(format!(
            "the pattern {} cannot be written to {}: it has whitespace, a control character, one of # = \" ' \\ ! ` or is empty, too long or starts with /",
            tmi_format::safe_text::line(pattern),
            attributes::FILE
        )));
    }
    Ok(format!("{pattern} format=plugin:{id}"))
}

/// `tmiki plugin add <file.wasm | id[@version]> [pattern ...] [--force]`.
fn add(argv: &[String], out: &mut Out) -> CmdResult {
    let cmd = Command::new("add").arg(positional("file", false)).arg(positional("patterns", true)).arg(flag("force", None, Some("force")));
    let m = args::parse(cmd, argv, &["file", "patterns"])?;
    let Some(file) = args::get_str(&m, "file") else { return Err(RepoError::forward("plugin add without a file (argparse's error)")) };
    let repo = Repo::open()?;
    crate::writes::begin_locked(&repo)?;
    let root = repo.require_worktree()?.to_path_buf();
    // a file, or an installed plugin: <id>[@<version>] (tmiki plugin install)
    let path = crate::cmd_plugin_tools::component_path(&file)?;
    let bytes = std::fs::read(&path).map_err(|e| RepoError::runtime(format!("{file}: {e}")))?;
    let plugin = load_component(&repo, &bytes, &file)?;
    let info = plugin.info.clone();
    let importer = plugin.importer();
    let id = info.id.clone();
    let rel = format!("{}/{id}.wasm", attributes::PLUGIN_DIR);
    let dest = root.join(attributes::PLUGIN_DIR).join(format!("{id}.wasm"));
    if let Ok(old) = std::fs::read(&dest) {
        if old != bytes {
            if let Ok(prev) = load_component(&repo, &old, &rel) {
                if semver(&info.version) < semver(&prev.info.version) && !args::get_flag(&m, "force") {
                    return Err(RepoError::runtime(format!(
                        "{rel} holds plugin {id} {}, newer than {}; use --force to replace it",
                        prev.info.version, info.version
                    )));
                }
            }
        }
    }
    let mut patterns = args::get_many(&m, "patterns");
    if patterns.is_empty() {
        for f in &info.formats {
            for ext in &f.extensions {
                // check_info already limits an extension to [a-z0-9]; a pattern is
                // checked again where the line is written (attribute_line)
                let p = format!("*.{}", ext.strip_prefix('.').unwrap_or(ext).to_ascii_lowercase());
                if !patterns.contains(&p) {
                    patterns.push(p);
                }
            }
        }
    }
    if patterns.is_empty() {
        return Err(RepoError::runtime(format!("plugin {id} names no file extensions; name the files it reads: tmiki plugin add <file.wasm> '*.ext'")));
    }
    // every line is made and checked before anything is written
    let mut lines = Vec::new();
    for p in &patterns {
        lines.push((p.clone(), attribute_line(p, &id)?));
    }
    // the component, then the lines .tmiattributes lacks
    tmi_repo::fsio::mkdir_parent(&dest)?;
    tmi_repo::fsio::write_bytes(&dest, &bytes)?;
    let attrs_path = root.join(attributes::FILE);
    let mut text = match std::fs::read(&attrs_path) {
        Ok(b) => String::from_utf8_lossy(&b).into_owned(),
        Err(_) => String::new(),
    };
    let current = Attributes::parse(&text);
    let mut added = Vec::new();
    for (p, line) in &lines {
        if !current.has_line(p, &id) {
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(line);
            text.push('\n');
            added.push(line.clone());
        }
    }
    if !added.is_empty() {
        tmi_repo::fsio::write_bytes(&attrs_path, text.as_bytes())?;
    }
    // Adding a plugin yourself is reading it on purpose: no notice later.
    // Writing still asks (trust.rs).
    let c = trust::Component { id: &id, version: &info.version, sha256: &importer.component_sha256 };
    if trust::setting(&repo)? != trust::Setting::Deny {
        trust::grant_read(&repo, &c)?;
    }
    let mut stager = Stager::new(&repo)?;
    let mut staged = Vec::new();
    for spec in [dest.clone(), attrs_path.clone()] {
        let (rel, changed) = stager.stage_file(&spec.to_string_lossy(), true, None)?;
        if changed {
            staged.push(rel);
        }
    }
    crate::cmd_add::emit_warnings(&mut stager, out);
    stager.finish()?;
    out.println(format!("added plugin {id} {} (sha256 {}..., {}) as {rel}", info.version, short(&importer.component_sha256), importer.abi));
    for line in &added {
        out.println(format!("{}: {line}", attributes::FILE));
    }
    for rel in staged {
        out.println(format!("staged {rel}"));
    }
    if importer.abi.starts_with("tmi:format@0.") {
        // nothing to do now; said once, so the 1.0 step is no surprise (issue #51)
        out.eprintln(format!(
            "tmiki: note: plugin {id} is built for {}, the experimental plugin interface. Nothing to do now; when tmi's interface reaches 1.0, 'tmiki plugin reimport {id}' records its files again.",
            importer.abi
        ));
    }
    let tracked = tracked_for(&repo, &Formats::for_repo(&repo), &id)?;
    if !tracked.is_empty() {
        out.println(format!(
            "{} tracked file(s) map to it; 'tmiki plugin reimport {id}' imports them with it",
            tracked.len()
        ));
    }
    Ok(0)
}

/// Tracked files (HEAD and index, not deleted, in the working tree) that
/// `.tmiattributes` maps to plugin `id`, or whose record plugin `id` made.
fn tracked_for(repo: &Repo, formats: &Formats, id: &str) -> Result<Vec<String>, RepoError> {
    let root = repo.require_worktree()?.to_path_buf();
    let head = repo.entries_for_commit(repo.current_commit()?.as_deref())?;
    let index = repo.index_entries()?;
    let mut rels: Vec<String> = head.keys().chain(index.keys()).cloned().collect();
    rels.sort();
    rels.dedup();
    let mut out = Vec::new();
    for rel in rels {
        let entry = index.get(&rel).or_else(|| head.get(&rel));
        if entry.is_some_and(tmi_repo::stage::is_deleted) || !root.join(&rel).is_file() {
            continue;
        }
        let recorded = entry.and_then(|e| e.get("importer")).and_then(|i| i.get("id")).and_then(Value::as_str) == Some(id);
        if recorded || formats.mapping(&rel).plugin_id() == Some(id) {
            out.push(rel);
        }
    }
    Ok(out)
}

/// `tmiki plugin list`: the built-in formats and the repository's plugins.
fn list(argv: &[String], out: &mut Out) -> CmdResult {
    args::parse(Command::new("list"), argv, &[])?;
    let repo = Repo::open()?;
    let formats = Formats::for_repo(&repo);
    let mut rows: Vec<[String; 8]> = vec![[
        "id".into(),
        "version".into(),
        "formats".into(),
        "patterns".into(),
        "sha256".into(),
        "abi".into(),
        "trust".into(),
        "cache".into(),
    ]];
    rows.push([
        "builtin".into(),
        env!("CARGO_PKG_VERSION").into(),
        "clip mdp2 png jpeg gif bmp tiff webp psd".into(),
        "*.clip *.mdp, image signatures".into(),
        "-".into(),
        "-".into(),
        "-".into(),
        "-".into(),
    ]);
    let attrs = formats.attributes();
    let root = repo.require_worktree()?.to_path_buf();
    for id in formats::plugin_ids(&repo, &formats) {
        let patterns = attrs.patterns_for(&id).join(" ");
        let patterns = if patterns.is_empty() { "-".to_string() } else { patterns };
        let path = root.join(attributes::PLUGIN_DIR).join(format!("{id}.wasm"));
        let Ok(bytes) = std::fs::read(&path) else {
            rows.push([id.clone(), "-".into(), "-".into(), patterns, "-".into(), "-".into(), "-".into(), "missing".into()]);
            continue;
        };
        let sha = tmi_store::oid::sha256_hex(&bytes);
        let host = formats::host(&repo).map_err(RepoError::runtime)?;
        let cached = host.is_cached(&sha);
        match host.load(&bytes) {
            Ok(p) => {
                let fmts: Vec<String> = p.info.formats.iter().map(|f| f.name.clone()).collect();
                let importer = p.importer();
                let c = trust::Component { id: &id, version: &p.info.version, sha256: &sha };
                rows.push([
                    id.clone(),
                    p.info.version.clone(),
                    fmts.join(" "),
                    patterns,
                    format!("{}...", short(&sha)),
                    importer.abi,
                    trust::describe(&repo, &c)?,
                    if cached { "cached".into() } else { "compiled now".into() },
                ]);
            }
            Err(e) => rows.push([id.clone(), "-".into(), "-".into(), patterns, format!("{}...", short(&sha)), "-".into(), "-".into(), format!("unusable: {e}")]),
        }
    }
    let widths: Vec<usize> = (0..8).map(|c| rows.iter().map(|r| r[c].chars().count()).max().unwrap_or(0)).collect();
    for r in &rows {
        let mut line = String::new();
        for (c, cell) in r.iter().enumerate() {
            if c + 1 == r.len() {
                line.push_str(cell);
            } else {
                line.push_str(cell);
                line.push_str(&" ".repeat(widths[c] - cell.chars().count() + 2));
            }
        }
        out.println(line.trim_end().to_string());
    }
    Ok(0)
}

/// `tmiki plugin prepare`: every plugin checked, compiled and trusted ahead
/// of use (what `clone` does at its end).
fn prepare(argv: &[String], out: &mut Out) -> CmdResult {
    args::parse(Command::new("prepare"), argv, &[])?;
    let repo = Repo::open()?;
    let formats = Formats::for_repo(&repo);
    let mut code = 0;
    for p in formats::prepare(&repo, &formats, &mut |id| eprintln!("compiling plugin {id} ...")) {
        match p.result {
            Ok(l) => out.println(format!("plugin {} {} ready (sha256 {}...)", p.id, l.importer.version, short(&l.importer.component_sha256))),
            Err(u) => {
                out.println(format!("plugin {}: not available ({})", p.id, u.why(&p.id)));
                code = 1;
            }
        }
    }
    Ok(code)
}

/// The end of `clone` (PLUGINS.md 4.3): the new working tree's plugins
/// compiled ahead, with the trust notice; nothing on stdout, failures as
/// warnings.
pub fn prepare_after_clone(target: &std::path::Path, out: &mut Out) {
    let Ok(repo) = Repo::open_at(target) else { return };
    if !repo.worktree.as_ref().is_some_and(|w| w.join(attributes::PLUGIN_DIR).is_dir() || w.join(attributes::FILE).is_file()) {
        return;
    }
    let formats = Formats::for_repo(&repo);
    for p in formats::prepare(&repo, &formats, &mut |id| eprintln!("compiling plugin {id} ...")) {
        if let Err(u) = p.result {
            out.eprintln(format!("tmiki: warning: plugin {} is not available ({})", p.id, u.why(&p.id)));
        }
    }
}

/// `tmiki plugin reimport <id>`: every tracked file the plugin reads, imported
/// again with the plugin as it is now (PLUGINS.md 4.3; `git add
/// --renormalize`).
fn reimport(argv: &[String], out: &mut Out) -> CmdResult {
    let m = args::parse(Command::new("reimport").arg(positional("id", false)), argv, &["id"])?;
    let Some(id) = args::get_str(&m, "id") else { return Err(RepoError::forward("reimport without an id (argparse's error)")) };
    let repo = Repo::open()?;
    crate::writes::begin_locked(&repo)?;
    crate::gates::no_merge_state(&repo)?;
    let mut stager = Stager::new(&repo)?;
    let rels = tracked_for(&repo, &stager.ctx.formats, &id)?;
    if rels.is_empty() {
        out.println(format!("no tracked files are read by plugin {id}"));
        return Ok(0);
    }
    let root = repo.require_worktree()?.to_path_buf();
    let mut staged = Vec::new();
    // a file the plugin cannot read is listed at the end; the rest go on
    // (issue #49)
    let mut failures = crate::cmd_add::Failures::new();
    for rel in &rels {
        let result = crate::cmd_add::stage_or_note(&mut stager, rel, &root.join(rel).to_string_lossy(), true, None, &mut failures);
        match result {
            Ok(changed) => {
                if changed == Some(true) {
                    staged.push(rel.clone());
                }
            }
            Err(e) => {
                crate::cmd_add::emit_warnings(&mut stager, out);
                stager.finish()?;
                return Err(e);
            }
        }
    }
    crate::cmd_add::emit_warnings(&mut stager, out);
    stager.finish()?;
    for rel in &staged {
        out.println(format!("staged {rel}"));
    }
    out.println(format!(
        "reimported {} file(s) with plugin {id} ({} changed)",
        rels.len() - failures.len(),
        staged.len()
    ));
    Ok(crate::cmd_add::report_failures(out, &failures))
}

/// `tmiki plugin remove <id>` (issue #50): stop using a plugin. Its component
/// `.tmiplugins/<id>.wasm` is removed (as `tmiki rm` does), `format=plugin:<id>`
/// is taken out of `.tmiattributes`, and the tracked files it read are
/// recorded again from their working files as tmi reads them without it
/// (`add --renormalize` of those files: for most formats an opaque blob),
/// all staged for the next commit. The records of earlier commits stay as
/// they are (`ls-layers <file> HEAD~1` still shows their layers).
fn remove(argv: &[String], out: &mut Out) -> CmdResult {
    let m = args::parse(Command::new("remove").arg(positional("id", false)), argv, &["id"])?;
    let Some(id) = args::get_str(&m, "id") else { return Err(RepoError::forward("remove without an id (argparse's error)")) };
    let repo = Repo::open()?;
    crate::writes::begin_locked(&repo)?;
    crate::gates::no_merge_state(&repo)?;
    let root = repo.require_worktree()?.to_path_buf();
    let rel = format!("{}/{id}.wasm", attributes::PLUGIN_DIR);
    let component = root.join(attributes::PLUGIN_DIR).join(format!("{id}.wasm"));
    let attrs_path = root.join(attributes::FILE);
    let text = match std::fs::read(&attrs_path) {
        Ok(b) => Some(String::from_utf8_lossy(&b).into_owned()),
        Err(_) => None,
    };
    let (new_text, changes) = attributes::remove_plugin(text.as_deref().unwrap_or(""), &id);
    let head = repo.entries_for_commit(repo.current_commit()?.as_deref())?;
    let index = repo.index_entries()?;
    let component_tracked = match index.get(&rel) {
        Some(e) => !tmi_repo::stage::is_deleted(e),
        None => head.contains_key(&rel),
    };
    // the files to record again: what the plugin read before this change
    let files = tracked_for(&repo, &Formats::for_repo(&repo), &id)?;
    if !component.exists() && !component_tracked && changes.is_empty() && files.is_empty() {
        return Err(RepoError::runtime(format!(
            "plugin {id} is not used in this repository (no {rel}, and {} maps no files to it)",
            attributes::FILE
        )));
    }
    if component_tracked {
        repo.remove_path(&rel, false, true)?;
        out.println(format!("removed {rel}"));
    } else if component.exists() {
        tmi_repo::fsio::unlink(&component)?;
        out.println(format!("removed {rel}"));
    }
    if !changes.is_empty() {
        tmi_repo::fsio::write_bytes(&attrs_path, new_text.as_bytes())?;
        for c in &changes {
            match &c.new {
                None => out.println(format!("{}: removed '{}'", attributes::FILE, c.old)),
                Some(n) => out.println(format!("{}: '{}' -> '{n}'", attributes::FILE, c.old)),
            }
        }
    }
    // a new Stager: it reads the .tmiattributes just written
    let mut stager = Stager::new(&repo)?;
    let mut staged = Vec::new();
    let mut failures = crate::cmd_add::Failures::new();
    if !changes.is_empty() {
        let (r, changed) = stager.stage_file(&attrs_path.to_string_lossy(), true, None)?;
        if changed {
            staged.push(r);
        }
    }
    for f in &files {
        let result = crate::cmd_add::stage_or_note(&mut stager, f, &root.join(f).to_string_lossy(), true, None, &mut failures);
        match result {
            Ok(changed) => {
                if changed == Some(true) {
                    staged.push(f.clone());
                }
            }
            Err(e) => {
                crate::cmd_add::emit_warnings(&mut stager, out);
                stager.finish()?;
                return Err(e);
            }
        }
    }
    crate::cmd_add::emit_warnings(&mut stager, out);
    stager.finish()?;
    for r in &staged {
        out.println(format!("staged {r}"));
    }
    let n = files.len() - failures.len();
    out.println(if files.is_empty() {
        format!("plugin {id} is no longer used here")
    } else {
        format!(
            "plugin {id} is no longer used here; {n} tracked {} it read {} recorded again without it",
            if n == 1 { "file" } else { "files" },
            if n == 1 { "is" } else { "are" }
        )
    });
    Ok(crate::cmd_add::report_failures(out, &failures))
}

/// `tmiki plugin trust <id> [--revoke]`: the component now in
/// `.tmiplugins/<id>.wasm` may read and write (merge write-back, `restore
/// --in-place`) files of this repository without asking; `--revoke` takes
/// that back.
fn trust_cmd(argv: &[String], out: &mut Out) -> CmdResult {
    let cmd = Command::new("trust").arg(positional("id", false)).arg(flag("revoke", None, Some("revoke")));
    let m = args::parse(cmd, argv, &["id"])?;
    let Some(id) = args::get_str(&m, "id") else { return Err(RepoError::forward("trust without an id (argparse's error)")) };
    let repo = Repo::open()?;
    let root = repo.require_worktree()?.to_path_buf();
    let rel = format!("{}/{id}.wasm", attributes::PLUGIN_DIR);
    let bytes = std::fs::read(root.join(&rel)).map_err(|_| RepoError::runtime(format!("{rel} does not exist")))?;
    let plugin = load_component(&repo, &bytes, &rel)?;
    let c = trust::Component { id: &id, version: &plugin.info.version, sha256: &plugin.sha256 };
    if args::get_flag(&m, "revoke") {
        trust::revoke(&repo, &c)?;
        out.println(format!("plugin {id} {} (sha256 {}...) is no longer trusted here", plugin.info.version, short(&plugin.sha256)));
    } else {
        trust::grant(&repo, &c)?;
        out.println(format!(
            "plugin {id} {} (sha256 {}...) may now read and write files of this repository",
            plugin.info.version,
            short(&plugin.sha256)
        ));
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::attribute_line;

    /// `plugin add` writes `.tmiattributes` lines from names a plugin gave
    /// (check_info already limits them): the writer refuses anything that
    /// could make a second line or a second rule.
    #[test]
    fn an_attribute_line_is_one_line_with_one_pattern() {
        assert_eq!(attribute_line("*.ora", "ora").unwrap(), "*.ora format=plugin:ora");
        assert_eq!(attribute_line("art/**/*.kra", "kra").unwrap(), "art/**/*.kra format=plugin:kra");
        for bad in [
            "*.evil\n*.png format=plugin:evil\n*",
            "*.a\r*",
            "*.a *",
            "*.a\t*",
            "*.a=b",
            "*.a#b",
            "*.\u{1b}[2K",
            "*.a\u{2028}*",
            "",
            "/abs",
            "*.a\\b",
            "*.\"a",
            "*.a!",
            &format!("*.{}", "a".repeat(300)),
        ] {
            assert!(attribute_line(bad, "evil").is_err(), "{bad:?}");
        }
        let e = attribute_line("*.evil\n*", "evil").unwrap_err().to_string();
        assert!(!e.contains('\n') || e.lines().count() == 1, "{e:?}");
    }
}