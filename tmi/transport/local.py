"""Filesystem-backed transport. Handles bare paths and ``file://`` URLs."""

from __future__ import annotations

from pathlib import Path
from urllib.parse import unquote, urlparse

from ..objects import ObjectStore
from ..lockfile import UNCHECKED, RefMovedError, Unchecked
from . import ProgressFn, RemoteRefMoved


class LocalTransport:
    def __init__(self, url: str) -> None:
        self.url = url
        self.path = _resolve_local_path(url)
        # Lazy import: Repo lives in tmi.repo which imports this package
        from ..repo import Repo
        self._repo = Repo.open_path(self.path)

    def list_refs(self) -> dict[str, str]:
        return self._repo.all_refs()

    def pull_objects(self, into_store: ObjectStore, on_progress: ProgressFn | None = None) -> int:
        return into_store.copy_missing_from(self._repo.store, on_progress=on_progress)

    def push_objects(self, from_store: ObjectStore, on_progress: ProgressFn | None = None) -> int:
        return self._repo.store.copy_missing_from(from_store, on_progress=on_progress)

    def update_ref(self, ref_name: str, value: str, expected: str | None | Unchecked = UNCHECKED) -> None:
        current_value = self._repo.read_ref(ref_name)
        if current_value == value:
            return
        if self._repo.worktree is not None and self._repo.head_target() == ref_name:
            raise RuntimeError(
                f"refusing to update checked-out branch in non-bare repository: {ref_name}"
            )
        # Compare-and-swap under <ref>.lock in the remote repository: a push
        # that raced another one fails here instead of overwriting it (#36).
        try:
            self._repo.write_ref(ref_name, value, expected=expected)
        except RefMovedError as exc:
            raise RemoteRefMoved(ref_name, exc.expected, exc.actual) from None
        if self._repo.current_branch() is None and ref_name.startswith("refs/heads/"):
            self._repo.set_head_ref(ref_name)

    def head_branch(self) -> str | None:
        return self._repo.current_branch()

    def repo_path(self) -> Path:
        return self.path


def _resolve_local_path(url: str) -> Path:
    if url.lower().startswith("file://"):
        parsed = urlparse(url)
        # urlparse on file:///C:/x gives path '/C:/x' on POSIX-style; strip leading slash for Win
        raw = unquote(parsed.path)
        if raw.startswith("/") and len(raw) > 3 and raw[2] == ":":
            raw = raw[1:]
        return Path(raw).expanduser().resolve()
    return Path(url).expanduser().resolve()
