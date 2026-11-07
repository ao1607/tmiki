"""RGBA pixels without Pillow (RUST_MIGRATION.md 2b).

``Rgba`` is a packed RGBA8 image (rows top to bottom, 4 bytes a pixel);
``Sparse`` is a layer that is transparent black outside one rectangle (a
layer blob, decoded without expanding it to the full canvas). The
operations are tmi_accel's Pillow-exact kernels (``tmi-pillow``,
``tmi-image``); each gives the bytes Pillow gave for the same call. Nothing
here imports Pillow: a command that only reads, decodes, compares or writes
PNG / JPEG pixels runs without it.

Which files are images at all is tmi's own rule, ``import_decode`` (PLUGINS.md
P0e): what tmi_accel's decoders read is an image, anything else an opaque
blob; Pillow takes no part in that verdict. ``decode`` (for data tmi has
stored itself) still falls back to Pillow for what tmi_accel leaves to it:
a stored image in a format other than PNG and JPEG, an animated PNG, a JPEG
libjpeg warns about, and the metadata (dpi, ICC presence) of headers this
module does not read with certainty (an MPO, a Photoshop or truncated
marker, an EXIF block it cannot fully follow).

The objects behave like the read-only part of an RGBA ``PIL.Image`` that tmi
uses (``mode``, ``size``, ``width``, ``height``, ``info``, ``tobytes()``,
``getpixel``), so callers that only read pixels take either.
"""
from __future__ import annotations

import io
import struct
import sys
from dataclasses import dataclass
from typing import Any, Iterable

from . import pixel_limit as _pixel_limit

PNG_SIGNATURE = b"\x89PNG\r\n\x1a\n"


def _native():
    from . import accel

    return accel.native()


class Rgba:
    """A ``width`` x ``height`` RGBA8 image over ``data`` (bytes-like).
    Treat it as immutable: operations return new images."""

    __slots__ = ("width", "height", "data", "info")
    mode = "RGBA"

    def __init__(self, width: int, height: int, data: Any = None) -> None:
        self.width = int(width)
        self.height = int(height)
        if data is None:
            data = bytes(self.width * self.height * 4)
        elif len(data) != self.width * self.height * 4:
            raise ValueError(f"RGBA buffer of {len(data)} bytes is not {self.width}x{self.height}")
        self.data = data
        self.info: dict = {}

    @property
    def size(self) -> tuple[int, int]:
        return self.width, self.height

    def tobytes(self) -> bytes:
        return self.data if isinstance(self.data, bytes) else bytes(self.data)

    def getpixel(self, xy: tuple[int, int]) -> tuple[int, int, int, int]:
        x, y = xy
        i = (y * self.width + x) * 4
        return tuple(self.data[i:i + 4])  # type: ignore[return-value]

    def __repr__(self) -> str:
        return f"<tmi.raster.Rgba {self.width}x{self.height}>"

    # ── conversions ──

    @classmethod
    def blank(cls, width: int, height: int, fill: tuple[int, int, int, int] = (0, 0, 0, 0)) -> "Rgba":
        if fill == (0, 0, 0, 0):
            return cls(width, height)
        return cls(width, height, bytes(fill) * (width * height))

    def to_pil(self):
        """An RGBA ``PIL.Image`` over a copy of the pixels."""
        from PIL import Image

        return Image.frombytes("RGBA", self.size, self.tobytes())

    @property
    def __array_interface__(self) -> dict:
        """``numpy.asarray(image)``: a read-only (height, width, 4) uint8
        array, as for an RGBA PIL image (numpy itself is not imported)."""
        return {"version": 3, "shape": (self.height, self.width, 4), "typestr": "|u1", "data": self.tobytes()}

    def dense(self) -> "Rgba":
        return self

    @property
    def rect(self) -> tuple[int, int, int, int] | None:
        return (0, 0, self.width, self.height) if self.width and self.height else None

    # ── operations (Pillow's, byte for byte) ──

    def crop(self, box: Iterable[int]) -> "Rgba":
        """``Image.crop(box)``: transparent black outside the image."""
        x0, y0, x1, y1 = (int(v) for v in box)
        if (x0, y0, x1, y1) == (0, 0, self.width, self.height):
            return self
        return Rgba(x1 - x0, y1 - y0, _native().rgba_crop(self.data, self.width, self.height, (x0, y0, x1, y1)))

    def region(self, box: Iterable[int]) -> "Rgba":
        return self.crop(box)

    def resize(self, size: tuple[int, int], box: tuple[float, float, float, float] | None = None) -> "Rgba":
        """``image.resize(size, LANCZOS, box=box)`` (the image itself when
        nothing changes)."""
        size = (int(size[0]), int(size[1]))
        if box is None and size == self.size:
            return self
        out = _native().rgba_resize(self.data, self.width, self.height, size, None if box is None else tuple(map(float, box)))
        return Rgba(size[0], size[1], out)

    def alpha_min(self) -> int:
        """``getchannel("A").getextrema()[0]``."""
        return _native().rgba_alpha_min(self.data)

    def alpha_bbox(self) -> tuple[int, int, int, int] | None:
        """``getbbox()``: the box of the non-zero alphas."""
        return _native().rgba_alpha_bbox(self.data, self.width)

    def bbox(self) -> tuple[int, int, int, int] | None:
        """``getbbox(alpha_only=False)``."""
        return _native().rgba_bbox(self.data, self.width)


