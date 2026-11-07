from __future__ import annotations

import os
import tempfile
from pathlib import Path

from . import diskspace

# Bump whenever the layer/composite rendering or blend code changes so stale
# previews are never served for the same source bytes. The version lives in the
# cache path, so an old version's entries are simply ignored (and can be swept).
RENDER_CACHE_VERSION = 14


class RenderCache:
    """Disk cache for the expensive composite + thumbnail render.

    Keyed by the source file's content digest and the renderer version, so it is
    fully content-addressed: identical bytes always map to the same entry, and a
    renderer change moves to a fresh version namespace that invalidates every
    prior entry. A miss (or any read error) just means "render it"; the cache is
    a pure optimisation and never the source of truth.

    Lives under ``.tmi/cache`` which is outside the worktree scan, so cached
    PNGs are never tracked or committed.
    """

    def __init__(self, root: Path):
        self.root = Path(root)

    def _paths(self, digest: str) -> tuple[Path, Path]:
        base = self.root / f"v{RENDER_CACHE_VERSION}" / digest[:2]
        stem = digest[2:]
        return base / f"{stem}.composite.png", base / f"{stem}.thumb.png"

    def get(self, digest: str) -> tuple[bytes, bytes] | None:
        composite_path, thumb_path = self._paths(digest)
        try:
            return composite_path.read_bytes(), thumb_path.read_bytes()
        except OSError:
            # Missing, partially written, or unreadable — treat as a miss.
            return None

    def put(self, digest: str, composite_png: bytes, thumbnail_png: bytes) -> None:
        if not composite_png or not thumbnail_png:
            return
        composite_path, thumb_path = self._paths(digest)
        try:
            composite_path.parent.mkdir(parents=True, exist_ok=True)
            _atomic_write_bytes(composite_path, composite_png)
            _atomic_write_bytes(thumb_path, thumbnail_png)
        except OSError:
            # A cache write failure must never break the surrounding render.
            pass


def _atomic_write_bytes(path: Path, data: bytes) -> None:
    """Write via a sibling temp file + rename so a crash mid-write can never
    leave a truncated PNG that a later ``get`` would read back as valid."""
    diskspace.before_new_file()
    fd, tmp_name = tempfile.mkstemp(prefix=path.name + ".", suffix=".tmp", dir=path.parent)
    tmp = Path(tmp_name)
    try:
        with os.fdopen(fd, "wb") as handle:
            handle.write(data)
        tmp.replace(path)
        diskspace.note_new_file(path)
    except BaseException:
        tmp.unlink(missing_ok=True)
        raise
