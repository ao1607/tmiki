"""The layer blob container ``TMIL`` (codec "fpaeth-z3").

A layer blob holds a layer's painted rectangle (see ``image_ops.LayerCrop``)
as interleaved RGBA8 rows. Every row is PNG-filtered, the encoder always
choosing Paeth, and the rows are cut into bands of about 256 KiB of raw RGBA,
each compressed on its own as one zstd frame (level 3). A band's first row
predicts from an all-zero row, so bands decode independently (in parallel)
and an edit changes only the bands it touches, which the object store then
shares between versions.

Byte layout, integers little-endian::

    0   4   magic b"TMIL"
    4   1   format version (1)
    5   3   reserved, zero
    8   4   layer width   (the full layer / canvas size)
    12  4   layer height
    16  4   x0            (where the rectangle sits in the layer)
    20  4   y0
    24  4   w             (rectangle size; 0 x 0 for an all-zero layer)
    28  4   h
    32  4   rows per band (0 for an all-zero layer)
    36  4   band count n  (= ceil(h / rows per band))
    40  4n  compressed size of each band
    ..      the n zstd frames back to back, nothing after them

A band of ``rows`` rows decompresses to ``rows * (1 + 4 * w)`` bytes: per row
a PNG filter-type byte (0 None, 1 Sub, 2 Up, 3 Average, 4 Paeth) and the
residuals, exactly a PNG scanline. Decoders accept all five types.

tmi_accel encodes and decodes it on its rayon pool. A Python codec (numpy +
zstandard + Pillow) that writes the same container is kept as the reference
in ``tmiref.layer_blob``; the zstd bytes are expected to be identical as well
(same libzstd 1.5.7, same parameters) but nothing depends on it: layer
identity is the pixel digest (``pixel_sha256``), never the blob bytes.
"""
from __future__ import annotations

import struct
from dataclasses import dataclass

MAGIC = b"TMIL"
VERSION = 1
HEADER_SIZE = 40
BAND_BYTES = 256 * 1024
ZSTD_LEVEL = 3

_HEADER = struct.Struct("<4sB3xIIIIIIII")
_PAETH = 4


class LayerBlobError(ValueError):
    """A layer blob that is not a valid TMIL container (corrupt or truncated)."""


@dataclass(frozen=True)
class LayerBlobHeader:
    size: tuple[int, int]  # full layer size
    x0: int
    y0: int
    width: int  # painted rectangle; 0 x 0 when the layer is all zero
    height: int
    rows_per_band: int
    band_sizes: tuple[int, ...]

    @property
    def rect(self) -> tuple[int, int, int, int] | None:
        if not self.width:
            return None
        return (self.x0, self.y0, self.x0 + self.width, self.y0 + self.height)

    def bands(self) -> list[tuple[int, int, int, int]]:
        """``(y0, y1, offset, size)`` of every band: its rows in the
        rectangle and where its frame sits in the blob."""
        out = []
        offset = HEADER_SIZE + 4 * len(self.band_sizes)
        for i, size in enumerate(self.band_sizes):
            y = i * self.rows_per_band
            out.append((y, min(self.height, y + self.rows_per_band), offset, size))
            offset += size
        return out


def is_layer_blob(data: bytes | memoryview) -> bool:
    return bytes(data[:4]) == MAGIC


