//! `tmiki config --list` and `tmiki config [--global] <key>` (cli.py
//! `cmd_config`); setting and unsetting are Python's.

use crate::args::{self, flag, positional};
use crate::cmd_log::fwd;
use crate::out::Out;
use crate::CmdResult;
use clap::Command;
use std::collections::BTreeMap;
use tmi_repo::config::{get_value, iter_dotted_keys, read_user_config};
use tmi_repo::repo::Repo;
use tmi_repo::{RepoError, Result};
use tmi_store::json::Value;

fn config_cmd() -> Command {
    Command::new("config")
        .arg(flag("global", None, Some("global")))
        .arg(flag("list", Some('l'), Some("list")))
        .arg(flag("unset", None, Some("unset")))
        .arg(positional("key", false))
        .arg(positional("value", false))
}

fn shown(value: &Value) -> Result<String> {
    value.py_str().ok_or_else(|| fwd("container config value"))
}

pub fn config(argv: &[String], out: &mut Out) -> CmdResult {
    let m = args::parse(config_cmd(), argv, &["key", "value"])?;
    if args::get_flag(&m, "list") {
        let repo_cfg = match Repo::open() {
            Ok(repo) => Some(repo.read_config()?),
            Err(RepoError::Runtime(_)) => None,
            Err(e) => return Err(e),
        };
        let user_cfg = read_user_config()?;
        let mut merged: BTreeMap<String, (&str, Value)> = BTreeMap::new();
        for (key, value) in iter_dotted_keys(&user_cfg)? {
            merged.insert(key, ("global", value));
        }
        if let Some(cfg) = &repo_cfg {
            for (key, value) in iter_dotted_keys(cfg)? {
                merged.insert(key, ("local", value));
            }
        }
        for (key, (scope, value)) in merged {
            out.println(format!("{scope}\t{key}={}", shown(&value)?));
        }
        return Ok(0);
    }
    let Some(key) = args::get_str(&m, "key").filter(|k| !k.is_empty()) else {
        return Err(RepoError::runtime("config requires a key (or --list)"));
    };
    if args::get_flag(&m, "unset") || args::get_str(&m, "value").is_some() {
        return crate::cmd_config_write::write(
            args::get_flag(&m, "global"),
            args::get_flag(&m, "unset"),
            &key,
            args::get_str(&m, "value"),
        );
    }
    let global = args::get_flag(&m, "global");
    let data = if global {
        read_user_config()?
    } else {
        let repo = Repo::open()?;
        if key == "core.maxImagePixels" {
            return Err(fwd("core.maxImagePixels: the effective value"));
        }
        repo.read_config()?
    };
    match get_value(&data, &key)? {
        None | Some(Value::Null) => Ok(1),
        Some(v) => {
            out.println(shown(v)?);
            Ok(0)
        }
    }
}
