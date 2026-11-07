from __future__ import annotations

import hashlib
import json
import os
import re
import shutil
import stat
import sys
import tempfile
import time
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path, PurePosixPath
from typing import TYPE_CHECKING, Any, Callable

ProgressFn = Callable[[str], None]

from . import config as _config
from . import diskspace as _diskspace
from . import hooks as _hooks
from . import ignore as _ignore
from . import lockfile as _lockfile
from . import pixel_limit as _pixel_limit
from . import recpath as _recpath
from . import reflog as _reflog
from .plugins import PluginContext
from .plugins.loader import get_api as _plugin_api
from .objects import ObjectStore, canonical_json_bytes
from .render_cache import RenderCache

_Unchecked = _lockfile.Unchecked
_UNCHECKED = _lockfile.UNCHECKED

if TYPE_CHECKING:
    from .importers import ImportedFile, ImportedLayer


# The image stack (Pillow, numpy, the CLIP/MDP readers, tmi_accel) costs ~0.3s
# to import. status/commit/log/branch never touch pixels, so it is loaded on
# first use through these thin wrappers instead of at module import.
def import_file(*args: Any, **kwargs: Any) -> "ImportedFile":
    from .importers import import_file as impl

    return impl(*args, **kwargs)


def find_layer(*args: Any, **kwargs: Any) -> Any:
    from .importers import find_layer as impl

    return impl(*args, **kwargs)


def walk_imported_layers(*args: Any, **kwargs: Any) -> Any:
    from .importers import walk_imported_layers as impl

    return impl(*args, **kwargs)


def pixels_from_bytes(data: bytes) -> Any:
    from .image_ops import pixels_from_bytes as impl

    return impl(data)


def layer_blob_to_png(data: bytes) -> bytes:
    from .image_ops import layer_blob_to_png as impl

    return impl(data)


def replace_layer_pixels(*args: Any, **kwargs: Any) -> bytes:
    from .native import replace_layer_pixels as impl

    return impl(*args, **kwargs)


def _in_background(fn: Callable[[], Any]) -> Any:
    """Run ``fn`` on a thread of its own; the returned future's ``.result()``
    joins it and raises what it raised."""
    from concurrent.futures import ThreadPoolExecutor

    pool = ThreadPoolExecutor(max_workers=1)
    try:
        return pool.submit(fn)
    finally:
        pool.shutdown(wait=False)


def _reuse_decoded(blob_reuse: dict[str, dict]) -> dict[str, dict] | None:
    """The layers of a ``_layer_blob_reuse`` map that may skip decoding.

    ``TMI_NO_DECODE_REUSE=1`` turns this off (always decode) as a safety valve.
    """
    if os.environ.get("TMI_NO_DECODE_REUSE", "").strip().lower() in {"1", "true", "yes", "on"}:
        return None
    return {sid: d for sid, d in blob_reuse.items() if d.get("pixel_source")} or None


def _reuses_blob(reuse: dict | None, layer: Any) -> bool:
    # Invariant: a tree layer's pixel_sha256 always describes the pixels of
    # its blob_ref. When the prior commit/index has a layer with the same
    # stable id and matching pixel digest, reuse that blob_ref without
    # writing — this also dedupes when a caller eagerly encoded blob_bytes
    # for pixels that did not change.
    return (
        reuse is not None
        and reuse.get("pixel_sha256") is not None
        and layer.pixel_sha256 is not None
        and reuse["pixel_sha256"] == layer.pixel_sha256
    )


def is_supported_path(path: Path) -> bool:
    # Same answer as importers.is_supported_path (tmi versions any regular
    # file), defined here so the worktree scan never imports the image stack.
    return True


TMI_DIR = ".tmi"
DEFAULT_BRANCH = "main"
INDEX_ONLY_SOURCE_META_KEYS = {
    "staged_layer_selector",
    "staged_layer_name",
    "staged_layer_stable_source_id",
}
INDEX_DELETE_MARKER = "deleted"
# `restore --source WORKTREE --output`: export from the working file.
WORKTREE_SOURCE = "WORKTREE"
PARTIAL_STAGE_LAYER_FIELDS = (
    "name",
    "kind",
    "visible",
    "opacity",
    "blend_mode",
    "clip_to_below",
    "offset_x",
    "offset_y",
)
PARTIAL_STAGE_SOURCE_META_KEYS = (
    "type",
    "parent_id",
    "folder",
    "composite_id",
    "masking",
    "masking_type",
    "pixel_width",
    "pixel_height",
    "channel_profile",
    "render_payload_sha256",
    "mask_payload_sha256",
    "width",
    "height",
    "color",
    "lumi",
    "protect_alpha",
    "draft",
    "halftone_type",
    "halftone_line",
    "transform_offset_x",
    "transform_offset_y",
    "xml_attrs",
    "attributes",
    "text",
    "text_lines",
    "text_layer_attributes_sha256",
    "text_layer_add_attributes_v01_sha256",
    "text_layer_attributes_version",
    "text_layer_name_auto_change_enabled",
    "vector_normal_type",
    "vector_indices",
    "vector_outer_ids",
    "vector_normal_stroke_sha256",
    "vector_normal_fill_sha256",
    "vector_normal_balloon_sha256",
)
PAYLOAD_DIGEST_SOURCE_META_KEYS = {"render_payload_sha256", "mask_payload_sha256"}


@dataclass
class StatusItem:
    state: str
    path: str
    detail: str = ""


