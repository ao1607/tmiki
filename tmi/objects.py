from __future__ import annotations

import hashlib
import json
import os
import re
import tempfile
import zlib
from pathlib import Path
from typing import Any, Callable, Iterator

from . import diskspace
from . import recpath as _recpath


def canonical_json_bytes(value: Any) -> bytes:
    return json.dumps(
        value,
        ensure_ascii=False,
        sort_keys=True,
        separators=(",", ":"),
    ).encode("utf-8")


# How deep arrays and objects of a stored object may nest: deeper is refused
# (tmi-store's json.rs `MAX_DEPTH` is the same rule). Real objects nest a
# dozen levels; CPython's parser would die of RecursionError near 1000.
MAX_JSON_DEPTH = 256

_JSON_STRING_RE = re.compile(rb'"(?:[^"\\]|\\.)*"', re.DOTALL)
_JSON_BRACKETS = bytes.maketrans(b"[{]}", b"\x01\x01\xff\xff")
_JSON_NOT_BRACKETS = bytes(b for b in range(256) if b not in b"[{]}")


class JsonTooDeepError(ValueError):
    """A JSON object nested past ``MAX_JSON_DEPTH``."""


def json_depth_exceeds(payload: bytes, limit: int = MAX_JSON_DEPTH) -> bool:
    """Whether arrays / objects nest deeper than ``limit`` (strings skipped).
    A cheap bracket count rules most payloads out before any scan."""
    if payload.count(b"[") + payload.count(b"{") <= limit:
        return False
    stripped = _JSON_STRING_RE.sub(b"", payload).translate(_JSON_BRACKETS, _JSON_NOT_BRACKETS)
    depth = 0
    for step in stripped:
        depth += 1 if step == 1 else -1
        if depth > limit:
            return True
    return False


def parse_json_bytes(payload: bytes) -> Any:
    if json_depth_exceeds(payload):
        raise JsonTooDeepError(f"JSON nested deeper than {MAX_JSON_DEPTH} levels")
    return json.loads(payload.decode("utf-8"))


class UnsafeTreeError(RuntimeError):
    """A tree that records a path ``recpath`` refuses: not received. (Not a
    ValueError: the transports word those as a corrupt transfer.)"""


def _check_tree_paths(oid: str, payload: bytes) -> None:
    """The receiving gate (``write_raw``): a repository tree's entries name
    files the working tree will be written with, so a tree that records a path
    breaking ``recpath``'s rule is refused before anything of it is stored.
    Every tree is read (a forged one need not look like tmi's own JSON); a
    payload that is not JSON is left for the reader to refuse."""
    try:
        tree = parse_json_bytes(payload)
    except (ValueError, RecursionError):
        return
    if not isinstance(tree, dict) or tree.get("kind") != "repository":
        return
    entries = tree.get("entries")
    if not isinstance(entries, list):
        return
    for entry in entries:
        path = entry.get("path") if isinstance(entry, dict) else None
        if not isinstance(path, str):
            continue
        problem = _recpath.record_path_problem(path)
        if problem is not None:
            raise UnsafeTreeError(
                f"refusing tree {oid[:12]}: it records an invalid path: {_recpath.display_path(path)} ({problem})"
            )


_SAMPLE_SIZE = 64 * 1024
_SAMPLE_COUNT = 3
# Below this estimated ratio compression pays for itself; above it the payload
# is already compressed (CLIP/MDP sources ~0.94, big PNG layers ~0.99) and
# deflating it costs seconds (14s for one CLIP add on a 4-core laptop) to
# save a few percent of disk.
_COMPRESS_IF_RATIO_BELOW = 0.9
# Below this a blob is stored whole: splitting saves little and costs objects.
CHUNKED_MIN_SIZE = 1024 * 1024


