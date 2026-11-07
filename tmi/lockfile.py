"""Lock files, as Git has them (issues #35, #36).

Every write to the index or to a ref goes through a lock file created with
O_CREAT | O_EXCL, so two tmi processes cannot interleave their reads and
writes of the same file:

* ``<repo>/index.lock`` is taken by a command before it reads the index it is
  going to change and kept until the command ends (``Repo.lock_index``).
* ``<ref path>.lock`` (also ``HEAD.lock``) is taken for one ref update; the new
  value is written into it and it is renamed over the ref, after the ref has
  been re-read under the lock and found equal to the value the caller expects
  (compare-and-swap; ``Repo.write_ref(..., expected=...)``).

A lock that exists makes the second tmi stop at once with ``lock_message``'s
text, a non-zero exit and nothing written; there is no waiting and no retry.
The Rust executable (tmi-repo ``lockfile.rs``) does exactly the same, with the
same words.

Windows. Refs, HEAD and the index are read without a lock (as in Git), so a
read can meet the rename of the process that holds the lock:

* a reader that opens the file without FILE_SHARE_DELETE (Python's open()
  does) while the rename is in flight fails with EACCES; ``read_text`` opens
  with it, which never collides with a rename;
* MoveFileEx (os.replace) onto a file that any process has open fails with
  ERROR_ACCESS_DENIED for as long as the handle stays open; ``replace``
  waits that out, for at most ``REPLACE_RETRY_SECONDS``, and a ref whose file
  stays open longer is ``busy_message``, never a bare PermissionError.

tmi-repo has the same two rules (``fsio::rename_over``; Rust's own reads
already pass FILE_SHARE_DELETE).
"""

from __future__ import annotations

import atexit
import os
import time
from pathlib import Path


class Unchecked:
    """The type of ``UNCHECKED``: "do not compare", as opposed to None ("must not exist")."""


UNCHECKED = Unchecked()


class RefMovedError(RuntimeError):
    """A ref was not what the caller had read when its update took the lock."""

    def __init__(self, ref: str, expected: str | None, actual: str | None) -> None:
        self.ref = ref
        self.expected = expected
        self.actual = actual
        super().__init__(
            f"{ref} was changed by another tmi process while this command was running "
            f"(it was {_short(expected)}, it is now {_short(actual)}); nothing was written"
        )


def _short(value: str | None) -> str:
    return value[:12] if value else "missing"


def lock_message(display: str) -> str:
    return (
        f"unable to create '{display}': File exists. Another tmi process seems to be running in this "
        "repository. If no other tmi process is running, a previous one probably crashed: "
        "remove the file and try again."
    )


def busy_message(display: str) -> str:
    return (
        f"unable to update '{display}': another program keeps the file open (another tmi process "
        "reading it, a virus scanner, a backup or sync tool); nothing was written. Try again."
    )


# Windows: MoveFileEx (os.replace) onto a file another process has open fails
# with ERROR_ACCESS_DENIED (5) while that handle stays open, whatever its share
# mode: an unlocked reader of the ref (`list_refs` of a concurrent push, `log`,
# `status`; another tmi version), Defender or an indexer looking at a file it
# has just seen written. Such handles live for microseconds to milliseconds.
# These are the only errors retried (5, and ERROR_SHARING_VIOLATION (32) which
# some file systems give for the same thing), on Windows only, with a doubling
# delay from 1 ms up to 100 ms, for REPLACE_RETRY_SECONDS in total. A
# directory in the way and every other error raise at once; a read-only
# target (also error 5) raises when the time is up. tmi-repo fsio.rs
# `rename_over` has the same rule and numbers.
REPLACE_RETRY_SECONDS = 2.0
_SHARING_ERRORS = (5, 32)


class FileBusyError(PermissionError):
    """``replace`` gave up: the target stayed open in another process."""


def _is_sharing_error(exc: BaseException) -> bool:
    return os.name == "nt" and isinstance(exc, PermissionError) and getattr(exc, "winerror", None) in _SHARING_ERRORS


def replace(src: Path | str, dst: Path | str) -> None:
    """``os.replace(src, dst)``, waiting (boundedly) for a Windows handle open
    on ``dst`` to close. Raises FileBusyError when the time is up."""
    deadline = time.monotonic() + REPLACE_RETRY_SECONDS
    delay = 0.001
    while True:
        try:
            os.replace(src, dst)
            return
        except PermissionError as exc:
            # a directory in the way is not a handle that goes away
            if not _is_sharing_error(exc) or os.path.isdir(dst):
                raise
            now = time.monotonic()
            if now >= deadline:
                raise FileBusyError(*exc.args) from exc
            time.sleep(min(delay, deadline - now))
            delay = min(delay * 2, 0.1)


if os.name == "nt":
    import _winapi
    import msvcrt

    _FILE_SHARE_ALL = 0x1 | 0x2 | 0x4  # FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE

    def _read_bytes(path: Path) -> bytes:
        # CreateFile raises FileNotFoundError / PermissionError like open()
        handle = _winapi.CreateFile(str(path), _winapi.GENERIC_READ, _FILE_SHARE_ALL, 0, _winapi.OPEN_EXISTING, 0, 0)
        try:
            fd = msvcrt.open_osfhandle(handle, os.O_RDONLY | os.O_BINARY)
        except BaseException:
            _winapi.CloseHandle(handle)
            raise
        with open(fd, "rb") as f:
            return f.read()

else:

    def _read_bytes(path: Path) -> bytes:
        with open(path, "rb") as f:
            return f.read()


def read_text(path: Path) -> str:
    """``path.read_text(encoding="utf-8")`` of a file another tmi process may
    be renaming a lock over right now (a ref, HEAD, the index, the config).
    On Windows the file is opened with FILE_SHARE_DELETE, so the read and the
    rename do not collide (Python's open() leaves the flag out and fails with
    EACCES while a rename onto the file is in flight)."""
    text = _read_bytes(path).decode("utf-8")
    return text.replace("\r\n", "\n").replace("\r", "\n") if "\r" in text else text


def acquire(path: Path, display: str) -> int:
    """Create ``path`` exclusively and return its open file descriptor, or
    raise RuntimeError(lock_message(display)) if it exists."""
    try:
        # O_BINARY: on Windows os.write would turn "\n" into "\r\n"
        return os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_BINARY", 0), 0o644)
    except FileExistsError:
        raise RuntimeError(lock_message(display)) from None
    except PermissionError:
        # Windows: the file exists and its owner is deleting it right now.
        if os.name == "nt":
            raise RuntimeError(lock_message(display)) from None
        raise


_held: dict[Path, str] = {}


def hold(path: Path, display: str) -> bool:
    """Take the lock ``path`` for the rest of the process (or until
    ``release``); False when this process already holds it."""
    if path in _held:
        return False
    fd = acquire(path, display)
    try:
        os.write(fd, f"{os.getpid()}\n".encode("ascii"))
    finally:
        os.close(fd)
    _held[path] = display
    return True


def release(path: Path) -> None:
    if _held.pop(path, None) is not None:
        try:
            path.unlink()
        except OSError:
            pass


def suspend() -> list[tuple[Path, str]]:
    """Give up the locks this process holds while a hook runs (a hook may run
    ``tmiki add``); ``resume`` takes them again."""
    held = list(_held.items())
    for path, _ in held:
        release(path)
    return held


def resume(held: list[tuple[Path, str]]) -> None:
    for path, display in held:
        hold(path, display)


def release_all() -> None:
    for path in list(_held):
        release(path)


atexit.register(release_all)