class Repo:
    def __init__(self, worktree: Path | None, repo_dir: Path):
        self.worktree = worktree.resolve() if worktree else None
        self.repo_dir = repo_dir.resolve()
        self.store = ObjectStore(self.repo_dir / "objects")
        # Content-addressed cache for the expensive composite/thumbnail render,
        # under .tmi/cache (outside the worktree scan, never tracked).
        self.render_cache = RenderCache(self.repo_dir / "cache" / "render")
        # Working-tree files this Repo object has rewritten, in order (see
        # _rewrote). The CLI tells the user to reopen them in the drawing app.
        self.rewritten: list[Path] = []

    def _rewrote(self, path: Path) -> Path:
        """Record that tmi just replaced the bytes of working-tree ``path``."""
        if path not in self.rewritten:
            self.rewritten.append(path)
        return path

    @classmethod
    def create(cls, path: Path, bare: bool = False) -> "Repo":
        target = path.resolve()
        repo_dir = target if bare else target / TMI_DIR
        repo = cls(None if bare else target, repo_dir)
        repo._init_storage()
        return repo

    @classmethod
    def open(cls, start: Path | None = None) -> "Repo":
        start = (start or Path.cwd()).resolve()
        cur = start if start.is_dir() else start.parent
        while True:
            repo_dir = cur / TMI_DIR
            if repo_dir.is_dir():
                # The repo the command runs in decides core.maxImagePixels.
                _pixel_limit.set_repo_dir(repo_dir.resolve())
                return cls(cur, repo_dir)
            if cur.parent == cur:
                raise RuntimeError("not a tmi repository")
            cur = cur.parent

    @classmethod
    def open_path(cls, value: str | Path) -> "Repo":
        path = Path(value).expanduser().resolve()
        if (path / TMI_DIR).is_dir():
            return cls(path, path / TMI_DIR)
        if (path / "objects").is_dir() and (path / "refs").is_dir():
            return cls(None, path)
        raise RuntimeError(f"not a tmi repository: {path}")

    def _init_storage(self) -> None:
        self.repo_dir.mkdir(parents=True, exist_ok=True)
        for rel in [
            "objects",
            "refs/heads",
            "refs/tags",
            "refs/remotes",
            "logs",
            "diff",
            "conflicts",
            "hooks",
        ]:
            (self.repo_dir / rel).mkdir(parents=True, exist_ok=True)
        head = self.repo_dir / "HEAD"
        if not head.exists():
            _atomic_write_bytes(head, f"ref: refs/heads/{DEFAULT_BRANCH}\n".encode("utf-8"))
        config = self.repo_dir / "config"
        if not config.exists():
            _atomic_write_bytes(config, b'{"version":1,"remotes":{}}\n')
        index = self.repo_dir / "index"
        if not index.exists():
            self.write_index({"version": 1, "entries": {}})

    def require_worktree(self) -> Path:
        if self.worktree is None:
            raise RuntimeError("operation requires a working tree")
        return self.worktree

    def relpath(self, path: Path) -> str:
        root = self.require_worktree()
        full = path.resolve()
        try:
            rel = full.relative_to(root)
        except ValueError:
            raise RuntimeError(f"path is outside working tree: {full}")
        return rel.as_posix()

    def abspath(self, relpath: str) -> Path:
        """The file a recorded path names in the working tree, after
        ``recpath``'s rule, inside the working tree and outside ``.tmi``, with
        no symbolic link or junction on the way. Every write, delete and
        existence check of a recorded path goes through this."""
        return _recpath.worktree_path(self.require_worktree(), self.repo_dir, relpath)

    def pathspec_full_path(self, pathspec: str) -> Path:
        path = Path(pathspec)
        if path.is_absolute():
            return path.resolve()
        cwd_path = (Path.cwd() / path).resolve()
        if cwd_path.exists():
            return cwd_path
        root_path = (self.require_worktree() / path).resolve()
        if root_path.exists():
            return root_path
        return cwd_path

    def pathspec_link(self, pathspec: str) -> tuple[str, str, bool] | None:
        """``(kind, path, is_pathspec)`` of the symbolic link or junction
        ``pathspec`` is (``is_pathspec``), or lies beyond (``path`` is
        worktree-relative; links are not followed), if any. tmi does not track
        links (issue #44)."""
        root = self.require_worktree()
        path = Path(pathspec)
        for candidate in ([path] if path.is_absolute() else [Path.cwd() / path, root / path]):
            candidate = Path(os.path.abspath(candidate))
            if not os.path.lexists(candidate):
                continue
            try:
                rel = candidate.relative_to(root)
            except ValueError:
                continue
            current = root
            for part in rel.parts:
                current = current / part
                kind = link_kind(current)
                if kind:
                    return kind, current.relative_to(root).as_posix(), current == candidate
            return None
        return None

    def pathspec_relpath(self, pathspec: str, known_paths: set[str] | None = None) -> str:
        path = Path(pathspec)
        if path.is_absolute():
            return self.relpath(path)

        root = self.require_worktree()
        candidates: list[str] = []
        for candidate in ((Path.cwd() / path).resolve(), (root / path).resolve()):
            try:
                rel = candidate.relative_to(root).as_posix()
            except ValueError:
                continue
            if rel not in candidates:
                candidates.append(rel)

        if known_paths:
            for rel in candidates:
                if rel in known_paths:
                    return rel
        if candidates:
            return candidates[0]
        return path.as_posix()

    def read_config(self) -> dict[str, Any]:
        path = self.repo_dir / "config"
        if not path.exists():
            return {"version": 1, "remotes": {}}
        import json

        return json.loads(_lockfile.read_text(path))

    def write_config(self, config: dict[str, Any]) -> None:
        import json

        _atomic_write_bytes(
            self.repo_dir / "config",
            (json.dumps(config, ensure_ascii=False, sort_keys=True, indent=2) + "\n").encode("utf-8"),
        )

    def read_index(self) -> dict[str, Any]:
        import json

        path = self.repo_dir / "index"
        if not path.exists():
            return {"version": 1, "entries": {}}
        return json.loads(_lockfile.read_text(path))

    def _lock_display(self, path: Path) -> str:
        """A lock file as the error message names it: ``.tmi/index.lock``."""
        return f"{self.repo_dir.name}/{path.relative_to(self.repo_dir).as_posix()}"

    def lock_index(self) -> bool:
        """Take ``index.lock`` (O_CREAT | O_EXCL) and keep it until the process
        ends or ``unlock_index`` (issue #35). A command that reads the index in
        order to change it calls this before the read, so no other tmi
        process can write the index in between. Raises at once if another
        process holds it; False when this process already does."""
        path = self.repo_dir / "index.lock"
        return _lockfile.hold(path, self._lock_display(path))

    def unlock_index(self) -> None:
        _lockfile.release(self.repo_dir / "index.lock")

    def write_index(self, index: dict[str, Any]) -> None:
        import json

        # Atomic: a crash mid-write to the index file would brick the repo
        # (every add/commit rewrites it). Write to a sibling temp file in the
        # same directory and rename — same-dir rename is atomic on POSIX and
        # uses os.replace on Windows, which atomically overwrites.
        # Under index.lock: taken here for this one write when the command
        # does not already hold it for its whole read-modify-write.
        taken = self.lock_index()
        try:
            _atomic_write_bytes(
                self.repo_dir / "index",
                (json.dumps(index, ensure_ascii=False, sort_keys=True, indent=2) + "\n").encode("utf-8"),
            )
        finally:
            if taken:
                self.unlock_index()

    def _stat_cache_path(self) -> Path:
        return self.repo_dir / "stat_cache"

    def read_stat_cache(self) -> dict[str, Any]:
        """Load the worktree stat cache, returning an empty skeleton if it is
        missing or corrupt. The cache is a pure optimization (self-validating
        against size/mtime), so a bad file is silently rebuilt rather than
        raised — never let a damaged cache break ``status``."""
        empty = {"version": 1, "written_ns": 0, "entries": {}}
        path = self._stat_cache_path()
        try:
            data = json.loads(path.read_text(encoding="utf-8"))
        except (FileNotFoundError, ValueError, OSError):
            return empty
        if not isinstance(data, dict) or not isinstance(data.get("entries"), dict):
            return empty
        data.setdefault("version", 1)
        data.setdefault("written_ns", 0)
        return data

    def write_stat_cache(self, cache: dict[str, Any]) -> None:
        # Stamp the write moment so racy entries (mtime within the same
        # granularity window as this write) are distrusted on the next read.
        cache["written_ns"] = time.time_ns()
        _atomic_write_bytes(
            self._stat_cache_path(),
            (json.dumps(cache, ensure_ascii=False, sort_keys=True, indent=2) + "\n").encode("utf-8"),
        )

    def head_target(self) -> str:
        head = _lockfile.read_text(self.repo_dir / "HEAD").strip()
        if head.startswith("ref: "):
            return head[5:]
        return head

    def current_branch(self) -> str | None:
        target = self.head_target()
        if target.startswith("refs/heads/"):
            return target.removeprefix("refs/heads/")
        return None

    def current_commit(self) -> str | None:
        target = self.head_target()
        if target.startswith("refs/"):
            return self.read_ref(target)
        return target if target else None

    def read_ref(self, ref: str) -> str | None:
        path = self._ref_path(ref, strict=False)
        # is_file, not exists: "refs/heads/feature" is a *directory* when a
        # branch like "feature/x" exists, and read_text on a directory dies
        # with PermissionError/IsADirectoryError instead of "no such ref".
        if not path.is_file():
            return None
        value = _lockfile.read_text(path).strip()
        return value or None

    def write_ref(self, ref: str, value: str, expected: str | None | _Unchecked = _UNCHECKED) -> None:
        """Point ``ref`` at ``value`` through ``<ref path>.lock``: the lock is
        created exclusively, the ref is re-read under it and, when ``expected``
        is given (None = the ref must not exist), compared with it (a
        compare-and-swap; RefMovedError if it differs), the new value is
        written into the lock file and the lock is renamed over the ref."""
        path = self._ref_path(ref)
        self._locked_write(path, (value + "\n").encode("utf-8"), ref, expected)

    def _locked_write(
        self, path: Path, data: bytes, name: str, expected: str | None | _Unchecked = _UNCHECKED
    ) -> None:
        path.parent.mkdir(parents=True, exist_ok=True)
        lock = path.with_name(path.name + ".lock")
        fd = _lockfile.acquire(lock, self._lock_display(lock))
        try:
            try:
                if expected is not _UNCHECKED:
                    actual = self._read_ref_file(path)
                    if actual != expected:
                        raise _lockfile.RefMovedError(name, expected, actual)
                os.write(fd, data)
            finally:
                os.close(fd)
            _lockfile.replace(lock, path)
        except _lockfile.FileBusyError:
            lock.unlink(missing_ok=True)
            raise RuntimeError(_lockfile.busy_message(self._lock_display(path))) from None
        except BaseException:
            lock.unlink(missing_ok=True)
            raise

    @staticmethod
    def _read_ref_file(path: Path) -> str | None:
        if not path.is_file():
            return None
        return _lockfile.read_text(path).strip() or None

    def delete_ref(self, ref: str, expected: str | None | _Unchecked = _UNCHECKED) -> None:
        """Remove ``ref`` under its lock (compare-and-swap like ``write_ref``)."""
        path = self._ref_path(ref, strict=False)
        lock = path.with_name(path.name + ".lock")
        os.close(_lockfile.acquire(lock, self._lock_display(lock)))
        try:
            if expected is not _UNCHECKED:
                actual = self._read_ref_file(path)
                if actual != expected:
                    raise _lockfile.RefMovedError(ref, expected, actual)
            path.unlink(missing_ok=True)
        finally:
            lock.unlink(missing_ok=True)

    def _ref_path(self, ref: str, *, strict: bool = True) -> Path:
        """The file of ref ``ref``. ``strict`` (writes) applies Git's whole
        rule for ref names; reads and deletes accept what could exist as a
        file (so a ref made before the rule, or ``main~1`` tried as a name by
        a revision lookup, does not fail)."""
        problem = ref_name_problem(ref, reading=not strict)
        if problem is None:
            parts = tuple(ref.split("/"))
            if parts[0] != "refs":
                problem = "it does not start with refs/"
        if problem is not None:
            shown = ref.removeprefix("refs/heads/") if ref.startswith("refs/heads/") else ref.removeprefix("refs/tags/")
            raise RuntimeError(f"invalid ref name: {shown} ({problem})")
        return self.repo_dir.joinpath(*tuple(ref.split("/")))

    def set_head_ref(self, ref: str) -> None:
        self._ref_path(ref)
        self._locked_write(self.repo_dir / "HEAD", f"ref: {ref}\n".encode("utf-8"), "HEAD")

    def set_current_commit(self, commit: str, expected: str | None | _Unchecked = _UNCHECKED) -> None:
        """Move the current branch (or a detached HEAD) to ``commit``;
        ``expected`` as in ``write_ref``."""
        target = self.head_target()
        if target.startswith("refs/"):
            self.write_ref(target, commit, expected)
        else:
            self._locked_write(self.repo_dir / "HEAD", (commit + "\n").encode("utf-8"), "HEAD", expected)

    def detach_head(self, commit: str) -> None:
        self._locked_write(self.repo_dir / "HEAD", (commit + "\n").encode("utf-8"), "HEAD")

    def resolve_rev(self, rev: str | None = None) -> str | None:
        if not rev or rev == "HEAD":
            return self.current_commit()
        if rev == "MERGE_HEAD":
            merge_head = self.pending_merge_head()
            if not merge_head:
                raise RuntimeError("MERGE_HEAD: no merge in progress")
            return merge_head
        # Git's disambiguation order (gitrevisions(7)): tags take precedence
        # over branches, and ambiguity gets a warning rather than silence.
        candidates = [
            f"refs/tags/{rev}",
            f"refs/heads/{rev}",
            f"refs/remotes/{rev}",
            f"refs/remotes/origin/{rev}",
        ]
        found = [(ref, value) for ref in candidates if (value := self.read_ref(ref))]
        if found:
            resolved = self._resolve_object_as_commit(found[0][1], rev)
            if len(found) > 1:
                others = set()
                for ref, value in found[1:]:
                    try:
                        others.add(self._resolve_object_as_commit(value, rev))
                    except RuntimeError:
                        pass
                if others - {resolved}:
                    import sys

                    print(
                        f"tmiki: warning: refname {rev!r} is ambiguous "
                        f"({', '.join(ref for ref, _ in found)}); using {found[0][0]}",
                        file=sys.stderr,
                    )
            return resolved
        first_parent_match = re.fullmatch(r"(.+)~(\d+)", rev)
        if first_parent_match:
            base = self.resolve_rev(first_parent_match.group(1))
            steps = int(first_parent_match.group(2))
            for _ in range(steps):
                if not base:
                    raise RuntimeError(f"revision has no parent: {rev}")
                parents = self.read_commit(base).get("parents") or []
                if not parents:
                    raise RuntimeError(f"revision has no parent: {rev}")
                base = parents[0]
            return base
        if re.fullmatch(r"[0-9a-fA-F]{64}", rev):
            return self._resolve_object_as_commit(rev.lower(), rev)
        if re.fullmatch(r"[0-9a-fA-F]{7,63}", rev):
            matches = [oid for oid in self.store.iter_object_ids() if oid.startswith(rev.lower())]
            if len(matches) == 1:
                return self._resolve_object_as_commit(matches[0], rev)
            if len(matches) > 1:
                raise RuntimeError(f"ambiguous revision prefix: {rev}")
        raise RuntimeError(f"unknown revision: {rev}")

    def _resolve_object_as_commit(self, oid: str, rev: str, seen: set[str] | None = None) -> str:
        oid = oid.lower()
        seen = seen or set()
        if oid in seen:
            raise RuntimeError(f"tag cycle while resolving revision: {rev}")
        seen.add(oid)
        try:
            object_type, _ = self.store.read(oid)
        except FileNotFoundError as exc:
            raise RuntimeError(f"unknown revision: {rev}") from exc
        if object_type == "tag":
            tag = self.store.read_json(oid, "tag")
            target = tag.get("object")
            if not isinstance(target, str) or not target:
                raise RuntimeError(f"tag does not point to a commit: {rev}")
            return self._resolve_object_as_commit(target, rev, seen)
        if object_type != "commit":
            raise RuntimeError(f"revision does not name a commit: {rev}")
        return oid

    def stage_file(
        self,
        spec: str,
        on_progress: ProgressFn | None = None,
        *,
        force: bool = False,
        source: tuple[bytes, str] | None = None,
    ) -> tuple[str, str | None, bool]:
        """Stage ``spec`` (a path, or ``path:layer`` for one layer).

        ``source`` may pass the file's ``(bytes, sha256)`` when the caller has
        just read and hashed it, so the import does not do it again.

        When the disk is full the objects and cache files this file's import
        wrote are removed again (nothing refers to them yet) and the error says
        which file and how much space is missing (issue #42)."""
        with _diskspace.track_new_files() as created:
            try:
                return self._stage_file(spec, on_progress, force=force, source=source)
            except OSError as exc:
                if not _diskspace.is_disk_full(exc):
                    raise
                _diskspace.discard(created)
                file_part = split_pathspec(spec)[0]
                full = self.pathspec_full_path(file_part)
                try:
                    rel = self.relpath(full)
                    needed = full.stat().st_size
                except (OSError, RuntimeError):
                    rel, needed = file_part, 0
                free = _diskspace.free_bytes(self.repo_dir)
                raise _diskspace.DiskFullError(_diskspace.disk_full_message(rel, needed, free)) from exc

    def _stage_file(
        self,
        spec: str,
        on_progress: ProgressFn | None = None,
        *,
        force: bool = False,
        source: tuple[bytes, str] | None = None,
    ) -> tuple[str, str | None, bool]:
        file_part, layer_selector = split_pathspec(spec)
        source_bytes, source_digest = source if source is not None else (None, None)
        full = self.pathspec_full_path(file_part)
        head_entries = self.entries_for_commit(self.current_commit())
        if not full.exists():
            rel = self.pathspec_relpath(file_part, set(head_entries))
            if not layer_selector and rel in head_entries:
                index = self.read_index()
                entries = index.setdefault("entries", {})
                delete_entry = _deleted_index_entry(rel)
                changed = not same_entry(entries.get(rel), delete_entry)
                if changed:
                    entries[rel] = delete_entry
                    self.write_index(index)
                self.clear_conflict_files(rel)
                return rel, None, changed
            raise FileNotFoundError(f"no such file to add: {full}")
        rel = self.relpath(full)
        # a record is made by the rule every tree is read by (recpath.py)
        _recpath.check_new_record_path(rel)
        if not force and rel not in head_entries:
            existing = self.read_index().get("entries", {}).get(rel)
            already_tracked = existing is not None and not _is_deleted_index_entry(existing)
            if not already_tracked and _ignore.is_file_ignored(rel, self.parsed_ignore_patterns()):
                # Explicitly adding an ignored, untracked path needs -f, like
                # git. Tracked files stay stageable regardless of .tmiignore.
                raise RuntimeError(
                    f"path is ignored by .tmiignore: {rel} (use 'tmiki add -f' to add anyway)"
                )
        if on_progress:
            on_progress(f"parsing {file_part}")
        rel = self.relpath(full)
        if layer_selector:
            # A partial layer stage needs the selected layer's decoded pixel
            # bytes (see materialize_layer_stage), so never skip its encode.
            imported = import_file(
                full, cache=self.render_cache, on_progress=on_progress,
                source=source_bytes, source_digest=source_digest,
            )
        else:
            # Reuse already-stored blobs for layers whose pixels are unchanged
            # vs. the index (preferred — it is what would be committed) or HEAD.
            index_entries = self.read_index().get("entries", {})
            blob_reuse = self._layer_blob_reuse(head_entries.get(rel))
            blob_reuse.update(self._layer_blob_reuse(index_entries.get(rel)))
            reuse_pixels = {sid: d["pixel_sha256"] for sid, d in blob_reuse.items()} or None
            if source_bytes is None:
                source_bytes, source_digest = read_file_digest(full)
            # A file whose bytes tmi has not stored needs at least that much
            # room: say so before writing anything (issue #42).
            known_digests = {(index_entries.get(rel) or {}).get("source_digest"), (head_entries.get(rel) or {}).get("source_digest")}
            if source_digest not in known_digests:
                free = _diskspace.free_bytes(self.repo_dir)
                if free < len(source_bytes):
                    raise _diskspace.DiskFullError(_diskspace.not_enough_message(rel, len(source_bytes), free))
            # A plugin format is refused before anything is written (PLUGINS.md 4.4).
            from . import attributes as _attributes

            _attributes.check_import(full)
            # The file's bytes go to the object store while they are decoded
            # (objects are content-addressed: the store ends up the same).
            source_job = _in_background(lambda data=source_bytes: self.store.write_large_blobs([data])[0])
            try:
                imported = import_file(
                    full,
                    cache=self.render_cache,
                    reuse_pixels=reuse_pixels,
                    reuse_decoded=_reuse_decoded(blob_reuse),
                    on_progress=on_progress,
                    composite_base=self._composite_base(index_entries.get(rel)) or self._composite_base(head_entries.get(rel)),
                    source=source_bytes,
                    source_digest=source_digest,
                )
            except BaseException:
                # Let the source write finish (or fail) first: whoever cleans
                # up after this error must see every file it wrote.
                try:
                    source_job.result()
                except BaseException:
                    pass
                raise
        selected_layer = find_layer(imported.layers, layer_selector) if layer_selector else None
        if layer_selector and not selected_layer:
            raise RuntimeError(f"layer not found in {file_part}: {layer_selector}")
        if layer_selector:
            entry = self.materialize_layer_stage(
                rel,
                imported,
                selected_layer,
                layer_selector,
                on_progress=on_progress,
            )
        else:
            entry = self.materialize_import(
                rel, imported, on_progress=on_progress, blob_reuse=blob_reuse, source_ref=source_job.result(),
            )
            self._apply_layer_ignore(rel, entry, on_progress=on_progress)
        index = self.read_index()
        entries = index.setdefault("entries", {})
        head_entry = head_entries.get(rel)
        if same_entry(self._committable_entry(entry), head_entry):
            changed = rel in entries
            if changed:
                del entries[rel]
                self.write_index(index)
            _diskspace.keep_new_files()
            conflict_changed = rel in self.pending_conflict_paths()
            self.clear_conflict_files(rel)
            return rel, layer_selector, changed or conflict_changed
        changed = not same_entry(entries.get(rel), entry)
        if changed:
            entries[rel] = entry
            self.write_index(index)
        _diskspace.keep_new_files()
        self.clear_conflict_files(rel)
        return rel, layer_selector, changed

    def stage_delete(self, rel: str) -> None:
        index = self.read_index()
        index.setdefault("entries", {})[rel] = _deleted_index_entry(rel)
        self.write_index(index)
        self.clear_conflict_files(rel)

    def unstage(self, spec: str) -> tuple[str, bool]:
        """Reset a path's index entry to match HEAD (git restore --staged).

        Returns (relpath, was_staged). Removing the index override leaves the
        working-tree change intact; status recomputes the path against HEAD.
        During a merge, a file whose conflict was resolved by staging it
        becomes an unresolved conflict again (issue #12), even when what was
        staged equals HEAD and so left no index entry.
        """
        file_part, _layer_selector = split_pathspec(spec)
        index = self.read_index()
        entries = index.get("entries", {})
        head_entries = self.entries_for_commit(self.current_commit())
        resolved = set(self.resolved_conflict_paths()) if self.pending_merge_head() else set()
        rel = self.pathspec_relpath(file_part, set(entries) | set(head_entries) | resolved)
        if rel in resolved:
            if rel in entries:
                del entries[rel]
                self.write_index(index)
            self.reopen_conflict(rel)
            return rel, True
        if rel in entries:
            del entries[rel]
            self.write_index(index)
            return rel, True
        if rel in head_entries:
            # Already matching HEAD; nothing staged to undo (no-op, like git).
            return rel, False
        raise RuntimeError(f"pathspec did not match any staged or tracked file: {rel}")

    def reset_path(self, spec: str, source: str | None = None) -> tuple[str, bool]:
        commit = self.resolve_rev(source)
        target_entries = self.entries_for_commit(commit)
        index = self.read_index()
        entries = index.setdefault("entries", {})
        current_entries = self.entries_for_commit(self.current_commit())
        known = set(entries) | set(target_entries) | set(current_entries)
        rel = self.pathspec_relpath(spec, known)
        changed = False
        if rel in target_entries:
            before = entries.get(rel)
            target_entry = target_entries[rel]
            if same_entry(target_entry, current_entries.get(rel)):
                if rel in entries:
                    del entries[rel]
                    changed = True
            elif not same_entry(before, target_entry):
                entries[rel] = target_entry
                changed = True
        elif rel in entries or rel in current_entries:
            if rel in entries:
                del entries[rel]
                changed = True
            if rel in current_entries:
                entries[rel] = _deleted_index_entry(rel)
                changed = True
        else:
            raise RuntimeError(f"pathspec did not match any file: {rel}")
        self.write_index(index)
        return rel, changed

    def materialize_layer_stage(
        self,
        relpath: str,
        imported: ImportedFile,
        selected_layer: ImportedLayer,
        layer_selector: str,
        on_progress: ProgressFn | None = None,
    ) -> dict[str, Any]:
        head_commit = self.current_commit()
        head_entry = self.entries_for_commit(head_commit).get(relpath) if head_commit else None
        if not head_entry:
            entry = self.materialize_import(relpath, imported, on_progress=on_progress)
            self._apply_layer_ignore(relpath, entry, on_progress=on_progress)
            entry.setdefault("source_meta", {})["staged_layer_selector"] = layer_selector
            entry["source_meta"]["staged_layer_name"] = selected_layer.name
            entry["source_meta"]["staged_layer_stable_source_id"] = selected_layer.stable_source_id
            return entry

        source_format = str(head_entry.get("source_format") or "").lower()
        if source_format != imported.source_format:
            raise RuntimeError(
                f"cannot stage a single layer after file format changed "
                f"({source_format} -> {imported.source_format}); stage the whole file"
            )
        if source_format not in {"mdp2", "clip"}:
            # Flat formats (raster images, opaque binaries) have no separable
            # layers, so a layer selector just restages the whole file.
            entry = self.materialize_import(relpath, imported, on_progress=on_progress)
            entry.setdefault("source_meta", {})["staged_layer_selector"] = layer_selector
            entry["source_meta"]["staged_layer_name"] = selected_layer.name
            entry["source_meta"]["staged_layer_stable_source_id"] = selected_layer.stable_source_id
            return entry
        self._ensure_partial_layer_stage_safe(head_entry, imported, selected_layer, layer_selector)
        if selected_layer.blob_bytes is None:
            capability = (selected_layer.source_meta or {}).get("write_capability")
            hint = "; stage the whole file for metadata-only changes" if capability == "metadata-only" else ""
            raise RuntimeError(f"layer has no stageable pixel blob: {layer_selector}{hint}")

        if on_progress:
            on_progress(f"staging layer {relpath}:{selected_layer.name}")
        base_source = self.store.read_blob(head_entry["source_blob_ref"])
        base_path = self._write_tmp_source(relpath, base_source, "base")
        merged_path = self._tmp_source_path(relpath, "merged")
        errors: list[str] = []
        try:
            merged_bytes: bytes | None = None
            selectors = [
                selected_layer.stable_source_id,
                selected_layer.layer_id,
                layer_selector,
                selected_layer.name,
            ]
            for native_selector in dict.fromkeys(s for s in selectors if s):
                try:
                    merged_bytes = replace_layer_pixels(
                        base_path,
                        source_format,
                        native_selector,
                        selected_layer.blob_bytes,
                    )
                    break
                except Exception as exc:
                    errors.append(f"{native_selector}: {exc}")
            if merged_bytes is None:
                raise RuntimeError(
                    f"could not stage layer {relpath}:{layer_selector}; "
                    + "; ".join(errors[:3])
                )
            merged_path.write_bytes(merged_bytes)
            entry = self.materialize_file_from_path(relpath, merged_path, base_entry=head_entry, source=merged_bytes)
            if self._committable_entry(entry) == head_entry:
                raise RuntimeError(
                    f"layer has no pixel changes to stage separately: {layer_selector}; "
                    "stage the whole file for metadata-only changes"
                )
        finally:
            base_path.unlink(missing_ok=True)
            merged_path.unlink(missing_ok=True)

        entry.setdefault("source_meta", {})["staged_layer_selector"] = layer_selector
        entry["source_meta"]["staged_layer_name"] = selected_layer.name
        entry["source_meta"]["staged_layer_stable_source_id"] = selected_layer.stable_source_id
        return entry

    def _ensure_partial_layer_stage_safe(
        self,
        head_entry: dict[str, Any],
        imported: ImportedFile,
        selected_layer: ImportedLayer,
        layer_selector: str,
    ) -> None:
        if head_entry.get("canvas_meta") != imported.canvas_meta:
            raise RuntimeError(
                f"cannot stage a single layer while canvas/global settings changed: {layer_selector}; "
                "stage the whole file"
            )

        old_layer = next(
            (
                layer for layer in self.flatten_layers(head_entry["tree_ref"])
                if layer.get("stable_source_id") == selected_layer.stable_source_id
            ),
            None,
        )
        if old_layer is None:
            raise RuntimeError(
                f"cannot stage a newly added layer separately: {layer_selector}; "
                "stage the whole file"
            )

        for key in PARTIAL_STAGE_LAYER_FIELDS:
            if old_layer.get(key) != getattr(selected_layer, key):
                raise RuntimeError(
                    f"layer metadata changed for {layer_selector}; "
                    "stage the whole file for metadata-only/global changes"
                )

        old_meta = old_layer.get("source_meta") or {}
        new_meta = selected_layer.source_meta or {}
        for key in PARTIAL_STAGE_SOURCE_META_KEYS:
            if key not in old_meta and key not in new_meta:
                continue
            old_value = self._old_layer_source_meta_value(head_entry, old_layer, old_meta, key)
            new_value = new_meta.get(key)
            if key in PAYLOAD_DIGEST_SOURCE_META_KEYS and key not in old_meta and (old_value is None or new_value is None):
                continue
            if old_value != new_value:
                raise RuntimeError(
                    f"layer metadata changed for {layer_selector}; "
                    "stage the whole file for metadata-only/global changes"
                )

    def _old_layer_source_meta_value(
        self,
        old_entry: dict[str, Any],
        old_layer: dict[str, Any],
        old_meta: dict[str, Any],
        key: str,
    ) -> Any:
        if key in old_meta:
            return old_meta.get(key)
        if key in PAYLOAD_DIGEST_SOURCE_META_KEYS and old_entry.get("source_format") == "clip":
            outer_key = "render_outer_id" if key == "render_payload_sha256" else "mask_outer_id"
            return self._clip_legacy_payload_digest(old_entry, old_meta.get(outer_key))
        if key in {"width", "height"} and old_layer.get("blob_ref"):
            try:
                image = pixels_from_bytes(self.store.read_blob(old_layer["blob_ref"]))
            except Exception:
                return None
            return image.width if key == "width" else image.height
        return None

    def _clip_legacy_payload_digest(self, old_entry: dict[str, Any], outer_id: str | None) -> str | None:
        source_ref = old_entry.get("source_blob_ref")
        if not source_ref or not outer_id:
            return None
        cache = getattr(self, "_clip_legacy_payload_digest_cache", None)
        if cache is None:
            cache = self._clip_legacy_payload_digest_cache = {}
        cache_key = (source_ref, outer_id)
        if cache_key in cache:
            return cache[cache_key]
        try:
            source_cache = getattr(self, "_clip_source_exta_cache", None)
            if source_cache is None:
                source_cache = self._clip_source_exta_cache = {}
            if source_ref not in source_cache:
                raw = self.store.read_blob(source_ref)
                from .formats.clip import parse_container

                source_cache[source_ref] = (raw, parse_container(raw).index_extas(raw))
            raw, extas = source_cache[source_ref]
            from .importers import _clip_normalized_exta_payload_sha256

            chunk = extas.get(outer_id)
            if chunk is None:
                digest = None
            else:
                payload = raw[chunk.payload_off:chunk.payload_off + chunk.payload_size]
                digest = _clip_normalized_exta_payload_sha256(payload)
        except Exception:
            digest = None
        cache[cache_key] = digest
        return digest

    def _write_tmp_source(self, relpath: str, source: bytes, label: str) -> Path:
        path = self._tmp_source_path(relpath, label)
        path.write_bytes(source)
        return path

    def _tmp_source_path(self, relpath: str, label: str) -> Path:
        tmp_dir = self.repo_dir / "tmp" / "stage"
        tmp_dir.mkdir(parents=True, exist_ok=True)
        suffix = Path(relpath).suffix or ".bin"
        digest = hashlib.sha256(f"{relpath}\0{label}\0{datetime.now(timezone.utc).isoformat()}".encode("utf-8")).hexdigest()[:16]
        return tmp_dir / f"{safe_filename(relpath)}-{label}-{digest}{suffix}"

    def _apply_layer_ignore(
        self,
        rel: str,
        entry: dict[str, Any],
        on_progress: ProgressFn | None = None,
    ) -> None:
        """For each `<file>:<layer>` pattern matching `rel`, replace the new
        layer's blob_ref with the HEAD entry's blob_ref so the ignored layer's
        prior state is carried forward unchanged.

        If HEAD has no matching layer (new file, restructured tree) the ignore
        is dropped with a warning so the on-disk file is never silently
        corrupted by a stale reference.
        """
        layer_patterns = _ignore.layer_patterns_for(rel, self.parsed_ignore_patterns())
        if not layer_patterns:
            return
        head_commit = self.current_commit()
        head_entry = self.entries_for_commit(head_commit).get(rel) if head_commit else None
        head_layers_by_sid: dict[str, dict[str, Any]] = {}
        if head_entry:
            for layer in self.flatten_layers(head_entry["tree_ref"]):
                sid = layer.get("stable_source_id")
                if sid:
                    head_layers_by_sid[sid] = layer
        if not head_layers_by_sid:
            for pat in layer_patterns:
                print(
                    f"tmiki: warning: ignore pattern {rel}:{pat.layer_glob!r} skipped "
                    "(no matching prior commit to inherit from)"
                )
            return
        substitutions: dict[str, dict[str, Any]] = {}
        for layer in self.flatten_layers(entry["tree_ref"]):
            name = layer.get("name") or layer.get("path_name") or ""
            sid = layer.get("stable_source_id")
            if not sid:
                continue
            if not _ignore.is_layer_ignored(rel, name, layer_patterns):
                continue
            prior = head_layers_by_sid.get(sid)
            if not prior:
                print(f"tmiki: warning: ignore layer {rel}:{name} skipped (no prior layer with sid {sid[:12]})")
                continue
            # Carry both blob_ref and pixel_sha256 so the invariant (a layer's
            # pixel_sha256 describes its blob_ref) survives ignore substitution.
            substitutions[sid] = {
                "blob_ref": prior.get("blob_ref"),
                "pixel_sha256": prior.get("pixel_sha256"),
            }
        if not substitutions:
            return
        if on_progress:
            on_progress(f"applying .tmiignore layer substitutions ({len(substitutions)})")
        entry["tree_ref"] = self._rewrite_layer_tree(entry["tree_ref"], substitutions)
        entry.setdefault("source_meta", {})["ignored_layer_sids"] = sorted(substitutions)

    def _rewrite_layer_tree(self, tree_ref: str, blob_subs: dict[str, dict[str, Any]]) -> str:
        tree = self.store.read_json(tree_ref, "tree")
        if tree.get("kind") != "layer-tree":
            return tree_ref
        rewritten_layers: list[dict[str, Any]] = []
        changed = False
        for layer in tree.get("layers") or []:
            new_layer = dict(layer)
            sid = new_layer.get("stable_source_id")
            sub = blob_subs.get(sid)
            if sub is not None and (
                new_layer.get("blob_ref") != sub.get("blob_ref")
                or new_layer.get("pixel_sha256") != sub.get("pixel_sha256")
            ):
                new_layer["blob_ref"] = sub.get("blob_ref")
                new_layer["pixel_sha256"] = sub.get("pixel_sha256")
                changed = True
            if new_layer.get("subtree_ref"):
                new_sub = self._rewrite_layer_tree(new_layer["subtree_ref"], blob_subs)
                if new_sub != new_layer["subtree_ref"]:
                    new_layer["subtree_ref"] = new_sub
                    changed = True
            rewritten_layers.append(new_layer)
        if not changed:
            return tree_ref
        return self.store.write_json("tree", {"version": tree.get("version", 1), "kind": "layer-tree", "layers": rewritten_layers})

    def materialize_import(
        self,
        relpath: str,
        imported: ImportedFile,
        on_progress: ProgressFn | None = None,
        blob_reuse: dict[str, dict] | None = None,
        source_ref: str | None = None,
    ) -> dict[str, Any]:
        """``source_ref``, when given, is the blob ``imported.source_bytes``
        is already stored as (``write_large_blobs``)."""
        if on_progress:
            on_progress(f"writing source blob for {relpath}")
        # The source, every layer blob that needs writing and the composite
        # go to the object store in one batch, which spreads their pieces
        # over a thread pool; the tree pass below then only looks refs up.
        new_layers = self._layers_needing_blobs(imported.layers, blob_reuse)
        payloads = [] if source_ref is not None else [imported.source_bytes]
        payloads.extend(layer.blob_bytes for layer in new_layers)
        if imported.composite_png:
            payloads.append(imported.composite_png)
        refs = self.store.write_large_blobs(payloads) if payloads else []
        if source_ref is None:
            source_ref, refs = refs[0], refs[1:]
        layer_refs = {id(layer): ref for layer, ref in zip(new_layers, refs)}
        layer_tree_ref = self._write_layer_tree(
            imported.layers, on_progress=on_progress, path_prefix=relpath, blob_reuse=blob_reuse,
            layer_refs=layer_refs,
        )
        if on_progress:
            on_progress(f"writing thumbnail for {relpath}")
        # Opaque binaries carry no composite/thumbnail; keep the refs null so
        # diff/log skip the visual path for them.
        thumbnail_ref = self.store.write("blob", imported.thumbnail_png) if imported.thumbnail_png else None
        composite_ref = refs[-1] if imported.composite_png else None
        return {
            "path": relpath,
            "source_format": imported.source_format,
            "source_blob_ref": source_ref,
            "source_digest": imported.source_digest,
            "tree_ref": layer_tree_ref,
            "thumbnail_ref": thumbnail_ref,
            "composite_ref": composite_ref,
            "canvas_meta": imported.canvas_meta,
            "source_meta": imported.source_meta,
        }

    def materialize_file_from_path(
        self,
        relpath: str,
        path: Path,
        *,
        blob_reuse: dict[str, dict] | None = None,
        on_progress: ProgressFn | None = None,
        base_entry: dict[str, Any] | None = None,
        source: bytes | None = None,
    ) -> dict[str, Any]:
        """Import ``path`` and store it as the entry for ``relpath``.

        ``blob_reuse`` (a ``_layer_blob_reuse``-shaped map) lets unchanged
        layers skip decode/encode exactly as ``stage_file`` does.
        ``base_entry`` is the version ``path`` was derived from; its composite
        is the starting point of an incremental composite. ``source`` may
        pass the bytes the caller just wrote to ``path``."""
        reuse = blob_reuse or {}
        imported = import_file(
            path,
            cache=self.render_cache,
            reuse_pixels={sid: d["pixel_sha256"] for sid, d in reuse.items()} or None,
            reuse_decoded=_reuse_decoded(reuse),
            on_progress=on_progress,
            composite_base=self._composite_base(base_entry),
            source=source,
        )
        entry = self.materialize_import(relpath, imported, on_progress=on_progress, blob_reuse=reuse or None)
        entry["path"] = relpath
        return entry

    def _write_layer_tree(
        self,
        layers: list[ImportedLayer],
        on_progress: ProgressFn | None = None,
        path_prefix: str = "",
        blob_reuse: dict[str, dict] | None = None,
        layer_refs: dict[int, str] | None = None,
    ) -> str:
        """``layer_refs`` maps ``id(layer)`` to the blob already written for
        that layer's ``blob_bytes`` (see ``_layers_needing_blobs``)."""
        entries = []
        for layer in layers:
            if on_progress:
                label = f"{path_prefix}:{layer.name}" if path_prefix else layer.name
                on_progress(f"writing layer {label}")
            reuse = (blob_reuse or {}).get(layer.stable_source_id)
            if _reuses_blob(reuse, layer):
                blob_ref = reuse.get("blob_ref")
            elif layer.blob_bytes is not None:
                blob_ref = (layer_refs or {}).get(id(layer)) or self.store.write_large_blob(layer.blob_bytes)
            elif layer.pixel_sha256 is not None:
                raise RuntimeError(
                    f"layer {layer.stable_source_id!r} had its pixel encode skipped "
                    "but no reusable blob_ref was found (pixel_sha256 set, blob_bytes None)"
                )
            else:
                blob_ref = None
            subtree_ref = (
                self._write_layer_tree(
                    layer.children,
                    on_progress=on_progress,
                    path_prefix=path_prefix,
                    blob_reuse=blob_reuse,
                    layer_refs=layer_refs,
                )
                if layer.children
                else None
            )
            entries.append({
                "layer_id": layer.layer_id,
                "stable_source_id": layer.stable_source_id,
                "name": layer.name,
                "kind": layer.kind,
                "visible": layer.visible,
                "opacity": layer.opacity,
                "blend_mode": layer.blend_mode,
                "clip_to_below": layer.clip_to_below,
                "offset_x": layer.offset_x,
                "offset_y": layer.offset_y,
                "mask_ref": layer.mask_ref,
                "blob_ref": blob_ref,
                "subtree_ref": subtree_ref,
                "pixel_bbox": layer.pixel_bbox,
                "pixel_sha256": layer.pixel_sha256,
                "source_meta": layer.source_meta,
            })
        return self.store.write_json("tree", {"version": 1, "kind": "layer-tree", "layers": entries})

    @staticmethod
    def _layers_needing_blobs(
        layers: list[ImportedLayer],
        blob_reuse: dict[str, dict] | None,
    ) -> list[ImportedLayer]:
        """The layers whose ``blob_bytes`` ``_write_layer_tree`` would store."""
        out: list[ImportedLayer] = []
        for layer in walk_imported_layers(layers):
            if layer.blob_bytes is not None and not _reuses_blob((blob_reuse or {}).get(layer.stable_source_id), layer):
                out.append(layer)
        return out

    def stage_all_modified(self, on_progress: ProgressFn | None = None) -> list[str]:
        """Stage every tracked file whose worktree content differs from HEAD.

        Returns the list of paths that were staged.
        """
        conflicts = self.pending_conflict_paths()
        if conflicts:
            summary = ", ".join(conflicts[:5])
            if len(conflicts) > 5:
                summary += f", +{len(conflicts) - 5} more"
            raise RuntimeError(f"unresolved merge conflicts: {summary}")
        head_entries = self.entries_for_commit(self.current_commit())
        index_entries = self.read_index().get("entries", {})
        root = self.require_worktree()
        # Share the stat cache with status: an unchanged file hashed by a prior
        # `tmiki status` is skipped here, and vice versa.
        cache = _StatCache(self)
        staged: list[str] = []
        # Walk HEAD ∪ index, not just HEAD: staged-new files must be re-staged
        # when the worktree moved on (or their deletion staged when removed),
        # and a stale staged entry must be dropped when the worktree was
        # reverted — git's `commit -a` re-stages every *known* file.
        for rel in sorted(set(head_entries) | set(index_entries)):
            full = root / Path(rel)
            index_entry = index_entries.get(rel)
            index_deleted = index_entry is not None and _is_deleted_index_entry(index_entry)
            try:
                st = full.stat()
            except OSError:
                if not index_deleted:
                    self.stage_delete(rel)
                    staged.append(rel)
                continue
            # What would currently be committed for this path: the staged entry
            # if present, else HEAD's entry. A delete marker with the file back
            # on disk has no target and always re-stages.
            target = None if index_deleted else (index_entry or head_entries.get(rel))
            source = None
            if target is not None:
                modified, source = cache.changed(rel, full, target.get("source_digest"), st=st)
                if not modified:
                    continue
            # Pass the absolute path: a bare root-relative string would be
            # re-resolved against the *cwd* first and can hit the wrong file
            # when running from a subdirectory. The import takes the bytes
            # just hashed, if any, instead of reading and hashing them again.
            _rel, _layer, changed = self.stage_file(str(full), on_progress=on_progress, force=True, source=source)
            source = None
            if changed:
                staged.append(rel)
        cache.flush()
        return staged

    def commit_amend(self, message: str | None = None) -> str:
        """Replace HEAD with a new commit. Parents and (by default) message are preserved.

        Currently staged entries (if any) override the previous tree's entries
        on a per-path basis, matching Git's `commit --amend` behaviour.
        """
        if self.pending_merge_head():
            raise RuntimeError("cannot amend during a merge; finish the merge with 'tmiki commit' or discard it")
        current = self.current_commit()
        if not current:
            raise RuntimeError("nothing to amend (no commits yet)")
        previous = self.read_commit(current)
        index = self.read_index()
        staged = {
            path: self._committable_entry(entry)
            for path, entry in index.get("entries", {}).items()
            if not _is_deleted_index_entry(entry)
        }
        deleted = {
            path
            for path, entry in index.get("entries", {}).items()
            if _is_deleted_index_entry(entry)
        }
        entries = self.entries_for_commit(current)
        entries.update(staged)
        for path in deleted:
            entries.pop(path, None)
        new_message = message if message is not None else previous.get("message", "")
        from . import attributes as _attributes

        _attributes.refuse_dev_records(entries.items())
        # Amend creates a commit like any other; pre-commit must get its veto
        # (git runs pre-/post-commit for --amend too).
        _hooks.run(self.repo_dir, "pre-commit", extra_env={"TMI_COMMIT_MSG": new_message})
        tree_ref = self.write_repository_tree(entries)
        thumbnail_ref = (
            next((e.get("thumbnail_ref") for e in staged.values() if e.get("thumbnail_ref")), None)
            or next((e.get("thumbnail_ref") for e in entries.values() if e.get("thumbnail_ref")), None)
        )
        commit = {
            "version": 1,
            "tree_ref": tree_ref,
            "parents": list(previous.get("parents") or []),
            "author": self._author(),
            "timestamp": _object_timestamp(),
            "message": new_message,
            "thumbnail_ref": thumbnail_ref,
            "canvas_meta": next((e.get("canvas_meta") for e in entries.values()), {}),
        }
        oid = self.store.write_json("commit", commit)
        self.set_current_commit(oid, expected=current)
        self.write_index({"version": 1, "entries": {}})
        first_line = (new_message.splitlines() or [""])[0]
        reason = f"commit (amend): {first_line}"
        self._log_ref_move("HEAD", current, oid, reason)
        target = self.head_target()
        if target.startswith("refs/heads/"):
            self._log_ref_move(target, current, oid, reason)
        _hooks.run(
            self.repo_dir,
            "post-commit",
            extra_env={"TMI_COMMIT": oid, "TMI_COMMIT_MSG": new_message},
            fatal=False,
        )
        self._emit_plugin_event("post-commit", {"commit": oid, "message": new_message})
        return oid

    def commit(self, message: str, allow_empty: bool = False) -> str:
        index = self.read_index()
        staged = {
            path: self._committable_entry(entry)
            for path, entry in index.get("entries", {}).items()
            if not _is_deleted_index_entry(entry)
        }
        deleted = {
            path
            for path, entry in index.get("entries", {}).items()
            if _is_deleted_index_entry(entry)
        }
        current = self.current_commit()
        merge_other = self.pending_merge_head()
        if not staged and not deleted and not allow_empty and not merge_other:
            raise RuntimeError("nothing to commit")
        if merge_other:
            conflicts = self.pending_conflict_paths()
            if conflicts:
                summary = ", ".join(conflicts[:5])
                if len(conflicts) > 5:
                    summary += f", +{len(conflicts) - 5} more"
                raise RuntimeError(f"unresolved merge conflicts: {summary}")
        entries = self.entries_for_commit(current)
        before_entries = dict(entries)
        entries.update(staged)
        for path in deleted:
            entries.pop(path, None)
        if _entries_equal(entries, before_entries) and not allow_empty and not merge_other:
            raise RuntimeError("nothing to commit")
        from . import attributes as _attributes

        _attributes.refuse_dev_records(entries.items())
        _hooks.run(self.repo_dir, "pre-commit", extra_env={"TMI_COMMIT_MSG": message})
        # Like git, index changes made by the pre-commit hook (auto-fixups
        # staged via `tmiki add`) belong in this commit: recompute from the live
        # index rather than the pre-hook snapshot.
        index = self.read_index()
        staged = {
            path: self._committable_entry(entry)
            for path, entry in index.get("entries", {}).items()
            if not _is_deleted_index_entry(entry)
        }
        deleted = {
            path
            for path, entry in index.get("entries", {}).items()
            if _is_deleted_index_entry(entry)
        }
        entries = self.entries_for_commit(current)
        entries.update(staged)
        for path in deleted:
            entries.pop(path, None)
        if _entries_equal(entries, before_entries) and not allow_empty and not merge_other:
            # A hook may have unstaged everything.
            raise RuntimeError("nothing to commit")
        _attributes.refuse_dev_records(entries.items())
        tree_ref = self.write_repository_tree(entries)
        parents = [current] if current else []
        if merge_other and merge_other not in parents:
            parents.append(merge_other)
        thumbnail_ref = next((entry.get("thumbnail_ref") for entry in staged.values() if entry.get("thumbnail_ref")), None)
        if not thumbnail_ref:
            thumbnail_ref = next((entry.get("thumbnail_ref") for entry in entries.values() if entry.get("thumbnail_ref")), None)
        commit = {
            "version": 1,
            "tree_ref": tree_ref,
            "parents": parents,
            "author": self._author(),
            "timestamp": _object_timestamp(),
            "message": message,
            "thumbnail_ref": thumbnail_ref,
            "canvas_meta": next((entry.get("canvas_meta") for entry in entries.values()), {}),
        }
        oid = self.store.write_json("commit", commit)
        self.set_current_commit(oid, expected=current)
        self.write_index({"version": 1, "entries": {}})
        first_line = (message.splitlines() or [""])[0]
        reason = f"merge: {first_line}" if merge_other else f"commit: {first_line}"
        self._log_ref_move("HEAD", current, oid, reason)
        target = self.head_target()
        if target.startswith("refs/heads/"):
            self._log_ref_move(target, current, oid, reason)
        if merge_other:
            self.clear_pending_merge()
            self.clear_all_conflict_files()
        _hooks.run(
            self.repo_dir,
            "post-commit",
            extra_env={"TMI_COMMIT": oid, "TMI_COMMIT_MSG": message},
            fatal=False,
        )
        self._emit_plugin_event("post-commit", {"commit": oid, "message": message})
        return oid

    def _committable_entry(self, entry: dict[str, Any]) -> dict[str, Any]:
        cleaned = dict(entry)
        source_meta = dict(cleaned.get("source_meta") or {})
        for key in INDEX_ONLY_SOURCE_META_KEYS:
            source_meta.pop(key, None)
        cleaned["source_meta"] = source_meta
        return cleaned

    def write_repository_tree(self, entries: dict[str, dict[str, Any]]) -> str:
        ordered = [entries[key] for key in sorted(entries)]
        return self.store.write_json("tree", {"version": 1, "kind": "repository", "entries": ordered})

    def entries_for_commit(self, commit: str | None) -> dict[str, dict[str, Any]]:
        if not commit:
            return {}
        obj = self.read_commit(commit)
        tree = self.store.read_json(obj["tree_ref"], "tree")
        if tree.get("kind") != "repository":
            raise ValueError(f"commit tree is not a repository tree: {obj['tree_ref']}")
        return {entry["path"]: entry for entry in tree.get("entries", [])}

    def entries_for_checkout(self, commit: str | None) -> dict[str, dict[str, Any]]:
        """The entries of ``commit`` that are about to be written to the
        working tree, each path checked against ``recpath``'s rule first (a
        RuntimeError before any ref moves or file is touched: a tree can hold
        what tmi's own commands never record)."""
        entries = self.entries_for_commit(commit)
        for rel in entries:
            _recpath.check_record_path(rel)
        return entries

    def read_commit(self, commit: str) -> dict[str, Any]:
        return self.store.read_json(commit, "commit")

    def iter_commits(self, start: str | None = None) -> list[tuple[str, dict[str, Any]]]:
        head = self.resolve_rev(start) if start else self.current_commit()
        if not head:
            return []
        collected: dict[str, dict[str, Any]] = {}
        stack = [head]
        while stack:
            cur = stack.pop()
            if cur in collected:
                continue
            commit = self.read_commit(cur)
            collected[cur] = commit
            for parent in commit.get("parents") or []:
                if parent and parent not in collected:
                    stack.append(parent)
        return sorted(
            collected.items(),
            key=lambda kv: kv[1].get("timestamp", ""),
            reverse=True,
        )

    def status_items(
        self,
        on_progress: ProgressFn | None = None,
        *,
        with_layer_detail: bool = True,
    ) -> list[StatusItem]:
        root = self.require_worktree()
        if on_progress:
            on_progress("reading index")
        index_entries = self.read_index().get("entries", {})
        if on_progress:
            on_progress("reading HEAD tree")
        head_entries = self.entries_for_commit(self.current_commit())
        items: list[StatusItem] = []
        cache = _StatCache(self)

        for rel, entry in sorted(index_entries.items()):
            if _is_deleted_index_entry(entry):
                items.append(StatusItem("staged", rel, "delete"))
                continue
            detail = entry.get("source_meta", {}).get("staged_layer_selector")
            items.append(StatusItem("staged", rel, f"layer {detail}" if detail else ""))
            # A staged path can be edited (or deleted) again afterwards; git
            # shows that as a second, unstaged change (MM / AD). Compare the
            # worktree against the *staged* entry, not HEAD.
            full = root / Path(rel)
            try:
                st = full.stat()
            except OSError:
                items.append(StatusItem("deleted", rel))
                continue
            modified, source = cache.changed(rel, full, entry.get("source_digest"), st=st, want_bytes=with_layer_detail)
            if not modified:
                continue
            source_bytes, source_digest = source or (None, None)
            source = None
            unstaged_detail = ""
            semantically_same = False
            if with_layer_detail and is_supported_path(full):
                try:
                    imported = self._import_to_compare(full, entry, source_bytes, source_digest)
                    changes = self.layer_changes(entry, imported)
                    if not changes and entry.get("canvas_meta") == imported.canvas_meta:
                        # Partial layer staging re-encodes the source, so the
                        # staged bytes rarely equal the worktree bytes even
                        # when every semantic change is staged. Suppress the
                        # noise when the layer model matches exactly.
                        semantically_same = True
                    else:
                        unstaged_detail = "; ".join(changes[:6])
                        if len(changes) > 6:
                            unstaged_detail += f"; +{len(changes) - 6} more"
                except Exception as exc:
                    unstaged_detail = f"layer scan failed: {exc}"
            if not semantically_same:
                items.append(StatusItem("modified", rel, unstaged_detail))

        tracked_total = len(head_entries)
        for idx, (rel, entry) in enumerate(sorted(head_entries.items()), start=1):
            if rel in index_entries:
                continue
            full = root / Path(rel)
            try:
                st = full.stat()
            except OSError:
                items.append(StatusItem("deleted", rel))
                continue
            if on_progress:
                on_progress(f"hashing [{idx}/{tracked_total}] {rel}")
            modified, source = cache.changed(rel, full, entry.get("source_digest"), st=st, want_bytes=with_layer_detail)
            if modified:
                source_bytes, source_digest = source or (None, None)
                source = None
                detail = ""
                if with_layer_detail and is_supported_path(full):
                    try:
                        if on_progress:
                            on_progress(f"scanning layers [{idx}/{tracked_total}] {rel}")
                        imported = self._import_to_compare(full, entry, source_bytes, source_digest)
                        changes = self.layer_changes(entry, imported)
                        detail = "; ".join(changes[:6])
                        if len(changes) > 6:
                            detail += f"; +{len(changes) - 6} more"
                    except Exception as exc:
                        detail = f"layer scan failed: {exc}"
                items.append(StatusItem("modified", rel, detail))

        if on_progress:
            on_progress("scanning working tree for untracked files")
        ignored = self.parsed_ignore_patterns()
        tracked = set(head_entries) | set(index_entries)
        # (links are left out: a link would list its target a second time)
        for full in sorted(iter_supported_files(root, ignored, [])):
            rel = self.relpath(full)
            if rel not in tracked:
                items.append(StatusItem("untracked", rel))
        cache.flush()
        return items

    def _import_to_compare(
        self,
        full: Path,
        entry: dict[str, Any],
        source_bytes: bytes | None,
        source_digest: str | None,
    ) -> ImportedFile:
        """Import working file ``full`` just far enough for
        ``layer_changes(entry, ...)``, which compares pixel digests: no
        composite preview, layers whose pixels ``entry`` already holds are not
        decoded, and no layer is PNG-encoded except those ``layer_changes``
        may compare by blob (``entry`` layers without a stored pixel digest).
        """
        reuse = self._layer_blob_reuse(entry)
        legacy = frozenset(
            layer["stable_source_id"]
            for layer in (self.flatten_layers(entry["tree_ref"]) if entry.get("tree_ref") else [])
            if layer.get("pixel_sha256") is None and layer.get("stable_source_id")
        )
        return import_file(
            full,
            render_composite=False,
            reuse_pixels={sid: d["pixel_sha256"] for sid, d in reuse.items()} or None,
            reuse_decoded=_reuse_decoded(reuse),
            source=source_bytes,
            source_digest=source_digest,
            layer_blobs=legacy,
        )

    def layer_changes(self, old_entry: dict[str, Any], imported: ImportedFile) -> list[str]:
        """One line per added / changed / deleted layer. A changed layer lists
        what changed in readable form (``changed layer 影 (pixels, opacity:
        100% -> 50%)``), see :mod:`tmi.layer_props`."""
        from .layer_props import RASTER_LAYER_ID, format_change, layer_props, property_changes, walk_imported, walk_tree

        old_layers = {layer["stable_source_id"]: (layer, parent) for layer, parent in walk_tree(self, old_entry["tree_ref"])}
        new_layers = {layer.stable_source_id: (layer, parent) for layer, parent in walk_imported(imported.layers)}
        changes: list[str] = []
        for sid, (layer, new_parent) in sorted(new_layers.items(), key=lambda item: item[1][0].name):
            if sid not in old_layers:
                changes.append(f"added layer {layer.name}")
                continue
            old, old_parent = old_layers[sid]
            old_pixel = old.get("pixel_sha256")
            if old_pixel is None and old.get("blob_ref"):
                old_pixel = self._blob_pixel_digest(old["blob_ref"])
            if old_pixel is not None and layer.pixel_sha256 is not None:
                pixels_changed = old_pixel != layer.pixel_sha256
            else:
                # Legacy fallback: no pixel digest available on one side. Compare
                # the encoded blob object ids (requires the new blob bytes).
                old_blob = old.get("blob_ref")
                new_blob = (
                    self.store.object_id("blob", layer.blob_bytes)
                    if layer.blob_bytes is not None
                    else None
                )
                pixels_changed = old_blob != new_blob
            prop_changed = any(
                old.get(key) != getattr(layer, attr)
                for key, attr in [
                    ("visible", "visible"),
                    ("opacity", "opacity"),
                    ("blend_mode", "blend_mode"),
                    ("clip_to_below", "clip_to_below"),
                    ("offset_x", "offset_x"),
                    ("offset_y", "offset_y"),
                ]
            )
            metadata_changed = self._layer_metadata_changed(old_entry, old, layer)
            # A property change nothing readable explains is "other
            # properties"; with a pixel edit, pixel-storage fields change too.
            props = property_changes(
                layer_props(old, old_parent),
                layer_props(layer, new_parent),
                metadata_changed=(prop_changed or metadata_changed) and not pixels_changed,
            )
            if pixels_changed or props:
                # The per-layer pixel diff bbox was dropped from status: it forced
                # decoding both the old and new PNG for every changed layer. The
                # layer-level "changed/added/deleted" verdict is what status needs.
                items = (["pixels"] if pixels_changed and props else []) + [format_change(*c) for c in props]
                detail = f" ({', '.join(items)})" if items else ""
                if sid == RASTER_LAYER_ID and not props:
                    changes.append("changed pixels")  # a PNG/JPEG has no layers
                else:
                    changes.append(f"changed layer {layer.name}{detail}")
        for sid, (old, _parent) in sorted(old_layers.items(), key=lambda item: item[1][0].get("name", "")):
            if sid not in new_layers:
                changes.append(f"deleted layer {old.get('name', sid)}")
        return changes

    def _layer_metadata_changed(
        self,
        old_entry: dict[str, Any],
        old_layer: dict[str, Any],
        new_layer: ImportedLayer,
    ) -> bool:
        for key in PARTIAL_STAGE_LAYER_FIELDS:
            if old_layer.get(key) != getattr(new_layer, key):
                return True
        old_meta = old_layer.get("source_meta") or {}
        new_meta = new_layer.source_meta or {}
        if old_entry.get("source_format") != "clip":
            return old_meta.get("semantic_digest") != new_meta.get("semantic_digest")
        for key in PARTIAL_STAGE_SOURCE_META_KEYS:
            if key not in old_meta and key not in new_meta:
                continue
            old_value = self._old_layer_source_meta_value(old_entry, old_layer, old_meta, key)
            new_value = new_meta.get(key)
            if key in PAYLOAD_DIGEST_SOURCE_META_KEYS and key not in old_meta and (old_value is None or new_value is None):
                # Legacy CLIP commits did not store normalized Exta payload
                # digests. Falling back to the old semantic digest would report
                # every layer whose storage id was rewritten as changed.
                continue
            if old_value != new_value:
                return True
        return False

    def flatten_layers(self, tree_ref: str, prefix: str = "") -> list[dict[str, Any]]:
        tree = self.store.read_json(tree_ref, "tree")
        layers: list[dict[str, Any]] = []
        for layer in tree.get("layers", []):
            item = dict(layer)
            item["path_name"] = f"{prefix}/{layer['name']}" if prefix else layer["name"]
            layers.append(item)
            if layer.get("subtree_ref"):
                layers.extend(self.flatten_layers(layer["subtree_ref"], item["path_name"]))
        return layers

    def _composite_base(self, entry: dict[str, Any] | None) -> Any | None:
        """An earlier version of a CLIP file (index / commit entry) as the
        starting point of an incremental layered composite, or None. Whether
        its composite can serve is decided from the blobs, only if the new
        file needs a layered render at all."""
        if not entry or _is_deleted_index_entry(entry) or entry.get("source_format") != "clip":
            return None
        source_ref, composite_ref = entry.get("source_blob_ref"), entry.get("composite_ref")
        if not source_ref or not composite_ref:
            return None
        from .formats.clip_partial import CompositeBase

        store = self.store
        return CompositeBase(lambda: store.read_blob(source_ref), lambda: store.read_blob(composite_ref))

    def _layer_blob_reuse(self, entry: dict[str, Any] | None) -> dict[str, dict]:
        """Map ``stable_source_id -> {pixel_sha256, blob_ref, pixel_bbox, pixel_source}``.

        Used to let a re-import reuse already-stored blobs for unchanged layers
        instead of re-encoding/re-storing them — and, when ``pixel_source`` still
        matches, skip decoding them at all. Only layers carrying BOTH a pixel
        digest and a blob ref are included (the reuse decision needs both).
        A None/deleted-marker entry yields an empty map.
        """
        if not entry or _is_deleted_index_entry(entry) or not entry.get("tree_ref"):
            return {}
        out: dict[str, dict] = {}
        for layer in self.flatten_layers(entry["tree_ref"]):
            sid = layer.get("stable_source_id")
            pixel_sha = layer.get("pixel_sha256")
            blob_ref = layer.get("blob_ref")
            if sid and pixel_sha is not None and blob_ref is not None:
                out[sid] = {
                    "pixel_sha256": pixel_sha,
                    "blob_ref": blob_ref,
                    "pixel_bbox": layer.get("pixel_bbox"),
                    "pixel_source": (layer.get("source_meta") or {}).get("pixel_source"),
                }
        return out

    def _blob_pixel_digest(self, blob_ref: str | None) -> str | None:
        """Decode a stored layer blob and return its pixel digest (memoized).

        Lets the legacy change-detection path compare pixels for commits written
        before ``pixel_sha256`` existed. Any decode/read failure yields None so
        the caller can fall back to the object-id comparison.
        """
        if not blob_ref:
            return None
        cache = getattr(self, "_blob_pixel_digest_cache", None)
        if cache is None:
            cache = self._blob_pixel_digest_cache = {}
        if blob_ref in cache:
            return cache[blob_ref]
        digest: str | None = None
        try:
            from .image_ops import layer_blob_digest

            digest = layer_blob_digest(self.store.read_blob(blob_ref))
        except Exception:
            digest = None
        cache[blob_ref] = digest
        return digest

    def ignore_patterns(self) -> list[str]:
        return _ignore.file_globs(self.parsed_ignore_patterns())

    def parsed_ignore_patterns(self) -> list[_ignore.IgnorePattern]:
        baseline = _ignore.parse_lines([
            ".tmi/*",
            ".git/*",
            ".tmi-diff/*",
            ".*-smoke-*/*",
            "__pycache__/*",
            "*.pyc",
            "*.pyo",
        ])
        if self.worktree:
            user = _ignore.parse_file(self.worktree / ".tmiignore")
            return baseline + user
        return baseline

    def restore(
        self,
        spec: str,
        source: str | None = None,
        in_place: bool = False,
        output: Path | None = None,
        operation: str = "restore",
    ) -> Path:
        """``operation`` names the command in the backup reason (``checkout``
        restores paths through here too)."""
        if source == WORKTREE_SOURCE:
            return self._export_from_worktree(spec, in_place=in_place, output=output)
        commit = self.resolve_rev(source)
        if not commit:
            raise RuntimeError("no commit to restore from")
        file_part, layer_selector = split_pathspec(spec)
        entries = self.entries_for_commit(commit)
        label = commit[:12]
        if source is None:
            # Like `git restore <path>`: without --source the content comes
            # from the index, so restoring a staged file brings back the staged
            # bytes, not HEAD's.
            for path, entry in self.read_index().get("entries", {}).items():
                if _is_deleted_index_entry(entry):
                    entries.pop(path, None)
                else:
                    entries[path] = entry
            label = "index"
        rel = self.pathspec_relpath(file_part, set(entries))
        if rel not in entries:
            raise RuntimeError(f"path not found in {label}: {rel}")
        entry = entries[rel]
        if layer_selector:
            layer = self.find_committed_layer(entry, layer_selector)
            if not layer:
                raise RuntimeError(f"layer not found in {rel}: {layer_selector}")
            if not layer.get("blob_ref"):
                if layer.get("subtree_ref") and not in_place:
                    return self._export_folder(rel, entry, layer, output)
                raise RuntimeError(f"layer has no restorable pixel blob: {layer_selector}")
            png_bytes = self.store.read_blob(layer["blob_ref"])
            # Target the native write-back by the layer's stable id (CLIP uuid /
            # mdp2:<id>) rather than the raw user string: a bare name can match
            # several layers and would otherwise overwrite the wrong one.
            native_selector = layer.get("stable_source_id") or layer_selector
            if in_place:
                out = self.abspath(rel)
                out.parent.mkdir(parents=True, exist_ok=True)
                committed_source = self.store.read_blob(entry["source_blob_ref"])
                if not out.exists():
                    out.write_bytes(committed_source)
                invalidate_preview_on_noop = out.read_bytes() != committed_source
                native_bytes = replace_layer_pixels(
                    out,
                    entry.get("source_format", ""),
                    native_selector,
                    png_bytes,
                    invalidate_preview_on_noop=invalidate_preview_on_noop,
                )
                tmp = out.with_suffix(out.suffix + ".tmp")
                tmp.write_bytes(native_bytes)
                tmp.replace(out)
                return self._rewrote(out)
            out = output or self.abspath(rel).with_name(f"{Path(rel).stem}__{safe_filename(layer['name'])}.png")
            out.parent.mkdir(parents=True, exist_ok=True)
            out.write_bytes(layer_blob_to_png(png_bytes))
            return out
        if output is not None and _whole_file_output(rel, output) == "png":
            # The file's image, not its bytes, when the name asks for a PNG
            # (issue #30): the composite stored with it.
            if not entry.get("composite_ref"):
                raise RuntimeError(_no_image_message(rel))
            output.parent.mkdir(parents=True, exist_ok=True)
            output.write_bytes(self.store.read_blob(entry["composite_ref"]))
            return output
        out = output or self.abspath(rel)
        out.parent.mkdir(parents=True, exist_ok=True)
        if output is None:
            self.backup_worktree_file(rel, f"{operation} {rel}", known_digests={entry.get("source_digest")})
        out.write_bytes(self.store.read_blob(entry["source_blob_ref"]))
        return self._rewrote(out) if output is None else out

    def _export_from_worktree(self, spec: str, *, in_place: bool, output: Path | None) -> Path:
        """``restore --source WORKTREE --output <png> <file>:<layer>``: export
        a layer (or a folder's composite) of the working file as it is now,
        changes not yet added included (issue #15)."""
        if in_place or output is None:
            raise RuntimeError(
                f"--source {WORKTREE_SOURCE} only exports from the working file; use it with --output <file>"
            )
        file_part, layer_selector = split_pathspec(spec)
        full = self.pathspec_full_path(file_part)
        if not full.is_file():
            raise RuntimeError(f"no such file in the working tree: {file_part}")
        if not layer_selector and _whole_file_output(file_part, output) == "png":
            imported = import_file(full, cache=self.render_cache)
            if not imported.composite_png:
                raise RuntimeError(_no_image_message(file_part))
            output.parent.mkdir(parents=True, exist_ok=True)
            output.write_bytes(imported.composite_png)
            return output
        output.parent.mkdir(parents=True, exist_ok=True)
        if not layer_selector:
            output.write_bytes(full.read_bytes())
            return output
        rel = self.relpath(full)
        # Layers whose pixels a stored version already has are not decoded.
        blob_reuse = self._layer_blob_reuse(self.entries_for_commit(self.current_commit()).get(rel))
        blob_reuse.update(self._layer_blob_reuse(self.read_index().get("entries", {}).get(rel)))
        imported = import_file(
            full,
            render_composite=False,
            reuse_pixels={sid: d["pixel_sha256"] for sid, d in blob_reuse.items()} or None,
            reuse_decoded=_reuse_decoded(blob_reuse),
        )
        try:
            layer = find_layer(imported.layers, layer_selector)
        except ValueError as exc:
            raise RuntimeError(str(exc)) from None
        if layer is None:
            raise RuntimeError(f"layer not found in {file_part}: {layer_selector}")
        if layer.children or layer.kind == "folder":
            from .image_ops import image_to_png_bytes
            from .native import render_folder

            image = render_folder(full, imported.source_format, layer.stable_source_id or layer.name)
            output.write_bytes(image_to_png_bytes(image))
            return output
        blob = layer.blob_bytes
        reused = blob_reuse.get(layer.stable_source_id)
        if blob is None and reused and layer.pixel_sha256 and reused.get("pixel_sha256") == layer.pixel_sha256:
            blob = self.store.read_blob(reused["blob_ref"])
        if blob is None:
            raise RuntimeError(f"layer has no restorable pixel blob: {layer_selector}")
        output.write_bytes(layer_blob_to_png(blob))
        return output

    def _export_folder(self, rel: str, entry: dict[str, Any], layer: dict[str, Any], output: Path | None) -> Path:
        """Write a folder's composited contents as a PNG."""
        from .image_ops import image_to_png_bytes
        from .native import render_folder

        tmp_dir = self.repo_dir / "tmp" / "export"
        tmp_dir.mkdir(parents=True, exist_ok=True)
        source = tmp_dir / f"{safe_filename(Path(rel).stem)}{Path(rel).suffix}"
        source.write_bytes(self.store.read_blob(entry["source_blob_ref"]))
        try:
            image = render_folder(source, entry.get("source_format", ""), layer.get("stable_source_id") or layer["name"])
        finally:
            source.unlink(missing_ok=True)
        out = output or self.abspath(rel).with_name(f"{Path(rel).stem}__{safe_filename(layer['name'])}.png")
        out.parent.mkdir(parents=True, exist_ok=True)
        out.write_bytes(image_to_png_bytes(image))
        return out

    def restore_all(self, commit: str, *, remove_missing_from: str | None = None, reason: str) -> list[Path]:
        """Make the working tree match ``commit`` (removing files that only
        ``remove_missing_from`` had). A file holding work that no commit or
        stash has is backed up first (see ``backup_worktree_file``), under
        ``reason`` — the operation, e.g. "switch --discard main" — so
        ``recover --files`` can tell what overwrote it. Bytes that only the
        index has do not count as safe: callers either run on a clean index
        or reset it, which leaves those blobs to gc."""
        target_entries = self.entries_for_checkout(commit)
        old_entries = self.entries_for_commit(remove_missing_from) if remove_missing_from else {}
        head_entries = self.entries_for_commit(self.current_commit())
        restored: list[Path] = []
        # nothing is written or deleted before every path is known to be safe
        for rel in sorted(set(old_entries) - set(target_entries)):
            self.abspath(rel)
        for rel in target_entries:
            self.abspath(rel)

        def known(rel: str) -> set[str | None]:
            return {
                (entries.get(rel) or {}).get("source_digest")
                for entries in (target_entries, old_entries, head_entries)
            }

        for rel in sorted(set(old_entries) - set(target_entries), reverse=True):
            out = self.abspath(rel)
            if out.exists() and out.is_file():
                self.backup_worktree_file(rel, reason, known_digests=known(rel), check_head=False)
                out.unlink()
                self._remove_empty_parent_dirs(out.parent)
                restored.append(out)
        for rel, entry in target_entries.items():
            out = self.abspath(rel)
            if out.is_file() and self.backup_worktree_file(rel, reason, known_digests=known(rel), check_head=False) == entry.get("source_digest"):
                continue  # already identical: no 60 MB rewrite
            out.parent.mkdir(parents=True, exist_ok=True)
            out.write_bytes(self.store.read_blob(entry["source_blob_ref"]))
            restored.append(self._rewrote(out))
        return restored

    # -------- automatic backups of overwritten work --------

    @property
    def backups_path(self) -> Path:
        return self.repo_dir / "backups.json"

    def read_backups(self) -> list[dict[str, Any]]:
        try:
            data = json.loads(self.backups_path.read_text(encoding="utf-8"))
        except (FileNotFoundError, ValueError):
            return []
        return [item for item in data if isinstance(item, dict)] if isinstance(data, list) else []

    def backup_worktree_file(
        self,
        rel: str,
        reason: str,
        *,
        known_digests: set[str | None] = frozenset(),
        check_head: bool = True,
    ) -> str | None:
        """Before tmi overwrites or deletes a working-tree file, keep a copy of
        it unless its bytes are already safe in HEAD, the index or
        ``known_digests``. Returns the file's digest (None if absent).

        Illustration work saved but not committed is hours of drawing; an
        overwrite by restore / switch --discard / reset --hard / merge --abort
        is recoverable with ``tmiki recover --files``."""
        out = self.abspath(rel)
        if not out.is_file():
            return None
        digest = sha256_file(out)
        safe = set(known_digests)
        if check_head:
            safe.add((self.entries_for_commit(self.current_commit()).get(rel) or {}).get("source_digest"))
            safe.add((self.read_index().get("entries", {}).get(rel) or {}).get("source_digest"))
        if digest in safe:
            return digest
        oid = self.store.write_large_blob(out.read_bytes())
        backups = self.read_backups()
        backups.insert(0, {
            "id": datetime.now(timezone.utc).strftime("%Y%m%d%H%M%S%f"),
            "path": rel,
            "blob": oid,
            "digest": digest,
            "size": out.stat().st_size,
            "reason": reason,
            "timestamp": datetime.now(timezone.utc).isoformat(),
        })
        _atomic_write_bytes(
            self.backups_path,
            (json.dumps(backups, ensure_ascii=False, indent=2) + "\n").encode("utf-8"),
        )
        return digest

    def restore_backup(self, spec: str, output: Path | None = None) -> tuple[dict[str, Any], Path]:
        """Write backup ``spec`` (its index in ``read_backups()`` or id prefix)
        to ``output`` or back to its path (backing up what is there now)."""
        backups = self.read_backups()
        found = None
        if spec.isdigit() and int(spec) < len(backups):
            found = backups[int(spec)]
        else:
            matches = [b for b in backups if str(b.get("id", "")).startswith(spec)]
            found = matches[0] if len(matches) == 1 else None
        if not found:
            raise RuntimeError(f"no such backup: {spec}")
        out = output or self.abspath(found["path"])
        if output is None:
            self.backup_worktree_file(found["path"], f"recover {found['id']}", known_digests={found.get("digest")})
        out.parent.mkdir(parents=True, exist_ok=True)
        out.write_bytes(self.store.read_blob(found["blob"]))
        return found, (self._rewrote(out) if output is None else out)

    def reset(self, rev: str | None = None, mode: str = "mixed") -> str:
        if mode not in {"soft", "mixed", "hard"}:
            raise RuntimeError(f"unknown reset mode: {mode}")
        commit = self.resolve_rev(rev)
        if not commit:
            raise RuntimeError("no commit to reset to")
        previous = self.current_commit()
        if mode == "hard":
            self._ensure_no_untracked_overwrite(self.entries_for_checkout(commit), previous, "reset --hard")
        if mode == "soft":
            # Like git, --soft must keep "what would be committed" identical:
            # that is the previous HEAD's tree *plus* anything already staged,
            # re-expressed as overrides against the new HEAD.
            soft_target = self.entries_for_commit(previous)
            for path, entry in self.read_index().get("entries", {}).items():
                if _is_deleted_index_entry(entry):
                    soft_target.pop(path, None)
                else:
                    soft_target[path] = self._committable_entry(entry)
        self.set_current_commit(commit)
        if mode == "soft":
            self.write_index(self._index_diff_to_entries(commit, soft_target))
        if mode in {"mixed", "hard"}:
            self.write_index({"version": 1, "entries": {}})
        if mode == "hard":
            self.restore_all(commit, remove_missing_from=previous, reason=f"reset --hard {rev or 'HEAD'}")
            self.clear_pending_merge()
            self.clear_all_conflict_files()
        reason = f"reset --{mode}: {rev or 'HEAD'}"
        self._log_ref_move("HEAD", previous, commit, reason)
        target = self.head_target()
        if target.startswith("refs/heads/"):
            self._log_ref_move(target, previous, commit, reason)
        return commit

    def _index_diff_between(self, base: str | None, target: str | None) -> dict[str, Any]:
        return self._index_diff_to_entries(base, self.entries_for_commit(target))

    def _index_diff_to_entries(self, base: str | None, target_entries: dict[str, dict[str, Any]]) -> dict[str, Any]:
        base_entries = self.entries_for_commit(base)
        entries: dict[str, dict[str, Any]] = {}
        for rel in sorted(set(base_entries) | set(target_entries)):
            before = base_entries.get(rel)
            after = target_entries.get(rel)
            if same_entry(before, after):
                continue
            entries[rel] = _deleted_index_entry(rel) if after is None else after
        return {"version": 1, "entries": entries}

    def checkout_detached(self, rev: str, discard: bool = False) -> str:
        commit = self.resolve_rev(rev)
        if not commit:
            raise RuntimeError(f"unknown revision: {rev}")
        self.entries_for_checkout(commit)
        if self.pending_merge_head() and not discard:
            raise RuntimeError("merge in progress; finish it (tmiki add + tmiki commit) or give it up with 'tmiki merge --abort' before checkout")
        dirty = [item for item in self.status_items(with_layer_detail=False) if item.state != "untracked"]
        if dirty and not discard:
            raise RuntimeError(_DIRTY_WORKTREE_MESSAGE)
        previous_commit = self.current_commit()
        if not discard:
            self._ensure_no_untracked_overwrite(self.entries_for_commit(commit), previous_commit, "checkout")
        self.detach_head(commit)
        self.restore_all(
            commit,
            remove_missing_from=previous_commit,
            reason=f"checkout --discard {rev}" if discard else f"checkout {rev}",
        )
        if discard:
            self.write_index({"version": 1, "entries": {}})
            self.clear_pending_merge()
            self.clear_all_conflict_files()
        self._log_ref_move("HEAD", previous_commit, commit, f"checkout: moving to {rev}")
        return commit

    def _remove_empty_parent_dirs(self, start: Path) -> None:
        root = self.require_worktree()
        cur = start.resolve()
        while cur != root and root in cur.parents:
            try:
                cur.rmdir()
            except OSError:
                return
            cur = cur.parent

    def remove_path(self, spec: str, *, cached: bool = False, force: bool = False) -> str:
        head_entries = self.entries_for_commit(self.current_commit())
        index = self.read_index()
        index_entries = index.get("entries", {})
        rel = self.pathspec_relpath(spec, set(head_entries) | set(index_entries))
        if rel not in head_entries and rel not in index_entries:
            raise RuntimeError(f"pathspec did not match any tracked file: {rel}")
        full = self.abspath(rel)
        index_entry = index_entries.get(rel)
        if (
            not cached
            and not force
            and rel not in head_entries
            and index_entry is not None
            and not _is_deleted_index_entry(index_entry)
        ):
            # The staged content has never been committed: deleting the worktree
            # file would destroy the only user-visible copy of it.
            raise RuntimeError(
                f"{rel} has changes staged in the index but no commit; "
                "use --cached to keep the file or -f to delete it anyway"
            )
        if not cached and full.exists() and rel in head_entries and not force:
            digest = sha256_file(full)
            if digest != head_entries[rel].get("source_digest"):
                raise RuntimeError(f"{rel} has local modifications; use -f to remove")
        if rel not in head_entries and rel in index_entries:
            # Path only ever existed in the index: dropping the entry is the
            # whole undo — a delete marker for a path HEAD never had would
            # linger in status as a meaningless staged deletion.
            entries = index.setdefault("entries", {})
            entries.pop(rel, None)
            self.write_index(index)
            self.clear_conflict_files(rel)
        else:
            self.stage_delete(rel)
        if not cached and full.exists():
            if full.is_dir():
                raise RuntimeError(f"not removing directory: {rel}")
            full.unlink()
            self._remove_empty_parent_dirs(full.parent)
        return rel

    def move_path(self, source: str, dest: str, *, force: bool = False) -> tuple[str, str]:
        root = self.require_worktree()
        head_entries = self.entries_for_commit(self.current_commit())
        index_entries = self.read_index().get("entries", {})
        src_rel = self.pathspec_relpath(source, set(head_entries) | set(index_entries))
        src = self.abspath(src_rel)
        if not src.exists():
            raise RuntimeError(f"source does not exist: {src_rel}")
        dst = self.pathspec_full_path(dest)
        if dst.exists() and dst.is_dir():
            dst = dst / src.name
        try:
            dst.relative_to(root)
        except ValueError:
            raise RuntimeError(f"destination is outside working tree: {dst}")
        dst_rel = self.relpath(dst)
        # Case-only rename on a case-insensitive filesystem: src and dst are
        # the same file, so the "destination exists" guard must not fire, and
        # unlinking dst would delete the source itself.
        same_file = False
        if dst.exists():
            try:
                same_file = os.path.samefile(src, dst)
            except OSError:
                same_file = False
        if dst.exists() and not same_file:
            if not force:
                raise RuntimeError(f"destination exists: {dst_rel}")
            if dst.is_dir():
                raise RuntimeError(f"destination is a directory: {dst_rel}")
            dst.unlink()
        if same_file and src_rel == dst_rel:
            raise RuntimeError(f"source and destination are the same file: {src_rel}")
        dst.parent.mkdir(parents=True, exist_ok=True)
        if same_file:
            os.replace(str(src), str(dst))
        else:
            shutil.move(str(src), str(dst))
        self.stage_delete(src_rel)
        self.stage_file(str(dst), force=True)
        return src_rel, dst_rel

    @property
    def stash_path(self) -> Path:
        return self.repo_dir / "stash.json"

    def read_stashes(self) -> list[dict[str, Any]]:
        try:
            data = json.loads(self.stash_path.read_text(encoding="utf-8"))
        except FileNotFoundError:
            return []
        if not isinstance(data, list):
            return []
        return [item for item in data if isinstance(item, dict)]

    def write_stashes(self, stashes: list[dict[str, Any]]) -> None:
        _atomic_write_bytes(
            self.stash_path,
            (json.dumps(stashes, ensure_ascii=False, sort_keys=True, indent=2) + "\n").encode("utf-8"),
        )

    def stash_push(self, message: str | None = None, on_progress: ProgressFn | None = None) -> dict[str, Any]:
        current = self.current_commit()
        if not current:
            raise RuntimeError("cannot stash without a commit")
        if self.pending_merge_head():
            raise RuntimeError("cannot stash during a merge")
        head_entries = self.entries_for_commit(current)
        index_entries = self.read_index().get("entries", {})
        entries = self._working_tree_entries_for_stash(on_progress=on_progress)
        if _entries_equal(entries, head_entries) and not index_entries:
            raise RuntimeError("no local changes to save")
        label = message or "WIP"
        from . import attributes as _attributes

        _attributes.refuse_dev_records(
            [*entries.items(), *((p, e) for p, e in index_entries.items() if not _is_deleted_index_entry(e))]
        )
        commit = {
            "version": 1,
            "tree_ref": self.write_repository_tree(entries),
            "index_entries": index_entries,
            "parents": [current],
            "author": self._author(),
            "timestamp": _object_timestamp(),
            "message": f"stash: {label}",
            "thumbnail_ref": next((entry.get("thumbnail_ref") for entry in entries.values() if entry.get("thumbnail_ref")), None),
            "canvas_meta": next((entry.get("canvas_meta") for entry in entries.values()), {}),
        }
        oid = self.store.write_json("commit", commit)
        stash_id = datetime.now(timezone.utc).strftime("%Y%m%d%H%M%S%f")
        ref = f"refs/stash/{stash_id}"
        self.write_ref(ref, oid)
        stashes = self.read_stashes()
        stashes.insert(0, {
            "id": stash_id,
            "ref": ref,
            "commit": oid,
            "base": current,
            "message": label,
            "timestamp": commit["timestamp"],
        })
        self.write_stashes(stashes)
        self.restore_all(current, remove_missing_from=oid, reason="stash push")
        self.write_index({"version": 1, "entries": {}})
        return stashes[0]

    def _working_tree_entries_for_stash(self, on_progress: ProgressFn | None = None) -> dict[str, dict[str, Any]]:
        head_entries = self.entries_for_commit(self.current_commit())
        index_entries = self.read_index().get("entries", {})
        root = self.require_worktree()
        entries: dict[str, dict[str, Any]] = {}
        for rel in sorted(set(head_entries) | set(index_entries)):
            full = root / Path(rel)
            if full.exists():
                if on_progress:
                    on_progress(f"stashing {rel}")
                base = index_entries.get(rel) or head_entries.get(rel)
                entries[rel] = self.materialize_file_from_path(rel, full, base_entry=base)
                continue
            indexed = index_entries.get(rel)
            if indexed and not _is_deleted_index_entry(indexed):
                entries[rel] = self._committable_entry(indexed)
        return entries

    def resolve_stash(self, spec: str | None = None) -> tuple[int, dict[str, Any]]:
        stashes = self.read_stashes()
        if not stashes:
            raise RuntimeError("no stash entries")
        if not spec:
            return 0, stashes[0]
        match = re.fullmatch(r"stash@\{(\d+)\}", spec)
        if match:
            idx = int(match.group(1))
        elif spec.isdigit():
            idx = int(spec)
        else:
            raise RuntimeError(f"unknown stash spec: {spec}")
        if idx < 0 or idx >= len(stashes):
            raise RuntimeError(f"stash not found: {spec}")
        return idx, stashes[idx]

    def stash_apply(self, spec: str | None = None) -> dict[str, Any]:
        _idx, stash = self.resolve_stash(spec)
        commit = stash.get("commit")
        if not isinstance(commit, str) or not commit:
            raise RuntimeError("corrupt stash entry")
        current = self.current_commit()
        self._ensure_clean_tracked("stash apply")
        # Apply only the paths the stash actually changed relative to the
        # commit it was taken on. Restoring the whole stash tree would also
        # rewrite files the stash never touched, silently reverting anything
        # committed after the stash was taken.
        base = stash.get("base")
        base_entries = self.entries_for_commit(base) if isinstance(base, str) and base else {}
        stash_entries = self.entries_for_checkout(commit)
        changed = {
            rel
            for rel in set(base_entries) | set(stash_entries)
            if not same_entry(base_entries.get(rel), stash_entries.get(rel))
        }
        touched = {rel: stash_entries[rel] for rel in changed if rel in stash_entries}
        self._ensure_no_untracked_overwrite(touched, current, "stash apply")
        for rel in sorted(changed, reverse=True):
            if rel in stash_entries:
                continue
            out = self.abspath(rel)
            if out.exists() and out.is_file():
                out.unlink()
                self._remove_empty_parent_dirs(out.parent)
        for rel, entry in sorted(touched.items()):
            out = self.abspath(rel)
            out.parent.mkdir(parents=True, exist_ok=True)
            out.write_bytes(self.store.read_blob(entry["source_blob_ref"]))
            self._rewrote(out)
        stash_commit = self.read_commit(commit)
        stored_index_entries = stash_commit.get("index_entries")
        if isinstance(stored_index_entries, dict):
            restored_index = {
                str(path): entry
                for path, entry in stored_index_entries.items()
                if isinstance(entry, dict)
            }
        else:
            restored_index = {}
        self.write_index({"version": 1, "entries": restored_index})
        return stash

    def stash_drop(self, spec: str | None = None) -> dict[str, Any]:
        idx, stash = self.resolve_stash(spec)
        stashes = self.read_stashes()
        dropped = stashes.pop(idx)
        ref = dropped.get("ref")
        if isinstance(ref, str):
            self._ref_path(ref).unlink(missing_ok=True)
        self.write_stashes(stashes)
        return dropped

    def stash_clear(self) -> int:
        stashes = self.read_stashes()
        for stash in stashes:
            ref = stash.get("ref")
            if isinstance(ref, str):
                self._ref_path(ref).unlink(missing_ok=True)
        self.write_stashes([])
        return len(stashes)

    def find_committed_layer(self, entry: dict[str, Any], selector: str) -> dict[str, Any] | None:
        """By id / stable id, folder path (``人物/塗り/ドレス``) or unique name."""
        from .importers import _ambiguous_message

        normalized = normalize_selector(selector)
        id_matches: list[dict[str, Any]] = []
        path_matches: list[dict[str, Any]] = []
        name_matches: list[dict[str, Any]] = []
        for layer in self.flatten_layers(entry["tree_ref"]):
            if (
                normalize_selector(str(layer.get("layer_id", ""))) == normalized
                or normalize_selector(str(layer.get("stable_source_id", ""))) == normalized
            ):
                id_matches.append(layer)
            elif normalize_selector(str(layer.get("path_name", ""))) == normalized:
                path_matches.append(layer)
            elif normalize_selector(str(layer.get("name", ""))) == normalized:
                name_matches.append(layer)
        if id_matches:
            return id_matches[0]
        for matches in (path_matches, name_matches):
            if len(matches) > 1:
                raise RuntimeError(_ambiguous_message(
                    selector, [(str(m.get("path_name") or m.get("name")), str(m.get("layer_id"))) for m in matches]
                ))
            if matches:
                return matches[0]
        return None

    def list_branches(self) -> list[tuple[str, str | None, bool]]:
        current = self.current_branch()
        rows = []
        for name, commit in self.list_head_refs().items():
            rows.append((name, commit, name == current))
        return sorted(rows)

    def list_head_refs(self) -> dict[str, str | None]:
        root = self.repo_dir / "refs" / "heads"
        refs: dict[str, str | None] = {}
        if not root.exists():
            return refs
        for path in root.rglob("*"):
            if path.is_file() and not path.name.endswith(".lock"):
                name = path.relative_to(root).as_posix()
                refs[name] = _lockfile.read_text(path).strip() or None
        return refs

    def create_branch(self, name: str, start: str | None = None) -> str:
        check_ref_short_name(name)
        commit = self.resolve_rev(start)
        if not commit:
            raise RuntimeError("cannot create branch without a commit")
        ref_name = f"refs/heads/{name}"
        if self._ref_path(ref_name).exists():
            raise RuntimeError(f"branch already exists: {name}")
        self.write_ref(ref_name, commit, expected=None)
        self._log_ref_move(ref_name, None, commit, f"branch: created from {start or 'HEAD'}")
        return commit

    def delete_branch(self, name: str, force: bool = False) -> str:
        ref = f"refs/heads/{name}"
        target = self.read_ref(ref)
        if target is None:
            raise RuntimeError(f"branch not found: {name}")
        current = self.current_branch()
        if name == current:
            raise RuntimeError(f"cannot delete the current branch: {name}")
        if current is not None:
            # Name-equality alone misses case-insensitive filesystems: on
            # Windows `branch -d MAIN` opens the same ref file as `main`, so
            # deleting it would drop the checked-out branch and dangle HEAD.
            try:
                if os.path.samefile(self._ref_path(ref, strict=False), self._ref_path(f"refs/heads/{current}", strict=False)):
                    raise RuntimeError(f"cannot delete the current branch: {name}")
            except OSError:
                pass
        if not force:
            head = self.current_commit()
            merged = bool(head) and (target == head or self.is_ancestor(target, head))
            if not merged:
                raise RuntimeError(
                    f"branch {name} is not fully merged into HEAD; "
                    "use -D to delete it anyway (recover via 'tmiki reflog')"
                )
        # Leave the branch's own reflog (logs/refs/heads/<name>) in place so the
        # commits stay reachable for 'tmiki recover'/gc; only the ref is removed.
        self.delete_ref(ref, expected=target)
        return target

    def delete_tag(self, name: str) -> str:
        ref = f"refs/tags/{name}"
        target = self.read_ref(ref)
        if target is None:
            raise RuntimeError(f"tag not found: {name}")
        self.delete_ref(ref, expected=target)
        return target

    def switch(self, name: str, discard: bool = False) -> str | None:
        ref = f"refs/heads/{name}"
        commit = self.read_ref(ref)
        if commit is None:
            raise RuntimeError(f"branch not found: {name}")
        self.entries_for_checkout(commit)
        if self.pending_merge_head() and not discard:
            raise RuntimeError("merge in progress; finish it (tmiki add + tmiki commit) or give it up with 'tmiki merge --abort' before switching")
        dirty = [item for item in self.status_items(with_layer_detail=False) if item.state != "untracked"]
        if not discard and dirty:
            raise RuntimeError(_DIRTY_WORKTREE_MESSAGE)
        previous_branch = self.current_branch()
        previous_commit = self.current_commit()
        # Validate the untracked-overwrite guard before touching HEAD: if this
        # raises after set_head_ref, HEAD points at the new branch while the
        # worktree still holds the old branch's content, and status/commit then
        # operate on inconsistent state.
        if commit and not discard:
            self._ensure_no_untracked_overwrite(self.entries_for_commit(commit), previous_commit, "switch")
        _hooks.run(
            self.repo_dir,
            "pre-switch",
            extra_env={
                "TMI_BRANCH_FROM": previous_branch or "",
                "TMI_BRANCH_TO": name,
            },
        )
        self.set_head_ref(ref)
        backup_reason = f"switch --discard {name}" if discard else f"switch {name}"
        if commit:
            self.restore_all(commit, remove_missing_from=previous_commit, reason=backup_reason)
        if discard:
            target_entries = self.entries_for_commit(commit) if commit else {}
            index_entries = self.read_index().get("entries", {})
            for rel in sorted(set(index_entries) - set(target_entries), reverse=True):
                out = self.abspath(rel)
                if out.exists() and out.is_file():
                    # A staged new file: only the index (reset below) has it.
                    self.backup_worktree_file(rel, backup_reason, check_head=False)
                    out.unlink()
                    self._remove_empty_parent_dirs(out.parent)
            self.write_index({"version": 1, "entries": {}})
            self.clear_pending_merge()
            self.clear_all_conflict_files()
        reason = f"switch: {previous_branch or '(detached)'} -> {name}"
        self._log_ref_move("HEAD", previous_commit, commit, reason)
        _hooks.run(
            self.repo_dir,
            "post-switch",
            extra_env={
                "TMI_BRANCH_FROM": previous_branch or "",
                "TMI_BRANCH_TO": name,
                "TMI_COMMIT": commit or "",
            },
            fatal=False,
        )
        self._emit_plugin_event("post-switch", {"from": previous_branch, "to": name, "commit": commit})
        return commit

    def create_tag(self, name: str, target: str | None = None, message: str | None = None) -> str:
        check_ref_short_name(name)
        commit = self.resolve_rev(target)
        if not commit:
            raise RuntimeError("cannot tag without a commit")
        ref_name = f"refs/tags/{name}"
        if self._ref_path(ref_name).exists():
            raise RuntimeError(f"tag already exists: {name}")
        if message is None:
            self.write_ref(ref_name, commit, expected=None)
            return commit
        tag = {
            "version": 1,
            "object": commit,
            "type": "commit",
            "tag": name,
            "tagger": self._author(),
            "timestamp": _object_timestamp(),
            "message": message,
        }
        oid = self.store.write_json("tag", tag)
        self.write_ref(ref_name, oid, expected=None)
        return oid

    def list_tags(self) -> list[tuple[str, str]]:
        root = self.repo_dir / "refs" / "tags"
        out: list[tuple[str, str]] = []
        if not root.exists():
            return out
        for path in root.rglob("*"):
            if path.is_file() and not path.name.endswith(".lock"):
                out.append((path.relative_to(root).as_posix(), _lockfile.read_text(path).strip()))
        return sorted(out)

    def add_remote(self, name: str, url: str) -> None:
        from .transport import canonical_url

        check_ref_short_name(name)
        config = self.read_config()
        remotes = config.setdefault("remotes", {})
        if name in remotes:
            raise RuntimeError(f"remote {name} already exists")
        remotes[name] = {"url": canonical_url(url)}
        self.write_config(config)

    def remove_remote(self, name: str) -> None:
        config = self.read_config()
        remotes = config.setdefault("remotes", {})
        if name not in remotes:
            raise RuntimeError(f"no such remote: {name}")
        remotes.pop(name)
        self.write_config(config)

    def rename_remote(self, old: str, new: str) -> None:
        config = self.read_config()
        remotes = config.setdefault("remotes", {})
        if old not in remotes:
            raise RuntimeError(f"no such remote: {old}")
        if new in remotes:
            raise RuntimeError(f"remote {new} already exists")
        check_ref_short_name(new)
        self._ref_path(f"refs/remotes/{new}/placeholder")
        remotes[new] = remotes.pop(old)
        self.write_config(config)
        old_dir = self.repo_dir / "refs" / "remotes" / old
        new_dir = self.repo_dir / "refs" / "remotes" / new
        if old_dir.exists():
            if new_dir.exists():
                # shutil.move into an existing directory would nest old/ inside
                # it (refs/remotes/new/old/...), leaving any stale refs of a
                # previous remote named `new` shadowing the moved ones. Move the
                # tracking refs file-by-file, overwriting stale entries.
                for path in sorted(old_dir.rglob("*")):
                    if not path.is_file():
                        continue
                    target = new_dir / path.relative_to(old_dir)
                    target.parent.mkdir(parents=True, exist_ok=True)
                    path.replace(target)
                shutil.rmtree(old_dir, ignore_errors=True)
            else:
                new_dir.parent.mkdir(parents=True, exist_ok=True)
                shutil.move(str(old_dir), str(new_dir))

    def set_remote_url(self, name: str, url: str) -> None:
        from .transport import canonical_url

        config = self.read_config()
        remotes = config.setdefault("remotes", {})
        if name not in remotes:
            raise RuntimeError(f"no such remote: {name}")
        remotes[name]["url"] = canonical_url(url)
        self.write_config(config)

    def remotes(self) -> dict[str, dict[str, str]]:
        return self.read_config().get("remotes", {})

    def remote_transport(self, name: str):
        remote = self.remotes().get(name)
        if not remote:
            raise RuntimeError(f"remote not found: {name}")
        # Imported here: urllib and the transports cost status/log startup time.
        from .transport import open_transport

        return open_transport(remote["url"])

    def remote_repo(self, name: str) -> "Repo":
        remote = self.remotes().get(name)
        if not remote:
            raise RuntimeError(f"remote not found: {name}")
        return Repo.open_path(remote["url"])

    def fetch(self, name: str = "origin", on_progress: ProgressFn | None = None) -> dict[str, Any]:
        from .transport import checked_refs

        transport = self.remote_transport(name)
        copied = transport.pull_objects(self.store, on_progress=on_progress)
        all_refs = checked_refs(transport.list_refs())
        head_refs = {
            ref.removeprefix("refs/heads/"): oid
            for ref, oid in all_refs.items()
            if ref.startswith("refs/heads/")
        }
        for branch, commit in head_refs.items():
            if commit:
                self.write_ref(f"refs/remotes/{name}/{branch}", commit)
        return {"objects": copied, "refs": head_refs, "tags": self.take_remote_tags(all_refs)}

    def take_remote_tags(self, all_refs: dict[str, str]) -> list[str]:
        """Make the tags the remote has local too, as Git's fetch does for
        tags that point at what it fetched. A tag we already have keeps its
        value (tags are never moved). Returns the new ones (issue #40)."""
        new: list[str] = []
        for ref, oid in sorted(all_refs.items()):
            if not ref.startswith("refs/tags/") or not oid:
                continue
            if self.read_ref(ref) is None and self.store.path_for(oid).exists():
                self.write_ref(ref, oid)
                new.append(ref.removeprefix("refs/tags/"))
        return new

    def push(
        self,
        name: str = "origin",
        branch: str | None = None,
        on_progress: ProgressFn | None = None,
        force: bool = False,
        force_with_lease: bool = False,
    ) -> dict[str, Any]:
        branch = branch or self.current_branch()
        if not branch:
            raise RuntimeError("cannot infer current branch")
        commit = self.read_ref(f"refs/heads/{branch}")
        if not commit:
            raise RuntimeError(f"branch has no commit: {branch}")
        from .transport import checked_refs

        transport = self.remote_transport(name)
        remote_commit = checked_refs(transport.list_refs()).get(f"refs/heads/{branch}")
        tracking_ref = f"refs/remotes/{name}/{branch}"
        tracked_remote_commit = self.read_ref(tracking_ref)
        forced = False
        if remote_commit and remote_commit != commit:
            # Git rejects a push that is not a fast-forward of the remote ref:
            # the remote tip must be an ancestor of what we are pushing. If we
            # do not even have the remote commit locally the branches have
            # diverged. Either way the user must fetch/merge first unless they
            # explicitly force.
            if not self.is_ancestor(remote_commit, commit):
                if force_with_lease:
                    if not tracked_remote_commit:
                        raise RuntimeError(
                            f"cannot force-with-lease without a local tracking ref for {name}/{branch}; "
                            "fetch first"
                        )
                    if tracked_remote_commit != remote_commit:
                        raise RuntimeError(
                            f"force-with-lease rejected: remote {name}/{branch} is at {remote_commit[:12]}, "
                            f"but local tracking ref is {tracked_remote_commit[:12]}. Fetch first."
                        )
                    forced = True
                elif force:
                    forced = True
                else:
                    raise RuntimeError(
                        f"updates were rejected: remote {name}/{branch} is at "
                        f"{remote_commit[:12]}, which is not an ancestor of {commit[:12]} "
                        "(non-fast-forward). Fetch and merge first, or pass --force to overwrite."
                    )
        _hooks.run(
            self.repo_dir,
            "pre-push",
            extra_env={
                "TMI_REMOTE": name,
                "TMI_BRANCH": branch,
                "TMI_COMMIT": commit,
                "TMI_REMOTE_COMMIT": remote_commit or "",
            },
        )
        copied = transport.push_objects(self.store, on_progress=on_progress)
        # Compare-and-swap against the remote tip this push checked (the
        # remote ref is updated under its lock): a push that raced another
        # one is rejected like a non-fast-forward instead of overwriting it.
        # A plain --force is the one update that does not compare.
        from .transport import RemoteRefMoved

        try:
            transport.update_ref(
                f"refs/heads/{branch}",
                commit,
                expected=_UNCHECKED if forced and not force_with_lease else (remote_commit or None),
            )
        except RemoteRefMoved as exc:
            raise RuntimeError(
                f"updates were rejected: remote {name}/{branch} is at "
                f"{(exc.actual or 'nothing')[:12]}, which is not an ancestor of {commit[:12]} "
                "(non-fast-forward). Fetch and merge first, or pass --force to overwrite."
            ) from None
        # Mirror Git: a successful push advances the local remote-tracking ref.
        self.write_ref(f"refs/remotes/{name}/{branch}", commit)
        return {
            "objects": copied,
            "branch": branch,
            "commit": commit,
            "remote_commit": remote_commit,
            "force_with_lease": force_with_lease,
            "forced": forced,
        }

    def is_tag_push(self, ref: str | None) -> bool:
        """Whether ``tmiki push <remote> <ref>`` pushes a tag: ``refs/tags/x``,
        or a name with a tag and no branch of that name (issue #40)."""
        if not ref:
            return False
        if ref.startswith("refs/tags/"):
            return True
        return self.read_ref(f"refs/heads/{ref}") is None and self.read_ref(f"refs/tags/{ref}") is not None

    def push_tags(
        self,
        name: str = "origin",
        tags: list[str] | None = None,
        on_progress: ProgressFn | None = None,
        force: bool = False,
    ) -> dict[str, Any]:
        """Send tags (``tags=None``: every tag) to remote ``name``. As in Git
        a tag is never moved: one the remote has under another value is
        rejected (before anything is sent) unless ``force``. Issue #40."""
        local = dict(self.list_tags())
        wanted = sorted(local) if tags is None else [t.removeprefix("refs/tags/") for t in tags]
        for tag in wanted:
            if tag not in local:
                raise RuntimeError(f"tag not found: {tag}")
        from .transport import checked_refs

        transport = self.remote_transport(name)
        remote_refs = checked_refs(transport.list_refs())
        pending: list[tuple[str, str, str]] = []
        up_to_date: list[str] = []
        for tag in wanted:
            value = local[tag]
            remote_value = remote_refs.get(f"refs/tags/{tag}") or ""
            if remote_value == value:
                up_to_date.append(tag)
                continue
            if remote_value and not force:
                raise RuntimeError(
                    f"updates were rejected: tag {tag} already exists on {name} at {remote_value[:12]}, "
                    f"not {value[:12]}. Tags are not moved; pass --force to replace it."
                )
            pending.append((tag, value, remote_value))
        for tag, value, remote_value in pending:
            _hooks.run(
                self.repo_dir,
                "pre-push",
                extra_env={
                    "TMI_REMOTE": name,
                    "TMI_BRANCH": "",
                    "TMI_TAG": tag,
                    "TMI_COMMIT": value,
                    "TMI_REMOTE_COMMIT": remote_value,
                },
            )
        copied = transport.push_objects(self.store, on_progress=on_progress) if pending else 0
        from .transport import RemoteRefMoved

        for tag, value, remote_value in pending:
            # Compare-and-swap against the tag value checked above (issue #36);
            # --force is the one update that does not compare.
            try:
                transport.update_ref(
                    f"refs/tags/{tag}", value, expected=_UNCHECKED if force else (remote_value or None)
                )
            except RemoteRefMoved as exc:
                raise RuntimeError(
                    f"updates were rejected: tag {tag} was changed on {name} while pushing "
                    f"(now {(exc.actual or 'missing')[:12]}, was {(remote_value or 'missing')[:12]}). "
                    "Fetch first; tags are not moved, pass --force to replace it."
                ) from None
        return {"objects": copied, "pushed": [(tag, value) for tag, value, _ in pending], "up_to_date": up_to_date}

    def pull(
        self,
        name: str = "origin",
        branch: str | None = None,
        on_progress: ProgressFn | None = None,
        fetched: bool = False,
    ) -> dict[str, Any]:
        """``fetched``: the fetch from ``name`` has just been done (the Rust
        front door did it before handing the merge over)."""
        if fetched:
            fetch_result: dict[str, Any] = {"objects": 0, "refs": {}}
        else:
            fetch_result = self.fetch(name, on_progress=on_progress)
        branch = branch or self.current_branch() or DEFAULT_BRANCH
        incoming = self.read_ref(f"refs/remotes/{name}/{branch}")
        if not incoming:
            raise RuntimeError(f"remote branch not found: {name}/{branch}")
        current = self.current_commit()
        if not current:
            self._ensure_clean_tracked("pull")
            self._ensure_no_untracked_overwrite(self.entries_for_checkout(incoming), None, "pull")
            self.set_current_commit(incoming)
            self.restore_all(incoming, reason=f"pull {name}/{branch}")
            self._log_ref_move("HEAD", current, incoming, f"pull: {name}/{branch}")
            target = self.head_target()
            if target.startswith("refs/heads/"):
                self._log_ref_move(target, current, incoming, f"pull: {name}/{branch}")
            return {"mode": "initial", "fetch": fetch_result, "commit": incoming}
        merge = self.merge(
            incoming, f"Merge {name}/{branch}", on_progress=on_progress, operation=f"pull {name}/{branch}"
        )
        merge["fetch"] = fetch_result
        return merge

    def merge(
        self,
        other_rev: str,
        message: str | None = None,
        on_progress: ProgressFn | None = None,
        *,
        operation: str | None = None,
    ) -> dict[str, Any]:
        """``operation`` names the command for backups of overwritten files
        (default "merge <other_rev>")."""
        backup_reason = operation or f"merge {other_rev}"
        if self.pending_merge_head():
            raise RuntimeError("merge already in progress; resolve it with 'tmiki add' and 'tmiki commit' or use 'tmiki switch --discard'")
        current = self.current_commit()
        other = self.resolve_rev(other_rev)
        if not other:
            raise RuntimeError(f"unknown merge target: {other_rev}")
        if current == other:
            return {"mode": "already-up-to-date", "commit": current}
        if current and self.is_ancestor(other, current):
            return {"mode": "already-up-to-date", "commit": current}
        if not current or self.is_ancestor(current, other):
            self._ensure_clean_tracked("merge")
            self._ensure_no_untracked_overwrite(self.entries_for_checkout(other), current, "merge")
            self.set_current_commit(other)
            if on_progress and operation is None:
                # (not for pull's fast-forward, which the Rust front door
                # does itself without this phase)
                on_progress("updating working tree")
            self.restore_all(other, remove_missing_from=current, reason=backup_reason)
            reason = f"merge ff: {other_rev}"
            self._log_ref_move("HEAD", current, other, reason)
            target = self.head_target()
            if target.startswith("refs/heads/"):
                self._log_ref_move(target, current, other, reason)
            _hooks.run(
                self.repo_dir,
                "post-merge",
                extra_env={"TMI_COMMIT": other, "TMI_MERGED_FROM": other},
                fatal=False,
            )
            self._emit_plugin_event("post-merge", {"commit": other, "merged_from": other, "mode": "fast-forward"})
            return {"mode": "fast-forward", "commit": other}

        self._ensure_clean_tracked("merge")
        base = self.merge_base(current, other)
        base_entries = self.entries_for_commit(base) if base else {}
        current_entries = self.entries_for_commit(current)
        other_entries = self.entries_for_checkout(other)
        merged: dict[str, dict[str, Any]] = {}
        conflicts: list[str] = []
        conflict_sides: list[tuple[str, dict[str, Any] | None, dict[str, Any] | None, Any]] = []
        # Conflicted paths whose working-tree file is the partial merge (every
        # change that merged, conflicting layers at OURS) rather than OURS.
        conflict_worktree: dict[str, dict[str, Any]] = {}
        for path in sorted(set(base_entries) | set(current_entries) | set(other_entries)):
            base_entry = base_entries.get(path)
            current_entry = current_entries.get(path)
            other_entry = other_entries.get(path)
            if same_entry(current_entry, other_entry):
                if current_entry:
                    merged[path] = current_entry
            elif same_entry(base_entry, current_entry):
                if other_entry:
                    merged[path] = other_entry
            elif same_entry(base_entry, other_entry):
                if current_entry:
                    merged[path] = current_entry
            else:
                from .merge import merge_entries

                if on_progress:
                    on_progress(f"merging layers of {path}")
                layer_merge = merge_entries(self, path, base_entry, current_entry, other_entry, on_progress=on_progress)
                if layer_merge.entry:
                    merged[path] = layer_merge.entry
                else:
                    conflicts.append(path)
                    conflict_sides.append((path, current_entry, other_entry, layer_merge))
                    if current_entry:
                        merged[path] = current_entry
                    if layer_merge.partial_entry:
                        conflict_worktree[path] = layer_merge.partial_entry
        if conflicts:
            conflict_set = set(conflicts)
            # Guard before writing any merge state: if this raises, no conflict
            # dirs may exist yet, or later add -A / commit -a would be blocked
            # by "unresolved merge conflicts" from a merge that never started.
            self._ensure_no_untracked_overwrite(merged, current, "merge")
            details: dict[str, list[dict[str, Any]]] = {}
            for path, current_entry, other_entry, layer_merge in conflict_sides:
                worktree_entry = conflict_worktree.get(path)
                details[path] = self.materialize_conflict(
                    path, current_entry, other_entry, layer_merge=layer_merge, worktree_entry=worktree_entry
                )
                if worktree_entry:
                    out = self.abspath(path)
                    out.parent.mkdir(parents=True, exist_ok=True)
                    out.write_bytes(self.store.read_blob(worktree_entry["source_blob_ref"]))
                    self._rewrote(out)
            self._stage_merge_result(current_entries, merged, conflict_set)
            self._restore_merge_result(current_entries, merged, conflict_set)
            self.set_pending_merge(other, message or f"Merge {other[:12]}")
            return {
                "mode": "conflict",
                "conflicts": conflicts,
                "conflict_layers": details,
                "conflict_dir": str(self.repo_dir / "conflicts"),
            }

        self._ensure_no_untracked_overwrite(merged, current, "merge")
        from . import attributes as _attributes

        _attributes.refuse_dev_records(merged.items())
        tree_ref = self.write_repository_tree(merged)
        commit = {
            "version": 1,
            "tree_ref": tree_ref,
            "parents": [current, other],
            "author": self._author(),
            "timestamp": _object_timestamp(),
            "message": message or f"Merge {other[:12]}",
            "thumbnail_ref": next((entry.get("thumbnail_ref") for entry in merged.values()), None),
            "canvas_meta": next((entry.get("canvas_meta") for entry in merged.values()), {}),
        }
        oid = self.store.write_json("commit", commit)
        self.set_current_commit(oid)
        if on_progress:
            on_progress("updating working tree")
        self.restore_all(oid, remove_missing_from=current, reason=backup_reason)
        self.clear_pending_merge()
        self.clear_all_conflict_files()
        reason = f"merge: {commit['message'].splitlines()[0] if commit['message'] else other_rev}"
        self._log_ref_move("HEAD", current, oid, reason)
        target = self.head_target()
        if target.startswith("refs/heads/"):
            self._log_ref_move(target, current, oid, reason)
        _hooks.run(
            self.repo_dir,
            "post-merge",
            extra_env={"TMI_COMMIT": oid, "TMI_MERGED_FROM": other},
            fatal=False,
        )
        self._emit_plugin_event("post-merge", {"commit": oid, "merged_from": other, "mode": "merge"})
        return {"mode": "merge", "commit": oid}

    def merge_abort(self) -> list[str]:
        """Undo a conflicted merge: put every tracked working-tree file back
        to HEAD and drop the merge state. Returns the paths rewritten.

        Which files to put back is decided from the working tree, not from the
        index or the conflict records: while resolving, ``add`` /
        ``restore --staged`` / ``restore --in-place`` can leave a file changed
        that neither lists (issue #13). Every file that differs from HEAD is
        backed up first. If a file cannot be put back (e.g. the drawing app
        holds it locked), this raises and keeps the merge in progress, so
        ``merge --abort`` can simply be run again."""
        if not self.pending_merge_head():
            raise RuntimeError("no merge in progress")
        head_entries = self.entries_for_commit(self.current_commit())
        candidates = (
            set(head_entries)
            | set(self.read_index().get("entries", {}))
            | set(self.pending_conflict_paths())
            | set(self.resolved_conflict_paths())
        )
        rewritten: list[str] = []
        failed: list[str] = []
        for rel in sorted(candidates):
            entry = head_entries.get(rel)
            want = (entry or {}).get("source_digest")
            out = self.abspath(rel)
            try:
                digest = self.backup_worktree_file(rel, "merge --abort", known_digests={want}, check_head=False)
                if entry is None:
                    if out.is_file():
                        out.unlink()
                        self._remove_empty_parent_dirs(out.parent)
                        rewritten.append(rel)
                    continue
                if digest == want:
                    continue
                out.parent.mkdir(parents=True, exist_ok=True)
                out.write_bytes(self.store.read_blob(entry["source_blob_ref"]))
                self._rewrote(out)
                rewritten.append(rel)
                if sha256_file(out) != want:
                    failed.append(f"{rel} (content does not match HEAD after writing)")
            except OSError as exc:
                failed.append(f"{rel} ({exc.strerror or exc})")
        if failed:
            summary = ", ".join(failed[:5])
            if len(failed) > 5:
                summary += f", +{len(failed) - 5} more"
            raise RuntimeError(
                f"merge --abort could not put {len(failed)} file(s) back to HEAD: {summary}; "
                "the merge is still in progress. Close the file(s) in your drawing app and run "
                "'tmiki merge --abort' again (overwritten work is in 'tmiki recover --files')"
            )
        self.write_index({"version": 1, "entries": {}})
        self.clear_pending_merge()
        self.clear_all_conflict_files()
        return rewritten

    def resolve_conflict(self, spec: str, side: str) -> Path:
        """Take ``side`` ("ours" / "theirs") for a conflicted file, or for one
        layer of it with ``path:layer`` (that layer's pixels are written into
        the working-tree file as it is now). Stage the result with ``tmiki add``."""
        if side not in {"ours", "theirs"}:
            raise ValueError(f"invalid side: {side}")
        file_part, layer_selector = split_pathspec(spec)
        if not self.pending_merge_head():
            raise RuntimeError(f"no merge in progress; --{side} only applies to a file that conflicted in a merge")
        rel = self.pathspec_relpath(
            file_part, set(self.pending_conflict_paths()) | set(self.resolved_conflict_paths())
        )
        # A conflict already staged with `tmiki add` still has its record (kept
        # aside), so taking a side keeps working; re-stage the result.
        meta = self.read_conflict_meta(rel)
        if not meta:
            source_flag = "--source MERGE_HEAD"
            raise RuntimeError(
                f"{rel} has no merge conflict to take --{side} from; to copy their version anyway use "
                f"'tmiki restore {source_flag} {rel}' (whole file) or "
                f"'tmiki restore {source_flag} --in-place {rel}:<layer>' (one layer)"
                if side == "theirs"
                else f"{rel} has no merge conflict to take --{side} from; our version is HEAD: "
                f"'tmiki restore --source HEAD {rel}'"
            )
        if not layer_selector:
            out = self.apply_conflict_resolution(rel, side)
            self._record_conflict_choice(rel, None, side)
            return out
        wanted = normalize_selector(layer_selector)
        rows = [
            row for row in meta.get("layers", [])
            if wanted in {
                normalize_selector(str(row.get(k) or ""))
                for k in ("name", "path_name", "stable_source_id")
            }
        ]
        if not rows:
            raise RuntimeError(f"layer not found in {rel}: {layer_selector}")
        if len(rows) > 1:
            choices = ", ".join(f"{row.get('path_name') or row.get('name')} [{row['stable_source_id']}]" for row in rows)
            raise RuntimeError(f"layer selector {layer_selector!r} is ambiguous in {rel}: {choices}")
        row = rows[0]
        blob_ref = row.get(f"{side}_blob_ref")
        if not blob_ref:
            raise RuntimeError(f"{side.upper()} has no pixels for layer {layer_selector!r}")
        target = self.abspath(rel)
        if not target.exists():
            target.write_bytes((self._conflict_record_dir(rel) / f"OURS{meta.get('suffix') or target.suffix}").read_bytes())
        merged = replace_layer_pixels(target, str(meta.get("source_format") or ""), row["stable_source_id"], self.store.read_blob(blob_ref))
        target.write_bytes(merged)
        self._record_conflict_choice(rel, row["stable_source_id"], side)
        return self._rewrote(target)

    def _record_conflict_choice(self, rel: str, stable_source_id: str | None, side: str) -> None:
        """Note in the conflict record which side ``restore --ours/--theirs``
        took (for one layer, or the whole file when ``stable_source_id`` is
        None) and the file's digest right after, so ``status`` can say what
        was taken and whether the file was edited since (issue #15)."""
        meta_path = self._conflict_record_dir(rel) / "conflict.json"
        try:
            meta = json.loads(meta_path.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            return
        taken = {} if stable_source_id is None else dict(meta.get("taken") or {})
        taken[stable_source_id or "*"] = side
        meta["taken"] = taken
        full = self.abspath(rel)
        meta["taken_digest"] = sha256_file(full) if full.is_file() else None
        meta_path.write_text(json.dumps(meta, ensure_ascii=False, indent=2), encoding="utf-8")

    def conflict_choices(self, rel: str) -> list[dict[str, Any]]:
        """``rel``'s conflicts, each with ``taken`` (the side ``restore
        --ours/--theirs`` took for it, or None), plus whether the working file
        changed since that (``edited_since``) or, if nothing was taken, since
        the merge stopped (``edited_since_merge``)."""
        meta = self.read_conflict_meta(rel) or {}
        taken = meta.get("taken") or {}
        full = self.abspath(rel)
        digest = sha256_file(full) if full.is_file() else None
        if taken:
            edited_since = digest != meta.get("taken_digest")
        else:
            edited_since = bool(meta.get("worktree_digest")) and digest != meta.get("worktree_digest")
        out: list[dict[str, Any]] = []
        for item in meta.get("conflicts") or []:
            side = taken.get(item.get("stable_source_id") or "") or taken.get("*")
            out.append({**item, "taken": side, "edited_since": edited_since})
        return out

    def _stage_merge_result(
        self,
        current_entries: dict[str, dict[str, Any]],
        merged_entries: dict[str, dict[str, Any]],
        conflict_paths: set[str],
    ) -> None:
        staged: dict[str, dict[str, Any]] = {}
        for path in sorted(set(current_entries) | set(merged_entries)):
            if path in conflict_paths:
                continue
            before = current_entries.get(path)
            after = merged_entries.get(path)
            if same_entry(before, after):
                continue
            staged[path] = _deleted_index_entry(path) if after is None else after
        self.write_index({"version": 1, "entries": staged})

    def _restore_merge_result(
        self,
        current_entries: dict[str, dict[str, Any]],
        merged_entries: dict[str, dict[str, Any]],
        conflict_paths: set[str],
    ) -> None:
        for path in sorted(set(current_entries) | set(merged_entries), reverse=True):
            if path in conflict_paths:
                continue
            before = current_entries.get(path)
            after = merged_entries.get(path)
            if same_entry(before, after):
                continue
            out = self.abspath(path)
            if after is None:
                if out.exists() and out.is_file():
                    out.unlink()
                    self._remove_empty_parent_dirs(out.parent)
                continue
            out.parent.mkdir(parents=True, exist_ok=True)
            out.write_bytes(self.store.read_blob(after["source_blob_ref"]))
            self._rewrote(out)

    def materialize_conflict(
        self,
        path: str,
        current_entry: dict[str, Any] | None,
        other_entry: dict[str, Any] | None,
        *,
        layer_merge: Any = None,
        worktree_entry: dict[str, Any] | None = None,
    ) -> list[dict[str, Any]]:
        """Record a conflicted path under ``.tmi/conflicts/<path>/``.

        Writes both sides' source files, an OURS/THEIRS PNG pair for each
        conflicting layer only (named after the layer, Unicode kept), and
        conflict.json with the per-layer reasons. Returns those reasons."""
        target = self.conflict_dir_for(path)
        if target.exists():
            shutil.rmtree(target)
        target.mkdir(parents=True, exist_ok=True)
        suffix = Path(path).suffix or ".bin"
        layer_conflicts = list(getattr(layer_merge, "layer_conflicts", None) or [])
        wanted = {c.stable_source_id for c in layer_conflicts}
        per_side_layers: dict[str, list[dict[str, Any]]] = {}
        for label, entry in [("OURS", current_entry), ("THEIRS", other_entry)]:
            if not entry:
                per_side_layers[label] = []
                continue
            (target / f"{label}{suffix}").write_bytes(self.store.read_blob(entry["source_blob_ref"]))
            layers = self.flatten_layers(entry["tree_ref"])
            per_side_layers[label] = layers
            for layer in layers:
                blob = layer.get("blob_ref")
                if not blob or layer.get("stable_source_id") not in wanted:
                    continue
                name = safe_filename(layer.get("path_name") or layer.get("name") or layer.get("layer_id"))
                (target / f"{name}__{label}.png").write_bytes(layer_blob_to_png(self.store.read_blob(blob)))
        meta = self._build_conflict_meta(path, per_side_layers, current_entry, other_entry, suffix)
        details = [
            {"stable_source_id": c.stable_source_id, "name": c.name, "path_name": c.path_name, "reason": c.reason}
            for c in layer_conflicts
        ]
        if not details:
            reason = (getattr(layer_merge, "conflicts", None) or [f"{path}: conflict"])[0]
            details = [{"stable_source_id": None, "name": None, "path_name": None, "reason": reason.split(": ", 1)[-1]}]
        meta["conflicts"] = details
        # What the working tree held when the merge stopped: adding it back
        # unchanged keeps OURS for every conflicting layer (see add's warning).
        entry_in_worktree = worktree_entry or current_entry
        meta["worktree_digest"] = (entry_in_worktree or {}).get("source_digest")
        meta["worktree_is_partial_merge"] = worktree_entry is not None
        (target / "conflict.json").write_text(json.dumps(meta, ensure_ascii=False, indent=2), encoding="utf-8")
        return details

    def conflict_left_untouched(self, rel: str) -> list[dict[str, Any]] | None:
        """The conflicts of ``rel`` if its working-tree file is still exactly what
        the merge left there (so staging it silently keeps OURS for them)."""
        meta = self.read_conflict_meta(rel)
        if not meta or not meta.get("worktree_digest"):
            return None
        if "*" in (meta.get("taken") or {}):
            # `restore --ours/--theirs <file>` chose a side for the whole file
            # (issue #43): staging it is no surprise.
            return None
        full = self.abspath(rel)
        if not full.is_file() or sha256_file(full) != meta["worktree_digest"]:
            return None
        return list(meta.get("conflicts") or [])

    def _build_conflict_meta(
        self,
        path: str,
        per_side_layers: dict[str, list[dict[str, Any]]],
        current_entry: dict[str, Any] | None,
        other_entry: dict[str, Any] | None,
        suffix: str,
    ) -> dict[str, Any]:
        layers_meta: dict[str, dict[str, Any]] = {}
        for label, layers in per_side_layers.items():
            for layer in layers:
                sid = layer.get("stable_source_id")
                if not sid:
                    continue
                key = safe_filename(layer.get("path_name") or layer.get("name") or layer.get("layer_id"))
                row = layers_meta.setdefault(
                    sid,
                    {
                        "stable_source_id": sid,
                        "name": layer.get("name"),
                        "path_name": layer.get("path_name"),
                        "file_key": key,
                        "ours_blob_ref": None,
                        "theirs_blob_ref": None,
                    },
                )
                row[f"{label.lower()}_blob_ref"] = layer.get("blob_ref")
                row.setdefault("file_key", key)
                if not row.get("name"):
                    row["name"] = layer.get("name")
                if not row.get("path_name"):
                    row["path_name"] = layer.get("path_name")
        for row in layers_meta.values():
            row["differs"] = row.get("ours_blob_ref") != row.get("theirs_blob_ref")
        return {
            "version": 1,
            "path": path,
            "suffix": suffix,
            "source_format": (current_entry or other_entry or {}).get("source_format"),
            "has_ours": bool(current_entry),
            "has_theirs": bool(other_entry),
            "layers": sorted(layers_meta.values(), key=lambda r: (r.get("path_name") or r.get("name") or "")),
        }

    # -------- merge state --------

    @property
    def merge_head_path(self) -> Path:
        return self.repo_dir / "MERGE_HEAD"

    @property
    def merge_msg_path(self) -> Path:
        return self.repo_dir / "MERGE_MSG"

    def pending_merge_head(self) -> str | None:
        try:
            return self.merge_head_path.read_text(encoding="utf-8").strip() or None
        except FileNotFoundError:
            return None

    def pending_merge_message(self) -> str | None:
        try:
            return self.merge_msg_path.read_text(encoding="utf-8")
        except FileNotFoundError:
            return None

    def set_pending_merge(self, other: str, message: str) -> None:
        self.merge_head_path.write_text(other, encoding="utf-8")
        self.merge_msg_path.write_text(message, encoding="utf-8")

    def clear_pending_merge(self) -> None:
        for p in (self.merge_head_path, self.merge_msg_path):
            try:
                p.unlink()
            except FileNotFoundError:
                pass

    # -------- conflict resolution --------

    def conflict_dir_for(self, path: str) -> Path:
        return self.repo_dir / "conflicts" / safe_filename(path)

    def resolved_conflict_dir_for(self, path: str) -> Path:
        """Where a conflict's record is kept once it is staged during the
        merge, so ``restore --staged`` can reopen it and ``restore --ours /
        --theirs`` keep working (issue #12). ``safe_filename`` never yields a
        leading dot, so this cannot collide with a live conflict dir."""
        return self.repo_dir / "conflicts" / RESOLVED_CONFLICTS_DIR / safe_filename(path)

    def _conflict_record_dir(self, path: str) -> Path:
        live = self.conflict_dir_for(path)
        if (live / "conflict.json").exists():
            return live
        resolved = self.resolved_conflict_dir_for(path)
        if (resolved / "conflict.json").exists():
            return resolved
        return live

    def pending_conflict_paths(self) -> list[str]:
        return _conflict_paths_in(self.repo_dir / "conflicts")

    def resolved_conflict_paths(self) -> list[str]:
        """Paths that conflicted in the pending merge and were staged since."""
        return _conflict_paths_in(self.repo_dir / "conflicts" / RESOLVED_CONFLICTS_DIR)

    def read_conflict_meta(self, path: str) -> dict[str, Any] | None:
        """The conflict record of ``path``: the unresolved one, else the one
        kept after it was staged during the pending merge."""
        meta_path = self._conflict_record_dir(path) / "conflict.json"
        try:
            return json.loads(meta_path.read_text(encoding="utf-8"))
        except FileNotFoundError:
            return None

    def reopen_conflict(self, path: str) -> bool:
        """Turn a conflict staged during the pending merge back into an
        unresolved one. Returns False when there is nothing to reopen."""
        resolved = self.resolved_conflict_dir_for(path)
        if not self.pending_merge_head() or not (resolved / "conflict.json").exists():
            return False
        live = self.conflict_dir_for(path)
        if live.exists():
            shutil.rmtree(live)
        live.parent.mkdir(parents=True, exist_ok=True)
        resolved.replace(live)
        return True

    def apply_conflict_resolution(self, path: str, choice: str) -> Path:
        """Write OURS or THEIRS source file from the conflict dir back to the worktree."""
        if choice not in {"ours", "theirs"}:
            raise ValueError(f"invalid choice: {choice}")
        meta = self.read_conflict_meta(path)
        if not meta:
            raise RuntimeError(f"no conflict recorded for {path}")
        suffix = meta.get("suffix") or (Path(path).suffix or ".bin")
        side_file = self._conflict_record_dir(path) / f"{choice.upper()}{suffix}"
        if not side_file.exists():
            raise RuntimeError(f"missing conflict source for {choice}: {side_file}")
        target = self.abspath(path)
        target.parent.mkdir(parents=True, exist_ok=True)
        self.backup_worktree_file(path, f"restore --{choice}", known_digests={meta.get("worktree_digest")})
        target.write_bytes(side_file.read_bytes())
        return self._rewrote(target)

    def apply_conflict_layer_resolution(self, path: str, layer_picks: dict[str, str]) -> Path:
        """Start from OURS.<ext>; for each stable_source_id whose pick is 'theirs',
        replace that layer's pixels with the THEIRS png. Writes the merged file
        back to the worktree path and returns the path written."""
        meta = self.read_conflict_meta(path)
        if not meta:
            raise RuntimeError(f"no conflict recorded for {path}")
        suffix = meta.get("suffix") or (Path(path).suffix or ".bin")
        source_format = str(meta.get("source_format") or "").lower()
        conflict_dir = self._conflict_record_dir(path)
        base = conflict_dir / f"OURS{suffix}"
        if not base.exists():
            raise RuntimeError(f"missing OURS side for layer merge: {base}")
        target = self.abspath(path)
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(base.read_bytes())
        layer_by_sid = {row["stable_source_id"]: row for row in meta.get("layers", []) if row.get("stable_source_id")}
        for sid, pick in layer_picks.items():
            if pick != "theirs":
                continue
            row = layer_by_sid.get(sid)
            if not row:
                continue
            theirs_ref = row.get("theirs_blob_ref")
            if not theirs_ref:
                continue
            png_bytes = self.store.read_blob(theirs_ref)
            merged = replace_layer_pixels(target, source_format, sid, png_bytes)
            target.write_bytes(merged)
        return self._rewrote(target)

    def clear_conflict_files(self, path: str) -> None:
        """Mark ``path``'s conflict resolved (it was staged). During a merge
        its record is kept aside (see ``resolved_conflict_dir_for``); it goes
        away with the rest when the merge is committed or aborted."""
        sub = self.conflict_dir_for(path)
        if not sub.exists():
            return
        if self.pending_merge_head() and (sub / "conflict.json").exists():
            kept = self.resolved_conflict_dir_for(path)
            if kept.exists():
                shutil.rmtree(kept, ignore_errors=True)
            kept.parent.mkdir(parents=True, exist_ok=True)
            try:
                sub.replace(kept)
                return
            except OSError:
                pass
        shutil.rmtree(sub, ignore_errors=True)

    def clear_all_conflict_files(self) -> None:
        root = self.repo_dir / "conflicts"
        if not root.exists():
            return
        for sub in root.iterdir():
            if sub.is_dir():
                shutil.rmtree(sub, ignore_errors=True)
            else:
                sub.unlink(missing_ok=True)

    def _ensure_clean_tracked(self, operation: str) -> None:
        dirty = [item for item in self.status_items(with_layer_detail=False) if item.state != "untracked"]
        if not dirty:
            return
        summary = ", ".join(f"{item.state} {item.path}" for item in dirty[:5])
        if len(dirty) > 5:
            summary += f", +{len(dirty) - 5} more"
        raise RuntimeError(f"working tree has changes; commit or restore them before {operation}: {summary}")

    def _ensure_no_untracked_overwrite(
        self,
        target_entries: dict[str, dict[str, Any]],
        current_commit: str | None,
        operation: str,
    ) -> None:
        if not self.worktree:
            return
        tracked = set(self.entries_for_commit(current_commit)) if current_commit else set()
        staged = set(self.read_index().get("entries", {}))
        collisions: list[str] = []
        for rel in sorted(set(target_entries) - tracked - staged):
            if self.abspath(rel).exists():
                collisions.append(rel)
        if not collisions:
            return
        summary = ", ".join(collisions[:5])
        if len(collisions) > 5:
            summary += f", +{len(collisions) - 5} more"
        raise RuntimeError(f"untracked files would be overwritten by {operation}: {summary}")

    def is_ancestor(self, ancestor: str, descendant: str) -> bool:
        return ancestor in self.ancestor_set(descendant)

    def ancestor_set(self, commit: str) -> set[str]:
        seen: set[str] = set()
        stack = [commit]
        while stack:
            cur = stack.pop()
            if cur in seen:
                continue
            seen.add(cur)
            try:
                obj = self.read_commit(cur)
            except Exception:
                continue
            stack.extend(obj.get("parents") or [])
        return seen

    def merge_base(self, left: str, right: str) -> str | None:
        left_ancestors = self.ancestor_set(left)
        stack = [right]
        seen: set[str] = set()
        while stack:
            cur = stack.pop(0)
            if cur in seen:
                continue
            if cur in left_ancestors:
                return cur
            seen.add(cur)
            try:
                stack.extend(self.read_commit(cur).get("parents") or [])
            except Exception:
                pass
        return None

    def all_refs(self) -> dict[str, str]:
        """Return every ref currently stored under .tmi/refs/."""
        out: dict[str, str] = {}
        base = self.repo_dir / "refs"
        if not base.is_dir():
            return out
        for path in base.rglob("*"):
            if not path.is_file() or path.name.endswith(".lock"):
                continue
            rel = path.relative_to(self.repo_dir).as_posix()
            value = _lockfile.read_text(path).strip()
            if value:
                out[rel] = value
        head = self.repo_dir / "MERGE_HEAD"
        if head.exists():
            value = _lockfile.read_text(head).strip()
            if value:
                out["MERGE_HEAD"] = value
        return out

    def _walk_reachable(self, roots: set[str]) -> set[str]:
        """Walk the object graph from `roots` and return every reachable oid."""
        reachable: set[str] = set()
        stack: list[str] = [oid for oid in roots if oid]
        while stack:
            oid = stack.pop()
            if oid in reachable:
                continue
            try:
                object_type, _ = self.store.read(oid)
            except (FileNotFoundError, ValueError):
                continue
            reachable.add(oid)
            if object_type == "blob":
                continue
            try:
                obj = self.store.read_json(oid)
            except Exception:
                continue
            stack.extend(self._referenced_oids(object_type, obj))
        return reachable

    def _referenced_oids(self, object_type: str, obj: dict[str, Any]) -> list[str]:
        refs: list[str] = []
        if object_type == "commit":
            refs.extend(obj.get("parents") or [])
            for key in ("tree_ref", "thumbnail_ref", "composite_ref"):
                value = obj.get(key)
                if value:
                    refs.append(value)
            index_entries = obj.get("index_entries")
            if isinstance(index_entries, dict):
                for entry in index_entries.values():
                    if isinstance(entry, dict):
                        refs.extend(self._entry_referenced_oids(entry))
        elif object_type == "tag":
            target = obj.get("object")
            if target:
                refs.append(target)
        elif object_type == "chunked":
            refs.extend(part for part in obj.get("parts") or [] if isinstance(part, str))
            if obj.get("glue"):
                refs.append(obj["glue"])
        elif object_type == "tree":
            kind = obj.get("kind")
            if kind == "repository":
                for entry in obj.get("entries") or []:
                    refs.extend(self._entry_referenced_oids(entry))
            elif kind == "layer-tree":
                for layer in obj.get("layers") or []:
                    for key in ("blob_ref", "subtree_ref", "mask_ref"):
                        value = layer.get(key)
                        if value:
                            refs.append(value)
        return refs

    def _entry_referenced_oids(self, entry: dict[str, Any]) -> list[str]:
        refs: list[str] = []
        for key in ("source_blob_ref", "tree_ref", "thumbnail_ref", "composite_ref"):
            value = entry.get(key)
            if value:
                refs.append(value)
        return refs

    def gc(self, prune_age_days: int = 90) -> dict[str, int]:
        """Delete unreachable objects.

        Reachability roots: every ref + MERGE_HEAD + every oid kept by reflog
        after pruning entries older than `prune_age_days`. `prune_age_days <= 0`
        truncates the reflog entirely (still --prune=now is destructive).
        """
        pruned = _reflog.prune(self.repo_dir, prune_age_days)
        roots: set[str] = set(self.all_refs().values())
        # A detached HEAD is not a ref under refs/ and, once the reflog is
        # pruned (--prune=now), nothing else anchors the checked-out commit —
        # gc would delete the very commit HEAD points at.
        head_commit = self.current_commit()
        if head_commit:
            roots.add(head_commit)
        roots.update(oid for oid in _reflog.iter_all_reachable_oids(self.repo_dir) if oid != _reflog.ZERO_OID)
        roots.update(backup["blob"] for backup in self._prune_backups(prune_age_days) if backup.get("blob"))
        for entry in self.read_index().get("entries", {}).values():
            if _is_deleted_index_entry(entry):
                continue
            for key in ("source_blob_ref", "tree_ref", "thumbnail_ref", "composite_ref"):
                value = entry.get(key)
                if value:
                    roots.add(value)
        roots.discard("")
        reachable = self._walk_reachable(roots)
        existing = set(self.store.iter_object_ids())
        unreachable = existing - reachable
        for oid in unreachable:
            path = self.store.path_for(oid)
            try:
                path.unlink()
            except OSError:
                pass
        # also remove empty fanout dirs
        if self.store.objects_dir.exists():
            for sub in self.store.objects_dir.iterdir():
                if sub.is_dir() and not any(sub.iterdir()):
                    sub.rmdir()
        # Derived data that is never needed again: diff reports, the decode
        # cache (rebuilt on demand) and leftover temporaries.
        scratch_bytes = 0
        for name in ("diff", "cache", "tmp"):
            scratch = self.repo_dir / name
            if not scratch.is_dir():
                continue
            for child in scratch.iterdir():
                try:
                    if child.is_dir():
                        scratch_bytes += sum(p.stat().st_size for p in child.rglob("*") if p.is_file())
                        shutil.rmtree(child)
                    else:
                        scratch_bytes += child.stat().st_size
                        child.unlink()
                except OSError:
                    pass
        return {
            "kept": len(reachable),
            "removed": len(unreachable),
            "reflog_entries_removed": pruned,
            "scratch_bytes_removed": scratch_bytes,
        }

    def _prune_backups(self, prune_age_days: int) -> list[dict[str, Any]]:
        """Drop automatic backups older than the reflog's prune window (all of
        them for --prune=now) and return the ones kept."""
        backups = self.read_backups()
        if not backups:
            return []
        cutoff = datetime.now(timezone.utc).timestamp() - max(0, prune_age_days) * 86400
        kept = []
        for backup in backups:
            try:
                when = datetime.fromisoformat(backup["timestamp"]).timestamp()
            except (KeyError, ValueError):
                when = 0.0
            if prune_age_days > 0 and when >= cutoff:
                kept.append(backup)
        if len(kept) != len(backups):
            _atomic_write_bytes(
                self.backups_path,
                (json.dumps(kept, ensure_ascii=False, indent=2) + "\n").encode("utf-8"),
            )
        return kept

    def _author(self) -> dict[str, str]:
        return _config.resolve_identity(self.read_config())

    def _safe_identity(self) -> dict[str, str] | None:
        try:
            return self._author()
        except RuntimeError:
            return None

    def _emit_plugin_event(self, event: str, payload: dict[str, Any]) -> None:
        try:
            api = _plugin_api()
        except Exception:
            return
        api.emit(event, PluginContext(
            event=event,
            repo_dir=self.repo_dir,
            worktree=self.worktree,
            payload=payload,
        ))

    def _log_ref_move(
        self,
        ref_name: str,
        old: str | None,
        new: str,
        reason: str,
    ) -> None:
        try:
            _reflog.append(self.repo_dir, ref_name, old, new, self._safe_identity(), reason)
        except OSError:
            # reflog is best-effort; never block the user-facing operation
            pass


# Bidi controls and marks: a name that reads backwards in a terminal. Same set
# as tmi_repo::repo::is_format_char.
_FORMAT_CHARS = frozenset("‎‏‪‫‬‭‮⁦⁧⁨⁩")


def ref_name_problem(name: str, *, reading: bool = False) -> str | None:
    """Why ``name`` (a full ref such as ``refs/heads/x``, or a branch / tag /
    remote name) cannot name a ref, or None: ``git check-ref-format``'s
    rules, plus the characters Windows file names cannot hold. The Rust
    ``tmi_repo::repo::ref_name_problem`` is the same function (same order,
    same texts). Issue #41.

    ``reading``: only what could never be a file under refs/ (what tmi has
    always refused). Looking a revision up (``HEAD~1``, ``main^``) tries it as
    a ref name first, so a read must not fail for ``~`` or ``^``."""
    if not name:
        return "it is empty"
    for ch in name:
        if ord(ch) < 32:
            return "it contains a control character"
        if ch in '\\:?*<>"|':
            return f"it contains '{ch}'"
        if reading:
            continue
        if ord(ch) == 127 or 0x80 <= ord(ch) <= 0x9F or ch in _FORMAT_CHARS:
            return "it contains a control character"
        if ch == " ":
            return "it contains a space"
        if ch in "~^[":
            return f"it contains '{ch}'"
    parts = name.split("/")
    if any(part == "" for part in parts):
        return "it has an empty part (a leading, trailing or doubled '/')"
    if reading:
        if any(part in {".", ".."} for part in parts):
            return "a part of it is '.' or '..'"
        return None
    if ".." in name:
        return "it contains '..'"
    if any(part.startswith(".") for part in parts):
        return "a part of it starts with '.'"
    if "@{" in name:
        return "it contains '@{'"
    if any(part.endswith(".lock") for part in parts):
        return "a part of it ends with '.lock'"
    if name.endswith("."):
        return "it ends with '.'"
    if name == "@":
        return "it is '@'"
    if any(_recpath._is_reserved_device(part) for part in parts):
        return "a part of it is a reserved Windows device name"
    return None


def check_ref_short_name(name: str) -> None:
    """A new branch, tag or remote name: the ref rules, and not ``HEAD`` or a
    name starting with ``-`` (it would read as an option)."""
    problem = ref_name_problem(name)
    if problem is None and name == "HEAD":
        problem = "HEAD is reserved"
    if problem is None and name.startswith("-"):
        problem = "it starts with '-'"
    if problem is not None:
        raise RuntimeError(f"invalid ref name: {name} ({problem})")


def _object_timestamp() -> str:
    """The time a new commit, tag or stash commit records.

    ``TMI_COMMIT_TIMESTAMP`` (ISO 8601, stored as given) fixes it, so the
    same steps give the same object ids: the conformance suite
    (tools/conformance.py) runs a scenario twice and compares the repos.
    """
    fixed = os.environ.get("TMI_COMMIT_TIMESTAMP")
    if fixed:
        try:
            datetime.fromisoformat(fixed)
        except ValueError:
            raise RuntimeError(f"TMI_COMMIT_TIMESTAMP is not an ISO 8601 time: {fixed!r}") from None
        return fixed
    return datetime.now(timezone.utc).isoformat()


def split_pathspec(spec: str) -> tuple[str, str | None]:
    # Windows drive paths contain a colon after the drive letter; only treat the
    # final colon as a layer selector when it appears after a file extension.
    path = Path(spec)
    if path.exists():
        return spec, None
    match = re.match(r"^(.+\.(?:png|jpg|jpeg|mdp|clip)):(.+)$", spec, re.IGNORECASE)
    if match:
        return match.group(1), match.group(2)
    return spec, None


# Image formats `restore --output` does not convert a whole file to (issue
# #30): their extension on the output path is refused rather than given
# bytes of another format.
_UNCONVERTED_IMAGE_SUFFIXES = frozenset({
    ".jpg", ".jpeg", ".gif", ".bmp", ".tif", ".tiff", ".webp", ".psd", ".clip", ".mdp", ".apng", ".avif", ".ico",
})


def _whole_file_output(rel: str, output: Path) -> str:
    """What ``restore --output <output> <rel>`` (no layer) writes: "source"
    (the file as it is) when the output names the same format, no format or
    one that is not an image; "png" (its image) for a ``.png`` path. Another
    image format would get bytes that are not what its name says: refused."""
    src = Path(rel).suffix.lower()
    dst = output.suffix.lower()
    if not dst or dst.replace(".jpeg", ".jpg") == src.replace(".jpeg", ".jpg"):
        return "source"
    if dst == ".png":
        return "png"
    if dst in _UNCONVERTED_IMAGE_SUFFIXES:
        hint = f"{src} for the file as it is, or .png for its image" if src else ".png for its image"
        raise RuntimeError(f"cannot convert {rel} to {dst}; give --output a path ending in {hint}")
    return "source"


def _no_image_message(rel: str) -> str:
    src = Path(rel).suffix.lower()
    keep = f"a path ending in {src}" if src else "a path without an extension"
    return f"{rel} has no image to write as .png; give --output {keep} for the file as it is"


RESOLVED_CONFLICTS_DIR = ".resolved"


def _conflict_paths_in(root: Path) -> list[str]:
    """The ``path`` of every ``<root>/<dir>/conflict.json``."""
    if not root.is_dir():
        return []
    paths: list[str] = []
    for sub in sorted(root.iterdir()):
        meta_path = sub / "conflict.json"
        if not meta_path.exists():
            continue
        try:
            meta = json.loads(meta_path.read_text(encoding="utf-8"))
        except Exception:
            continue
        p = meta.get("path")
        if isinstance(p, str):
            paths.append(p)
    return paths


def _deleted_index_entry(rel: str) -> dict[str, Any]:
    return {"path": rel, INDEX_DELETE_MARKER: True}


def _is_deleted_index_entry(entry: dict[str, Any]) -> bool:
    return bool(entry.get(INDEX_DELETE_MARKER))


def sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            h.update(chunk)
    return h.hexdigest()


def read_file_digest(path: Path) -> tuple[bytes, str]:
    """The file's bytes and their sha256, for callers that import the file
    when its digest differs (``import_file(source=, source_digest=)``) so it
    is read and hashed once."""
    data = path.read_bytes()
    return data, hashlib.sha256(data).hexdigest()


class _StatCache:
    """In-memory view of ``.tmi/stat_cache`` for one command run.

    Maps each tracked worktree path to ``{size, mtime_ns, digest}``. When the
    file's current ``(size, mtime_ns)`` match a cached entry we trust the stored
    sha256 and skip reading the (potentially tens-of-MB) file — the dominant
    cost of ``status`` on illustration repos.

    The persisted file is an optimization, never the source of truth: every
    lookup re-stats the file, so the cache self-validates and needs no explicit
    invalidation on commit/checkout/merge/restore. Concurrent writers are safe —
    each run reads the base, hashes its own misses, and flushes via an atomic
    replace; a lost write only costs a recompute next time.
    """

    def __init__(self, repo: "Repo") -> None:
        self._repo = repo
        cache = repo.read_stat_cache()
        self.entries: dict[str, dict[str, Any]] = cache.get("entries", {})
        self._loaded_written_ns: int = int(cache.get("written_ns") or 0)
        self._seen: set[str] = set()
        self._dirty = False

    def digest_for(self, rel: str, full: Path, *, st: os.stat_result | None = None) -> str:
        """Return the sha256 of ``full``'s current bytes, using the cache when
        the file is unchanged. ``st`` lets callers pass a stat they already took
        to avoid a second syscall."""
        return self.lookup(rel, full, st=st, want_bytes=False)[0]

    def lookup(
        self,
        rel: str,
        full: Path,
        *,
        st: os.stat_result | None = None,
        want_bytes: bool = True,
    ) -> tuple[str, bytes | None]:
        """``digest_for`` plus the bytes it hashed: None on a cache hit, else
        (with ``want_bytes``) the file's bytes, so a caller that goes on to
        import the file can pass them on (``import_file(source=,
        source_digest=)``) instead of reading and hashing it again."""
        if st is None:
            st = full.stat()
        self._seen.add(rel)
        ent = self.entries.get(rel)
        # Racy-git guard: only trust entries whose mtime is strictly older than
        # the cache's last write. An mtime in the same granularity window as the
        # write may have changed again with the same size, so re-hash it.
        if (
            ent is not None
            and ent.get("size") == st.st_size
            and ent.get("mtime_ns") == st.st_mtime_ns
            and st.st_mtime_ns < self._loaded_written_ns
        ):
            return ent["digest"], None
        if want_bytes:
            data, digest = read_file_digest(full)
        else:
            data, digest = None, sha256_file(full)
        self.entries[rel] = {"size": st.st_size, "mtime_ns": st.st_mtime_ns, "digest": digest}
        self._dirty = True
        return digest, data

    def changed(
        self,
        rel: str,
        full: Path,
        expected: str | None,
        *,
        st: os.stat_result | None = None,
        want_bytes: bool = True,
    ) -> tuple[bool, tuple[bytes, str] | None]:
        """Whether ``full`` no longer hashes to ``expected``; when it does
        not and the file was read, also its ``(bytes, sha256)`` (else None)."""
        digest, data = self.lookup(rel, full, st=st, want_bytes=want_bytes)
        if digest == expected:
            return False, None
        return True, ((data, digest) if data is not None else None)

    def flush(self) -> None:
        """Persist the cache, pruning entries for paths not seen this run
        (deleted/renamed files). Writes only when something changed."""
        stale = set(self.entries) - self._seen
        if stale:
            for rel in stale:
                del self.entries[rel]
            self._dirty = True
        if self._dirty:
            self._repo.write_stat_cache({"version": 1, "entries": self.entries})


# Directories that can never contain trackable worktree files and are never
# re-included by a `!` ignore rule, so it is always safe to prune them from the
# walk instead of descending and filtering file-by-file. `.tmi` in particular
# holds the (potentially huge) object store.
_WALK_PRUNE_DIRS = frozenset({TMI_DIR, ".git", "__pycache__"})


def link_kind(path: Path) -> str | None:
    """"symbolic link" or "junction" if ``path`` itself is one (not
    followed), else None."""
    try:
        if path.is_symlink():
            return "symbolic link"
        if sys.platform == "win32":
            tag = getattr(os.lstat(path), "st_reparse_tag", 0)
            if tag == stat.IO_REPARSE_TAG_MOUNT_POINT:
                return "junction"
    except OSError:
        pass
    return None


def iter_supported_files(
    root: Path,
    ignore_patterns: list[_ignore.IgnorePattern],
    links: list[tuple[str, str]] | None = None,
) -> list[Path]:
    """The files to track under ``root``. With ``links``, symbolic links and
    junctions (files and directories; none is followed) are left out of the
    result and appended to ``links`` as ``(worktree path, kind)``; without
    it, a link to a file counts as that file."""
    out: list[Path] = []
    for dirpath, dirnames, filenames in os.walk(root):
        # Prune in place so os.walk never descends into these subtrees.
        dirnames[:] = [d for d in dirnames if d not in _WALK_PRUNE_DIRS]
        base = Path(dirpath)
        if links is not None:
            kept = []
            for d in dirnames:
                kind = link_kind(base / d)
                if kind is None:
                    kept.append(d)
                else:
                    rel = (base / d).relative_to(root).as_posix()
                    if not _ignore.is_file_ignored(rel, ignore_patterns):
                        links.append((rel, kind))
            dirnames[:] = kept
        for name in filenames:
            path = base / name
            if links is not None:
                kind = link_kind(path)
                if kind is not None:
                    rel = path.relative_to(root).as_posix()
                    if not _ignore.is_file_ignored(rel, ignore_patterns):
                        links.append((rel, kind))
                    continue
            if not path.is_file() or not is_supported_path(path):
                continue
            rel = path.relative_to(root).as_posix()
            if _ignore.is_file_ignored(rel, ignore_patterns):
                continue
            out.append(path)
    return out


_DIRTY_WORKTREE_MESSAGE = (
    "working tree has changes that are not committed; keep them first with "
    "'tmiki commit -a' or 'tmiki stash', or pass --discard to overwrite them "
    "(they are backed up: 'tmiki recover --files')"
)

_UNSAFE_FILENAME_CHARS = re.compile(r'[<>:"|?*\x00-\x1f]')


def safe_filename(value: str) -> str:
    """A file name for ``value`` that keeps layer names readable (Unicode
    included). Path separators become "／"; characters Windows forbids become
    "_". When that loses information, a short hash of the original keeps
    distinct inputs apart."""
    cleaned = value.replace("\\", "／").replace("/", "／")
    cleaned = _UNSAFE_FILENAME_CHARS.sub("_", cleaned).strip(" .")
    lossy = cleaned != value.replace("\\", "／").replace("/", "／") or "／" in value
    if lossy or not cleaned:
        digest = hashlib.sha256(value.encode("utf-8")).hexdigest()[:8]
        cleaned = f"{cleaned}-{digest}" if cleaned else digest
    return cleaned or "layer"


def normalize_selector(value: str) -> str:
    return re.sub(r"\s+", " ", value.strip()).casefold()


def same_entry(left: dict[str, Any] | None, right: dict[str, Any] | None) -> bool:
    if left is None or right is None:
        return left is right
    return canonical_json_bytes(left) == canonical_json_bytes(right)


def _entries_equal(left: dict[str, dict[str, Any]], right: dict[str, dict[str, Any]]) -> bool:
    if set(left) != set(right):
        return False
    return all(same_entry(left[path], right[path]) for path in left)


def _atomic_write_bytes(path: Path, data: bytes) -> None:
    """Write ``data`` to ``path`` so a crash mid-write cannot leave a partial
    file behind. Writes to a sibling temp file then renames; same-directory
    rename is atomic on POSIX and uses ``os.replace`` on Windows."""
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp_name = tempfile.mkstemp(prefix=path.name + ".", suffix=".tmp", dir=path.parent)
    tmp = Path(tmp_name)
    try:
        with os.fdopen(fd, "wb") as handle:
            handle.write(data)
        _lockfile.replace(tmp, path)  # waits out a reader's handle on Windows
    except BaseException as exc:
        tmp.unlink(missing_ok=True)
        if _diskspace.is_disk_full(exc) and not exc.filename:
            exc.filename = str(path)  # the message names what could not be written
        raise


