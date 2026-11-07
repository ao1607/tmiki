"""tmi's numpy PNG encoder from before RUST_MIGRATION.md 1e (tmi encodes
RGBA PNGs with tmi_accel's mtpng only). Kept for tests: its PNGs use every
filter type, row by row (tests/test_rust_image_kernels.py decodes them)."""
from __future__ import annotations

_BAND_BYTES = 256 * 1024

_DICT_BYTES = 32 * 1024


def encode_png_banded(
    rgba: bytes,
    width: int,
    height: int,
    text: tuple[str, str] | None = None,
) -> bytes:
    """RGBA → PNG with each ~256 KiB band of rows deflated on its own.

    Built like tmi_accel's mtpng encoder (tmi's PNG writer), and tmi's
    fallback until RUST_MIGRATION.md 1e; now a second PNG writer for tests
    (it picks a different filter type per row, as libpng does): a
    band's compressed bytes depend only on its rows and the 32 KiB before it
    (the deflate dictionary), so a local edit changes a few bands and the
    object store shares the rest between versions (one deflate stream, as
    Pillow writes, differs from the edit to the end). Bands are filtered and
    compressed on a thread pool; zlib and numpy release the GIL."""
    import os
    import struct
    import zlib
    from concurrent.futures import ThreadPoolExecutor

    import numpy as np

    stride = width * 4
    pixels = np.frombuffer(rgba, dtype=np.uint8).reshape(height, stride)
    rows_per_band = max(1, _BAND_BYTES // stride)
    bands = [(y, min(height, y + rows_per_band)) for y in range(0, height, rows_per_band)]

    def filtered(band: tuple[int, int]) -> bytes:
        y0, y1 = band
        cur = pixels[y0:y1]
        up = np.empty_like(cur)
        up[0] = pixels[y0 - 1] if y0 else 0
        up[1:] = cur[:-1]
        left = np.zeros_like(cur)
        left[:, 4:] = cur[:, :-4]
        upleft = np.zeros_like(cur)
        upleft[:, 4:] = up[:, :-4]
        a, b, c = left.astype(np.int16), up.astype(np.int16), upleft.astype(np.int16)
        pa, pb, pc = np.abs(b - c), np.abs(a - c), np.abs(a + b - 2 * c)
        paeth = np.where((pa <= pb) & (pa <= pc), left, np.where(pb <= pc, up, upleft))
        average = (left >> 1) + (up >> 1) + (left & up & 1)
        # PNG filter types 0-4: None, Sub, Up, Average, Paeth (uint8 math
        # wraps mod 256 as PNG wants). Pick per row the one with the smallest
        # sum of |residual| as signed bytes (the libpng heuristic);
        # min(x, -x) is that magnitude for a uint8 x.
        candidates = np.stack([cur, cur - left, cur - up, cur - average, cur - paeth])
        choice = np.minimum(candidates, 0 - candidates).sum(axis=2, dtype=np.uint32).argmin(axis=0)
        out = np.empty((y1 - y0, stride + 1), dtype=np.uint8)
        out[:, 0] = choice
        out[:, 1:] = candidates[choice, np.arange(y1 - y0)]
        return out.tobytes()

    workers = min(len(bands), os.cpu_count() or 1)
    with ThreadPoolExecutor(max_workers=workers) as pool:
        streams = list(pool.map(filtered, bands))

        def deflate(i: int) -> bytes:
            options = {"zdict": streams[i - 1][-_DICT_BYTES:]} if i else {}
            comp = zlib.compressobj(1, zlib.DEFLATED, -15, **options)
            last = i == len(streams) - 1
            return comp.compress(streams[i]) + comp.flush(zlib.Z_FINISH if last else zlib.Z_SYNC_FLUSH)

        pieces = list(pool.map(deflate, range(len(streams))))
    adler = 1
    for stream in streams:
        adler = zlib.adler32(stream, adler)
    pieces[0] = b"\x78\x01" + pieces[0]
    pieces[-1] += struct.pack(">I", adler)

    def chunk(kind: bytes, data: bytes) -> bytes:
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))

    out = [b"\x89PNG\r\n\x1a\n", chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0))]
    if text is not None:
        out.append(chunk(b"tEXt", text[0].encode("latin-1") + b"\0" + text[1].encode("latin-1")))
    out.extend(chunk(b"IDAT", piece) for piece in pieces)
    out.append(chunk(b"IEND", b""))
    return b"".join(out)