class Sparse:
    """A ``size`` layer, transparent black except the ``crop`` at ``(x0,
    y0)`` (None: all of it transparent black). What a layer blob holds."""

    __slots__ = ("width", "height", "x0", "y0", "crop_image", "info")
    mode = "RGBA"

    def __init__(self, size: tuple[int, int], x0: int = 0, y0: int = 0, crop: Rgba | None = None) -> None:
        self.width, self.height = int(size[0]), int(size[1])
        self.x0, self.y0 = int(x0), int(y0)
        self.crop_image = crop if crop is not None and crop.width and crop.height else None
        self.info: dict = {}

    @property
    def size(self) -> tuple[int, int]:
        return self.width, self.height

    @property
    def rect(self) -> tuple[int, int, int, int] | None:
        c = self.crop_image
        return None if c is None else (self.x0, self.y0, self.x0 + c.width, self.y0 + c.height)

    def region(self, box: Iterable[int]) -> Rgba:
        """The pixels of ``box`` (canvas coordinates; transparent black
        outside the layer)."""
        x0, y0, x1, y1 = (int(v) for v in box)
        c = self.crop_image
        if c is None:
            return Rgba(x1 - x0, y1 - y0)
        # The layer outside its own size is transparent black too.
        bx0, by0 = max(x0, 0), max(y0, 0)
        bx1, by1 = min(x1, self.width), min(y1, self.height)
        if (x0, y0, x1, y1) == self.rect:
            return c
        if bx0 >= bx1 or by0 >= by1:
            return Rgba(x1 - x0, y1 - y0)
        if (bx0, by0, bx1, by1) == (x0, y0, x1, y1):
            return c.crop((x0 - self.x0, y0 - self.y0, x1 - self.x0, y1 - self.y0))
        inner = c.crop((bx0 - self.x0, by0 - self.y0, bx1 - self.x0, by1 - self.y0))
        return compose(x1 - x0, y1 - y0, [("paste", inner, bx0 - x0, by0 - y0)])

    crop = region

    def dense(self) -> Rgba:
        return self.region((0, 0, self.width, self.height))

    def tobytes(self) -> bytes:
        return self.dense().tobytes()

    def getpixel(self, xy: tuple[int, int]) -> tuple[int, int, int, int]:
        x, y = xy
        r = self.rect
        if r is None or not (r[0] <= x < r[2] and r[1] <= y < r[3]):
            return (0, 0, 0, 0)
        return self.crop_image.getpixel((x - self.x0, y - self.y0))  # type: ignore[union-attr]

    def to_pil(self):
        return self.dense().to_pil()

    @property
    def __array_interface__(self) -> dict:
        return self.dense().__array_interface__

    def __repr__(self) -> str:
        return f"<tmi.raster.Sparse {self.width}x{self.height} rect={self.rect}>"


Image = Any  # an Rgba, a Sparse or an RGBA-convertible PIL.Image


def as_rgba(image: Image) -> Rgba | Sparse:
    """``image`` as an ``Rgba`` / ``Sparse`` (a PIL image is converted to
    RGBA and its pixels copied)."""
    if isinstance(image, (Rgba, Sparse)):
        return image
    if image.mode != "RGBA":
        image = image.convert("RGBA")
    return Rgba(image.width, image.height, image.tobytes())


def dense(image: Image) -> Rgba:
    return as_rgba(image).dense()


# ─────────────────────────────── compositing ──────────────────────────────


def compose(
    width: int,
    height: int,
    ops: Iterable[tuple[str, Rgba, int, int]],
    fill: tuple[int, int, int, int] = (0, 0, 0, 0),
    base: Rgba | None = None,
) -> Rgba:
    """A ``width`` x ``height`` canvas (``base``, or filled with ``fill``)
    with each ``("over" | "paste", image, x, y)`` applied in order:
    ``alpha_composite(image, (x, y))`` or ``paste(image, (x, y))``."""
    kinds = {"over": 0, "paste": 1}
    native_ops = [(kinds[kind], img.tobytes(), img.width, img.height, int(x), int(y)) for kind, img, x, y in ops]
    data = _native().rgba_compose(
        int(width), int(height), native_ops, tuple(fill), None if base is None else base.tobytes()
    )
    return Rgba(width, height, data)


