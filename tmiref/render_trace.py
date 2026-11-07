"""The reference renderer's ``TMI_RENDER_TRACE`` records (the format:
tmi/formats/render_trace.py).

Imported only when the variable is set (tmiref/clip.py tests a flag read at
import). Records are written through tmi's writer, so both renderers append
the same lines to the same file.
"""
from __future__ import annotations

import hashlib
import threading
from typing import Any

from tmi.formats.render_trace import write as _write

_local = threading.local()


def digest(image: Any, bounds: tuple[int, int, int, int]) -> str:
    rgba = image if image.mode == "RGBA" else image.convert("RGBA")
    h = hashlib.sha256("{},{},{},{}:".format(*bounds).encode("ascii"))
    h.update(rgba.tobytes())
    return h.hexdigest()


class Group:
    """One ``_composite_group_surface`` call being traced."""

    def __init__(self, clip: Any, target: Any) -> None:
        self.clip = clip
        self.window = list(target.canvas_bounds)
        self.depth = getattr(_local, "depth", 0)
        if self.depth == 0:
            _local.records = []
            _local.band = self.window
        self.records: list = _local.records
        self.band: list = _local.band

    def __enter__(self) -> "Group":
        _local.depth = self.depth + 1
        return self

    def __exit__(self, *exc) -> None:
        _local.depth = self.depth
        if self.depth == 0:
            _local.records = None
            if exc[0] is None:
                _emit(self.clip, self.band, self.records)

    def _record(self, phase: str, node: Any, bounds, sha) -> None:
        self.records.append({
            "phase": phase,
            "band": self.band,
            "main_id": getattr(node, "main_id", None),
            "window": self.window,
            "bounds": list(bounds) if bounds is not None else None,
            "sha256": sha,
        })

    def buffer(self, phase: str, node: Any, buf: Any) -> None:
        image = getattr(buf, "image", None)
        if image is None:
            self._record(phase, node, None, "empty")
            return
        bounds = buf.bounds
        self._record(phase, node, bounds, digest(image, bounds))

    def surface(self, phase: str, node: Any, target: Any) -> None:
        bounds = target.bounds
        if bounds is None:
            self._record(phase, node, None, None)
            return
        self._record(phase, node, bounds, digest(target.image, bounds))


def group(clip: Any, target: Any) -> Group:
    return Group(clip, target)


def _emit(clip: Any, band: list, records: list) -> None:
    memo = getattr(clip, "render_memo", None)
    blocks = getattr(memo, "trace_blocks", None)
    if blocks is not None:
        with memo._lock:
            blocks.setdefault(tuple(band), []).extend(records)
        return
    _write(records)


def write_blocks(blocks: dict) -> None:
    """A banded render's records, band by band from the top."""
    _write([rec for key in sorted(blocks, key=lambda b: (b[1], b[0], b[3], b[2])) for rec in blocks[key]])
