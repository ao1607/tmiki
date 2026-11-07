"""``TMI_RENDER_TRACE=<path>``: a per-node trace of CLIP compositing.

Imported only when the variable is set (clip.py tests a flag read at import).
Each record is one JSON line appended to the file:

    {"phase": "prerender" | "clip" | "apply",
     "band": [x0, y0, x1, y1],     # the window of the outermost group render
     "main_id": 12,                # the sibling's MainId
     "window": [x0, y0, x1, y1],   # the group's target window (canvas_bounds)
     "bounds": [x0, y0, x1, y1],   # the buffer's / surface's extent, or null
     "sha256": "..."}              # over "x0,y0,x1,y1:" + RGBA bytes;
                                   # "empty" for an empty buffer, null for none

* ``prerender``: each sibling's buffer after it is pre-rendered;
* ``clip``: a clipped sibling's buffer after its base's alpha is applied;
* ``apply``: the group's target after each sibling is composited onto it.

Groups nest; a record belongs to the outermost group, whose window is the
``band``. A banded render's records come band by band from the top, each
band's in execution order: the file is the same on every run.

tmi_accel's ``render_clip(trace=True)`` returns these records and tmi writes
them here; the reference renderer (``tmiref``) writes the same records
(tmiref/render_trace.py), so ``tools/render_diff.py`` can walk the two
traces in step and find where they part.
"""
from __future__ import annotations

import json
import os
import threading

PATH = os.environ.get("TMI_RENDER_TRACE", "")

_write_lock = threading.Lock()


def write(records: list) -> None:
    """Append ``records`` (dicts as above) to the trace file."""
    if not records or not PATH:
        return
    text = "".join(json.dumps(rec, separators=(",", ":")) + "\n" for rec in records)
    with _write_lock:
        with open(PATH, "a", encoding="utf-8", newline="\n") as fh:
            fh.write(text)