def pad(image: Rgba, size: tuple[int, int]) -> Rgba:
    """``image`` alpha-composited onto a transparent ``size`` canvas (what
    tmi's diff did to sides of different sizes)."""
    if image.size == size:
        return image
    return compose(size[0], size[1], [("over", image, 0, 0)])


def same_size(left: Image, right: Image) -> tuple[Rgba, Rgba]:
    left, right = dense(left), dense(right)
    size = (max(left.width, right.width), max(left.height, right.height))
    return pad(left, size), pad(right, size)


def blend(a: Rgba, b: Rgba, alpha: float) -> Rgba:
    """``Image.blend(a, b, alpha)`` of two images of one size."""
    return Rgba(a.width, a.height, _native().rgba_blend(a.data, b.data, float(alpha)))


def outline(image: Rgba, box: Iterable[int], ink=(255, 0, 0, 255), width: int = 1) -> Rgba:
    """A copy of ``image`` with ``ImageDraw.rectangle(box, outline=ink,
    width=width)`` drawn on it."""
    x0, y0, x1, y1 = (int(v) for v in box)
    if width == 0:
        return image  # ImageDraw.rectangle draws no outline of width 0
    return Rgba(image.width, image.height, _native().rgba_outline(image.data, image.width, image.height, (x0, y0, x1, y1), tuple(ink), int(width)))


def thumbnail(image: Image, size: int = 384) -> Rgba:
    """tmi's thumbnail: ``thumbnail((size, size), LANCZOS)`` centred with
    ``alpha_composite`` on a transparent ``size`` x ``size`` canvas."""
    img = dense(image)
    return Rgba(size, size, _native().rgba_thumbnail(img.data, img.width, img.height, int(size)))


def webp(image: Image, quality: float, method: int) -> bytes:
    """Lossy WebP (alpha kept), as ``save(..., "WEBP", quality=, method=)``."""
    img = dense(image)
    return _native().rgba_webp(img.data, img.width, img.height, float(quality), int(method))


def png(image: Image, text: tuple[str, str] | None = None) -> bytes:
    """tmi's PNG of RGBA pixels (mtpng, the bytes of ``image_to_png_bytes``)."""
    img = dense(image)
    return _native().encode_png_rgba(img.data, img.width, img.height, text)


# ───────────────────────────────── diff ───────────────────────────────────


def diff_bbox(left: Image, right: Image) -> list[int] | None:
    """tmi's visible difference box of two images (``image_ops.diff_bbox``):
    where the alpha differs, or a colour differs by enough to show at the
    larger alpha. Images of different sizes are compared on the larger
    canvas. Only the union of the two painted rectangles is examined:
    outside it both sides are transparent black."""
    a, b = as_rgba(left), as_rgba(right)
    width, height = max(a.width, b.width), max(a.height, b.height)
    ra, rb = _clip_rect(a), _clip_rect(b)
    if ra is None and rb is None:
        return None
    rects = [r for r in (ra, rb) if r is not None]
    box = (min(r[0] for r in rects), min(r[1] for r in rects), max(r[2] for r in rects), max(r[3] for r in rects))
    box = (max(box[0], 0), max(box[1], 0), min(box[2], width), min(box[3], height))
    if box[0] >= box[2] or box[1] >= box[3]:
        return None
    # A pixel can differ only inside one of the two painted rectangles, and
    # whether it differs depends on its two values alone: when the
    # rectangles are far apart, look at each instead of the box around both
    # (the same box comes out).
    if ra is not None and rb is not None:
        if _inside(a, ra) and _inside(b, rb) and _area(ra) + _area(rb) < _area(box):
            found = [f for f in (_difference_in(a, b, ra), _difference_in(a, b, rb)) if f is not None]
            if not found:
                return None
            return [min(f[0] for f in found), min(f[1] for f in found), max(f[2] for f in found), max(f[3] for f in found)]
    found = _difference_in(a, b, box)
    return None if found is None else list(found)


def _inside(image: Rgba | Sparse, r: tuple[int, int, int, int]) -> bool:
    return r[0] >= 0 and r[1] >= 0 and r[2] <= image.width and r[3] <= image.height


def _area(r: tuple[int, int, int, int]) -> int:
    return max(0, r[2] - r[0]) * max(0, r[3] - r[1])


def _difference_in(a: Rgba | Sparse, b: Rgba | Sparse, box: tuple[int, int, int, int]) -> tuple[int, int, int, int] | None:
    """The visible difference box of ``a`` and ``b`` within ``box``."""
    blank_a, blank_b = _blank_in(a, box), _blank_in(b, box)
    if blank_a and blank_b:
        return None
    if blank_a or blank_b:
        # Against transparent black a pixel differs visibly exactly when its
        # alpha is not 0: the box is the other side's alpha box.
        p = (b if blank_a else a).region(box)
        found = _native().rgba_alpha_bbox(p.data, p.width)
    else:
        pa, pb = a.region(box), b.region(box)
        found = _native().rgba_diff_bbox(pa.data, pb.data, pa.width)
    if found is None:
        return None
    return (found[0] + box[0], found[1] + box[1], found[2] + box[0], found[3] + box[1])


