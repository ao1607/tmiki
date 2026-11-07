//! `tmiki recover` / `tmiki reflog` listings (cli.py `cmd_recover`): HEAD's
//! reflog and `--files`. `--file N` and `--as` write: Python.

use crate::args::{self, flag, opt, positional};
use crate::cmd_log::fwd;
use crate::out::Out;
use crate::CmdResult;
use clap::Command;
use tmi_repo::reflog;
use tmi_repo::repo::{prefix_chars, read_text, Repo};
use tmi_store::json::{self, Value};

fn recover_cmd() -> Command {
    Command::new("recover")
        .arg(opt("as_branch", None, Some("as")))
        .arg(positional("rev", false))
        .arg(flag("files", None, Some("files")))
        .arg(opt("backup", None, Some("file")))
        .arg(opt("output", None, Some("output")))
}

/// `f"{x:.1f}"` of a size in MB.
fn mb(size: &Value) -> Result<String, tmi_repo::RepoError> {
    let bytes = match size {
        v if !v.truthy() => 0.0,
        Value::Int(json::Int::Small(n)) => *n as f64,
        Value::Float(f) => *f,
        _ => return Err(fwd("backup size is not a number")),
    };
    Ok(format!("{:.1}", bytes / 1e6))
}

pub fn recover(argv: &[String], out: &mut Out) -> CmdResult {
    let m = args::parse(recover_cmd(), argv, &["rev"])?;
    let repo = Repo::open()?;
    if let Some(spec) = args::get_str(&m, "backup") {
        return crate::cmd_refs_write::recover_file(&repo, &spec, args::get_str(&m, "output"), out);
    }
    if args::get_flag(&m, "files") {
        let backups: Vec<Value> = match read_text(&repo.repo_dir.join("backups.json"))? {
            None => Vec::new(),
            Some(text) => match json::loads(&text) {
                Ok(Value::Array(items)) => items.into_iter().filter(|i| i.as_object().is_some()).collect(),
                Ok(_) => Vec::new(),
                Err(_) => return Err(fwd("backups.json does not parse here")),
            },
        };
        if backups.is_empty() {
            out.println("no backed-up files");
            return Ok(0);
        }
        for (idx, backup) in backups.iter().enumerate() {
            let text = |key: &str| -> Result<String, tmi_repo::RepoError> {
                match backup.get(key) {
                    None => Ok("None".into()),
                    Some(v) => v.py_str().ok_or_else(|| fwd("container backup field")),
                }
            };
            let when_raw = match backup.get("timestamp") {
                None => String::new(),
                Some(v) => v.py_str().ok_or_else(|| fwd("container timestamp"))?,
            };
            let when = prefix_chars(&when_raw, 19).replace('T', " ");
            let size = mb(backup.get("size").unwrap_or(&Value::Null))?;
            out.println(format!("{idx:3}  {when}  {}  {size} MB  ({})", text("path")?, text("reason")?));
        }
        return Ok(0);
    }
    if let Some(as_branch) = args::get_str(&m, "as_branch").filter(|s| !s.is_empty()) {
        return crate::cmd_refs_write::recover_as(&repo, &as_branch, args::get_str(&m, "rev"), out);
    }
    let entries = reflog::read(&repo.repo_dir, "HEAD")?;
    if entries.is_empty() {
        out.println("no reflog yet");
        return Ok(0);
    }
    for (idx, entry) in entries.iter().rev().enumerate() {
        let ts = entry.timestamp.replace('T', " ");
        let ts = ts.trim_end_matches('Z');
        out.println(format!("HEAD@{{{idx}}}  {}  {ts}  {}", prefix_chars(&entry.new_oid, 12), entry.reason));
    }
    Ok(0)
}
