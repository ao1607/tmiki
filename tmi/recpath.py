"""The rule for the paths a tree records (tmi-repo's ``recpath.rs``: same
rule, same words), and the one way a recorded path becomes a file system path.

A recorded path comes from data other people write (a clone, a fetch), so it
is never joined to the working tree before :func:`check_record_path` has
accepted it. The rule is the same on every OS: a repository made on Linux
must check out on Windows. What it refuses (each with its reason):

- an empty path, an absolute path (``/x``, ``//server/share``), a drive
  (``c:/x``, any ``:``, which on NTFS is also a stream), a backslash (a
  record's separator is ``/`` only);
- control characters and the characters Windows cannot hold (``<>:"|?*``);
- a part that is empty, ``.`` or ``..`` (it would leave its directory);
- a part that ends with ``.`` or a space (Windows drops them: ``.tmi.`` is
  ``.tmi``);
- a part that is a Windows device name (``CON``, ``NUL``, ``COM1``,
  ``LPT1``, ..., with or without an extension);
- a part that is ``.tmi`` or ``.git`` in any letter case, or their NTFS short
  names (``TMI~1``, ``GIT~1``), at any depth: ``.tmi/hooks/`` runs programs,
  and a ``.git`` directory is read by Git the same way.
"""

from __future__ import annotations

import os
import stat
import sys
from pathlib import Path

_RESERVED_DEVICES = frozenset({"CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$"})
_SUPERSCRIPTS = frozenset({"\u00b9", "\u00b2", "\u00b3"})
_DIGITS = frozenset("123456789")
_ASCII_UP = {c: c - 32 for c in range(ord("a"), ord("z") + 1)}
_ASCII_LOW = {c: c + 32 for c in range(ord("A"), ord("Z") + 1)}


def _ascii_upper(text: str) -> str:
    return text.translate(_ASCII_UP)


def _ascii_lower(text: str) -> str:
    return text.translate(_ASCII_LOW)


def _is_reserved_device(part: str) -> bool:
    # Windows looks at the name before the first '.', without trailing spaces
    stem = part.split(".", 1)[0].rstrip(" ")
    up = _ascii_upper(stem)
    if up in _RESERVED_DEVICES:
        return True
    for prefix in ("COM", "LPT"):
        if up.startswith(prefix) and len(up) == len(prefix) + 1:
            last = up[-1]
            if last in _DIGITS or last in _SUPERSCRIPTS:
                return True
    return False


def _is_protected_dir(part: str) -> bool:
    low = _ascii_lower(part)
    if low in (".tmi", ".git"):
        return True
    for short in ("tmi~", "git~"):
        if low.startswith(short):
            rest = low[len(short):]
            if rest and all("0" <= c <= "9" for c in rest):
                return True
    return False


def record_path_problem(path: str) -> str | None:
    """Why ``path`` cannot be a recorded path, or None."""
    if not path:
        return "it is empty"
    if path.startswith("/"):
        return "it is absolute"
    for ch in path:
        if ord(ch) < 32 or ord(ch) == 127:
            return "it contains a control character"
        if ch == "\\":
            return "it contains a backslash"
        if ch in '<>:"|?*':
            return f"it contains '{ch}'"
    parts = path.split("/")
    if any(p == "" for p in parts):
        return "it has an empty part (a doubled or trailing '/')"
    if any(p in (".", "..") for p in parts):
        return "a part of it is '.' or '..'"
    if any(p.endswith((".", " ")) for p in parts):
        return "a part of it ends with '.' or a space"
    if any(_is_reserved_device(p) for p in parts):
        return "a part of it is a reserved Windows device name"
    if any(_is_protected_dir(p) for p in parts):
        return "a part of it is '.tmi' or '.git'"
    return None


def display_path(path: str) -> str:
    """``path`` as a message shows it: controls and invisible characters
    escaped (a path from a remote must not move the terminal's cursor), long
    ones cut."""
    out: list[str] = []
    for i, ch in enumerate(path):
        if i == 120:
            out.append("...")
            break
        c = ord(ch)
        hide = (
            c < 0x20
            or 0x7F <= c <= 0x9F
            or 0x200B <= c <= 0x200F
            or 0x2028 <= c <= 0x202E
            or 0x2066 <= c <= 0x2069
            or c == 0xFEFF
        )
        if not hide:
            out.append(ch)
        elif c < 0x100:
            out.append(f"\\x{c:02x}")
        else:
            out.append(f"\\u{c:04x}")
    return "".join(out)


def invalid_path_message(path: str, problem: str) -> str:
    return f"invalid path in a tree: {display_path(path)} ({problem})"


def check_record_path(path: str) -> None:
    """Raise RuntimeError (with :func:`invalid_path_message`) when ``path``
    breaks the rule."""
    problem = record_path_problem(path)
    if problem is not None:
        raise RuntimeError(invalid_path_message(path, problem))


def check_new_record_path(path: str) -> None:
    """``check_record_path`` for a file about to be recorded (``add``): the
    words say what cannot be done, not where a tree came from."""
    problem = record_path_problem(path)
    if problem is not None:
        raise RuntimeError(f"cannot record {display_path(path)}: its path is not allowed in a tree ({problem})")


def join_rel(root: Path, rel: str) -> Path:
    """``root`` and the parts of the recorded path ``rel``; RuntimeError
    when the rule refuses it (nothing else may join a recorded path)."""
    check_record_path(rel)
    return root.joinpath(*rel.split("/"))


def _is_link(path: Path) -> bool:
    try:
        if path.is_symlink():
            return True
        if sys.platform == "win32":
            return getattr(os.lstat(path), "st_reparse_tag", 0) == stat.IO_REPARSE_TAG_MOUNT_POINT
    except OSError:
        pass
    return False


def worktree_path(worktree: Path, repo_dir: Path, rel: str) -> Path:
    """The file ``rel`` names under ``worktree``: the rule, the place (inside
    the working tree, outside ``repo_dir``), and no symbolic link or junction
    on the way (tmi does not track links; writing through one would leave the
    working tree). RuntimeError with a sentence otherwise."""
    check_record_path(rel)
    cur = worktree
    for part in rel.split("/"):
        cur = cur / part
        if _is_link(cur):
            shown = display_path(cur.relative_to(worktree).as_posix())
            raise RuntimeError(
                f"refusing to use {display_path(rel)}: it goes through a symbolic link or junction ({shown})"
            )
    if cur == repo_dir or repo_dir in cur.parents:
        raise RuntimeError(invalid_path_message(rel, "it is outside the working tree"))
    return cur