def _clip_rect(image: Rgba | Sparse) -> tuple[int, int, int, int] | None:
    return image.rect


def _inner_rect(image: Rgba | Sparse) -> tuple[int, int, int, int] | None:
    """The painted rectangle within the image's own size: where ``dense()``
    can be non-zero (None: nowhere)."""
    r = image.rect
    if r is None:
        return None
    x0, y0, x1, y1 = max(r[0], 0), max(r[1], 0), min(r[2], image.width), min(r[3], image.height)
    return (x0, y0, x1, y1) if x0 < x1 and y0 < y1 else None


def _blank_in(image: Rgba | Sparse, box: tuple[int, int, int, int]) -> bool:
    """Whether ``image.region(box)`` is surely all transparent black (its
    painted pixels miss ``box``)."""
    x0, y0, x1, y1 = box
    if isinstance(image, Sparse):
        if image.crop_image is None:
            return True
        if tuple(box) == image.rect:
            return False  # region() hands the crop back as it is
        r = _inner_rect(image)
    else:
        r = image.rect
    return r is None or r[0] >= x1 or r[2] <= x0 or r[1] >= y1 or r[3] <= y0


def _painted(image: Rgba | Sparse) -> tuple[Rgba, int, int] | None:
    """``image``'s painted pixels (within its size) and where they go."""
    r = _inner_rect(image)
    if r is None:
        return None
    return image.region(r), r[0], r[1]


def resize_region(image: Image, box: Iterable[int], size: tuple[int, int]) -> Rgba:
    """``image.region(box).resize(size)`` (LANCZOS) without building the
    region: tmi_accel resamples only the painted pixels inside ``box`` as
    the whole transparent region (the same bytes)."""
    x0, y0, x1, y1 = (int(v) for v in box)
    size = (int(size[0]), int(size[1]))
    image = as_rgba(image)
    crop, at = None, (0, 0)
    if isinstance(image, Sparse):
        c = image.crop_image
        if c is not None and (x0, y0, x1, y1) == image.rect:
            crop = c  # region() hands the crop back as it is
        elif c is not None:
            r = _inner_rect(image)
            if r is not None:
                # the crop within the layer (region() leaves out the rest)
                crop = c if r == image.rect else c.crop((r[0] - image.x0, r[1] - image.y0, r[2] - image.x0, r[3] - image.y0))
                at = (r[0] - x0, r[1] - y0)
    else:
        crop, at = image, (-x0, -y0)
    data, cw, ch = (b"", 0, 0) if crop is None else (crop.data, crop.width, crop.height)
    out = _native().rgba_resize_sparse(data, cw, ch, at[0], at[1], x1 - x0, y1 - y0, size, None)
    return Rgba(size[0], size[1], out)


def side_by_side_painted(left: Image, right: Image) -> Rgba:
    """``image_ops.side_by_side(left, right)`` drawing only the painted
    pixels (an alpha_composite of a transparent pixel changes nothing)."""
    from .image_ops import side_by_side

    left, right = as_rgba(left), as_rgba(right)
    if left.size != right.size:
        return side_by_side(left, right)
    w, h = left.size
    ops = []
    for image, dx in ((left, 0), (right, w + 16)):
        p = _painted(image)
        if p is not None:
            ops.append(("over", p[0], p[1] + dx, p[2]))
    return compose(w * 2 + 16, h, ops, fill=(32, 32, 32, 255))


def overlay_painted(left: Image, right: Image) -> Rgba:
    """``image_ops.overlay(left, right)`` blending only where a side is
    painted (``Image.blend`` of two 0 bytes is 0)."""
    from .image_ops import overlay

    left, right = as_rgba(left), as_rgba(right)
    if left.size != right.size:
        return overlay(left, right)
    w, h = left.size
    rects = [r for r in (_inner_rect(left), _inner_rect(right)) if r is not None]
    if len(rects) == 2:
        a, b = rects
        u = (min(a[0], b[0]), min(a[1], b[1]), max(a[2], b[2]), max(a[3], b[3]))
        if _area(a) + _area(b) >= _area(u):
            rects = [u]
    ops = []
    for r in rects:
        pa, pb = left.region(r), right.region(r)
        ops.append(("paste", blend(pa, pb, 0.5), r[0], r[1]))
    return compose(w, h, ops)


