"""Filesystem hook dispatch for tmi.

Hooks live in ``.tmi/hooks/<name>`` and are run by tmi at well-known points in
the commit/switch/merge/push lifecycle. Mirrors the broad shape of Git hooks
but with a tighter set of supported events (see plan §5).

A hook is "installed" if the file exists and (on POSIX) is executable, or (on
Windows) is a script with a recognised suffix. On Windows, scripts are launched
without a shebang via the registered file association; ``.py`` is supported
explicitly by invoking the current Python interpreter.
"""

from __future__ import annotations

import os
import stat
import sys
from pathlib import Path
from typing import Iterable


SUPPORTED_HOOKS = (
    "pre-commit",
    "post-commit",
    "pre-push",
    "post-merge",
    "pre-switch",
    "post-switch",
)


_WIN_HOOK_SUFFIXES = (".py", ".bat", ".cmd", ".ps1", ".exe")


def hook_path(repo_dir: Path, name: str) -> Path:
    """Locate the hook file. Returns the bare path if nothing exists yet."""
    base = repo_dir / "hooks" / name
    if os.name == "nt":
        # Prefer a runnable suffixed variant: a bare POSIX-style hook (shebang,
        # no extension) cannot be executed by CreateProcess, and picking it
        # over e.g. pre-commit.py would abort every commit with an OSError.
        for suffix in _WIN_HOOK_SUFFIXES:
            candidate = base.with_name(name + suffix)
            if candidate.is_file():
                return candidate
    return base


def is_installed(repo_dir: Path, name: str) -> bool:
    path = hook_path(repo_dir, name)
    if not path.is_file():
        return False
    if os.name == "nt":
        return True
    return bool(path.stat().st_mode & stat.S_IXUSR)


def list_installed(repo_dir: Path) -> list[str]:
    return [name for name in SUPPORTED_HOOKS if is_installed(repo_dir, name)]


def run(
    repo_dir: Path,
    name: str,
    *,
    extra_env: dict[str, str] | None = None,
    args: Iterable[str] = (),
    fatal: bool = True,
) -> int:
    """Run hook `name` if installed. Return exit code (0 if not installed).

    If `fatal=True` and the hook exits non-zero, raise RuntimeError so the
    caller can abort the operation (used by pre-* hooks).
    """
    if name not in SUPPORTED_HOOKS:
        raise RuntimeError(f"unknown hook: {name}")
    if not is_installed(repo_dir, name):
        return 0
    path = hook_path(repo_dir, name)
    env = os.environ.copy()
    env["TMI_REPO"] = str(repo_dir.parent if repo_dir.name == ".tmi" else repo_dir)
    env["TMI_HOOK"] = name
    if extra_env:
        env.update(extra_env)
    cmd = _build_command(path, list(args))
    import subprocess  # only when a hook is installed: keeps status/log startup light

    from . import lockfile

    held = lockfile.suspend()  # the hook may run tmi itself (issue #35)
    try:
        result = subprocess.run(cmd, env=env, check=False)
    except OSError as exc:
        if fatal:
            raise RuntimeError(f"hook {name} could not be executed: {exc}") from exc
        return 1
    finally:
        lockfile.resume(held)
    if result.returncode != 0 and fatal:
        raise RuntimeError(f"hook {name} aborted with exit code {result.returncode}")
    return result.returncode


def _build_command(path: Path, args: list[str]) -> list[str]:
    suffix = path.suffix.lower()
    if suffix == ".py":
        return [sys.executable, str(path), *args]
    if suffix == ".ps1":
        return ["powershell.exe", "-ExecutionPolicy", "Bypass", "-File", str(path), *args]
    return [str(path), *args]
