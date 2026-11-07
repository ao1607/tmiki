"""A full disk, in tmi's words (issue #42).

``add`` writes objects and render-cache files while it imports a file. When the
disk fills up half way, what was written for that file must not stay (it fills
the disk for good, and nothing refers to it), and the user needs to be told
which file, how much it needs and how much is free, not ``[Errno 28]``.

The Rust ``tmi_store::store`` (``track_begin`` / ``track_end`` / ``no_space``)
and ``tmi_repo::diskspace`` do the same.
"""

from __future__ import annotations

import contextlib
import errno
import os
import shutil
import threading
from pathlib import Path
from typing import Iterator

# OSError.errno values of "there is no room": ENOSPC (Windows' ERROR_DISK_FULL
# and ERROR_HANDLE_DISK_FULL map to it) and a quota.
_FULL_ERRNOS = {errno.ENOSPC, getattr(errno, "EDQUOT", errno.ENOSPC)}

_lock = threading.Lock()
_tracking: list[list[Path]] = []
_writes = 0


class DiskFullError(RuntimeError):
    """The disk has no room (for the file named in the message): `add` stops
    at once instead of going on to the next file."""

    stops_add = True


def is_disk_full(exc: BaseException) -> bool:
    return isinstance(exc, OSError) and exc.errno in _FULL_ERRNOS


@contextlib.contextmanager
def track_new_files() -> Iterator[list[Path]]:
    """The files the object store and the render cache create meanwhile."""
    created: list[Path] = []
    with _lock:
        _tracking.append(created)
    try:
        yield created
    finally:
        with _lock:
            _tracking.remove(created)


def note_new_file(path: Path) -> None:
    with _lock:
        for created in _tracking:
            created.append(path)


def keep_new_files() -> None:
    """The files written so far are referred to now (the index has them): a
    later failure must not remove them."""
    with _lock:
        for created in _tracking:
            created.clear()


def before_new_file() -> None:
    """``TMI_TEST_ENOSPC_AFTER=N``: the (N+1)th new object / cache file fails
    as a full disk does. For the tests; nothing sets it otherwise."""
    limit = os.environ.get("TMI_TEST_ENOSPC_AFTER")
    if not limit:
        return
    global _writes
    with _lock:
        _writes += 1
        count = _writes
    try:
        allowed = int(limit)
    except ValueError:
        return
    if count > allowed:
        raise OSError(errno.ENOSPC, os.strerror(errno.ENOSPC))


def discard(paths: list[Path]) -> None:
    for path in paths:
        try:
            path.unlink()
        except OSError:
            pass


def free_bytes(where: Path) -> int:
    """``TMI_TEST_FREE_BYTES`` stands in for the real figure in the tests."""
    forced = os.environ.get("TMI_TEST_FREE_BYTES")
    if forced:
        try:
            return int(forced)
        except ValueError:
            pass
    probe = where
    while not probe.exists() and probe != probe.parent:
        probe = probe.parent
    return shutil.disk_usage(probe).free


def human_size(size: int) -> str:
    if size >= 1024 * 1024 * 1024:
        return f"{size / (1024 * 1024 * 1024):.1f} GB"
    if size >= 1024 * 1024:
        return f"{size / (1024 * 1024):.1f} MB"
    if size >= 1024:
        return f"{size // 1024} KB"
    return f"{size} bytes"


def not_enough_message(rel: str, needed: int, free: int) -> str:
    return (
        f"not enough disk space to record {rel}: it needs at least {human_size(needed)} and "
        f"{human_size(free)} is free. Free some space and run the command again"
    )


def disk_full_message(rel: str, needed: int, free: int) -> str:
    return (
        f"the disk is full while recording {rel} (at least {human_size(needed)} needed, "
        f"{human_size(free)} free); tmi removed what it had written for it. "
        "Free some space and run the command again"
    )