def diff_preview_painted(left: Image, right: Image, bbox: Iterable[int] | None) -> Rgba:
    """``image_ops.diff_preview(left, right)`` given ``bbox = diff_bbox(left,
    right)``: for sides of one size the box it would find on the full
    images is that one (outside the painted rectangles both are
    transparent black), and the left side is never built."""
    from .image_ops import diff_preview

    left, right = as_rgba(left), as_rgba(right)
    if left.size != right.size:
        return diff_preview(left, right)
    right = right.dense()
    if not bbox:
        return right
    return outline(right, tuple(bbox), (255, 0, 0, 255), max(2, right.width // 300))


# ──────────────────────────────── decoding ────────────────────────────────


@dataclass
class Decoded:
    image: Rgba
    format: str  # "png", "jpeg", or what Pillow called it
    dpi: float | None = None  # what import_raster_image records as canvas dpi
    icc: bool = False  # an embedded colour profile (``"icc_profile" in info``)


class _Unsure(Exception):
    """A header this module does not read with certainty: Pillow decides."""


def decode(data: bytes, *, meta: bool = False) -> Decoded:
    """``Image.open(data).convert("RGBA")``: the pixels (and, with ``meta``,
    the dpi and ICC presence Pillow's ``info`` would show after loading).
    PNG and JPEG go to tmi_accel; anything it declines goes to Pillow,
    which raises what it raised before for data it cannot read."""
    data = bytes(data)
    try:
        header = _header(data, meta)
    except Exception:  # _Unsure, or a header this module misreads: Pillow decides
        return _pil_decode(data)
    if header is None:
        return _pil_decode(data)
    fmt, size, dpi, icc = header
    _check_pixels(size)
    try:
        rgba, w, h, rust_fmt = _native().image_decode(data)
    except ValueError:
        return _pil_decode(data)
    if rust_fmt != fmt or (w, h) != size:
        return _pil_decode(data)
    return Decoded(Rgba(w, h, rgba), fmt, dpi, icc)


def import_decode(data: bytes) -> Decoded | None:
    """tmi's rule for which files are images (PLUGINS.md P0e): the pixels,
    format, dpi and ICC presence of ``data`` when tmi_accel's own decoders
    read it (a recognised signature and a full decode with no decline), or
    None when they do not: the file is then stored as an opaque blob.

    The Rust import calls the same function (``tmi_image::import``), so both
    record the same objects for every input. Pillow is not consulted: the
    verdict does not depend on the Pillow installed, nor on there being one.
    An image over the raster pixel limit raises ``CanvasTooLargeError``."""
    limit = _pixel_limit.resolve(kind=_pixel_limit.RASTER)
    try:
        out = _native().image_import(bytes(data), limit.value or None)
    except ValueError as exc:
        text = str(exc)
        if not text.startswith("too large: "):
            raise
        width, height = (int(v) for v in text[len("too large: "):].split("x"))
        raise _pixel_limit.CanvasTooLargeError(max(1, width) * max(1, height), limit) from None
    if out is None:
        return None
    rgba, width, height, fmt, dpi, icc = out
    return Decoded(Rgba(width, height, rgba), fmt, dpi, icc)


def decode_rgba(data: bytes) -> Rgba:
    return decode(data).image


def _check_pixels(size: tuple[int, int]) -> None:
    """The limit Pillow applies on open (``pixel_limit._pil_check``)."""
    _pixel_limit.check_raster_pixels(size)


def _pil_decode(data: bytes) -> Decoded:
    from .image_ops import _pil, as_rgba as _pil_as_rgba

    Image = _pil()
    opened = Image.open(io.BytesIO(data))
    fmt = (opened.format or "").lower()
    image = _pil_as_rgba(opened)
    dpi = image.info.get("dpi")
    if dpi:
        dpi = float(dpi[0]) if isinstance(dpi, tuple) else float(dpi)
    else:
        dpi = None
    return Decoded(Rgba(image.width, image.height, image.tobytes()), fmt, dpi, "icc_profile" in image.info)


def _header(data: bytes, meta: bool):
    """(format, size, dpi, icc) from the header, or None for a format
    tmi_accel does not decode. Raises _Unsure where Pillow must decide."""
    if data.startswith(PNG_SIGNATURE):
        info = png_header(data, after_idat=True)
        dpi = info.get("dpi")
        return "png", info["size"], None if dpi is None else float(dpi[0]), "icc_profile" in info
    if data[:3] == b"\xff\xd8\xff":
        info = jpeg_header(data, dpi_from_exif=meta)
        return "jpeg", info["size"], info.get("dpi_value"), "icc_profile" in info
    return None


# ── PNG chunks (PngImagePlugin's handlers, the parts tmi reads) ──

_I16 = struct.Struct(">H")
_I32 = struct.Struct(">I")


def png_chunks(data: bytes):
    """(type, body, before_first_idat) of every chunk up to IEND, as
    PngImagePlugin walks them: before the image data a malformed or
    truncated chunk fails ``open`` (here: _Unsure); after it, the walk of
    ``load_end`` stops quietly at a header it cannot read or a chunk type
    that is not four word characters, and fails on a truncated chunk."""
    pos = 8
    seen_idat = False
    n = len(data)
    while True:
        if pos + 8 > n:
            if not seen_idat:
                raise _Unsure("truncated")
            return
        length, kind = struct.unpack_from(">I4s", data, pos)
        if not _CID.fullmatch(kind):
            if not seen_idat:
                raise _Unsure("broken chunk")
            return
        if kind == b"IEND":
            return
        if kind in (b"IDAT", b"fdAT"):
            seen_idat = True
        end = pos + 8 + length
        if end > n:
            raise _Unsure("truncated chunk")
        yield kind, data[pos + 8:end] if kind not in (b"IDAT", b"fdAT") else b"", not seen_idat
        pos = end + 4


_CID = __import__("re").compile(rb"\w\w\w\w")


_PNG_MODES = {(1, 0): "1", (2, 0): "L", (4, 0): "L", (8, 0): "L", (16, 0): "I;16", (8, 2): "RGB", (16, 2): "RGB",
              (1, 3): "P", (2, 3): "P", (4, 3): "P", (8, 3): "P", (8, 4): "LA", (16, 4): "RGBA", (8, 6): "RGBA", (16, 6): "RGBA"}


def png_header(data: bytes, *, after_idat: bool) -> dict:
    """What PngImagePlugin puts in ``info`` (dpi, icc_profile, exif,
    transparency, text) plus ``size``, ``mode``, ``palette`` and the copied
    ``chunks``, from the chunks before the image data (``after_idat``: also
    those after it, which ``load()`` adds). Raises _Unsure where a handler
    of Pillow's would fail."""
    import zlib

    info: dict = {"chunks": []}
    mode = None
    for kind, body, before in png_chunks(data):
        if not before and not after_idat:
            break
        if kind == b"IHDR":
            if len(body) < 13 or body[11]:
                raise _Unsure("IHDR")
            info["size"] = (_I32.unpack_from(body, 0)[0], _I32.unpack_from(body, 4)[0])
            mode = _PNG_MODES.get((body[8], body[9]))
            info["mode"] = mode
        elif kind == b"PLTE":
            if before and mode == "P":
                info["palette"] = body
        elif kind == b"tRNS":
            if mode == "P":
                if _simple_palette(body):
                    i = body.find(b"\0")
                    if i >= 0:
                        info["transparency"] = i
                else:
                    info["transparency"] = body
            elif mode in ("1", "L", "I;16", "RGB"):
                need = 6 if mode == "RGB" else 2
                if len(body) < need:
                    raise _Unsure("tRNS")
                if mode == "1":
                    info["transparency"] = 255 if _I16.unpack_from(body)[0] else 0
                elif mode == "RGB":
                    info["transparency"] = struct.unpack_from(">3H", body)
                else:
                    info["transparency"] = _I16.unpack_from(body)[0]
        elif kind == b"gAMA":
            if len(body) < 4:
                raise _Unsure("gAMA")
        elif kind == b"cHRM":
            if len(body) % 4:
                raise _Unsure("cHRM")
        elif kind == b"sRGB":
            if len(body) < 1:
                raise _Unsure("sRGB")
        elif kind == b"pHYs":
            if len(body) < 9:
                raise _Unsure("pHYs")
            if body[8] == 1:
                px, py = _I32.unpack_from(body, 0)[0], _I32.unpack_from(body, 4)[0]
                info["dpi"] = (px * 0.0254, py * 0.0254)
        elif kind == b"iCCP":
            i = body.find(b"\0")
            if i < 0 or i + 1 >= len(body) or body[i + 1] != 0:
                raise _Unsure("iCCP")
            # _safe_zlib_decompress: at most MAX_TEXT_CHUNK (1 MiB) inflated.
            inflater = zlib.decompressobj()
            try:
                profile = inflater.decompress(body[i + 2:], 1 << 20)
            except zlib.error:
                profile = None
            if profile is not None and inflater.unconsumed_tail:
                raise _Unsure("iCCP too large")
            info["icc_profile"] = profile
        elif kind == b"eXIf":
            info["exif"] = b"Exif\x00\x00" + body
        elif kind == b"tEXt":
            k, sep, v = body.partition(b"\0")
            if k:
                info.setdefault("text", {})[k.decode("latin-1")] = v.decode("latin-1", "replace")
                if before:
                    info.setdefault("text_before", {})[k.decode("latin-1")] = v.decode("latin-1", "replace")
        elif kind in (b"zTXt", b"iTXt"):
            k = body.partition(b"\0")[0]
            if k:
                info.setdefault("compressed_text_keys", set()).add(k.decode("latin-1"))
        elif kind in (b"acTL", b"fcTL"):
            info["animated"] = True
        if before:
            info["chunks"].append((kind, body))
    if "size" not in info:
        raise _Unsure("no IHDR")
    return info


def _simple_palette(s: bytes) -> bool:
    """``_simple_palette.match(s)``: ``^\\xff*\\x00\\xff*$`` (``$`` also
    before a final newline)."""
    def full(t: bytes) -> bool:
        return t.count(0) == 1 and all(v in (0, 255) for v in t)

    return full(s) or (s.endswith(b"\n") and full(s[:-1]))


def png_text_before_idat(data: bytes, key: str) -> str | None:
    """``Image.open(png).info.get(key)`` for a tEXt key (before ``load()``);
    raises _Unsure when a zTXt / iTXt chunk carries that key."""
    if not data.startswith(PNG_SIGNATURE):
        return None
    info = png_header(data, after_idat=False)
    if key in info.get("compressed_text_keys", ()):
        raise _Unsure("compressed text")
    return info.get("text_before", {}).get(key)


# ── JPEG markers (JpegImagePlugin._open) ──

_SOF = {0xC0, 0xC1, 0xC2, 0xC3, 0xC5, 0xC6, 0xC7, 0xC9, 0xCA, 0xCB, 0xCD, 0xCE, 0xCF, 0xDE}
_SKIP = {0xC4, 0xCC, 0xDA, 0xDC, 0xDD, 0xDF}
_NO_HANDLER = {0xC8, *range(0xD0, 0xDA), *range(0xF0, 0xFE)}


def jpeg_header(data: bytes, *, dpi_from_exif: bool = True) -> dict:
    """What JpegImageFile._open reads from the markers before the scan:
    ``size``, ``mode``, ``layers`` (sampling factors), ``quantization``,
    and the ``info`` entries tmi uses (dpi, icc_profile, exif, comment,
    xmp, progressive, adobe_transform). ``dpi_value`` is ``float(info["dpi"]
    [0])``. Raises _Unsure where Pillow's parse would fail or where this one
    does not follow it (an MPO, Photoshop resources, an EXIF block it cannot
    fully read)."""
    info: dict = {}
    icclist: list[bytes] = []
    n = len(data)
    pos = 3
    s = 0xFF  # the byte Pillow holds, as an int; None: read the next one
    marker_byte: int | None = s
    while True:
        if marker_byte is None:
            if pos >= n:
                raise _Unsure("no scan")
            marker_byte = data[pos]
            pos += 1
        if marker_byte != 0xFF:
            marker_byte = None  # skip non-0xFF junk
            continue
        if pos >= n:
            raise _Unsure("no scan")
        code = 0xFF00 | data[pos]
        pos += 1
        low = code & 0xFF
        if code == 0xFFFF:
            marker_byte = 0xFF
            continue
        if code == 0xFF00:
            marker_byte = None
            continue
        if low in _NO_HANDLER:
            marker_byte = None
            continue
        if not (low in _SOF or low in _SKIP or low in (0xDB, 0xFE) or 0xE0 <= low <= 0xEF):
            raise _Unsure("no marker found")
        if pos + 2 > n:
            raise _Unsure("truncated")
        length = _I16.unpack_from(data, pos)[0] - 2
        pos += 2
        size = max(length, 0)
        if pos + size > n:
            raise _Unsure("truncated")
        seg = data[pos:pos + size]
        pos += size
        if 0xE0 <= low <= 0xEF:
            _jpeg_app(low, seg, info, icclist)
        elif low == 0xFE:
            info["comment"] = seg
        elif low in _SOF:
            if len(seg) < 6:
                raise _Unsure("SOF")
            info["size"] = (_I16.unpack_from(seg, 3)[0], _I16.unpack_from(seg, 1)[0])
            if seg[0] != 8 or seg[5] not in (1, 3, 4):
                raise _Unsure("SOF mode")
            info["mode"] = {1: "L", 3: "RGB", 4: "CMYK"}[seg[5]]
            info["layer_count"] = seg[5]
            if low in (0xC2, 0xC6, 0xCA, 0xCE):
                info["progressive"] = 1
            if icclist:
                icclist.sort()
                if len(icclist[0]) < 14:
                    raise _Unsure("ICC")
                info["icc_profile"] = b"".join(p[14:] for p in icclist) if icclist[0][13] == len(icclist) else None
                icclist = []
            layers = info.setdefault("layers", [])
            for i in range(6, len(seg), 3):
                t = seg[i:i + 3]
                if len(t) < 3:
                    raise _Unsure("SOF layers")
                layers.append((t[0], t[1] // 16, t[1] & 15, t[2]))
        elif low == 0xDB:
            q = seg
            tables = info.setdefault("quantization", {})
            while q:
                precision = 1 if q[0] // 16 == 0 else 2
                qt_length = 1 + precision * 64
                if len(q) < qt_length:
                    raise _Unsure("DQT")
                tables[q[0] & 15] = None  # read by tmi_accel when needed
                q = q[qt_length:]
        if low == 0xDA:
            break
        marker_byte = None
    if "size" not in info or "mode" not in info:
        raise _Unsure("no frame")
    if "dpi" not in info and "exif" in info and dpi_from_exif:
        info["dpi"] = _exif_dpi(info["exif"])
    if "dpi" in info:
        d = info["dpi"]
        info["dpi_value"] = float(d[0])
    return info


def _jpeg_app(low: int, s: bytes, info: dict, icclist: list[bytes]) -> None:
    if low == 0xE0 and s.startswith(b"JFIF"):
        if len(s) < 7:
            raise _Unsure("JFIF")
        if len(s) >= 12:
            unit = s[7]
            density = (_I16.unpack_from(s, 8)[0], _I16.unpack_from(s, 10)[0])
            if unit == 1:
                info["dpi"] = density
            elif unit == 2:
                info["dpi"] = tuple(d * 2.54 for d in density)
    elif low == 0xE1 and s.startswith(b"Exif\0\0"):
        info["exif"] = info["exif"] + s[6:] if "exif" in info else s
    elif low == 0xE1 and s.startswith(b"http://ns.adobe.com/xap/1.0/\x00"):
        info["xmp"] = s.split(b"\x00", 1)[1]
    elif low == 0xE2 and s.startswith(b"ICC_PROFILE\0"):
        icclist.append(s)
    elif low == 0xE2 and s.startswith(b"MPF\0"):
        raise _Unsure("MPF")
    elif low == 0xED and s.startswith(b"Photoshop 3.0\x00"):
        raise _Unsure("Photoshop resources")
    elif low == 0xEE and s.startswith(b"Adobe"):
        if len(s) < 7:
            raise _Unsure("Adobe")
        if len(s) > 11:
            info["adobe_transform"] = s[11]


def _exif_dpi(exif: bytes) -> tuple[float, float]:
    """``JpegImageFile._read_dpi_from_exif`` for an EXIF block whose first
    directory this reads completely; _Unsure otherwise."""
    data = exif
    while data.startswith(b"Exif\x00\x00"):
        data = data[6:]
    if len(data) < 8:
        raise _Unsure("EXIF header")
    order = data[:2]
    if order == b"II" and data[2:4] == b"\x2a\x00":
        end = "<"
    elif order == b"MM" and data[2:4] == b"\x00\x2a":
        end = ">"
    else:
        raise _Unsure("EXIF byte order")
    (ifd,) = struct.unpack_from(end + "L", data, 4)
    if ifd + 2 > len(data):
        raise _Unsure("EXIF directory")
    (count,) = struct.unpack_from(end + "H", data, ifd)
    if ifd + 2 + 12 * count + 4 > len(data):
        raise _Unsure("EXIF directory")
    sizes = {1: 1, 2: 1, 3: 2, 4: 4, 5: 8, 6: 1, 7: 1, 8: 2, 9: 4, 10: 8, 11: 4, 12: 8, 13: 4, 16: 8, 17: 8, 18: 8}
    tags: dict[int, tuple[int, int, bytes]] = {}
    for i in range(count):
        tag, typ, cnt = struct.unpack_from(end + "HHL", data, ifd + 2 + 12 * i)
        raw = data[ifd + 2 + 12 * i + 8:ifd + 2 + 12 * i + 12]
        if typ not in sizes:
            continue  # Pillow skips unsupported types
        size = cnt * sizes[typ]
        if size > 4:
            (offset,) = struct.unpack_from(end + "L", raw)
            if offset + size > len(data):
                raise _Unsure("EXIF value out of range")  # Pillow stops reading there
            value = data[offset:offset + size]
        else:
            value = raw[:size]
        if not value:
            continue
        tags[tag] = (typ, cnt, value)
    if 0x0128 not in tags or 0x011A not in tags:
        return (72, 72)

    def first(tag: int):
        typ, cnt, value = tags[tag]
        if typ == 3:
            return struct.unpack_from(end + "H", value)[0]
        if typ == 4:
            return struct.unpack_from(end + "L", value)[0]
        if typ in (5, 10):
            num, den = struct.unpack_from(end + ("LL" if typ == 5 else "ll"), value)
            return (num, den)
        raise _Unsure("EXIF value type")

    unit = first(0x0128)
    xres = first(0x011A)
    if isinstance(unit, tuple):
        raise _Unsure("EXIF unit type")
    if isinstance(xres, tuple):
        num, den = xres
        if den == 0:
            return (72, 72)  # IFDRational(n, 0) is nan
        from fractions import Fraction

        dpi = float(Fraction(num, den))
    else:
        dpi = xres
    if unit == 3:
        dpi = dpi * 2.54
    return (dpi, dpi)


# ── modules that must not be loaded for raster work (for the tests) ──


def pillow_loaded() -> bool:
    return "PIL" in sys.modules
