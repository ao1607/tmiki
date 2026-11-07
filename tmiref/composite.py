"""Compositing surfaces and blending onto them, on PIL images: what the
Python CLIP compositor (``tmiref.clip``) and MDP2 compositor
(``tmiref.mdp2``) draw with. Moved out of ``tmi.formats.composite`` in
RUST_MIGRATION.md 2d, where tmi kept only the Pillow-free layer buffer and
rectangle helpers (re-exported here, so ``from tmiref import composite``
reaches all of them).

A buffer (``_LayerBuffer``) placed on the canvas, a target that grows as
layers land on it (``_CompositeSurface``), clipping by a base layer's alpha,
and the blend of a buffer onto a surface, which ``tmi_accel`` does
(``blend_rgba``, byte for byte the numpy blend of ``tmiref.clip``).
"""
from __future__ import annotations

from dataclasses import dataclass

from PIL import Image, ImageChops

from tmi import accel
from tmi.formats.composite import (  # noqa: F401  (re-exported)
    BLEND_NORMAL,
    BLEND_PASS_THROUGH,
    _canvas_size,
    _EMPTY_LAYER_BUFFER,
    _EmptyLayerBuffer,
    _intersect_bounds,
    _is_empty_buffer,
    _LayerBuffer,
    _union_bounds,
)


@dataclass
class _CompositeSurface:
    """A compositing target.

    ``canvas_bounds`` is the window being rendered: everything blended onto the
    surface is clipped to it. ``full_bounds`` is the document canvas, which
    layer renderers whose output depends on their own placement (fills,
    vectors, materials, comic frames, フチ) always work in, so that a window
    renders exactly the pixels a whole-canvas render has there (see
    ``render_layered_composite_rgba``'s ``region``). Both are the canvas for
    an ordinary whole-canvas render."""

    canvas_bounds: tuple[int, int, int, int]
    image: Image.Image | None = None
    x: int = 0
    y: int = 0
    fixed: bool = False
    full_bounds: tuple[int, int, int, int] | None = None
    # A fixed surface whose image nobody else holds: steps may replace the
    # image object instead of pasting into it (the caller reads ``image``).
    owned: bool = False

    @classmethod
    def fixed_canvas(cls, image: Image.Image) -> "_CompositeSurface":
        return cls((0, 0, image.width, image.height), image=image, fixed=True)

    @classmethod
    def empty(
        cls,
        canvas_bounds: tuple[int, int, int, int],
        full_bounds: tuple[int, int, int, int] | None = None,
    ) -> "_CompositeSurface":
        return cls(canvas_bounds, full_bounds=full_bounds)

    @property
    def full(self) -> tuple[int, int, int, int]:
        return self.full_bounds or self.canvas_bounds

    @property
    def bounds(self) -> tuple[int, int, int, int] | None:
        if self.image is None:
            return None
        return (self.x, self.y, self.x + self.image.width, self.y + self.image.height)

    def as_buffer(self) -> _LayerBuffer | None:
        if self.image is None:
            return None
        return _LayerBuffer(self.image, self.x, self.y)

    def copy(self) -> "_CompositeSurface":
        return _CompositeSurface(
            self.canvas_bounds,
            image=self.image.copy() if self.image is not None else None,
            x=self.x,
            y=self.y,
            fixed=self.fixed,
            full_bounds=self.full_bounds,
        )

    def ensure(self, bounds: tuple[int, int, int, int]) -> bool:
        clipped = _intersect_bounds(bounds, self.canvas_bounds)
        if clipped is None:
            return False
        if self.fixed:
            return True
        current = self.bounds
        if current is None:
            self.x, self.y = clipped[0], clipped[1]
            self.image = Image.new("RGBA", (clipped[2] - clipped[0], clipped[3] - clipped[1]), (0, 0, 0, 0))
            return True
        union = _union_bounds(current, clipped)
        if union == current:
            return True
        expanded = Image.new("RGBA", (union[2] - union[0], union[3] - union[1]), (0, 0, 0, 0))
        expanded.alpha_composite(self.image, (self.x - union[0], self.y - union[1]))
        self.image = expanded
        self.x, self.y = union[0], union[1]
        return True

    def trim(self) -> None:
        if self.fixed or self.image is None:
            return
        bbox = self.image.getchannel("A").getbbox()
        if bbox is None:
            self.image = None
            self.x = 0
            self.y = 0
            return
        if bbox != (0, 0, self.image.width, self.image.height):
            self.image = self.image.crop(bbox)
            self.x += bbox[0]
            self.y += bbox[1]


def _dilate_bounds(
    bounds: tuple[int, int, int, int],
    margin: int,
    limit: tuple[int, int, int, int],
) -> tuple[int, int, int, int]:
    """``bounds`` grown by ``margin`` on every side, kept inside ``limit``."""
    if margin <= 0:
        return bounds
    grown = (bounds[0] - margin, bounds[1] - margin, bounds[2] + margin, bounds[3] + margin)
    return _intersect_bounds(grown, limit) or bounds


def _clip_buffer_to_bounds(buf: _LayerBuffer, bounds: tuple[int, int, int, int]) -> _LayerBuffer | None:
    clipped = _intersect_bounds(buf.bounds, bounds)
    if clipped is None:
        return None
    if clipped == buf.bounds:
        return buf
    crop = (
        clipped[0] - buf.x,
        clipped[1] - buf.y,
        clipped[2] - buf.x,
        clipped[3] - buf.y,
    )
    return _LayerBuffer(buf.image.crop(crop), clipped[0], clipped[1])


