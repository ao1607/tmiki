//! `tmiki log` (cli.py `cmd_log` and its helpers, shared with `show`).

use crate::args::{self, flag, opt, opt_const, positional};
use crate::out::Out;
use crate::CmdResult;
use clap::Command;
use std::collections::{HashMap, HashSet, VecDeque};
use tmi_repo::py::{first_line, fnmatch, splitlines};
use tmi_repo::repo::{parents_of, same_entry, short_oid, Repo};
use tmi_repo::{RepoError, Result};
use tmi_store::json::Value;

/// The `args` fields `_print_log_commit` reads.
pub struct LogFormat {
    pub format: Option<String>,
    pub pretty: Option<String>,
    pub oneline: bool,
    pub parents: bool,
    pub abbrev: i64,
    pub abbrev_commit: bool,
    pub date: String,
    pub stat: bool,
    pub name_only: bool,
    pub name_status: bool,
}

pub fn fwd(why: &str) -> RepoError {
    RepoError::forward(why)
}

/// `str(commit.get(key, default))` for a str value; Forward otherwise.
pub fn str_field<'a>(obj: &'a Value, key: &str) -> Result<&'a str> {
    match obj.get(key) {
        None => Ok(""),
        Some(Value::Str(s)) => Ok(s),
        Some(_) => Err(fwd("non-str commit field")),
    }
}

fn author_field(commit: &Value, key: &str) -> Result<String> {
    match commit.get("author") {
        None => Ok(String::new()),
        Some(Value::Object(a)) => match a.get(key) {
            None => Ok(String::new()),
            Some(v) => v.py_str().ok_or_else(|| fwd("container author field")),
        },
        Some(_) => Err(fwd("author not a dict")),
    }
}

fn timestamp_str(commit: &Value) -> Result<String> {
    match commit.get("timestamp") {
        None => Ok(String::new()),
        Some(Value::Str(s)) => Ok(s.clone()),
        Some(_) => Err(fwd("non-str timestamp")),
    }
}

