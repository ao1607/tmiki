"""The TMIL layer blob codec in Python: numpy filters the rows, ``zstandard``
compresses the bands, Pillow reverses the PNG filters (each band wrapped as an
uncompressed zlib stream). The reference for tmi_accel's codec, which tmi
uses (the container: tmi/layer_blob.py); same API as that module.
"""
from __future__ import annotations

import os
import struct
import threading
import zlib
from concurrent.futures import ThreadPoolExecutor
from typing import Any

from tmi.layer_blob import (  # noqa: F401  (re-exported: the same API as tmi.layer_blob)
    _HEADER,
    _PAETH,
    HEADER_SIZE,
    MAGIC,
    VERSION,
    ZSTD_LEVEL,
    LayerBlobError,
    LayerBlobHeader,
    _check_pixels,
    _check_size,
    is_layer_blob,
    parse_header,
    rows_per_band,
)

# Run bands on a pool only when there are this many (a pool costs more than
# it saves on a small layer).
_POOL_MIN_BANDS = 4


def encode(rgba, w: int, h: int, size: tuple[int, int], x0: int = 0, y0: int = 0) -> bytes:
    """``tmi.layer_blob.encode``, in Python."""
    width, height = size
    if not w or not h:
        x0 = y0 = w = h = 0
    _check_size(width, height, x0, y0, w, h)
    if len(rgba) != w * h * 4:
        raise ValueError(f"rgba buffer is {len(rgba)} bytes, expected {w}x{h}x4")
    if not w:
        return _HEADER.pack(MAGIC, VERSION, width, height, 0, 0, 0, 0, 0, 0)
    return _encode_python(rgba, w, h, width, height, x0, y0)


def decode(data: bytes) -> tuple[LayerBlobHeader, bytes]:
    """``tmi.layer_blob.decode``, in Python."""
    header = parse_header(data)
    if not header.width:
        return header, b""
    _check_pixels(header.width, header.height)
    return header, decode_image_python(data, header).tobytes()


def decode_image(data: bytes, *, expand: bool):
    """``tmi.layer_blob.decode_image``, in Python."""
    from PIL import Image

    header = parse_header(data)
    if expand:
        _check_pixels(*header.size)
    if not header.width:
        return Image.new("RGBA", header.size, (0, 0, 0, 0)) if expand else None
    _check_pixels(header.width, header.height)
    if expand:
        full = Image.new("RGBA", header.size, (0, 0, 0, 0))
        decode_image_python(data, header, into=full, at=(header.x0, header.y0))
        return full
    return decode_image_python(data, header)


# ───────────────────────────── pure Python ─────────────────────────────

_local = threading.local()


def _zstd():
    try:
        import zstandard
    except ImportError as exc:  # pragma: no cover
        raise RuntimeError(
            "tmiref's TMIL codec needs the 'zstandard' package (pip install zstandard)"
        ) from exc
    return zstandard


def _compressor():
    # zstandard contexts are not thread-safe: one per thread.
    cctx = getattr(_local, "cctx", None)
    if cctx is None:
        cctx = _local.cctx = _zstd().ZstdCompressor(level=ZSTD_LEVEL)
    return cctx


def _decompressor():
    dctx = getattr(_local, "dctx", None)
    if dctx is None:
        dctx = _local.dctx = _zstd().ZstdDecompressor()
    return dctx


def _map(fn, items: list) -> list:
    if len(items) < _POOL_MIN_BANDS:
        return [fn(item) for item in items]
    with ThreadPoolExecutor(max_workers=min(len(items), os.cpu_count() or 1)) as pool:
        return list(pool.map(fn, items))


def _paeth_rows(rows: Any) -> bytes:
    """PNG scanlines of ``rows`` (uint8, shape (n, 4w)) under the Paeth
    filter, the first row predicting from zeros. numpy wraps uint8 mod 256
    as PNG does; the predictor reads the original rows only, so it
    vectorises (only the reverse is sequential)."""
    import numpy as np

    n, stride = rows.shape
    a = np.zeros((n, stride), dtype=np.int16)  # left
    a[:, 4:] = rows[:, :-4]
    b = np.zeros((n, stride), dtype=np.int16)  # up
    b[1:] = rows[:-1]
    c = np.zeros((n, stride), dtype=np.int16)  # up-left
    c[1:, 4:] = rows[:-1, :-4]
    pa = np.abs(b - c)
    pb = np.abs(a - c)
    pc = np.abs(a + b - 2 * c)
    pred = np.where((pa <= pb) & (pa <= pc), a, np.where(pb <= pc, b, c)).astype(np.uint8)
    out = np.empty((n, stride + 1), dtype=np.uint8)
    out[:, 0] = _PAETH
    np.subtract(rows, pred, out=out[:, 1:])
    return out.tobytes()


