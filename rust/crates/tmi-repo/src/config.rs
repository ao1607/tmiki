//! tmi/config.py: the per-user config file and dotted keys.

use crate::error::{RepoError, Result};
use std::path::PathBuf;
use tmi_store::json::{self, Value};

fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// `user_config_path()`.
pub fn user_config_path() -> PathBuf {
    if let Some(home) = env_nonempty("TMI_CONFIG_HOME") {
        return PathBuf::from(home).join("config");
    }
    if cfg!(windows) {
        if let Some(appdata) = env_nonempty("APPDATA") {
            return PathBuf::from(appdata).join("tmi").join("config");
        }
    }
    if let Some(xdg) = env_nonempty("XDG_CONFIG_HOME") {
        return PathBuf::from(xdg).join("tmi").join("config");
    }
    let home = if cfg!(windows) {
        std::env::var("USERPROFILE").ok()
    } else {
        std::env::var("HOME").ok()
    };
    PathBuf::from(home.unwrap_or_default()).join(".config").join("tmi").join("config")
}

/// `read_config_file(user_config_path())`. Damaged files are left to
/// Python (its message quotes the JSON error).
pub fn read_user_config() -> Result<Value> {
    let path = user_config_path();
    match crate::repo::read_text(&path)? {
        None => Ok(json::loads(r#"{"version":1}"#).unwrap()),
        Some(text) => {
            let value = json::loads(&text).map_err(|_| RepoError::forward("invalid user config"))?;
            if value.as_object().is_none() {
                return Err(RepoError::forward("user config is not an object"));
            }
            Ok(value)
        }
    }
}

/// `iter_dotted_keys(data)`: (section.name, value) for scalar values, sorted.
pub fn iter_dotted_keys(data: &Value) -> Result<Vec<(String, Value)>> {
    let mut out = Vec::new();
    let Some(map) = data.as_object() else {
        return Err(RepoError::forward("config is not an object"));
    };
    for (section, bucket) in map {
        if section == "version" {
            continue;
        }
        let Some(bucket) = bucket.as_object() else { continue };
        for (name, value) in bucket {
            if matches!(value, Value::Array(_) | Value::Object(_)) {
                continue;
            }
            out.push((format!("{section}.{name}"), value.clone()));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    if out.windows(2).any(|w| w[0].0 == w[1].0) {
        // Python would then compare the values (and may raise TypeError).
        return Err(RepoError::forward("duplicate dotted config keys"));
    }
    Ok(out)
}

/// `_split_key(key)`; Forward for keys Python rejects (its message uses repr).
pub fn split_key(key: &str) -> Result<(&str, &str)> {
    let Some((section, name)) = key.split_once('.') else {
        return Err(RepoError::forward("bad config key"));
    };
    if section.is_empty() || name.is_empty() || name.contains('.') {
        return Err(RepoError::forward("bad config key"));
    }
    Ok((section, name))
}

/// `get_value(data, key)`.
pub fn get_value<'a>(data: &'a Value, key: &str) -> Result<Option<&'a Value>> {
    let (section, name) = split_key(key)?;
    Ok(data.get(section).and_then(Value::as_object).and_then(|b| b.get(name)))
}