def rows_per_band(width: int) -> int:
    return max(1, BAND_BYTES // (4 * width))


def parse_header(data: bytes) -> LayerBlobHeader:
    if len(data) < HEADER_SIZE:
        raise LayerBlobError(f"layer blob truncated: {len(data)} bytes, header needs {HEADER_SIZE}")
    magic, version, width, height, x0, y0, w, h, rows, count = _HEADER.unpack_from(data)
    if magic != MAGIC:
        raise LayerBlobError("not a TMIL layer blob")
    if version != VERSION:
        raise LayerBlobError(f"unsupported TMIL layer blob version {version}")
    if data[5:8] != b"\0\0\0":
        raise LayerBlobError("TMIL layer blob: reserved header bytes are not zero")
    if not w or not h:
        if w or h or x0 or y0 or rows or count:
            raise LayerBlobError("TMIL layer blob: inconsistent header for an empty layer")
        if len(data) != HEADER_SIZE:
            raise LayerBlobError("TMIL layer blob: data after an empty layer's header")
        return LayerBlobHeader((width, height), 0, 0, 0, 0, 0, ())
    if x0 + w > width or y0 + h > height:
        raise LayerBlobError(f"TMIL layer blob: rectangle {w}x{h}+{x0}+{y0} outside the {width}x{height} layer")
    if not rows or count != -(-h // rows):
        raise LayerBlobError(f"TMIL layer blob: {count} bands of {rows} rows do not cover {h} rows")
    table_end = HEADER_SIZE + 4 * count
    if len(data) < table_end:
        raise LayerBlobError("TMIL layer blob truncated in the band table")
    sizes = struct.unpack_from(f"<{count}I", data, HEADER_SIZE)
    if table_end + sum(sizes) != len(data):
        raise LayerBlobError(
            f"TMIL layer blob is {len(data)} bytes, its band table says {table_end + sum(sizes)}"
        )
    return LayerBlobHeader((width, height), x0, y0, w, h, rows, sizes)


def _check_pixels(width: int, height: int) -> None:
    """The image-size limit Pillow applies when it opens an image (see
    ``tmi.pixel_limit``), before anything that size is allocated."""
    from . import pixel_limit

    pixel_limit.check_raster_pixels((width, height))


def _check_size(width: int, height: int, x0: int, y0: int, w: int, h: int) -> None:
    for value in (width, height, x0, y0, w, h):
        if not 0 <= value < 2**32:
            raise ValueError(f"layer blob dimension out of range: {value}")
    if (w == 0) != (h == 0) or x0 + w > width or y0 + h > height:
        raise ValueError(f"rectangle {w}x{h}+{x0}+{y0} does not fit the {width}x{height} layer")


def encode(
    rgba: bytes | memoryview,
    w: int,
    h: int,
    size: tuple[int, int],
    x0: int = 0,
    y0: int = 0,
) -> bytes:
    """The TMIL blob of a ``w`` x ``h`` RGBA rectangle placed at ``(x0, y0)``
    in a layer of ``size`` (``w == h == 0``: an all-zero layer)."""
    width, height = size
    if not w or not h:
        x0 = y0 = w = h = 0
    _check_size(width, height, x0, y0, w, h)
    if len(rgba) != w * h * 4:
        raise ValueError(f"rgba buffer is {len(rgba)} bytes, expected {w}x{h}x4")
    if not w:
        return _HEADER.pack(MAGIC, VERSION, width, height, 0, 0, 0, 0, 0, 0)
    from . import accel

    return accel.native().encode_layer_blob(rgba, w, h, width, height, x0, y0)


def _decode_rgba(data: bytes) -> bytes:
    from . import accel

    try:
        return accel.native().decode_layer_blob(data)
    except ValueError as exc:
        raise LayerBlobError(str(exc)) from None


def decode(data: bytes) -> tuple[LayerBlobHeader, bytes]:
    """``(header, rgba)``: the rectangle's RGBA rows (empty for an all-zero
    layer). Raises LayerBlobError for a corrupt or truncated blob."""
    header = parse_header(data)
    if not header.width:
        return header, b""
    _check_pixels(header.width, header.height)
    return header, _decode_rgba(data)


def decode_image(data: bytes, *, expand: bool):
    """The blob as a Pillow RGBA image: the full layer when ``expand``,
    else the painted rectangle (None for an all-zero layer). For the tests,
    tools/ and ``tmiref``; tmi itself reads blobs with ``decode`` (no
    Pillow)."""
    from PIL import Image

    header = parse_header(data)
    if expand:
        _check_pixels(*header.size)
    if not header.width:
        return Image.new("RGBA", header.size, (0, 0, 0, 0)) if expand else None
    _check_pixels(header.width, header.height)
    rgba = _decode_rgba(data)
    crop = Image.frombuffer("RGBA", (header.width, header.height), rgba, "raw", "RGBA", 0, 1)
    if not expand or (header.width, header.height) == header.size:
        return crop
    full = Image.new("RGBA", header.size, (0, 0, 0, 0))
    full.paste(crop, (header.x0, header.y0))
    return full
