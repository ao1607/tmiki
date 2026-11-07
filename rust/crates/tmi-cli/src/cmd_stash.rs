//! `tmiki stash list / apply / pop / drop / clear` (cli.py `cmd_stash`).
//! `stash push` / `save` import every tracked file: Python.

use crate::args::{self, positional};
use crate::cmd_log::fwd;
use crate::out::Out;
use crate::progress::{with_progress, Progress};
use crate::writes;
use crate::CmdResult;
use clap::Command;
use tmi_repo::repo::prefix_chars;
use tmi_repo::{Repo, RepoError};
use tmi_store::json::Value;

/// `str(stash.get(key, ''))`.
fn field(stash: &Value, key: &str) -> Result<String, RepoError> {
    match stash.get(key) {
        None => Ok(String::new()),
        Some(v) => v.py_str().ok_or_else(|| fwd("container stash field")),
    }
}

/// `stash.get('commit', '')[:12]` (slicing needs a str).
fn commit_of(stash: &Value) -> Result<String, RepoError> {
    match stash.get("commit") {
        None => Ok(String::new()),
        Some(Value::Str(c)) => Ok(prefix_chars(c, 12)),
        Some(_) => Err(fwd("stash commit is not a str")),
    }
}

pub fn stash(argv: &[String], out: &mut Out) -> CmdResult {
    let (sub, rest) = match argv.first() {
        // As in Git, `tmiki stash` alone is `tmiki stash push` (issue #39).
        None => ("push".to_string(), &argv[..0]),
        Some(s) if !s.starts_with('-') => (s.clone(), &argv[1..]),
        Some(_) => return Err(fwd("stash options before the subcommand")),
    };
    let spec = match sub.as_str() {
        "list" | "clear" => {
            args::parse(Command::new("stash-sub"), rest, &[])?;
            None
        }
        "apply" | "pop" | "drop" => {
            let m = args::parse(Command::new("stash-sub").arg(positional("spec", false)), rest, &["spec"])?;
            args::get_str(&m, "spec")
        }
        "push" | "save" => return push(&sub, rest, out),
        _ => return Err(fwd(&format!("stash {sub}"))),
    };
    let repo = Repo::open()?;
    if sub == "list" {
        for (idx, st) in repo.read_stashes()?.iter().enumerate() {
            let commit = match st.get("commit") {
                None => String::new(),
                Some(v) => prefix_chars(&v.py_str().ok_or_else(|| fwd("container stash commit"))?, 12),
            };
            out.println(format!("stash@{{{idx}}}: {} ({commit})", field(st, "message")?));
        }
        return Ok(0);
    }
    writes::begin_locked(&repo)?;
    let result = (|| -> CmdResult {
        match sub.as_str() {
            "apply" => {
                let st = with_progress("applying stash", || repo.stash_apply(spec.as_deref()))?;
                out.println(format!("applied {}", commit_of(&st)?));
            }
            "pop" => {
                let st = with_progress("applying stash", || repo.stash_apply(spec.as_deref()))?;
                let dropped = repo.stash_drop(spec.as_deref())?;
                let c = if dropped.get("commit").is_some() { commit_of(&dropped)? } else { commit_of(&st)? };
                out.println(format!("applied and dropped {c}"));
            }
            "drop" => {
                let dropped = repo.stash_drop(spec.as_deref())?;
                out.println(format!("dropped {}", commit_of(&dropped)?));
            }
            _ => {
                let count = repo.stash_clear()?;
                out.println(format!("dropped {count} stash entries"));
            }
        }
        Ok(0)
    })();
    // try: ... finally: _remind_reopen(repo.rewritten)
    if result.as_ref().is_err_and(RepoError::is_forward) && !tmi_repo::fsio::mutated() {
        return result;
    }
    writes::remind_reopen(out, &repo);
    result
}

/// `stash push [-m MSG] [words...]` / `stash save [words...]` (stage 4a:
/// every tracked file imported through tmi-import).
fn push(sub: &str, rest: &[String], out: &mut Out) -> CmdResult {
    let mut cmd = Command::new("stash-push").arg(positional("message_words", true));
    if sub == "push" {
        cmd = cmd.arg(crate::args::opt("message", Some('m'), Some("message")));
    }
    let m = args::parse(cmd, rest, &["message_words"])?;
    let words = args::get_many(&m, "message_words");
    let mut message = if sub == "push" { args::get_str(&m, "message") } else { None };
    if message.as_deref().is_none_or(str::is_empty) && !words.is_empty() {
        message = Some(words.join(" "));
    }
    let repo = Repo::open()?;
    writes::begin_locked(&repo)?;
    let progress = Progress::new();
    let report = |msg: &str| progress.report(msg);
    let mut ctx = tmi_diffmerge::merge::new_ctx(&repo)?;
    ctx.progress = Some(&report);
    let result = tmi_diffmerge::stash::stash_push(&mut ctx, message.as_deref());
    for line in ctx.warns.lines.drain(..) {
        out.eprintln(line);
    }
    progress.done();
    let result = result.and_then(|st| {
        out.println(format!("Saved working directory and index state stash@{{0}}: {}", field(&st, "message")?));
        Ok(0)
    });
    // try: ... finally: _remind_reopen(repo.rewritten)
    if result.as_ref().is_err_and(RepoError::is_forward) && !tmi_repo::fsio::mutated() {
        return result;
    }
    writes::remind_reopen(out, &repo);
    result
}