def _encode_python(rgba: bytes | memoryview, w: int, h: int, width: int, height: int, x0: int, y0: int) -> bytes:
    import numpy as np

    stride = 4 * w
    pixels = np.frombuffer(rgba, dtype=np.uint8).reshape(h, stride)
    rows = rows_per_band(w)
    spans = [(y, min(h, y + rows)) for y in range(0, h, rows)]
    frames = _map(lambda span: _compressor().compress(_paeth_rows(pixels[span[0]:span[1]])), spans)
    head = _HEADER.pack(MAGIC, VERSION, width, height, x0, y0, w, h, rows, len(frames))
    return b"".join([head, struct.pack(f"<{len(frames)}I", *map(len, frames)), *frames])


def _zstd_frame_size(data: bytes | memoryview, pos: int, end: int) -> int:
    """Length of the zstd frame at ``pos``: its header, then blocks until
    the one marked last (and the checksum, if the frame has one)."""
    if end - pos < 6 or bytes(data[pos:pos + 4]) != b"\x28\xb5\x2f\xfd":
        raise LayerBlobError("TMIL layer blob: band is not a zstd frame")
    fhd = data[pos + 4]
    single_segment = (fhd >> 5) & 1
    p = pos + 5 + (0 if single_segment else 1) + (0, 1, 2, 4)[fhd & 3]
    p += (1 if single_segment else 0, 2, 4, 8)[fhd >> 6]
    while True:
        if p + 3 > end:
            raise LayerBlobError("TMIL layer blob: band frame truncated")
        block = int.from_bytes(data[p:p + 3], "little")
        p += 3 + (1 if (block >> 1) & 3 == 1 else block >> 3)
        if block & 1:
            break
    return p + (4 if (fhd >> 2) & 1 else 0) - pos


def _band_payload(data: bytes, offset: int, size: int, rows: int, w: int) -> bytes:
    end = offset + size
    if _zstd_frame_size(data, offset, end) != size:
        raise LayerBlobError("TMIL layer blob: band frame size does not match the band table")
    expected = rows * (1 + 4 * w)
    try:
        payload = _decompressor().decompress(memoryview(data)[offset:end], max_output_size=expected)
    except _zstd().ZstdError as exc:
        raise LayerBlobError(f"TMIL layer blob: corrupt band: {exc}") from None
    if len(payload) != expected:
        raise LayerBlobError(f"TMIL layer blob: band holds {len(payload)} bytes, expected {expected}")
    return payload


def _zlib_stored(payload: bytes) -> bytes:
    """``payload`` as a zlib stream of stored (uncompressed) deflate blocks."""
    view = memoryview(payload)
    parts = [b"\x78\x01"]
    pos, n = 0, len(payload)
    while True:
        piece = view[pos:pos + 0xFFFF]
        pos += len(piece)
        parts.append(struct.pack("<BHH", 1 if pos >= n else 0, len(piece), len(piece) ^ 0xFFFF))
        parts.append(piece)
        if pos >= n:
            break
    parts.append(struct.pack(">I", zlib.adler32(payload)))
    return b"".join(parts)


def _unfilter_band(payload: bytes, w: int, rows: int):
    """The band's RGBA image: Pillow's PNG decoder ("zip") reverses the
    filters of the scanlines, given them as an uncompressed zlib stream."""
    from PIL import Image

    filters = payload[::1 + 4 * w]
    if max(filters) > _PAETH:
        raise LayerBlobError(f"TMIL layer blob: unknown row filter type {max(filters)}")
    try:
        return Image.frombytes("RGBA", (w, rows), _zlib_stored(payload), "zip", "RGBA", 0)
    except (ValueError, OSError) as exc:  # pragma: no cover - payload length is checked above
        raise LayerBlobError(f"TMIL layer blob: band does not decode: {exc}") from None


def decode_image_python(data: bytes, header: LayerBlobHeader | None = None, *, into=None, at=(0, 0)):
    """Decode without tmi_accel: zstandard, then Pillow for the filters, bands
    on a thread pool (both release the GIL). Pastes into ``into`` at ``at``
    when given (and returns it), else returns the rectangle's image."""
    from PIL import Image

    header = header or parse_header(data)
    w, h = header.width, header.height
    target = into if into is not None else Image.new("RGBA", (w, h))
    x, y = (at if into is not None else (0, 0))

    def band(item: tuple[int, int, int, int]) -> None:
        y0, y1, offset, size = item
        image = _unfilter_band(_band_payload(data, offset, size, y1 - y0, w), w, y1 - y0)
        target.paste(image, (x, y + y0))

    _map(band, header.bands())
    return target
