"""URL-dispatched transport layer for tmi remotes.

Each transport implementation knows how to enumerate refs on the remote and
move objects in either direction. Two transports ship today:

* ``LocalTransport`` — filesystem paths and ``file://`` URLs (bare repos too).
* ``HttpsTransport`` — ``http(s)://`` URLs served by tmihub. The URL points at
  a repository's API root (``…/api/<user>/<repo>``); see ``https.py`` for the
  wire contract and Bearer-token auth.
"""

from __future__ import annotations

from typing import Callable, Protocol
from urllib.parse import urlparse

from ..objects import ObjectStore


ProgressFn = Callable[[str], None]

from ..lockfile import UNCHECKED, Unchecked


class RemoteRefMoved(RuntimeError):
    """The remote ref changed between the push reading it and updating it."""

    def __init__(self, ref: str, expected: str | None, actual: str | None) -> None:
        super().__init__(f"remote ref {ref} moved while pushing")
        self.ref = ref
        self.expected = expected
        self.actual = actual


class Transport(Protocol):
    def list_refs(self) -> dict[str, str]: ...

    def pull_objects(self, into_store: ObjectStore, on_progress: ProgressFn | None = None) -> int: ...

    def push_objects(self, from_store: ObjectStore, on_progress: ProgressFn | None = None) -> int: ...

    def update_ref(self, ref_name: str, value: str, expected: str | None | Unchecked = UNCHECKED) -> None:
        """Point the remote ref at `value`. With `expected` (None = the ref
        must not exist) the update is a compare-and-swap: if the remote ref is
        no longer `expected`, raise `RemoteRefMoved` and change nothing
        (issue #36)."""
        ...

    def head_branch(self) -> str | None: ...


def checked_refs(refs: dict[str, str]) -> dict[str, str]:
    """What a remote lists is data other people write, so every branch and tag
    name must be one tmi could write (``ref_name_problem``) and every value
    empty or a 64-digit lower-case hex object id. The whole listing is refused
    otherwise, before any ref is written (an id reaches ref files, messages and
    the pre-push hook's environment). tmi-transport's ``check_refs`` is the
    same function."""
    from ..repo import ref_name_problem
    from ..textsafe import escape

    for name, value in refs.items():
        if name.startswith(("refs/heads/", "refs/tags/")):
            problem = ref_name_problem(name)
            if problem is not None:
                raise RuntimeError(f"remote sent an invalid ref name: {escape(name)} ({problem})")
        if value is None or (
            value != ""
            and not (isinstance(value, str) and len(value) == 64 and all(c in "0123456789abcdef" for c in value))
        ):
            raise RuntimeError(f"remote sent an invalid object id for {escape(name)}: {escape(str(value))}")
    return refs


def _is_local_url(url: str) -> bool:
    parsed = urlparse(url)
    scheme = (parsed.scheme or "").lower()
    # bare windows paths (C:\...) get scheme "c" — treat as local
    return not scheme or scheme == "file" or len(scheme) == 1


def open_transport(url: str) -> Transport:
    if _is_local_url(url):
        from .local import LocalTransport
        return LocalTransport(url)
    scheme = (urlparse(url).scheme or "").lower()
    if scheme in {"http", "https"}:
        from .https import HttpsTransport
        return HttpsTransport(url)
    raise RuntimeError(f"unsupported remote URL scheme: {scheme}://")


def canonical_url(url: str) -> str:
    """Normalise a remote URL for stable storage.

    Local filesystem remotes are resolved to an absolute path so later
    fetch/push/pull keep working regardless of the current directory (a bare
    relative path like ``../repo.tmi`` would otherwise be re-resolved against
    whatever directory the next command runs from). Network URLs are returned
    unchanged.
    """
    if _is_local_url(url):
        from .local import _resolve_local_path
        return str(_resolve_local_path(url))
    return url