def _trim_buffer(buf: _LayerBuffer) -> _LayerBuffer | None:
    bbox = buf.image.getchannel("A").getbbox()
    if bbox is None:
        return None
    if bbox == (0, 0, buf.image.width, buf.image.height):
        return buf
    return _LayerBuffer(buf.image.crop(bbox), buf.x + bbox[0], buf.y + bbox[1])


def _mask_by_alpha(image: Image.Image, mask_alpha: Image.Image) -> None:
    a = image.getchannel("A")
    image.putalpha(ImageChops.multiply(a, mask_alpha))


def _mask_buffer_by_base_alpha(
    source: _LayerBuffer | _EmptyLayerBuffer,
    base: _LayerBuffer | _EmptyLayerBuffer,
) -> _LayerBuffer | _EmptyLayerBuffer:
    if _is_empty_buffer(source) or _is_empty_buffer(base):
        return _EMPTY_LAYER_BUFFER
    overlap = _intersect_bounds(source.bounds, base.bounds)
    if overlap is None:
        return _EMPTY_LAYER_BUFFER
    source_crop = source.image.crop(
        (
            overlap[0] - source.x,
            overlap[1] - source.y,
            overlap[2] - source.x,
            overlap[3] - source.y,
        )
    )
    base_alpha = base.image.getchannel("A").crop(
        (
            overlap[0] - base.x,
            overlap[1] - base.y,
            overlap[2] - base.x,
            overlap[3] - base.y,
        )
    )
    _mask_by_alpha(source_crop, base_alpha)
    return _trim_buffer(_LayerBuffer(source_crop, overlap[0], overlap[1])) or _EMPTY_LAYER_BUFFER


def _blend_buffer_onto_surface(
    target: _CompositeSurface,
    source: _LayerBuffer | _EmptyLayerBuffer,
    mode: str,
    opacity: float,
) -> None:
    if opacity <= 0 or _is_empty_buffer(source):
        return
    source = _clip_buffer_to_bounds(source, target.canvas_bounds)
    if source is None:
        return
    if not target.ensure(source.bounds) or target.image is None:
        return
    _blend_onto_at(target.image, source.image, mode, opacity, (source.x - target.x, source.y - target.y))
    # No trim here: re-scanning the whole accumulated surface after every
    # layer is slow; callers trim once per group.


def _blend_onto(target: Image.Image, source: Image.Image, mode: str, opacity: float) -> None:
    """Composite ``source`` (RGBA) onto ``target`` (RGBA, same size) using the
    given blend mode and layer opacity. Mutates ``target`` in place."""
    _blend_onto_at(target, source, mode, opacity, (0, 0))


def _blend_onto_at(
    target: Image.Image,
    source: Image.Image,
    mode: str,
    opacity: float,
    offset: tuple[int, int],
) -> None:
    """Composite ``source`` onto ``target`` at ``offset`` in blend ``mode``
    (W3C source-over with a per-mode colour blend; ``tmi_accel.blend_rgba``,
    byte for byte the numpy blend of ``tmiref``). A mode tmi_accel does not
    know blends as normal, as the numpy blend did."""
    if opacity <= 0:
        return
    tw, th = target.size
    lw, lh = source.size
    ox, oy = offset
    sx0 = max(0, -ox)
    sy0 = max(0, -oy)
    sx1 = min(lw, tw - ox)
    sy1 = min(lh, th - oy)
    if sx0 >= sx1 or sy0 >= sy1:
        return
    dx = max(0, ox)
    dy = max(0, oy)
    if sx0 or sy0 or sx1 != lw or sy1 != lh:
        source = source.crop((sx0, sy0, sx1, sy1))
    if (mode in (BLEND_NORMAL, BLEND_PASS_THROUGH)) and opacity >= 0.999:
        if source.getchannel("A").getextrema() == (255, 255):
            # Pillow's alpha_composite gives a source pixel of alpha 255
            # back unchanged, whatever lies under it, so an opaque source is
            # a paste, at a fraction of the cost.
            target.paste(source, (dx, dy))
            return
        target.alpha_composite(source, (dx, dy))
        return
    # A fully transparent source pixel leaves the backdrop untouched, so the
    # blend runs over the source's non-transparent bounding box only.
    bbox = source.getchannel("A").getbbox()
    if bbox is None:
        return
    if bbox == (0, 0, source.width, source.height):
        target_box = (dx, dy, dx + source.width, dy + source.height)
        region_target, region_source = target.crop(target_box), source
        paste_at = (dx, dy)
    else:
        target_box = (dx + bbox[0], dy + bbox[1], dx + bbox[2], dy + bbox[3])
        region_target, region_source = target.crop(target_box), source.crop(bbox)
        paste_at = (target_box[0], target_box[1])
    native = accel.native()
    if mode not in _known_modes():
        mode = BLEND_NORMAL
    out = native.blend_rgba(
        region_target.tobytes(), region_source.tobytes(), region_source.width, region_source.height, mode, float(opacity)
    )
    target.paste(Image.frombytes("RGBA", region_source.size, out), paste_at)


_KNOWN_MODES: frozenset[str] | None = None


def _known_modes() -> frozenset[str]:
    global _KNOWN_MODES
    if _KNOWN_MODES is None:
        _KNOWN_MODES = frozenset(accel.native().supported_blend_modes())
    return _KNOWN_MODES
