from __future__ import annotations

import json
import os
import sys
from pathlib import Path
from typing import Any


CONFIG_VERSION = 1


def user_config_path() -> Path:
    """Resolve the per-user tmiki config path.

    Override:  $TMI_CONFIG_HOME/config  (intended for CI / tests / sandboxing)
    Windows:   %APPDATA%\\tmi\\config  (falls back to ~/.config/tmi/config)
    Other OS:  $XDG_CONFIG_HOME/tmi/config  (falls back to ~/.config/tmi/config)
    """
    override = os.environ.get("TMI_CONFIG_HOME")
    if override:
        return Path(override) / "config"
    if sys.platform == "win32":
        appdata = os.environ.get("APPDATA")
        if appdata:
            return Path(appdata) / "tmi" / "config"
    xdg = os.environ.get("XDG_CONFIG_HOME")
    if xdg:
        return Path(xdg) / "tmi" / "config"
    return Path.home() / ".config" / "tmi" / "config"


def _empty_config() -> dict[str, Any]:
    return {"version": CONFIG_VERSION}


def read_config_file(path: Path) -> dict[str, Any]:
    if not path.exists():
        return _empty_config()
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except json.JSONDecodeError as exc:
        raise RuntimeError(f"invalid config file {path}: {exc}") from exc
    if not isinstance(data, dict):
        raise RuntimeError(f"invalid config file {path}: top-level must be an object")
    data.setdefault("version", CONFIG_VERSION)
    return data


def write_config_file(path: Path, data: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        json.dumps(data, ensure_ascii=False, sort_keys=True, indent=2) + "\n",
        encoding="utf-8",
    )


def read_user_config() -> dict[str, Any]:
    return read_config_file(user_config_path())


def write_user_config(data: dict[str, Any]) -> None:
    write_config_file(user_config_path(), data)


def _split_key(key: str) -> tuple[str, str]:
    if "." not in key:
        raise RuntimeError(f"config key must be in 'section.name' form: {key!r}")
    section, _, name = key.partition(".")
    if not section or not name or "." in name:
        raise RuntimeError(f"config key must be 'section.name': {key!r}")
    return section, name


def get_value(data: dict[str, Any], key: str) -> Any:
    section, name = _split_key(key)
    bucket = data.get(section)
    if not isinstance(bucket, dict):
        return None
    return bucket.get(name)


def set_value(data: dict[str, Any], key: str, value: str) -> None:
    section, name = _split_key(key)
    bucket = data.get(section)
    if not isinstance(bucket, dict):
        bucket = {}
        data[section] = bucket
    bucket[name] = value


def unset_value(data: dict[str, Any], key: str) -> bool:
    section, name = _split_key(key)
    bucket = data.get(section)
    if not isinstance(bucket, dict) or name not in bucket:
        return False
    del bucket[name]
    if not bucket:
        del data[section]
    return True


def iter_dotted_keys(data: dict[str, Any]) -> list[tuple[str, Any]]:
    out: list[tuple[str, Any]] = []
    for section, bucket in data.items():
        if section == "version" or not isinstance(bucket, dict):
            continue
        for name, value in bucket.items():
            if isinstance(value, (dict, list)):
                continue
            out.append((f"{section}.{name}", value))
    return sorted(out)


def merged_value(repo_config: dict[str, Any], user_config: dict[str, Any], key: str) -> Any:
    """Lookup `key` in repo config first, then user config."""
    value = get_value(repo_config, key)
    if value is not None:
        return value
    return get_value(user_config, key)


def resolve_identity(repo_config: dict[str, Any]) -> dict[str, str]:
    """Resolve author identity with env > repo > user precedence.

    Raises RuntimeError if neither name nor email is available.
    """
    env_name = os.environ.get("TMI_AUTHOR_NAME")
    env_email = os.environ.get("TMI_AUTHOR_EMAIL")
    user_cfg = read_user_config()
    name = env_name or merged_value(repo_config, user_cfg, "user.name")
    email = env_email or merged_value(repo_config, user_cfg, "user.email")
    if not name or not email:
        raise RuntimeError(
            "author identity is not configured.\n"
            "  Set it with:\n"
            '    tmiki config --global user.name "Your Name"\n'
            '    tmiki config --global user.email "you@example.com"\n'
            "  Or per-repo by omitting --global, or via env vars TMI_AUTHOR_NAME / TMI_AUTHOR_EMAIL."
        )
    return {"name": str(name), "email": str(email)}
