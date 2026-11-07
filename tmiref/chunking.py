"""Content-defined chunking cut points in numpy: the reference tmi_accel's
``cdc_cuts`` is tested against (tmi/chunking.py has the scheme)."""
from __future__ import annotations

import numpy as np

from tmi.chunking import _WINDOW, CDC_BITS, GEAR_TABLE, PIECE_MIN, _greedy_cuts

_BLOCK = 4 * 1024 * 1024
GEAR = np.array(GEAR_TABLE, dtype=np.uint64)


def cdc_cuts_numpy(data: bytes | memoryview, start: int = 0, end: int | None = None) -> list[int]:
    """``tmi.chunking.cdc_cuts``: the rolling sums computed with numpy in
    blocks of 4 MiB."""
    end = len(data) if end is None else end
    if end - start <= PIECE_MIN:
        return [end]
    arr = np.frombuffer(data, dtype=np.uint8)
    candidates: list[np.ndarray] = []
    shift = np.uint64(64 - CDC_BITS)
    for block in range(start, end, _BLOCK):
        lo = max(start, block - _WINDOW)
        hi = min(end, block + _BLOCK)
        if hi - lo <= _WINDOW:
            continue
        sums = np.cumsum(GEAR[arr[lo:hi]], dtype=np.uint64)
        # window[k] = bytes lo+k+1 .. lo+k+WINDOW; a cut lands after its last byte
        window = sums[_WINDOW:] - sums[:-_WINDOW]
        candidates.append(np.flatnonzero((window >> shift) == 0) + (lo + _WINDOW + 1))
    found = np.concatenate(candidates).tolist() if candidates else []
    return _greedy_cuts(found, start, end)