/// `datetime.fromisoformat` for the forms tmiki writes
/// (`YYYY-MM-DDTHH:MM:SS[.ffffff][+HH:MM]`, "Z" turned into "+00:00");
/// the date when it parses, None when Python's parse would fail too, and
/// Forward for text Python might read in a way not reproduced here.
fn iso_date(raw: &str) -> Result<Option<String>> {
    if raw.is_empty() {
        return Ok(None);
    }
    let text = match raw.strip_suffix('Z') {
        Some(t) => format!("{t}+00:00"),
        None => raw.to_string(),
    };
    let b = text.as_bytes();
    let digits = |r: std::ops::Range<usize>| b.get(r.clone()).is_some_and(|s| s.iter().all(u8::is_ascii_digit));
    let ok_date = b.len() >= 10 && digits(0..4) && b[4] == b'-' && digits(5..7) && b[7] == b'-' && digits(8..10);
    if !ok_date {
        return Err(fwd("timestamp not in tmiki's form"));
    }
    let rest = &text[10..];
    let time_ok = {
        let r = rest.as_bytes();
        let mut ok = r.is_empty();
        if r.len() >= 9 && r[0] == b'T' && digits(11..13) && r[3] == b':' && digits(14..16) && r[6] == b':' && digits(17..19) {
            let mut i = 9;
            if r.get(i) == Some(&b'.') {
                let n = r[i + 1..].iter().take_while(|c| c.is_ascii_digit()).count();
                ok = n == 3 || n == 6;
                i += 1 + n;
            } else {
                ok = true;
            }
            let tz = &r[i..];
            ok = ok
                && (tz.is_empty()
                    || (tz.len() == 6
                        && (tz[0] == b'+' || tz[0] == b'-')
                        && tz[1..3].iter().all(u8::is_ascii_digit)
                        && tz[3] == b':'
                        && tz[4..6].iter().all(u8::is_ascii_digit)));
        }
        ok
    };
    if !time_ok {
        return Err(fwd("timestamp not in tmiki's form"));
    }
    let (y, m, d): (u32, u32, u32) = (text[0..4].parse().unwrap(), text[5..7].parse().unwrap(), text[8..10].parse().unwrap());
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let days = [31, if leap { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    if y == 0 || !(1..=12).contains(&m) || d == 0 || d > days[(m - 1) as usize] {
        return Err(fwd("out-of-range date"));
    }
    if rest.len() >= 9 {
        let (hh, mm, ss): (u32, u32, u32) = (rest[1..3].parse().unwrap(), rest[4..6].parse().unwrap(), rest[7..9].parse().unwrap());
        if hh > 23 || mm > 59 || ss > 59 {
            return Err(fwd("out-of-range time"));
        }
    }
    Ok(Some(text[..10].to_string()))
}

/// `_format_commit_date(commit, mode)` for "iso" and "short".
fn format_commit_date(commit: &Value, mode: &str) -> Result<String> {
    let raw = match commit.get("timestamp") {
        None => String::new(),
        Some(v) => v.py_str().ok_or_else(|| fwd("container timestamp"))?,
    };
    if !matches!(commit.get("timestamp"), Some(Value::Str(_))) {
        return Ok(raw);
    }
    match mode {
        "iso" => Ok(raw),
        "short" => Ok(iso_date(&raw)?.unwrap_or(raw)),
        _ => Ok(crate::logfilter::local_date(commit)?.unwrap_or(raw)),
    }
}

fn print_line(out: &mut Out, prefix: &str, line: &str) {
    let line = &tmi_repo::textsafe::escape(line, false);
    if !line.is_empty() {
        out.println(format!("{prefix}{line}"));
    } else if !prefix.is_empty() {
        out.println(prefix.trim_end_matches(|c: char| c.is_whitespace()).to_string());
    } else {
        out.println("");
    }
}

/// cli.py `_path_matches`.
pub fn path_matches(path: &str, filters: &[String]) -> Result<bool> {
    for raw in filters {
        let pattern = raw.replace('\\', "/");
        if pattern.contains(['*', '?', '[']) && fnmatch(path, &pattern).map_err(|_| fwd("bad glob"))? {
            return Ok(true);
        }
        let prefix = pattern.trim_end_matches('/');
        if path == prefix || path.starts_with(&format!("{prefix}/")) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// cli.py `_commit_changes`.
pub fn commit_changes(repo: &Repo, oid: &str, commit: &Value) -> Result<Vec<(&'static str, String)>> {
    let parents = parents_of(commit)?;
    let before = match parents.first() {
        Some(p) => repo.entries_for_commit(Some(p))?,
        None => Default::default(),
    };
    let after = repo.entries_for_commit(Some(oid))?;
    let mut paths: Vec<&String> = before.keys().chain(after.keys()).collect();
    paths.sort();
    paths.dedup();
    let mut changes = Vec::new();
    for path in paths {
        let (old, new) = (before.get(path), after.get(path));
        match (old, new) {
            (None, Some(_)) => changes.push(("A", path.clone())),
            (Some(_), None) => changes.push(("D", path.clone())),
            (Some(_), Some(_)) if !same_entry(old, new) => changes.push(("M", path.clone())),
            _ => {}
        }
    }
    Ok(changes)
}

fn print_change_summary(
    repo: &Repo,
    oid: &str,
    commit: &Value,
    f: &LogFormat,
    filters: &[String],
    pre: &mut Prefixes,
    out: &mut Out,
) -> Result<()> {
    if !(f.stat || f.name_only || f.name_status) {
        return Ok(());
    }
    let mut changes = Vec::new();
    for (status, path) in commit_changes(repo, oid, commit)? {
        if filters.is_empty() || path_matches(&path, filters)? {
            changes.push((status, path));
        }
    }
    if changes.is_empty() {
        return Ok(());
    }
    if f.name_only {
        for (_, path) in &changes {
            print_line(out, &pre.next(), path);
        }
        return Ok(());
    }
    if f.name_status {
        for (status, path) in &changes {
            print_line(out, &pre.next(), &format!("{status}\t{path}"));
        }
        return Ok(());
    }
    for (status, path) in &changes {
        let label = match *status {
            "A" => "new file",
            "M" => "modified",
            _ => "deleted",
        };
        print_line(out, &pre.next(), &format!(" {path} | {label}"));
    }
    let noun = if changes.len() == 1 { "file" } else { "files" };
    print_line(out, &pre.next(), &format!(" {} {noun} changed", changes.len()));
    Ok(())
}

fn render_format(oid: &str, commit: &Value, template: &str, decorations: &[String], f: &LogFormat) -> Result<String> {
    let parents = parents_of(commit)?;
    let message = str_field(commit, "message")?;
    let sentinel = "\0TMI_PERCENT\0";
    let mut rendered = template.replace("%%", sentinel);
    let deco_d = if decorations.is_empty() { String::new() } else { format!(" ({})", decorations.join(", ")) };
    // sorted(replacements, key=len, reverse=True): the three-letter tokens
    // first, each length in insertion order.
    let replacements: Vec<(&str, String)> = vec![
        ("%an", author_field(commit, "name")?),
        ("%ae", author_field(commit, "email")?),
        ("%ad", format_commit_date(commit, &f.date)?),
        ("%H", oid.to_string()),
        ("%h", short_oid(oid, f.abbrev)),
        ("%P", parents.join(" ")),
        ("%p", parents.iter().map(|p| short_oid(p, f.abbrev)).collect::<Vec<_>>().join(" ")),
        ("%s", first_line(message).to_string()),
        ("%B", message.to_string()),
        ("%D", decorations.join(", ")),
        ("%d", deco_d),
        ("%n", "\n".to_string()),
    ];
    for (token, value) in replacements {
        rendered = rendered.replace(token, &value);
    }
    Ok(rendered.replace(sentinel, "%"))
}

/// cli.py `_print_log_commit`.
#[allow(clippy::too_many_arguments)]
pub fn print_log_commit(
    repo: &Repo,
    oid: &str,
    commit: &Value,
    f: &LogFormat,
    decorations: &[String],
    filters: &[String],
    pre: &mut Prefixes,
    out: &mut Out,
) -> Result<()> {
    // pretty = args.format_ or args.pretty, then the elif chain
    let is_named = |p: &str| matches!(p, "short" | "medium" | "full" | "oneline");
    let mut pretty = f.format.clone().filter(|s| !s.is_empty()).or_else(|| f.pretty.clone());
    match pretty.as_deref() {
        Some(p) if p.starts_with("format:") => pretty = Some(p["format:".len()..].to_string()),
        Some(p) if !p.is_empty() && !is_named(p) => {}
        _ => {
            if f.oneline {
                pretty = Some("oneline".into());
            }
        }
    }
    if let Some(p) = pretty.as_deref().filter(|p| !p.is_empty() && !is_named(p)) {
        let rendered = render_format(oid, commit, p, decorations, f)?;
        let lines = splitlines(&rendered);
        let lines = if lines.is_empty() { vec![""] } else { lines };
        for line in &lines {
            print_line(out, &pre.next(), line);
        }
        return print_change_summary(repo, oid, commit, f, filters, pre, out);
    }
    let deco = if decorations.is_empty() { String::new() } else { format!(" ({})", decorations.join(", ")) };
    let parents = parents_of(commit)?;
    if pretty.as_deref() == Some("oneline") || f.oneline {
        let mut ids = vec![short_oid(oid, f.abbrev)];
        if f.parents {
            ids.extend(parents.iter().map(|p| short_oid(p, f.abbrev)));
        }
        let subject = first_line(str_field(commit, "message")?);
        print_line(out, &pre.next(), &format!("{}{deco} {subject}", ids.join(" ")));
        return print_change_summary(repo, oid, commit, f, filters, pre, out);
    }
    let commit_id = if f.abbrev_commit { short_oid(oid, f.abbrev) } else { oid.to_string() };
    let mut parent_text = String::new();
    if f.parents {
        let shown: Vec<String> =
            parents.iter().map(|p| if f.abbrev_commit { short_oid(p, f.abbrev) } else { p.clone() }).collect();
        if !shown.is_empty() {
            parent_text = format!(" {}", shown.join(" "));
        }
    }
    print_line(out, &pre.next(), &format!("commit {commit_id}{parent_text}{deco}"));
    if parents.len() > 1 {
        let ids: Vec<String> = parents.iter().map(|p| short_oid(p, f.abbrev)).collect();
        print_line(out, &pre.next(), &format!("Merge: {}", ids.join(" ")));
    }
    let author = format!("Author: {} <{}>", author_field(commit, "name")?, author_field(commit, "email")?);
    print_line(out, &pre.next(), &author);
    print_line(out, &pre.next(), &format!("Date:   {}", format_commit_date(commit, &f.date)?));
    if commit.get("thumbnail_ref").is_some_and(Value::truthy) {
        print_line(out, &pre.next(), &format!("Thumbnail: tmiki show --thumbnail {}", short_oid(oid, f.abbrev)));
    }
    print_line(out, &pre.next(), "");
    for line in splitlines(str_field(commit, "message")?) {
        print_line(out, &pre.next(), &format!("    {line}"));
    }
    print_line(out, &pre.next(), "");
    print_change_summary(repo, oid, commit, f, filters, pre, out)
}

fn short_ref_name(r: &str) -> String {
    if let Some(n) = r.strip_prefix("refs/heads/") {
        return n.to_string();
    }
    if let Some(n) = r.strip_prefix("refs/remotes/") {
        return n.to_string();
    }
    if let Some(n) = r.strip_prefix("refs/tags/") {
        return format!("tag: {n}");
    }
    r.to_string()
}

/// cli.py `_commit_decorations`.
pub fn commit_decorations(repo: &Repo, full: bool) -> Result<HashMap<String, Vec<String>>> {
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    let current_branch = repo.current_branch()?;
    let current_commit = repo.current_commit()?;
    for (r, value) in repo.all_refs_sorted()? {
        let Some(commit) = repo.object_as_commit(Some(&value))? else { continue };
        let mut label = if full { r.clone() } else { short_ref_name(&r) };
        if let (Some(name), Some(cb)) = (r.strip_prefix("refs/heads/"), current_branch.as_deref()) {
            if name == cb {
                label = format!("HEAD -> {}", if full { r.as_str() } else { cb });
            }
        }
        out.entry(commit).or_default().push(label);
    }
    if let (Some(c), None) = (current_commit, current_branch) {
        out.entry(c).or_default().insert(0, "HEAD".into());
    }
    Ok(out)
}

/// cli.py `_GraphPrefixes`: the graph column in front of each line one
/// commit prints (the commit line's, then the column moves after it, then
/// padding). What is left of `lines` goes before the next commit.
#[derive(Default)]
pub struct Prefixes {
    commit_line: Option<String>,
    lines: VecDeque<String>,
    padding: String,
}

impl Prefixes {
    /// No graph: every prefix is empty.
    pub fn none() -> Prefixes {
        Prefixes::default()
    }

    fn next(&mut self) -> String {
        if let Some(line) = self.commit_line.take() {
            return line;
        }
        self.lines.pop_front().unwrap_or_else(|| self.padding.clone())
    }
}

/// cli.py `_LogGraph` (Git's graph.c reduced, issue #31).
struct LogGraph {
    columns: Vec<String>,
    first_parent: bool,
}

/// `(" ".join(cells) + " ").ljust(width)`.
fn graph_cells(cells: &[&str], width: usize) -> String {
    let text = format!("{} ", cells.join(" "));
    let pad = width.saturating_sub(text.chars().count());
    format!("{text}{}", " ".repeat(pad))
}

impl LogGraph {
    fn prefixes(&mut self, oid: &str, commit: &Value) -> Result<Prefixes> {
        let mut old = self.columns.clone();
        let col = match old.iter().position(|a| a == oid) {
            Some(c) => c,
            None => {
                old.push(oid.to_string());
                old.len() - 1
            }
        };
        let mut parents: Vec<String> = parents_of(commit)?.into_iter().filter(|p| !p.is_empty()).collect();
        if self.first_parent {
            parents.truncate(1);
        }
        let extra = parents.len().saturating_sub(1);
        let mut new: Vec<String> = Vec::new();
        fn target(item: &String, new: &mut Vec<String>) -> usize {
            match new.iter().position(|n| n == item) {
                Some(i) => i,
                None => {
                    new.push(item.clone());
                    new.len() - 1
                }
            }
        }
        // (position after the merge's fan-out line, column it ends in)
        let mut edges: Vec<(usize, usize)> = Vec::new();
        for (idx, item) in old.iter().enumerate() {
            if idx == col {
                for (j, parent) in parents.iter().enumerate() {
                    let t = target(parent, &mut new);
                    edges.push((col + j, t));
                }
            } else {
                let t = target(item, &mut new);
                edges.push((if idx > col { idx + extra } else { idx }, t));
            }
        }
        let max_pos = edges.iter().map(|(p, _)| p + 1).max().unwrap_or(0);
        let width = 2 * old.len().max(new.len()).max(max_pos);
        let cells: Vec<&str> = (0..old.len()).map(|i| if i == col { "*" } else { "|" }).collect();
        let commit_line = graph_cells(&cells, width);
        let mut lines = VecDeque::new();
        if extra > 0 {
            let mut chars = vec![' '; width];
            for idx in 0..old.len() {
                if idx <= col {
                    chars[2 * idx] = '|';
                    if idx == col {
                        for j in 1..parents.len() {
                            chars[2 * (col + j) - 1] = '\\';
                        }
                    }
                } else {
                    chars[2 * (idx + extra) - 1] = '\\';
                }
            }
            lines.push_back(chars.into_iter().collect::<String>());
        }
        while edges.iter().any(|(p, t)| p > t) {
            let mut chars = vec![' '; width];
            for (pos, end) in edges.iter_mut() {
                if *pos > *end {
                    *pos -= 1;
                    chars[2 * *pos + 1] = '/';
                } else {
                    chars[2 * *pos] = '|';
                }
            }
            lines.push_back(chars.into_iter().collect::<String>());
        }
        let padding = if new.is_empty() { " ".repeat(width) } else { graph_cells(&vec!["|"; new.len()], width) };
        self.columns = new;
        Ok(Prefixes { commit_line: Some(commit_line), lines, padding })
    }
}

fn collect_commits(repo: &Repo, starts: &[String], first_parent: bool) -> Result<Vec<(String, Value)>> {
    let mut order: Vec<String> = Vec::new();
    let mut collected: HashMap<String, Value> = HashMap::new();
    let mut stack: Vec<String> = starts.iter().filter(|s| !s.is_empty()).cloned().collect();
    while let Some(cur) = stack.pop() {
        if collected.contains_key(&cur) {
            continue;
        }
        let commit = repo.read_commit(&cur)?;
        let mut parents = parents_of(&commit)?;
        if first_parent {
            parents.truncate(1);
        }
        stack.extend(parents);
        order.push(cur.clone());
        collected.insert(cur, commit);
    }
    let mut items: Vec<(String, Value, String)> = Vec::new();
    for oid in order {
        let commit = collected.remove(&oid).unwrap();
        let ts = timestamp_str(&commit)?;
        items.push((oid, commit, ts));
    }
    // sorted(..., key=timestamp, reverse=True): stable, ties keep order
    items.sort_by(|a, b| b.2.cmp(&a.2));
    Ok(items.into_iter().map(|(o, c, _)| (o, c)).collect())
}

fn log_cmd() -> Command {
    Command::new("log")
        .arg(flag("oneline", None, Some("oneline")))
        .arg(flag("graph", None, Some("graph")))
        .arg(opt_const("decorate", None, Some("decorate"), "short"))
        .arg(flag("no_decorate", None, Some("no-decorate")))
        .arg(flag("all", None, Some("all")))
        .arg(flag("first_parent", None, Some("first-parent")))
        .arg(opt("max_count", Some('n'), Some("max-count")))
        .arg(opt("skip", None, Some("skip")))
        .arg(flag("reverse", None, Some("reverse")))
        .arg(opt("author", None, Some("author")))
        .arg(opt("grep", None, Some("grep")))
        .arg(opt("since", None, Some("since")).alias("after"))
        .arg(opt("until", None, Some("until")).alias("before"))
        .arg(flag("merges", None, Some("merges")))
        .arg(flag("no_merges", None, Some("no-merges")))
        .arg(flag("parents", None, Some("parents")))
        .arg(opt("abbrev", None, Some("abbrev")))
        .arg(flag("abbrev_commit", None, Some("abbrev-commit")))
        .arg(flag("no_abbrev_commit", None, Some("no-abbrev-commit")))
        .arg(opt("format", None, Some("format")))
        .arg(opt("pretty", None, Some("pretty")))
        .arg(opt("date", None, Some("date")))
        .arg(flag("stat", None, Some("stat")))
        .arg(flag("name_only", None, Some("name-only")))
        .arg(flag("name_status", None, Some("name-status")))
        .arg(positional("revs_or_paths", true))
}

fn all_start_oids(repo: &Repo) -> Result<Vec<String>> {
    // Python walks the refs in rglob order; the order only matters for
    // commits with equal timestamps, which `log` checks for.
    let mut starts = Vec::new();
    if let Some(c) = repo.current_commit()? {
        starts.push(c);
    }
    for (_, value) in repo.all_refs_sorted()? {
        if let Some(c) = repo.object_as_commit(Some(&value))? {
            if !starts.contains(&c) {
                starts.push(c);
            }
        }
    }
    Ok(starts)
}

pub fn log(argv: &[String], out: &mut Out) -> CmdResult {
    let m = args::parse(log_cmd(), argv, &["revs_or_paths"])?;
    let decorate_value = args::get_str(&m, "decorate");
    args::check_choice(&decorate_value, &["short", "full", "no"])?;
    let date = args::get_str(&m, "date").unwrap_or_else(|| "iso".into());
    args::check_choice(&Some(date.clone()), &["iso", "local", "short"])?;
    let since_arg = args::get_str(&m, "since").filter(|s| !s.is_empty());
    let until_arg = args::get_str(&m, "until").filter(|s| !s.is_empty());
    let grep_arg = args::get_str(&m, "grep").filter(|s| !s.is_empty());
    let author = args::get_str(&m, "author");
    // dest="decorate": --decorate and --no-decorate, the last wins
    let decorate = match (args::last_index(&m, "decorate"), args::last_index(&m, "no_decorate")) {
        (Some(a), Some(b)) if b > a => Some("no".to_string()),
        (_, Some(_)) if decorate_value.is_none() => Some("no".to_string()),
        _ => decorate_value,
    };
    let abbrev_commit = match (args::last_index(&m, "abbrev_commit"), args::last_index(&m, "no_abbrev_commit")) {
        (Some(a), Some(b)) => a > b,
        (Some(_), None) => true,
        _ => false,
    };
    let f = LogFormat {
        format: args::get_str(&m, "format"),
        pretty: args::get_str(&m, "pretty"),
        oneline: args::get_flag(&m, "oneline"),
        parents: args::get_flag(&m, "parents"),
        abbrev: args::get_int(&m, "abbrev")?.unwrap_or(12),
        abbrev_commit,
        date,
        stat: args::get_flag(&m, "stat"),
        name_only: args::get_flag(&m, "name_only"),
        name_status: args::get_flag(&m, "name_status"),
    };
    let max_count = args::get_int(&m, "max_count")?;
    let skip = args::get_int(&m, "skip")?.unwrap_or(0);
    let (merges, no_merges) = (args::get_flag(&m, "merges"), args::get_flag(&m, "no_merges"));
    let first_parent = args::get_flag(&m, "first_parent");
    let all = args::get_flag(&m, "all");

    let repo = Repo::open()?;
    if merges && no_merges {
        return Err(RepoError::runtime("--merges and --no-merges cannot be combined"));
    }
    let mut warnings = Vec::new();
    let values = args::get_many(&m, "revs_or_paths");
    let (starts, filters): (Vec<String>, Vec<String>) = if all {
        (all_start_oids(&repo)?, values.clone())
    } else if values.is_empty() {
        (repo.current_commit()?.into_iter().collect(), Vec::new())
    } else {
        let resolved = repo.resolve_rev(Some(&values[0]), &mut warnings);
        for w in warnings.drain(..) {
            out.eprintln(w);
        }
        match resolved {
            Ok(c) => (vec![c.unwrap_or_default()], values[1..].to_vec()),
            Err(RepoError::Runtime(msg)) => {
                let first = &values[0];
                let worktree = repo.require_worktree()?;
                let mut plausible = first.contains(['*', '?', '['])
                    || std::env::current_dir().map(|d| d.join(first).exists()).unwrap_or(false)
                    || worktree.join(first).exists();
                if !plausible {
                    let tracked = repo.entries_for_commit(repo.current_commit()?.as_deref())?;
                    for path in tracked.keys() {
                        if path_matches(path, std::slice::from_ref(first))? {
                            plausible = true;
                            break;
                        }
                    }
                }
                if !plausible {
                    return Err(RepoError::Runtime(msg));
                }
                (repo.current_commit()?.into_iter().collect(), values.clone())
            }
            Err(e) => return Err(e),
        }
    };
    let commits = collect_commits(&repo, &starts, first_parent)?;
    if all {
        let mut seen = HashSet::new();
        for (_, c) in &commits {
            if !seen.insert(timestamp_str(c)?) {
                return Err(fwd("log --all with equal timestamps (rglob order)"));
            }
        }
    }
    let since = since_arg.as_deref().map(crate::logfilter::parse_log_date).transpose()?;
    let until = until_arg.as_deref().map(crate::logfilter::parse_log_date).transpose()?;
    let author_filter = author.filter(|a| !a.is_empty()).map(|a| crate::logfilter::casefold(&a)).transpose()?;
    let grep = grep_arg.as_deref().map(crate::logfilter::py_regex).transpose()?;
    let mut kept = Vec::new();
    for (oid, commit) in commits {
        let n_parents = parents_of(&commit)?.len();
        if merges && n_parents < 2 {
            continue;
        }
        if no_merges && n_parents >= 2 {
            continue;
        }
        if since.is_some() || until.is_some() {
            if let Some(t) = crate::logfilter::commit_instant(&commit)? {
                if since.is_some_and(|s| t < s) || until.is_some_and(|u| t > u) {
                    continue;
                }
            }
        }
        if let Some(af) = &author_filter {
            let text = format!("{} {}", author_field(&commit, "name")?, author_field(&commit, "email")?);
            if !crate::logfilter::casefold(&text)?.contains(af.as_str()) {
                continue;
            }
        }
        if let Some(re) = &grep {
            let message = match commit.get("message") {
                None => String::new(),
                Some(Value::Str(s)) => s.clone(),
                Some(_) => return Err(fwd("non-str message for --grep")),
            };
            if !re.is_match(&message) {
                continue;
            }
        }
        if !filters.is_empty() {
            let mut touches = false;
            for (_, path) in commit_changes(&repo, &oid, &commit)? {
                if path_matches(&path, &filters)? {
                    touches = true;
                    break;
                }
            }
            if !touches {
                continue;
            }
        }
        kept.push((oid, commit));
    }
    let mut commits = kept;
    if skip > 0 {
        commits = commits.into_iter().skip(skip as usize).collect();
    }
    if let Some(n) = max_count {
        commits.truncate(n.max(0) as usize);
    }
    if args::get_flag(&m, "reverse") {
        commits.reverse();
    }
    if commits.is_empty() {
        out.println("no commits");
        return Ok(0);
    }
    let decorations = commit_decorations(&repo, decorate.as_deref() == Some("full"))?;
    let show_decorations = !matches!(decorate.as_deref(), None | Some("no"));
    let mut graph = args::get_flag(&m, "graph").then(|| LogGraph { columns: Vec::new(), first_parent });
    // the previous commit's column moves its lines did not use
    let mut pending: VecDeque<String> = VecDeque::new();
    for (oid, commit) in &commits {
        let mut pre = Prefixes::none();
        if let Some(g) = graph.as_mut() {
            for line in pending.drain(..) {
                print_line(out, &line, "");
            }
            pre = g.prefixes(oid, commit)?;
        }
        let deco = if show_decorations { decorations.get(oid).cloned().unwrap_or_default() } else { Vec::new() };
        print_log_commit(&repo, oid, commit, &f, &deco, &filters, &mut pre, out)?;
        pending = std::mem::take(&mut pre.lines);
    }
    for line in pending.drain(..) {
        print_line(out, &line, "");
    }
    Ok(0)
}

