"""Layer buffers and rectangle helpers of the CLIP reader.

``_LayerBuffer`` is an RGBA image placed on the canvas (an offscreen's
painted rectangle, ``clip._decode_offscreen``); the bounds helpers clip and
join canvas rectangles (``clip_partial``'s dirty area, render windows).
Nothing here imports Pillow: CLIP files are composited by
``tmi_accel.render_clip`` in one call (``clip.render_layered_composite_rgba``)
and MDP2 files by ``tmi_accel.render_mdp2``. The compositing surface and
the blend helpers the Python compositors drew with live in the reference
package (``tmiref.composite``).
"""
from __future__ import annotations

from dataclasses import dataclass
from typing import Any

BLEND_NORMAL = "normal"
BLEND_PASS_THROUGH = "pass_through"


@dataclass
class _LayerBuffer:
    """``image`` (a ``tmi.raster.Rgba``, or in ``tmiref`` a PIL image) with
    its top-left corner at canvas ``(x, y)``."""

    image: Any
    x: int
    y: int

    @property
    def bounds(self) -> tuple[int, int, int, int]:
        return (self.x, self.y, self.x + self.image.width, self.y + self.image.height)

    def copy(self) -> "_LayerBuffer":
        return _LayerBuffer(self.image.copy(), self.x, self.y)


@dataclass(frozen=True)
class _EmptyLayerBuffer:
    pass


_EMPTY_LAYER_BUFFER = _EmptyLayerBuffer()


def _is_empty_buffer(buf: object) -> bool:
    return isinstance(buf, _EmptyLayerBuffer)


def _intersect_bounds(
    a: tuple[int, int, int, int],
    b: tuple[int, int, int, int],
) -> tuple[int, int, int, int] | None:
    x0 = max(a[0], b[0])
    y0 = max(a[1], b[1])
    x1 = min(a[2], b[2])
    y1 = min(a[3], b[3])
    if x0 >= x1 or y0 >= y1:
        return None
    return (x0, y0, x1, y1)


def _union_bounds(
    a: tuple[int, int, int, int],
    b: tuple[int, int, int, int],
) -> tuple[int, int, int, int]:
    return (min(a[0], b[0]), min(a[1], b[1]), max(a[2], b[2]), max(a[3], b[3]))


def _canvas_size(bounds: tuple[int, int, int, int]) -> tuple[int, int]:
    return (bounds[2] - bounds[0], bounds[3] - bounds[1])