def _zlib_level_for(data: bytes) -> int:
    """Level 1 when deflate helps, else level 0 (stored blocks).

    Level 0 is still a zlib stream, so readers need no special case. The
    estimate deflates up to three evenly spaced 64 KiB samples instead of the
    whole payload, which tracks the full ratio closely on real CLIP/MDP/PNG
    data.
    """
    if len(data) <= 2 * _SAMPLE_SIZE:
        return 1
    # Fewer samples for pieces of a chunked blob (~200 KiB), which would
    # otherwise be deflated whole just to learn they are incompressible.
    count = max(1, min(_SAMPLE_COUNT, len(data) // (8 * _SAMPLE_SIZE)))
    step = (len(data) - _SAMPLE_SIZE) // max(1, count - 1)
    compressed = sum(len(zlib.compress(data[i * step:i * step + _SAMPLE_SIZE], 1)) for i in range(count))
    return 1 if compressed < _COMPRESS_IF_RATIO_BELOW * _SAMPLE_SIZE * count else 0


# Caps on what one object may inflate to (tmi-store's limits.rs has the same
# numbers). A hostile object is tiny stored and huge inflated, so every object
# is inflated in pieces and refused past its type's cap. Commits, trees, tags
# and chunk manifests are JSON the readers hold whole.
MAX_META_OBJECT_SIZE = 32 * 1024 * 1024
# A plain blob: a whole file under CHUNKED_MIN_SIZE, a chunk, a chunked
# payload's glue.
MAX_BLOB_SIZE = 1024 * 1024 * 1024
# The declared size of a chunked blob, and how many parts its manifest lists.
MAX_CHUNKED_SIZE = 4 * 1024 * 1024 * 1024
MAX_CHUNK_PARTS = 262_144
_MAX_TYPE_LENGTH = 32
_JSON_TYPES = frozenset({"commit", "tag", "tree", "chunked"})
_OID_RE = re.compile(r"[0-9a-f]{64}\Z")
_INFLATE_STEP = 1 << 16


def size_cap_for(object_type: str) -> int:
    return MAX_BLOB_SIZE if object_type == "blob" else MAX_META_OBJECT_SIZE


def _inflate_pieces(raw: bytes) -> Iterator[bytes]:
    """``zlib.decompress(raw)`` in pieces of at most 64 KiB: one zlib stream,
    bytes after its end ignored, a truncated stream an error."""
    inflater = zlib.decompressobj()
    piece = inflater.decompress(raw, _INFLATE_STEP)
    while True:
        if piece:
            yield piece
        if inflater.eof:
            return
        if not inflater.unconsumed_tail:
            raise zlib.error("Error -5 while decompressing data: incomplete or truncated stream")
        piece = inflater.decompress(inflater.unconsumed_tail, _INFLATE_STEP)


def inflate_object(raw: bytes, keep_blob: bool = True) -> tuple[str, bytes, str]:
    """Inflate a stored object, returning ``(type, payload, oid)``. The oid is
    hashed while inflating; the payload of a blob is dropped (b"") when
    ``keep_blob`` is false. An object past its type's cap is a ValueError
    before the rest of it is inflated."""
    digest = hashlib.sha256()
    head = bytearray()
    object_type: str | None = None
    limit = 0
    size = 0
    kept: list[bytes] = []
    for piece in _inflate_pieces(raw):
        if object_type is None:
            head += piece
            nul = head.find(b"\0")
            if nul < 0:
                if len(head) > _MAX_TYPE_LENGTH:
                    raise ValueError("object type header too long")
                continue
            object_type = head[:nul].decode("ascii")
            digest.update(bytes(head[:nul + 1]))
            piece = bytes(head[nul + 1:])
            limit = size_cap_for(object_type)
        digest.update(piece)
        size += len(piece)
        if size > limit:
            raise ValueError(f"{object_type} object is larger than {limit} bytes")
        if keep_blob or object_type != "blob":
            kept.append(piece)
    if object_type is None:
        raise ValueError("not enough values to unpack (expected 2, got 1)")
    return object_type, b"".join(kept), digest.hexdigest()


def check_chunk_manifest(manifest: Any) -> None:
    """The shape of a chunked object's manifest, checked when it is received
    and when it is read: ``size`` within ``MAX_CHUNKED_SIZE``, at most
    ``MAX_CHUNK_PARTS`` parts, each an object id or a count of glue bytes, the
    glue counts within ``size``. What the parts add up to is checked as they
    are read."""
    if not isinstance(manifest, dict):
        raise ValueError("chunked manifest is not an object")
    size = manifest.get("size")
    parts = manifest.get("parts")
    if type(size) is not int or not 0 <= size <= MAX_CHUNKED_SIZE:
        raise ValueError("chunked manifest has a bad size")
    if not isinstance(parts, list) or len(parts) > MAX_CHUNK_PARTS:
        raise ValueError("chunked manifest has a bad part list")
    glue = manifest.get("glue")
    if glue and not (isinstance(glue, str) and _OID_RE.match(glue)):
        raise ValueError("chunked manifest has a bad glue id")
    glue_bytes = 0
    for part in parts:
        if isinstance(part, str):
            if not _OID_RE.match(part):
                raise ValueError("chunked manifest names a part that is not an object id")
        elif type(part) is int and part >= 0:
            glue_bytes += part
        else:
            raise ValueError("chunked manifest has a bad part")
    if glue_bytes > size:
        raise ValueError("chunked manifest takes more glue than its size")


class ObjectStore:
    def __init__(self, objects_dir: Path):
        self.objects_dir = objects_dir

    def object_id(self, object_type: str, payload: bytes) -> str:
        digest = hashlib.sha256()
        digest.update(object_type.encode("ascii") + b"\0")
        digest.update(payload)
        return digest.hexdigest()

    def write(self, object_type: str, payload: bytes) -> str:
        return self._write_as(self.object_id(object_type, payload), object_type, payload)

    def _write_as(self, oid: str, object_type: str, payload: bytes) -> str:
        path = self.path_for(oid)
        if not path.exists():
            data = object_type.encode("ascii") + b"\0" + payload
            self._write_file_atomic(path, zlib.compress(data, _zlib_level_for(data)))
        return oid

    def write_raw(self, oid: str, raw: bytes) -> None:
        """Store an already-encoded (compressed) object under ``oid``.

        Used by transports moving objects between stores. Verifies that the
        bytes really decode and hash to ``oid`` before storing — a corrupt or
        truncated transfer must never land under a valid object name, because
        ``copy_missing_from``/fetch skip existing paths and would otherwise
        never repair it. The write itself is atomic (temp + rename) so an
        interrupted copy leaves nothing behind.
        """
        try:
            object_type, payload, actual = inflate_object(raw, keep_blob=False)
        except Exception as exc:
            raise ValueError(f"corrupt object data for {oid}: {exc}") from exc
        if actual != oid:
            raise ValueError(f"object hash mismatch for {oid}: got {actual}")
        if object_type in _JSON_TYPES:
            # A payload nested past MAX_JSON_DEPTH would overflow a reader's
            # stack and a malformed chunk manifest would make every read of
            # the blob costly: neither is stored.
            if json_depth_exceeds(payload):
                raise ValueError(f"refusing {object_type} {oid[:12]}: JSON nested deeper than {MAX_JSON_DEPTH} levels")
            if object_type == "chunked":
                try:
                    check_chunk_manifest(parse_json_bytes(payload))
                except ValueError as exc:
                    raise ValueError(f"refusing chunked {oid[:12]}: {exc}") from exc
        if object_type == "tree":
            _check_tree_paths(oid, payload)
        path = self.path_for(oid)
        if path.exists():
            return
        self._write_file_atomic(path, raw)

    def _write_file_atomic(self, path: Path, data: bytes) -> None:
        path.parent.mkdir(parents=True, exist_ok=True)
        diskspace.before_new_file()
        fd, tmp_name = tempfile.mkstemp(prefix=path.name + ".", suffix=".tmp", dir=path.parent)
        tmp = Path(tmp_name)
        try:
            with os.fdopen(fd, "wb") as handle:
                handle.write(data)
            try:
                tmp.replace(path)
                diskspace.note_new_file(path)
            except PermissionError:
                # Windows refuses to replace a file another writer is
                # finishing; objects are content-addressed, so theirs is ours.
                if not path.exists():
                    raise
        except OSError as exc:
            if diskspace.is_disk_full(exc) and not exc.filename:
                exc.filename = str(path)  # the message names what could not be written
            raise
        finally:
            tmp.unlink(missing_ok=True)

    def write_json(self, object_type: str, value: Any) -> str:
        return self.write(object_type, canonical_json_bytes(value))

    def write_large_blob(self, payload: bytes) -> str:
        """Store a blob that later versions are likely to share most of.

        Payloads of CHUNKED_MIN_SIZE or more become a ``chunked`` object: the
        shareable pieces (see ``chunking.storage_segments``) as blobs of their
        own, the volatile bytes together in one blob, and a manifest
        ``{"size": n, "glue": oid|null, "parts": [...]}`` whose parts are a
        piece's oid or a count of bytes to take next from the glue blob.
        ``read_blob`` reassembles it, so callers see a blob either way."""
        return self.write_large_blobs([payload])[0]

    def write_large_blobs(self, payloads: list[bytes]) -> list[str]:
        """``write_large_blob`` for each payload, returning the oids in order.

        Segmenting, hashing, deflate and file writes all release the GIL, so
        every payload's pieces — a file's source, layer blobs and composite
        at once — share one thread pool instead of being written one blob
        after another. Objects are content-addressed, so the result does not
        depend on the order the pool finishes in."""
        if len(payloads) == 1 and len(payloads[0]) < CHUNKED_MIN_SIZE:
            return [self.write("blob", payloads[0])]
        from concurrent.futures import ThreadPoolExecutor

        from .chunking import storage_segments

        def segments_for(payload: bytes) -> list[Any] | None:
            if len(payload) < CHUNKED_MIN_SIZE:
                return None
            segments = storage_segments(payload)
            return segments if sum(1 for seg in segments if seg.shareable) >= 2 else None

        with ThreadPoolExecutor(max_workers=min(8, os.cpu_count() or 1)) as pool:
            plans = list(pool.map(segments_for, payloads))
            # Byte spans each stored as a "blob": a whole payload, a shareable
            # piece or a chunked payload's glue (its volatile bytes joined).
            spans: list[bytes | memoryview] = []
            layouts: list[tuple[int, list[Any], int | None] | int] = []
            for payload, segments in zip(payloads, plans):
                if segments is None:
                    layouts.append(len(spans))
                    spans.append(payload)
                    continue
                view = memoryview(payload)
                parts: list[Any] = []
                for seg in segments:
                    if seg.shareable:
                        parts.append(len(spans))
                        spans.append(view[seg.start:seg.end])
                    else:
                        parts.append(seg.end - seg.start)
                glue = b"".join(view[seg.start:seg.end] for seg in segments if not seg.shareable)
                glue_span = None
                if glue:
                    glue_span = len(spans)
                    spans.append(glue)
                # parts: a span index per shareable segment, else its byte count
                layouts.append((len(payload), parts, glue_span))
            oids = list(pool.map(lambda span: self.object_id("blob", span), spans))
            # Identical spans (runs of empty tiles) are written once.
            unique: dict[str, bytes | memoryview] = {}
            for oid, span in zip(oids, spans):
                unique.setdefault(oid, span)
            list(pool.map(lambda item: self._write_as(item[0], "blob", item[1]), unique.items()))
        out: list[str] = []
        for layout, segments in zip(layouts, plans):
            if isinstance(layout, int):
                out.append(oids[layout])
                continue
            size, parts, glue_span = layout
            manifest_parts = [
                oids[part] if seg.shareable else part
                for part, seg in zip(parts, segments)
            ]
            glue_oid = oids[glue_span] if glue_span is not None else None
            out.append(self.write_json("chunked", {"size": size, "glue": glue_oid, "parts": manifest_parts}))
        return out

    def read(self, oid: str) -> tuple[str, bytes]:
        path = self.path_for(oid)
        if not path.exists():
            raise FileNotFoundError(f"missing object {oid}")
        object_type, payload, actual = inflate_object(path.read_bytes())
        if actual != oid:
            raise ValueError(f"object hash mismatch for {oid}: got {actual}")
        return object_type, payload

    def read_json(self, oid: str, expected_type: str | None = None) -> Any:
        object_type, payload = self.read(oid)
        if expected_type and object_type != expected_type:
            raise ValueError(f"object {oid} is {object_type}, expected {expected_type}")
        return parse_json_bytes(payload)

    def read_blob(self, oid: str) -> bytes:
        object_type, payload = self.read(oid)
        if object_type == "chunked":
            return self._assemble_chunked(oid, parse_json_bytes(payload))
        if object_type != "blob":
            raise ValueError(f"object {oid} is {object_type}, expected blob")
        return payload

    def _assemble_chunked(self, oid: str, manifest: dict[str, Any]) -> bytes:
        from concurrent.futures import ThreadPoolExecutor

        try:
            check_chunk_manifest(manifest)
        except ValueError as exc:
            raise ValueError(f"chunked object {oid} has a malformed manifest: {exc}") from exc
        size = manifest["size"]

        def read_plain(part_oid: str) -> bytes:
            object_type, payload = self.read(part_oid)
            if object_type != "blob":
                raise ValueError(f"object {part_oid} is {object_type}, expected blob (chunked object {oid})")
            return payload

        def too_big() -> ValueError:
            return ValueError(f"chunked object {oid} reassembles to more than {size} bytes")

        with ThreadPoolExecutor(max_workers=min(8, os.cpu_count() or 1)) as pool:
            glue = memoryview(read_plain(manifest["glue"]) if manifest.get("glue") else b"")
            # The parts are read a few at a time and the running total is held
            # to the declared size, so a manifest that repeats a part or
            # lists far more than it declares stops early.
            total = 0
            refs = [part for part in manifest["parts"] if isinstance(part, str)]
            read: list[bytes] = []
            for start in range(0, len(refs), 16):
                for payload in pool.map(read_plain, refs[start:start + 16]):
                    total += len(payload)
                    if total > size:
                        raise too_big()
                    read.append(payload)
        pieces: list[bytes | memoryview] = []
        pos = 0
        index = 0
        for part in manifest["parts"]:
            if isinstance(part, int):
                pieces.append(glue[pos:pos + part])
                pos += part
            else:
                pieces.append(read[index])
                index += 1
        out = b"".join(pieces)
        if len(out) != size or pos != len(glue):
            raise ValueError(f"chunked object {oid} reassembles to {len(out)} bytes, expected {size}")
        return out

    def path_for(self, oid: str) -> Path:
        return self.objects_dir / oid[:2] / oid[2:]

    def iter_object_ids(self) -> list[str]:
        if not self.objects_dir.exists():
            return []
        ids: list[str] = []
        for subdir in self.objects_dir.iterdir():
            if not subdir.is_dir() or len(subdir.name) != 2:
                continue
            for path in subdir.iterdir():
                if path.is_file() and len(path.name) == 62:
                    ids.append(subdir.name + path.name)
        return sorted(ids)

    def copy_missing_from(
        self,
        other: "ObjectStore",
        on_progress: Callable[[str], None] | None = None,
    ) -> int:
        if on_progress:
            on_progress("enumerating remote objects")
        oids = other.iter_object_ids()
        total = len(oids)
        copied = 0
        for idx, oid in enumerate(oids, start=1):
            dst = self.path_for(oid)
            if dst.exists():
                continue
            if on_progress:
                on_progress(f"copying object [{idx}/{total}] {oid[:12]}")
            src = other.path_for(oid)
            self.write_raw(oid, src.read_bytes())
            copied += 1
        return copied
