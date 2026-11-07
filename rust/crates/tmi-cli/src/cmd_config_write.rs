//! `tmiki config [--global] <key> <value>` and `--unset` (cli.py
//! `cmd_config`; tmi/config.py `set_value`, `unset_value`,
//! `write_config_file`). `core.maxImagePixels` values are parsed by
//! Python's pixel_limit and stay there.

use crate::cmd_log::fwd;
use crate::writes;
use crate::CmdResult;
use tmi_repo::config::{read_user_config, split_key, user_config_path};
use tmi_repo::write::dumps_sorted;
use tmi_repo::{fsio, Repo, RepoError};
use tmi_store::json::{Int, Map, Value};

pub fn write(global: bool, unset: bool, key: &str, value: Option<String>) -> CmdResult {
    let (mut data, repo) = if global {
        let mut d = read_user_config()?;
        // read_config_file: data.setdefault("version", CONFIG_VERSION)
        if let Value::Object(map) = &mut d {
            if !map.contains_key("version") {
                map.insert("version".into(), Value::Int(Int::Small(1)));
            }
        }
        (d, None)
    } else {
        let repo = Repo::open()?;
        (repo.read_config()?, Some(repo))
    };
    let Value::Object(map) = &mut data else { return Err(fwd("config is not an object")) };
    let (section, name) = split_key(key)?;
    if unset {
        let removed = match map.get_mut(section) {
            Some(Value::Object(bucket)) if bucket.contains_key(name) => {
                bucket.shift_remove(name);
                if bucket.is_empty() {
                    map.shift_remove(section);
                }
                true
            }
            _ => false,
        };
        if !removed {
            return Err(RepoError::runtime(format!("key not set: {key}")));
        }
    } else {
        if key == "core.maxImagePixels" {
            return Err(fwd("core.maxImagePixels values are parsed by Python"));
        }
        let value = value.unwrap_or_default();
        if !matches!(map.get(section), Some(Value::Object(_))) {
            map.insert(section.to_string(), Value::Object(Map::new()));
        }
        if let Some(Value::Object(bucket)) = map.get_mut(section) {
            bucket.insert(name.to_string(), Value::Str(value));
        }
    }
    writes::begin()?;
    match repo {
        Some(repo) => repo.write_config(&data)?,
        None => {
            let path = user_config_path();
            fsio::mkdir_parent(&path)?;
            fsio::write_text(&path, &dumps_sorted(&data))?;
        }
    }
    Ok(0)
}
