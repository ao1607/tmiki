"""tmi_image's decoders for the formats of PLUGINS.md P0a–c (GIF, BMP / DIB,
TIFF, WebP, PSD) against Pillow 12.3.0: ``Image.open(data).convert("RGBA")``
and the ``info`` tmi reads (``dpi``, ``icc_profile``) must be the same, or
tmi_image must refuse the file. A refusal is either "corrupt" (Pillow must
fail too) or "declined" (Pillow may read it; tmi_image does not reproduce
it and leaves it to the caller). Different pixels are never allowed.

The inputs: Pillow's own test images, the BMP Suite and libtiff's test
images (``TMI_TEST_IMAGES``, default ``.test-images``; ``python
tools/fetch_test_images.py``), files Pillow writes in every mode and option,
and hand-built edge cases. ``TMI_IMAGE_REPORT=1`` prints the counts per
format (same / both refuse / declined, with the reasons).
"""
from __future__ import annotations

import io
import os
import struct
import sys
import unittest
import warnings
import zlib
from pathlib import Path

import numpy as np
from PIL import Image

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))

try:
    import tmi_accel as _native
except ImportError:  # pragma: no cover - tmi_accel not built
    _native = None


def _test_module():
    t = getattr(_native, "_test", None) if _native is not None else None
    return t if t is not None and hasattr(t, "raster_decode") else None


def _report(line: str) -> None:
    if os.environ.get("TMI_IMAGE_REPORT"):
        print(f"\n  {line}")


def _external(name: str) -> Path | None:
    base = Path(os.environ.get("TMI_TEST_IMAGES") or ROOT / ".test-images")
    path = base / name
    return path if path.is_dir() else None


# Pillow's decompression-bomb error threshold (tmi_image's max_pixels here).
BOMB = 2 * Image.MAX_IMAGE_PIXELS


def _float_pair(v):
    if v is None:
        return None
    if isinstance(v, tuple):
        return tuple(float(x) for x in v)
    return (float(v),)


def pillow_decode(data: bytes):
    """(rgba, size, format, dpi, icc) as Pillow gives them, or the name of
    the exception it raises."""
    try:
        with warnings.catch_warnings():
            warnings.simplefilter("ignore")
            opened = Image.open(io.BytesIO(data))
            fmt = (opened.format or "").lower()
            image = opened.convert("RGBA")
    except Exception as e:  # noqa: BLE001
        return type(e).__name__
    icc = image.info.get("icc_profile") if "icc_profile" in image.info else None
    return image.tobytes(), image.size, fmt, _float_pair(image.info.get("dpi")), icc


def tmi_decode(data: bytes):
    """The same from tmi_image, or ("declined" | "corrupt" | "too large" |
    "unknown", message)."""
    try:
        mode, w, h, rgba, fmt, dpi, icc = _test_module().raster_decode(data, BOMB)
    except ValueError as e:
        kind, _, msg = str(e).partition(": ")
        return kind, msg
    return rgba, (w, h), fmt, _float_pair(dpi), icc


class Tally:
    """Counts per format: same / both refuse / declined (by reason)."""

    def __init__(self) -> None:
        self.counts: dict[str, dict] = {}

    def add(self, fmt: str, what: str) -> None:
        c = self.counts.setdefault(fmt, {})
        c[what] = c.get(what, 0) + 1

    def get(self, fmt: str, what: str) -> int:
        return self.counts.get(fmt, {}).get(what, 0)

    def __str__(self) -> str:
        return "; ".join(f"{f}: {dict(sorted(c.items()))}" for f, c in sorted(self.counts.items()))


def compare(case: unittest.TestCase, label: str, data: bytes, tally: Tally) -> None:
    """One file: tmi_image's result against Pillow's, counted in ``tally``."""
    got = tmi_decode(data)
    if isinstance(got[0], str):
        kind, msg = got
        if kind == "unknown":
            tally.add("other", "unknown")
            return
        want = pillow_decode(data)
        fmt = want[2] if not isinstance(want, str) else msg.split(" ", 1)[0] if kind == "corrupt" else kind
        if kind == "declined":
            tally.add(fmt, f"declined ({msg})" + (", Pillow fails too" if isinstance(want, str) else ""))
            return
        case.assertIsInstance(want, str, f"{label}: tmi_image refuses ({kind}: {msg}), Pillow decodes")
        tally.add(fmt, "both refuse")
        return
    want = pillow_decode(data)
    case.assertNotIsInstance(want, str, f"{label}: Pillow refuses ({want}), tmi_image decodes")
    rgba, size, fmt, dpi, icc = got
    case.assertEqual((size, fmt), (want[1], want[2]), label)
    if rgba != want[0]:
        a = np.frombuffer(rgba, np.uint8).reshape(size[1], size[0], 4)
        b = np.frombuffer(want[0], np.uint8).reshape(size[1], size[0], 4)
        ys, xs = np.nonzero((a != b).any(axis=2))
        case.fail(f"{label}: {len(ys)} pixels differ, first at {(xs[0], ys[0])}: {a[ys[0], xs[0]]} != {b[ys[0], xs[0]]}")
    case.assertEqual(dpi, want[3], f"{label}: dpi")
    case.assertEqual(icc, want[4], f"{label}: icc_profile")
    tally.add(fmt, "same")


# ── GIF ────────────────────────────────────────────────────────────────────


def _pack_codes(codes: list[tuple[int, int]]) -> bytes:
    """(code, size) pairs, LSB first."""
    out = bytearray()
    acc = n = 0
    for code, size in codes:
        acc |= code << n
        n += size
        while n >= 8:
            out.append(acc & 0xFF)
            acc >>= 8
            n -= 8
    if n:
        out.append(acc & 0xFF)
    return bytes(out)


def lzw_codes(indices, min_size: int, *, end: bool = True) -> list[tuple[int, int]]:
    """GIF LZW (clear first, table reset at 4096, end code)."""
    clear, eoi = 1 << min_size, (1 << min_size) + 1
    size = min_size + 1
    codes = [(clear, size)]
    table = {(i,): i for i in range(clear)}
    nxt = eoi + 1
    since_clear = 0
    w: tuple = ()
    for k in indices:
        wk = w + (k,)
        if wk in table:
            w = wk
            continue
        codes.append((table[w], size))
        since_clear += 1
        if nxt < 4096:
            table[wk] = nxt
            nxt += 1
            if nxt > (1 << size) and size < 12:
                size += 1
        else:
            codes.append((clear, size))
            table = {(i,): i for i in range(clear)}
            nxt, size, since_clear = eoi + 1, min_size + 1, 0
        w = (k,)
    if w:
        codes.append((table[w], size))
        since_clear += 1
    if end:
        if since_clear >= 2 and nxt >= (1 << size) and size < 12 and nxt < 4096:
            size += 1
        codes.append((eoi, size))
    return codes


def sub_blocks(data: bytes, n: int = 255) -> bytes:
    out = bytearray()
    for i in range(0, len(data), n):
        chunk = data[i:i + n]
        out.append(len(chunk))
        out += chunk
    out.append(0)
    return bytes(out)


def gif_file(screen, gpal: bytes | None, blocks: list[bytes], *, version=b"GIF89a", trailer=True, flags=None, bg=0) -> bytes:
    w, h = screen
    if gpal is not None:
        bits = max(1, (len(gpal) // 3 - 1).bit_length())
        f = 0x80 | (bits - 1)
        gpal = gpal + bytes(3 * (1 << bits) - len(gpal))
    else:
        f = 0
    if flags is not None:
        f = flags
    out = version + struct.pack("<HHBBB", w, h, f, bg, 0) + (gpal or b"")
    for b in blocks:
        out += b
    if trailer:
        out += b";"
    return out


def gif_gce(transparency=None, disposal=0, delay=0, raw: bytes | None = None) -> bytes:
    if raw is not None:
        return b"!\xf9" + bytes([len(raw)]) + raw + b"\0"
    flags = (disposal << 2) | (1 if transparency is not None else 0)
    return b"!\xf9\x04" + struct.pack("<BHB", flags, delay, transparency or 0) + b"\0"


def gif_image(box, indices, *, min_size=8, lpal: bytes | None = None, interlace=False, data: bytes | None = None,
              lflags=None) -> bytes:
    x, y, w, h = box
    f = 0x40 if interlace else 0
    if lpal is not None:
        bits = max(1, (len(lpal) // 3 - 1).bit_length())
        f |= 0x80 | (bits - 1)
        lpal = lpal + bytes(3 * (1 << bits) - len(lpal))
    if lflags is not None:
        f = lflags
    if data is None:
        data = sub_blocks(_pack_codes(lzw_codes(indices, min_size)))
    return b"," + struct.pack("<HHHHB", x, y, w, h, f) + (lpal or b"") + bytes([min_size]) + data


def _interlace_order(h: int) -> list[int]:
    return [y for start, step in ((0, 8), (4, 8), (2, 4), (1, 2)) for y in range(start, h, step)]


def generated_gifs(rng: np.random.Generator) -> list[tuple[str, bytes]]:
    out: list[tuple[str, bytes]] = []

    def pil(label, im, **kw):
        buf = io.BytesIO()
        try:
            im.save(buf, "GIF", **kw)
        except (OSError, ValueError):
            return
        out.append((label, buf.getvalue()))

    # Pillow's writer, every mode and option.
    for size in ((1, 1), (7, 5), (40, 23), (17, 64)):
        w, h = size
        rgba = Image.fromarray(rng.integers(0, 256, (h, w, 4), dtype=np.uint8), "RGBA")
        for mode in ("1", "L", "P", "RGB", "RGBA", "LA", "I;16", "PA"):
            im = rgba.convert(mode) if mode not in ("P", "PA") else rgba.convert("RGB").quantize(16 if w < 20 else 200)
            if mode == "PA":
                im = im.convert("PA")
            pil(f"pil {mode} {size}", im)
            pil(f"pil {mode} {size} no interlace", im, interlace=False)
            pil(f"pil {mode} {size} optimize", im, optimize=True)
        p = rgba.convert("RGB").quantize(8)
        pil(f"pil P {size} transparency", p, transparency=3)
        pil(f"pil P {size} transparency 0 bg 2", p, transparency=0, background=2)
        pil(f"pil P {size} comment", p, comment=b"hello" * 70)
        pil(f"pil P {size} 2 colours", rgba.convert("RGB").quantize(2))
        pil(f"pil P {size} palette 256", rgba.convert("RGB").quantize(256))
        pil(f"pil L {size} transparency", rgba.convert("L"), transparency=10)
        grey = Image.fromarray(np.arange(w * h, dtype=np.uint8).reshape(h, w) % 7, "L")
        pil(f"pil L ramp {size}", grey)
        pil(f"pil L ramp {size} transparency", grey, transparency=3)
        frames = [Image.fromarray(rng.integers(0, 256, (h, w, 3), dtype=np.uint8), "RGB").quantize(12) for _ in range(3)]
        for disposal in (0, 1, 2, 3):
            pil(f"pil anim {size} disposal {disposal}", frames[0], save_all=True, append_images=frames[1:],
                disposal=disposal, duration=50, loop=0)
        pil(f"pil anim {size} transparency", frames[0], save_all=True, append_images=frames[1:], transparency=2)
        rgba_frames = [Image.fromarray(rng.integers(0, 256, (h, w, 4), dtype=np.uint8), "RGBA") for _ in range(2)]
        pil(f"pil anim RGBA {size}", rgba_frames[0], save_all=True, append_images=rgba_frames[1:])

    # Hand-built: palettes, extents, transparency, interlace, LZW edges.
    pal4 = bytes([255, 0, 0, 0, 255, 0, 0, 0, 255, 200, 200, 200])
    pal256 = bytes(rng.integers(0, 256, 768, dtype=np.uint8))
    ramp = bytes(v for i in range(256) for v in (i, i, i))
    px = list(rng.integers(0, 4, 6 * 5))
    img = gif_image((0, 0, 6, 5), px, min_size=2)
    out.append(("plain", gif_file((6, 5), pal4, [img])))
    out.append(("87a", gif_file((6, 5), pal4, [img], version=b"GIF87a")))
    out.append(("no trailer", gif_file((6, 5), pal4, [img], trailer=False)))
    out.append(("no global palette", gif_file((6, 5), None, [img])))
    out.append(("no palette, high indices", gif_file((6, 5), None, [gif_image((0, 0, 6, 5), [200, 3, 255, 0] * 7 + [9, 9], min_size=8)])))
    out.append(("local palette", gif_file((6, 5), None, [gif_image((0, 0, 6, 5), px, min_size=2, lpal=pal4)])))
    out.append(("local over global", gif_file((6, 5), pal256, [gif_image((0, 0, 6, 5), px, min_size=2, lpal=pal4)])))
    out.append(("global ramp", gif_file((6, 5), ramp, [gif_image((0, 0, 6, 5), list(rng.integers(0, 256, 30)), min_size=8)])))
    out.append(("local ramp, no global", gif_file((6, 5), None, [gif_image((0, 0, 6, 5), px, min_size=2, lpal=ramp[:12])])))
    out.append(("local ramp, global ramp", gif_file((6, 5), ramp, [gif_image((0, 0, 6, 5), px, min_size=2, lpal=ramp[:12])])))
    out.append(("local ramp under global (declined)", gif_file((6, 5), pal4, [gif_image((0, 0, 6, 5), px, min_size=2, lpal=ramp[:12])])))
    out.append(("2-entry palette, index 3", gif_file((6, 5), pal4[:6], [img])))
    out.append(("frame inside screen", gif_file((10, 9), pal4, [gif_image((2, 3, 6, 5), px, min_size=2)])))
    out.append(("frame inside screen, transparent", gif_file((10, 9), pal4, [gif_gce(2), gif_image((2, 3, 6, 5), px, min_size=2)])))
    out.append(("frame past screen", gif_file((4, 2), pal4, [gif_image((3, 1, 6, 5), px, min_size=2)])))
    out.append(("screen 0x0", gif_file((0, 0), pal4, [img])))
    out.append(("screen 0x0, frame 0x0", gif_file((0, 0), pal4, [gif_image((0, 0, 0, 0), [], min_size=2)])))
    out.append(("zero-width frame", gif_file((6, 5), pal4, [gif_image((0, 0, 0, 5), [], min_size=2)])))
    for t in (0, 1, 3, 7):
        out.append((f"transparency {t}", gif_file((6, 5), pal4, [gif_gce(t), img])))
        out.append((f"transparency {t}, no palette", gif_file((6, 5), None, [gif_gce(t), img])))
    out.append(("two GCEs", gif_file((6, 5), pal4, [gif_gce(1), gif_gce(None, 2), img])))
    out.append(("GCE disposal 2", gif_file((6, 5), pal4, [gif_gce(1, 2), img])))
    out.append(("short GCE", gif_file((6, 5), pal4, [gif_gce(raw=b"\x00\x01"), img])))
    out.append(("short GCE with flag", gif_file((6, 5), pal4, [gif_gce(raw=b"\x01\x01\x00"), img])))
    out.append(("long GCE", gif_file((6, 5), pal4, [gif_gce(raw=b"\x01\x00\x00\x02\x09\x09"), img])))
    out.append(("empty GCE", gif_file((6, 5), pal4, [b"!\xf9\x00", img])))
    comment = b"!\xfe" + sub_blocks(b"a comment " * 40)
    out.append(("comment", gif_file((6, 5), pal4, [comment, img])))
    app = b"!\xff\x0bNETSCAPE2.0\x03\x01\x00\x00\x00"
    out.append(("netscape", gif_file((6, 5), pal4, [app, img])))
    out.append(("unknown extension", gif_file((6, 5), pal4, [b"!\x01" + sub_blocks(b"x" * 300), img])))
    out.append(("garbage between blocks", gif_file((6, 5), pal4, [b"\x00\x07zz", img])))
    out.append(("only extensions", gif_file((6, 5), pal4, [comment])))
    out.append(("empty", gif_file((6, 5), pal4, [])))
    out.append(("header only", b"GIF89a\x06\x00\x05\x00"))
    out.append(("palette cut", gif_file((6, 5), pal256, [img])[:100]))
    for h in range(1, 12):
        idx = list(rng.integers(0, 4, 3 * h))
        rows = [idx[i * 3:(i + 1) * 3] for i in range(h)]
        stream = [v for y in _interlace_order(h) for v in rows[y]]
        out.append((f"interlaced h={h}", gif_file((3, h), pal4, [gif_image((0, 0, 3, h), stream, min_size=2, interlace=True)])))
    big = list(rng.integers(0, 256, 300 * 200))
    out.append(("big 256 colours", gif_file((300, 200), pal256, [gif_image((0, 0, 300, 200), big, min_size=8)])))
    out.append(("big interlaced", gif_file((300, 200), pal256, [gif_image((0, 0, 300, 200), big, min_size=8, interlace=True)])))
    flat = [5] * (120 * 100)
    out.append(("runs (KwKwK)", gif_file((120, 100), pal256, [gif_image((0, 0, 120, 100), flat, min_size=8)])))
    for m in (1, 2, 3, 4, 7, 8):
        idx = list(rng.integers(0, 1 << m, 40 * 9))
        out.append((f"min code size {m}", gif_file((40, 9), pal256, [gif_image((0, 0, 40, 9), idx, min_size=m)])))
    for m in (0, 9, 12, 13):
        codes = [((1 << m), m + 1)] + [(1, m + 1)] * 4 + [((1 << m) + 1, m + 1)]
        out.append((f"code size {m}", gif_file((2, 2), pal4, [gif_image((0, 0, 2, 2), [], min_size=m, data=sub_blocks(_pack_codes(codes)))])))
    # Every pixel behind its own clear code: the table never grows.
    codes = [c for v in px for c in ((4, 3), (v, 3))] + [(5, 3)]
    out.append(("clear before each pixel", gif_file((6, 5), pal4, [gif_image((0, 0, 6, 5), [], min_size=2, data=sub_blocks(_pack_codes(codes)))])))
    # Ends early: the end code, then only the trailer.
    short = lzw_codes(px[:20], 2)
    out.append(("ends early", gif_file((6, 5), pal4, [gif_image((0, 0, 6, 5), [], min_size=2, data=sub_blocks(_pack_codes(short)))])))
    # No end code, but the pixels are all there; extra codes after the image.
    out.append(("no end code", gif_file((6, 5), pal4, [gif_image((0, 0, 6, 5), [], min_size=2, data=sub_blocks(_pack_codes(lzw_codes(px, 2, end=False))))])))
    out.append(("extra pixels", gif_file((6, 5), pal4, [gif_image((0, 0, 6, 5), px + px, min_size=2)])))
    # An end code in the middle, then the rest of the codes in the same
    # sub-block (Pillow reads on after it when more data follows).
    first = lzw_codes(px[:10], 2)
    mid = first + [(4, first[-1][1])] + lzw_codes(px[10:], 2)[1:]
    out.append(("end code mid-stream", gif_file((6, 5), pal4, [gif_image((0, 0, 6, 5), [], min_size=2, data=sub_blocks(_pack_codes(mid)))])))
    # The same, with more than 64 KiB of image data after the end code.
    many = list(rng.integers(0, 256, 400 * 400))
    first = lzw_codes(many[:1000], 8)
    codes = first + [(256, first[-1][1])] + lzw_codes(many[1000:], 8)[1:]
    stream = sub_blocks(_pack_codes(codes))
    out.append(("end code mid-stream, 64 KiB later", gif_file((400, 400), pal256, [gif_image((0, 0, 400, 400), [], min_size=8, data=stream)])))
    # Corrupt codes: past the table, first code past clear.
    out.append(("code past table", gif_file((6, 5), pal4, [gif_image((0, 0, 6, 5), [], min_size=2, data=sub_blocks(_pack_codes([(4, 3), (1, 3), (7, 3), (5, 3)])))])))
    out.append(("first code past clear", gif_file((6, 5), pal4, [gif_image((0, 0, 6, 5), [], min_size=2, data=sub_blocks(_pack_codes([(4, 3), (6, 3), (5, 3)])))])))
    out.append(("no image data", gif_file((6, 5), pal4, [gif_image((0, 0, 6, 5), [], min_size=2, data=b"\0")])))
    full = gif_file((40, 23), pal256, [gif_image((0, 0, 40, 23), list(rng.integers(0, 256, 40 * 23)), min_size=8)])
    for cut in (30, 790, 800, 810, len(full) // 2, len(full) - 3, len(full) - 1):
        out.append((f"cut at {cut}", full[:cut]))
    # Sub-blocks of odd sizes.
    data = _pack_codes(lzw_codes(list(rng.integers(0, 256, 40 * 23)), 8))
    out.append(("1-byte sub-blocks", gif_file((40, 23), pal256, [gif_image((0, 0, 40, 23), [], min_size=8, data=sub_blocks(data, 1))])))
    out.append(("7-byte sub-blocks", gif_file((40, 23), pal256, [gif_image((0, 0, 40, 23), [], min_size=8, data=sub_blocks(data, 7))])))
    return out


@unittest.skipIf(_test_module() is None, "tmi_accel with raster_decode is not available")
class GifTests(unittest.TestCase):
    def test_generated(self) -> None:
        tally = Tally()
        for label, data in generated_gifs(np.random.default_rng(1)):
            compare(self, label, data, tally)
        _report(f"GIF generated: {tally}")
        self.assertGreater(tally.get("gif", "same"), 150)
        self.assertEqual(tally.get("gif", "declined (GIF: grey-ramp local palette under a global one)"), 1)

    def test_pillow_images(self) -> None:
        base = _external("pillow")
        if base is None:
            self.skipTest("no test images (python tools/fetch_test_images.py)")
        tally = Tally()
        for p in sorted(base.rglob("*.gif")):
            compare(self, p.name, p.read_bytes(), tally)
        _report(f"GIF Pillow images: {tally}")
        self.assertGreater(tally.get("gif", "same"), 25)


# ── BMP ────────────────────────────────────────────────────────────────────


def _bmp_rows(rng, w: int, h: int, bits: int) -> bytes:
    stride = ((w * bits + 31) >> 3) & ~3
    return bytes(rng.integers(0, 256, stride * h, dtype=np.uint8))


def bmp_file(w: int, h: int, bits: int, pixels: bytes, *, header: int = 40, compression: int = 0, colors: int = 0,
             palette: bytes = b"", masks: tuple | None = None, ppm=(2835, 2835), offset: int | None = None,
             dib: bool = False, masks_after: bool = False) -> bytes:
    """A BMP (or DIB) with the given info header size; ``masks`` go into the
    header (52 bytes and more) or after a 40-byte one (``masks_after``)."""
    if header == 12:
        info = struct.pack("<IHHHH", 12, w & 0xFFFF, h & 0xFFFF, 1, bits)
    else:
        info = struct.pack("<IiiHHIIiiII", header, w, h, 1, bits, compression, len(pixels), ppm[0], ppm[1], colors, 0)
        tail = b""
        if masks is not None and not masks_after:
            tail = struct.pack("<4I", *(tuple(masks) + (0,) * (4 - len(masks))))
        info += (tail + bytes(header))[:header - 40]
        if masks is not None and masks_after:
            info += struct.pack("<3I", *masks[:3])
    body = info + palette
    if dib:
        return body + pixels
    off = 14 + len(body) if offset is None else offset
    return b"BM" + struct.pack("<IHHI", 14 + len(body) + len(pixels), 0, 0, off) + body + pixels


def generated_bmps(rng: np.random.Generator) -> list[tuple[str, bytes]]:
    out: list[tuple[str, bytes]] = []

    def pil(label, im, fmt="BMP", **kw):
        buf = io.BytesIO()
        try:
            im.save(buf, fmt, **kw)
        except (OSError, ValueError, KeyError):
            return
        out.append((label, buf.getvalue()))

    for size in ((1, 1), (7, 5), (33, 17), (64, 3)):
        w, h = size
        rgba = Image.fromarray(rng.integers(0, 256, (h, w, 4), dtype=np.uint8), "RGBA")
        for mode in ("1", "L", "P", "RGB", "RGBA", "LA", "I;16", "CMYK"):
            im = rgba.convert(mode) if mode != "P" else rgba.convert("RGB").quantize(37)
            pil(f"pil {mode} {size}", im)
            pil(f"pil {mode} {size} dib", im, "DIB")
            pil(f"pil {mode} {size} dpi", im, dpi=(300, 72.5))
        pil(f"pil P {size} 2 colours", rgba.convert("RGB").quantize(2))
        pil(f"pil L ramp {size}", Image.fromarray((np.arange(w * h) % 256).astype(np.uint8).reshape(h, w), "L"))

    grey = lambda n, pad: b"".join(bytes([i & 255] * 3) + b"\0" * (pad - 3) for i in range(n))  # noqa: E731
    w, h = 13, 7
    for header in (12, 40, 52, 56, 64, 108, 124):
        pad = 3 if header == 12 else 4
        for bits in (1, 4, 8, 16, 24, 32):
            n = 1 << bits if bits <= 8 else 0
            pal = bytes(rng.integers(0, 256, n * pad, dtype=np.uint8))
            rows = _bmp_rows(rng, w, h, bits)
            out.append((f"h{header} {bits} bits", bmp_file(w, h, bits, rows, header=header, palette=pal)))
            out.append((f"h{header} {bits} bits dib", bmp_file(w, h, bits, rows, header=header, palette=pal, dib=True)))
            if header != 12:
                out.append((f"h{header} {bits} bits top-down", bmp_file(w, -h, bits, rows, header=header, palette=pal)))
            if bits <= 8:
                out.append((f"h{header} {bits} bits grey", bmp_file(w, h, bits, rows, header=header, palette=grey(n, pad))))
                bw = (b"\0\0\0" + b"\0" * (pad - 3) + b"\xff\xff\xff" + b"\0" * (pad - 3))
                if header != 12:
                    out.append((f"h{header} {bits} bits black/white", bmp_file(w, h, bits, rows, header=header, colors=2, palette=bw)))
                    out.append((f"h{header} {bits} bits 3 colours", bmp_file(w, h, bits, rows, header=header, colors=3, palette=pal[:3 * pad])))
                    out.append((f"h{header} {bits} bits offset 0", bmp_file(w, h, bits, rows, header=header, palette=pal, offset=0)))
                    out.append((f"h{header} {bits} bits offset at palette", bmp_file(w, h, bits, pal + rows, header=header, offset=14 + header)))
                out.append((f"h{header} {bits} bits short", bmp_file(w, h, bits, rows[:-3], header=header, palette=pal)))
    # Bit fields: Pillow's layouts and others.
    layouts = [
        (32, (0xFF0000, 0xFF00, 0xFF, 0)), (32, (0xFF000000, 0xFF0000, 0xFF00, 0)), (32, (0xFF000000, 0xFF00, 0xFF, 0)),
        (32, (0xFF000000, 0xFF0000, 0xFF00, 0xFF)), (32, (0xFF, 0xFF00, 0xFF0000, 0xFF000000)),
        (32, (0xFF0000, 0xFF00, 0xFF, 0xFF000000)), (32, (0xFF000000, 0xFF00, 0xFF, 0xFF0000)), (32, (0, 0, 0, 0)),
        (32, (0xFF, 0xFF00, 0xFF0000, 0)), (24, (0xFF0000, 0xFF00, 0xFF, 0)), (24, (0xFF, 0xFF00, 0xFF0000, 0)),
        (16, (0xF800, 0x7E0, 0x1F, 0)), (16, (0x7C00, 0x3E0, 0x1F, 0)), (16, (0x7C00, 0x3E0, 0x1F, 0x8000)),
        (16, (0xF00, 0xF0, 0xF, 0xF000)), (8, (0xE0, 0x1C, 0x3, 0)),
    ]
    for bits, masks in layouts:
        rows = _bmp_rows(rng, w, h, bits)
        for header in (40, 52, 56, 108, 124):
            out.append((f"bitfields {bits} {masks} h{header}", bmp_file(w, h, bits, rows, header=header, compression=3,
                                                                        masks=masks, masks_after=header == 40)))
    out.append(("bitfields h40 masks cut", bmp_file(w, h, 32, b"", compression=3, masks=(0xFF0000, 0xFF00, 0xFF), masks_after=True)[:58]))
    # RLE8 / RLE4.
    pal8 = bytes(rng.integers(0, 256, 256 * 4, dtype=np.uint8))
    pal4 = pal8[:64]
    rle_cases = {
        "runs and EOL": bytes([5, 1, 3, 2, 0, 0, 13, 7, 0, 0, 2, 9, 0, 2, 3, 1, 4, 5, 0, 1]),
        "absolute odd": bytes([0, 5, 1, 2, 3, 4, 5, 0, 0, 0, 0, 3, 7, 8, 9, 0, 6, 1, 0, 1]),
        "absolute even": bytes([0, 4, 1, 2, 3, 4, 0, 0, 0, 6, 9, 8, 7, 6, 5, 4, 0, 0] * 4 + [0, 1]),
        "overlong run": bytes([200, 3, 0, 0, 20, 4, 0, 0] * 5 + [0, 1]),
        "delta past end": bytes([3, 1, 0, 2, 200, 9, 0, 1]),
        "no end": bytes([13, 1] * 7),
        "short": bytes([13, 1] * 3),
        "cut absolute": bytes([13, 1, 0, 9, 1, 2]),
        "cut delta": bytes([13, 1, 0, 2, 1]),
        "end of bitmap early": bytes([13, 1, 0, 0, 0, 1]),
        "end of bitmap at end": bytes([13, 1, 0, 0] * 7 + [0, 1]),
        "padded rows": bytes([1, 1, 0, 0] * 7 + [0, 1]),
        "absolute then run": bytes([0, 3, 9, 8, 7, 0, 10, 2, 0, 0] * 7 + [0, 1]),
    }
    for name, stream in rle_cases.items():
        out.append((f"rle8 {name}", bmp_file(w, h, 8, stream, compression=1, palette=pal8)))
        out.append((f"rle4 {name}", bmp_file(w, h, 4, stream, compression=2, palette=pal4)))
        out.append((f"rle8 {name} top-down", bmp_file(w, -h, 8, stream, compression=1, palette=pal8)))
        out.append((f"rle8 {name} grey", bmp_file(w, h, 8, stream, compression=1, palette=grey(256, 4))))
    stream = bytes(v for _ in range(h) for v in (w, int(rng.integers(0, 256)), 0, 0)) + b"\0\1"
    out.append(("rle8 odd offset", bmp_file(w, h, 8, b"\0" + stream, compression=1, palette=pal8, offset=14 + 40 + 1024 + 1)))
    out.append(("rle8 on 24 bits", bmp_file(w, h, 24, stream, compression=1)))
    out.append(("rle8 black/white", bmp_file(w, h, 8, stream, compression=1, colors=2, palette=b"\0\0\0\0\xff\xff\xff\0")))
    out.append(("rle4 on 8 bits", bmp_file(w, h, 8, stream, compression=2, palette=pal8)))
    # Headers and fields.
    rows = _bmp_rows(rng, w, h, 24)
    for comp in (4, 5, 6, 11):
        out.append((f"compression {comp}", bmp_file(w, h, 24, rows, compression=comp)))
    for bits in (0, 2, 15, 48, 64):
        out.append((f"{bits} bits", bmp_file(w, h, bits, rows)))
    out.append(("width 0", bmp_file(0, h, 24, rows)))
    out.append(("height 0", bmp_file(w, 0, 24, rows)))
    out.append(("huge", bmp_file(70000, 70000, 24, rows)))
    out.append(("header 20", bmp_file(w, h, 24, rows)[:14] + struct.pack("<I", 20) + bytes(16) + rows))
    out.append(("header cut", bmp_file(w, h, 24, rows)[:30]))
    out.append(("no pixels", bmp_file(w, h, 24, b"")))
    out.append(("offset past end", bmp_file(w, h, 24, rows, offset=10 ** 6)))
    out.append(("colors 300", bmp_file(w, h, 8, _bmp_rows(rng, w, h, 8), colors=300, palette=bytes(rng.integers(0, 256, 1200, dtype=np.uint8)))))
    out.append(("colors 300 grey", bmp_file(w, h, 8, _bmp_rows(rng, w, h, 8), colors=300, palette=grey(300, 4))))
    out.append(("colors 70000", bmp_file(w, h, 8, _bmp_rows(rng, w, h, 8), colors=70000)))
    out.append(("palette cut", bmp_file(w, h, 8, b"", palette=pal8[:100])))
    out.append(("dpi 0", bmp_file(w, h, 24, rows, ppm=(0, 0))))
    out.append(("dpi odd", bmp_file(w, h, 24, rows, ppm=(11811, 1))))
    return out


def _corpus(*names: str, suffixes: tuple[str, ...]) -> list[Path]:
    files: list[Path] = []
    for name in names:
        base = _external(name)
        if base is not None:
            files += sorted(p for p in base.rglob("*") if p.is_file() and p.suffix.lower() in suffixes)
    return files


@unittest.skipIf(_test_module() is None, "tmi_accel with raster_decode is not available")
class BmpTests(unittest.TestCase):
    def test_generated(self) -> None:
        tally = Tally()
        for label, data in generated_bmps(np.random.default_rng(2)):
            compare(self, label, data, tally)
        _report(f"BMP generated: {tally}")
        self.assertGreater(tally.get("bmp", "same") + tally.get("dib", "same"), 300)

    def test_corpora(self) -> None:
        files = _corpus("pillow", "bmpsuite", suffixes=(".bmp", ".dib"))
        if not files:
            self.skipTest("no test images (python tools/fetch_test_images.py)")
        tally = Tally()
        for p in files:
            compare(self, str(p.relative_to(p.parents[1])), p.read_bytes(), tally)
        _report(f"BMP corpora: {tally}")
        self.assertGreater(tally.get("bmp", "same"), 80)


# ── TIFF ───────────────────────────────────────────────────────────────────


def tiff_lzw(data: bytes) -> bytes:
    """TIFF LZW (MSB first, early change, clear at the start and when the
    table is full, end code)."""
    out = bytearray()
    acc = n = 0
    nbits = 9

    def put(code: int) -> None:
        nonlocal acc, n
        acc = (acc << nbits) | code
        n += nbits
        while n >= 8:
            out.append((acc >> (n - 8)) & 0xFF)
            n -= 8
        acc &= (1 << n) - 1

    table: dict[bytes, int] = {}
    free = 258
    put(256)
    w = b""
    for b in data:
        wk = w + bytes([b])
        if len(wk) == 1 or wk in table:
            w = wk
            continue
        put(table[w] if len(w) > 1 else w[0])
        table[wk] = free
        free += 1
        if free == 4094:
            put(256)
            table.clear()
            free, nbits = 258, 9
        elif free > (1 << nbits) - 1:
            nbits += 1
        w = bytes([b])
    if w:
        put(table[w] if len(w) > 1 else w[0])
        free += 1
        if free > (1 << nbits) - 1 and nbits < 12:
            nbits += 1
    put(257)
    if n:
        out.append((acc << (8 - n)) & 0xFF)
    return bytes(out)


def packbits(data: bytes) -> bytes:
    out = bytearray()
    i = 0
    while i < len(data):
        run = 1
        while i + run < len(data) and run < 128 and data[i + run] == data[i]:
            run += 1
        if run > 1:
            out += bytes([257 - run, data[i]])
            i += run
            continue
        j = i
        while j < len(data) and j - i < 128 and (j + 1 >= len(data) or data[j + 1] != data[j]):
            j += 1
        j = max(j, i + 1)
        out.append(j - i - 1)
        out += data[i:j]
        i = j
    return bytes(out)


def _pack_rows(a: np.ndarray, bps: int, be: bool) -> bytes:
    """Rows of samples (h, n) as TIFF bytes."""
    if bps == 16:
        return a.astype(">u2" if be else "<u2").tobytes()
    if bps == 8:
        return a.astype(np.uint8).tobytes()
    per = 8 // bps
    h, n = a.shape
    padded = np.zeros((h, -(-n // per) * per), np.uint8)
    padded[:, :n] = a
    packed = np.zeros((h, padded.shape[1] // per), np.uint8)
    for k in range(per):
        packed |= (padded[:, k::per] << (8 - bps * (k + 1))).astype(np.uint8)
    return packed.tobytes()


def _predict(a: np.ndarray, stride: int, bps: int) -> np.ndarray:
    a = a.astype(np.int64)
    d = a.copy()
    d[:, stride:] = a[:, stride:] - a[:, :-stride]
    return d % (1 << bps)


TYPES = {1: "B", 2: "s", 3: "H", 4: "I", 5: "II", 7: "B", 8: "h", 9: "i", 11: "f", 12: "d"}


def tiff_file(px: np.ndarray, bps: int, photometric: int | None, *, be=False, compression=1, planar=1, rows=None,
              tile=None, predictor=1, tags=(), drop=(), fillorder=1, cut=None, magic=None) -> bytes:
    """A one-image TIFF from samples ``px`` (h, w, spp)."""
    h, w, spp = px.shape
    e = ">" if be else "<"
    planes = [px[:, :, s:s + 1] for s in range(spp)] if planar == 2 else [px]

    def compress(raw: bytes) -> bytes:
        c = {1: lambda b: b, 5: tiff_lzw, 8: zlib.compress, 32946: zlib.compress, 32773: packbits}.get(compression, lambda b: b)(raw)
        if fillorder == 2:
            c = bytes(int(f"{v:08b}"[::-1], 2) for v in c)
        return c

    striles = []
    if tile is None:
        rps = rows or h
        for plane in planes:
            for y in range(0, h, rps):
                block = plane[y:y + rps].reshape(-1, plane.shape[1] * plane.shape[2])
                if predictor == 2:
                    block = _predict(block, plane.shape[2], bps)
                striles.append(compress(_pack_rows(block, bps, be)))
    else:
        tw, th = tile
        for plane in planes:
            for y in range(0, h, th):
                for x in range(0, w, tw):
                    t = np.zeros((th, tw, plane.shape[2]), px.dtype)
                    part = plane[y:y + th, x:x + tw]
                    t[:part.shape[0], :part.shape[1]] = part
                    block = t.reshape(th, -1)
                    if predictor == 2:
                        block = _predict(block, plane.shape[2], bps)
                    striles.append(compress(_pack_rows(block, bps, be)))
    body = bytearray()
    offsets = []
    for s in striles:
        offsets.append(8 + len(body))
        body += s
        if len(body) % 2:
            body += b"\0"
    entries = {256: (4, [w]), 257: (4, [h]), 258: (3, [bps] * spp), 259: (3, [compression]), 277: (3, [spp])}
    if photometric is not None:
        entries[262] = (3, [photometric])
    if planar != 1:
        entries[284] = (3, [planar])
    if predictor != 1:
        entries[317] = (3, [predictor])
    if fillorder != 1:
        entries[266] = (3, [fillorder])
    if tile is None:
        entries[273] = (4, offsets)
        entries[278] = (4, [rows or h])
        entries[279] = (4, [len(s) for s in striles])
    else:
        entries[322] = (4, [tile[0]])
        entries[323] = (4, [tile[1]])
        entries[324] = (4, offsets)
        entries[325] = (4, [len(s) for s in striles])
    for tag, typ, values in tags:
        entries[tag] = (typ, values)
    for tag in drop:
        entries.pop(tag, None)
    ifd_at = 8 + len(body)
    extra = bytearray()
    table = bytearray(struct.pack(e + "H", len(entries)))
    data_at = ifd_at + 2 + 12 * len(entries) + 4
    for tag in sorted(entries):
        typ, values = entries[tag]
        if typ in (1, 7, 2):
            raw = bytes(values)
            count = len(raw)
        elif typ == 5:
            raw = b"".join(struct.pack(e + "II", *v) for v in values)
            count = len(values)
        else:
            raw = struct.pack(e + TYPES[typ] * len(values), *values)
            count = len(values)
        if len(raw) <= 4:
            table += struct.pack(e + "HHI", tag, typ, count) + raw.ljust(4, b"\0")
        else:
            table += struct.pack(e + "HHII", tag, typ, count, data_at + len(extra))
            extra += raw
            if len(extra) % 2:
                extra += b"\0"
    table += b"\0\0\0\0"
    head = magic or ((b"MM\x00\x2a" if be else b"II\x2a\x00"))
    out = head + struct.pack(e + "I", ifd_at) + bytes(body) + bytes(table) + bytes(extra)
    return out[:cut] if cut else out


def generated_tiffs(rng: np.random.Generator) -> list[tuple[str, bytes]]:
    out: list[tuple[str, bytes]] = []

    def pil(label, im, **kw):
        buf = io.BytesIO()
        try:
            with warnings.catch_warnings():
                warnings.simplefilter("ignore")
                im.save(buf, "TIFF", **kw)
        except (OSError, ValueError, KeyError, TypeError):
            return
        out.append((label, buf.getvalue()))

    icc = b"\0" * 128 + b"acsp" + bytes(range(100))
    for size in ((1, 1), (7, 5), (33, 17), (70, 40)):
        w, h = size
        rgba = Image.fromarray(rng.integers(0, 256, (h, w, 4), dtype=np.uint8), "RGBA")
        smooth = Image.fromarray(np.tile(np.arange(w, dtype=np.uint8) * 3, (h, 1)), "L")
        for mode in ("1", "L", "LA", "P", "RGB", "RGBA", "CMYK", "I;16", "I;16B", "YCbCr", "LAB", "PA", "I", "F"):
            try:
                im = rgba.convert(mode) if mode not in ("P", "PA", "I;16", "I;16B", "I", "F") else None
            except ValueError:
                continue
            if mode in ("P", "PA"):
                im = rgba.convert("RGB").quantize(50).convert(mode)
            elif mode in ("I;16", "I;16B"):
                a16 = rng.integers(0, 600, (h, w), dtype=np.uint16)
                im = Image.frombytes(mode, (w, h), a16.astype(">u2" if mode == "I;16B" else "<u2").tobytes())
            elif mode in ("I", "F"):
                im = rgba.convert("L").convert(mode)
            if im is None:
                continue
            for comp in (None, "tiff_lzw", "tiff_adobe_deflate", "tiff_deflate", "packbits", "jpeg"):
                if comp == "jpeg" and mode not in ("L", "RGB"):
                    continue  # libtiff aborts the process on some modes
                pil(f"pil {mode} {size} {comp}", im, compression=comp)
            pil(f"pil {mode} {size} lzw strips", im, compression="tiff_lzw", strip_size=64)
            pil(f"pil {mode} {size} raw strips", im, strip_size=64)
            if mode != "1":  # libtiff aborts the process
                pil(f"pil {mode} {size} lzw predictor", im, compression="tiff_lzw", tiffinfo={317: 2})
                pil(f"pil {mode} {size} deflate predictor", im, compression="tiff_adobe_deflate", tiffinfo={317: 2})
        pil(f"pil L smooth {size} predictor", smooth, compression="tiff_lzw", tiffinfo={317: 2})
        rgb = rgba.convert("RGB")
        pil(f"pil RGB {size} dpi", rgb, dpi=(300, 72.5))
        pil(f"pil RGB {size} dpi cm", rgb, dpi=(300, 72.5), resolution_unit=3)
        pil(f"pil RGB {size} resolution unit 1", rgb, resolution=40, resolution_unit=1)
        pil(f"pil RGB {size} icc", rgb, icc_profile=icc)
        pil(f"pil RGB {size} icc lzw", rgb, icc_profile=icc, compression="tiff_lzw")
        for o in range(1, 9):
            pil(f"pil RGB {size} orientation {o}", rgb, tiffinfo={274: o})
            pil(f"pil RGB {size} orientation {o} lzw", rgb, tiffinfo={274: o}, compression="tiff_lzw")
        pil(f"pil multipage {size}", rgb, save_all=True, append_images=[rgba.convert("L"), rgb])
        pil(f"pil big-endian {size}", rgb, tiffinfo={}, big_tiff=False)

    # Hand-built: layouts, depths, codecs, predictor, byte orders.
    w, h = 23, 13
    shapes = [
        ("grey 8", 1, 8, 1, ()), ("miniswhite 8", 0, 8, 1, ()), ("grey 1", 1, 1, 1, ()), ("miniswhite 1", 0, 1, 1, ()),
        ("grey 2", 1, 2, 1, ()), ("grey 4", 1, 4, 1, ()), ("miniswhite 4", 0, 4, 1, ()), ("grey 16", 1, 16, 1, ()),
        ("miniswhite 16", 0, 16, 1, ()), ("palette 1", 3, 1, 1, ()), ("palette 2", 3, 2, 1, ()), ("palette 4", 3, 4, 1, ()),
        ("palette 8", 3, 8, 1, ()), ("palette + extra", 3, 8, 2, (0,)), ("grey + alpha", 1, 8, 2, (2,)),
        ("rgb 8", 2, 8, 3, ()), ("rgba missing extra", 2, 8, 4, None), ("rgbx", 2, 8, 4, (0,)), ("rgbxx", 2, 8, 5, (0, 0)),
        ("rgba assoc", 2, 8, 4, (1,)), ("rgba assoc + x", 2, 8, 5, (1, 0)), ("rgba unassoc", 2, 8, 4, (2,)),
        ("rgba unassoc + xx", 2, 8, 6, (2, 0, 0)), ("rgba corel", 2, 8, 4, (999,)), ("rgb 16", 2, 16, 3, ()),
        ("rgba 16", 2, 16, 4, (2,)), ("rgba 16 assoc", 2, 16, 4, (1,)), ("rgbx 16", 2, 16, 4, (0,)), ("rgba 16 no extra", 2, 16, 4, None),
        ("cmyk", 5, 8, 4, ()), ("cmyk + x", 5, 8, 5, (0,)), ("cmyk 16", 5, 16, 4, ()),
        ("no photometric", None, 8, 1, ()), ("ycbcr", 6, 8, 3, ()), ("rgb 4", 2, 4, 3, ()),
    ]
    for name, photo, bps, spp, extra in shapes:
        hi = 1 << bps
        px = rng.integers(0, hi, (h, w, spp), dtype=np.uint16 if bps == 16 else np.uint8)
        if extra and (1 in extra[:1]) and bps == 8:
            px[:, :, 3] = rng.integers(0, 256, (h, w))
            px[:, :, :3] = np.minimum(px[:, :, :3], px[:, :, 3:4])
        tags = []
        if extra:
            tags.append((338, 3, list(extra)))
        if photo == 3:
            tags.append((320, 3, [int(v) for v in rng.integers(0, 65536, 3 * hi)]))
        for be in (False, True):
            for comp in (1, 5, 8, 32946, 32773):
                for planar in (1, 2):
                    if planar == 2 and spp == 1 and comp not in (1, 5):
                        continue
                    label = f"{name} {'MM' if be else 'II'} c{comp} planar{planar}"
                    out.append((label, tiff_file(px, bps, photo, be=be, compression=comp, planar=planar, tags=tags)))
                    out.append((label + " strips", tiff_file(px, bps, photo, be=be, compression=comp, planar=planar, rows=4, tags=tags)))
                    out.append((label + " tiles", tiff_file(px, bps, photo, be=be, compression=comp, planar=planar, tile=(16, 16), tags=tags)))
                    if bps in (8, 16) and comp in (5, 8):
                        out.append((label + " predictor", tiff_file(px, bps, photo, be=be, compression=comp, planar=planar, rows=5, predictor=2, tags=tags)))
                        out.append((label + " tiles predictor", tiff_file(px, bps, photo, be=be, compression=comp, planar=planar, tile=(16, 16), predictor=2, tags=tags)))
            if bps <= 8:
                for comp in (1, 5, 32773):
                    out.append((f"{name} {'MM' if be else 'II'} c{comp} fill order 2", tiff_file(px, bps, photo, be=be, compression=comp, fillorder=2, tags=tags)))
    rgb = rng.integers(0, 256, (h, w, 3), dtype=np.uint8)
    for comp in (1, 5, 8, 32773):
        out.append((f"rows past height c{comp}", tiff_file(rgb, 8, 2, compression=comp, rows=100, tags=[(278, 4, [100])])))
        out.append((f"rows per strip max c{comp}", tiff_file(rgb, 8, 2, compression=comp, tags=[(278, 4, [2 ** 32 - 1])])))
        out.append((f"no rows per strip c{comp}", tiff_file(rgb, 8, 2, compression=comp, drop=(278,))))
        out.append((f"no byte counts c{comp}", tiff_file(rgb, 8, 2, compression=comp, drop=(279,))))
        out.append((f"cut c{comp}", tiff_file(rgb, 8, 2, compression=comp, rows=4, cut=300)))
        out.append((f"cut data c{comp}", tiff_file(rgb, 8, 2, compression=comp, rows=4)[:-200]))
        out.append((f"odd tile c{comp}", tiff_file(rgb, 8, 2, compression=comp, tile=(16, 32))))
        out.append((f"predictor 3 c{comp}", tiff_file(rgb, 8, 2, compression=comp, tags=[(317, 3, [3])])))
        out.append((f"predictor 2 on 4 bits c{comp}", tiff_file(rng.integers(0, 16, (h, w, 1), dtype=np.uint8), 4, 1, compression=comp, predictor=2)))
        for o in (0, 1, 2, 5, 6, 8, 9):
            out.append((f"orientation {o} c{comp}", tiff_file(rgb, 8, 2, compression=comp, tags=[(274, 3, [o])])))
        out.append((f"bad magic MM*0 c{comp}", tiff_file(rgb, 8, 2, compression=comp, be=True, magic=b"MM\x2a\x00")))
    for res in (((300, 1), (300, 1), None), ((300, 1), (150, 2), 2), ((118, 1), (118, 1), 3), ((72, 1), (72, 1), 1),
                ((0, 1), (72, 1), 2), ((72, 0), (72, 1), 2), ((1, 3), (2, 3), 3)):
        tags = [(282, 5, [res[0]]), (283, 5, [res[1]])] + ([(296, 3, [res[2]])] if res[2] else [])
        out.append((f"resolution {res}", tiff_file(rgb, 8, 2, tags=tags)))
    out.append(("resolution as long", tiff_file(rgb, 8, 2, tags=[(282, 4, [300]), (283, 4, [300])])))
    out.append(("icc undefined", tiff_file(rgb, 8, 2, tags=[(34675, 7, list(b"icc profile bytes"))])))
    out.append(("icc byte", tiff_file(rgb, 8, 2, tags=[(34675, 1, list(b"icc profile bytes"))])))
    out.append(("width as short", tiff_file(rgb, 8, 2, tags=[(256, 3, [w]), (257, 3, [h])])))
    out.append(("no width", tiff_file(rgb, 8, 2, drop=(256,))))
    out.append(("no offsets", tiff_file(rgb, 8, 2, drop=(273,))))
    out.append(("unknown compression", tiff_file(rgb, 8, 2, compression=99)))
    out.append(("jpeg compression", tiff_file(rgb, 8, 2, compression=7)))
    out.append(("spp 7", tiff_file(rng.integers(0, 256, (h, w, 7), dtype=np.uint8), 8, 2)))
    out.append(("bps 3 values for spp 1", tiff_file(rng.integers(0, 256, (h, w, 1), dtype=np.uint8), 8, 1, tags=[(258, 3, [8, 8, 8])])))
    out.append(("bps 1 value for rgb", tiff_file(rgb, 8, 2, tags=[(258, 3, [8])])))
    out.append(("first IFD 0", b"II\x2a\x00\0\0\0\0" + bytes(20)))
    out.append(("IFD past end", b"II\x2a\x00\xff\0\0\0" + bytes(20)))
    out.append(("bigtiff", b"II\x2b\x00\x08\x00\x00\x00" + bytes(40)))
    out.append(("header only", b"II\x2a\x00"))
    return out


@unittest.skipIf(_test_module() is None, "tmi_accel with raster_decode is not available")
class TiffTests(unittest.TestCase):
    def test_generated(self) -> None:
        tally = Tally()
        for label, data in generated_tiffs(np.random.default_rng(3)):
            compare(self, label, data, tally)
        _report(f"TIFF generated: {tally}")
        self.assertGreater(tally.get("tiff", "same"), 1000)

    def test_corpora(self) -> None:
        files = _corpus("pillow", "libtiff", suffixes=(".tif", ".tiff"))
        if not files:
            self.skipTest("no test images (python tools/fetch_test_images.py)")
        tally = Tally()
        for p in files:
            compare(self, str(p.relative_to(p.parents[1])), p.read_bytes(), tally)
        _report(f"TIFF corpora: {tally}")
        self.assertGreater(tally.get("tiff", "same"), 60)


# ── WebP ───────────────────────────────────────────────────────────────────


def _riff_chunks(data: bytes) -> list[tuple[bytes, bytes]]:
    out, pos = [], 12
    while pos + 8 <= len(data):
        tag, size = data[pos:pos + 4], struct.unpack("<I", data[pos + 4:pos + 8])[0]
        out.append((tag, data[pos + 8:pos + 8 + size]))
        pos += 8 + size + (size & 1)
    return out


def _riff(chunks: list[tuple[bytes, bytes]]) -> bytes:
    body = b"WEBP" + b"".join(t + struct.pack("<I", len(c)) + c + (b"\0" if len(c) % 2 else b"") for t, c in chunks)
    return b"RIFF" + struct.pack("<I", len(body)) + body


def generated_webps(rng: np.random.Generator) -> list[tuple[str, bytes]]:
    out: list[tuple[str, bytes]] = []

    def pil(label, im, **kw):
        buf = io.BytesIO()
        try:
            im.save(buf, "WEBP", **kw)
        except (OSError, ValueError):
            return
        out.append((label, buf.getvalue()))

    icc = b"\0" * 128 + b"acsp" + bytes(range(100))
    for size in ((1, 1), (7, 5), (33, 17), (300, 200)):
        w, h = size
        a = rng.integers(0, 256, (h, w, 4), dtype=np.uint8)
        yy, xx = np.mgrid[0:h, 0:w]
        smooth = np.stack([xx * 255 // max(w, 1), yy * 255 // max(h, 1), (xx + yy) % 256, (xx * 7) % 256], axis=2).astype(np.uint8)
        for name, arr in (("noise", a), ("smooth", smooth)):
            rgba = Image.fromarray(arr, "RGBA")
            for mode in ("RGB", "RGBA", "L", "LA", "P", "1", "CMYK"):
                im = rgba.convert(mode) if mode != "P" else rgba.convert("RGB").quantize(30)
                for q in (0, 50, 80, 100):
                    pil(f"{name} {mode} {size} q{q}", im, quality=q)
                pil(f"{name} {mode} {size} lossless", im, lossless=True)
                pil(f"{name} {mode} {size} lossless exact", im, lossless=True, exact=True)
            pil(f"{name} RGBA {size} method 0", rgba, method=0)
            pil(f"{name} RGBA {size} method 6", rgba, method=6, quality=90)
            pil(f"{name} RGBA {size} alpha q 10", rgba, alpha_quality=10)
            pil(f"{name} RGBA {size} icc", rgba, icc_profile=icc)
            pil(f"{name} RGB {size} icc lossless", rgba.convert("RGB"), icc_profile=icc, lossless=True)
            pil(f"{name} RGB {size} exif", rgba.convert("RGB"), exif=b"Exif\0\0II*\0\x08\0\0\0\0\0")
            opaque = rgba.copy()
            opaque.putalpha(255)
            pil(f"{name} opaque RGBA {size}", opaque)
        frames = [Image.fromarray(rng.integers(0, 256, (h, w, 4), dtype=np.uint8), "RGBA") for _ in range(3)]
        pil(f"anim {size}", frames[0], save_all=True, append_images=frames[1:], duration=40, loop=0)
        pil(f"anim {size} lossless", frames[0], save_all=True, append_images=frames[1:], lossless=True)
        rgbf = [f.convert("RGB") for f in frames]
        pil(f"anim RGB {size}", rgbf[0], save_all=True, append_images=rgbf[1:], background=(10, 20, 30, 255))
        pil(f"anim {size} kmax", frames[0], save_all=True, append_images=frames[1:], kmin=1, kmax=2, allow_mixed=True)

    base = dict(out)
    still = base.get("smooth RGBA (33, 17) q80")
    if still:
        chunks = _riff_chunks(still)
        out.append(("cut", still[:len(still) // 2]))
        out.append(("cut header", still[:20]))
        out.append(("bad riff size", still[:4] + struct.pack("<I", 10 ** 6) + still[8:]))
        out.append(("extra chunk", _riff(chunks + [(b"ABCD", b"xyz")])))
        out.append(("garbage payload", _riff([(t, bytes(len(c))) for t, c in chunks])))
    with_icc = base.get("smooth RGBA (33, 17) icc")
    if with_icc:
        chunks = _riff_chunks(with_icc)
        out.append(("empty ICCP", _riff([(t, b"" if t == b"ICCP" else c) for t, c in chunks])))
    return out


@unittest.skipIf(_test_module() is None, "tmi_accel with raster_decode is not available")
class WebpTests(unittest.TestCase):
    def test_generated(self) -> None:
        tally = Tally()
        for label, data in generated_webps(np.random.default_rng(4)):
            compare(self, label, data, tally)
        _report(f"WebP generated: {tally}")
        self.assertGreater(tally.get("webp", "same"), 250)

    def test_pillow_images(self) -> None:
        files = _corpus("pillow", suffixes=(".webp",))
        if not files:
            self.skipTest("no test images (python tools/fetch_test_images.py)")
        tally = Tally()
        for p in files:
            compare(self, p.name, p.read_bytes(), tally)
        _report(f"WebP Pillow images: {tally}")
        self.assertGreater(tally.get("webp", "same"), 8)


# ── PSD ────────────────────────────────────────────────────────────────────


def _psd_packbits_rows(plane: bytes, row: int) -> tuple[list[int], bytes]:
    counts, data = [], bytearray()
    for y in range(0, len(plane), row):
        c = packbits(plane[y:y + row])
        counts.append(len(c))
        data += c
    return counts, bytes(data)


def psd_file(planes: list[bytes], w: int, h: int, *, mode: int, bits: int = 8, channels: int | None = None,
             compression: int = 0, colour: bytes = b"", resources: bytes = b"", layers: bytes = b"",
             version: int = 1, counts_for: int | None = None, cut: int | None = None) -> bytes:
    """A PSD whose image data section holds ``planes`` (one per channel)."""
    channels = len(planes) if channels is None else channels
    out = b"8BPS" + struct.pack(">H6xHIIHH", version, channels, h, w, bits, mode)
    out += struct.pack(">I", len(colour)) + colour
    out += struct.pack(">I", len(resources)) + resources
    out += struct.pack(">I", len(layers)) + layers
    out += struct.pack(">H", compression)
    row = -(-w * bits // 8)
    if compression == 1:
        all_counts, body = [], b""
        for p in planes:
            c, d = _psd_packbits_rows(p, row)
            all_counts += c
            body += d
        out += struct.pack(f">{len(all_counts)}H", *all_counts) + body
    else:
        out += b"".join(planes)
    return out[:cut] if cut else out


def psd_resource(rid: int, data: bytes, name: bytes = b"") -> bytes:
    n = bytes([len(name)]) + name
    if len(n) % 2:
        n += b"\0"
    return b"8BIM" + struct.pack(">H", rid) + n + struct.pack(">I", len(data)) + data + (b"\0" if len(data) % 2 else b"")


def generated_psds(rng: np.random.Generator) -> list[tuple[str, bytes]]:
    out: list[tuple[str, bytes]] = []
    icc = b"\0" * 128 + b"acsp" + bytes(range(101))
    res = psd_resource(1005, bytes(16)) + psd_resource(1039, icc, b"icc") + psd_resource(1036, bytes(7))
    for w, h in ((1, 1), (7, 5), (33, 17), (130, 40)):
        noise = lambda: bytes(rng.integers(0, 256, w * h, dtype=np.uint8))  # noqa: E731
        flat = lambda: bytes(rng.integers(0, 4, w * h, dtype=np.uint8) * 60)  # noqa: E731
        bitmap = bytes(rng.integers(0, 256, -(-w // 8) * h, dtype=np.uint8))
        pal = bytes(rng.integers(0, 256, 768, dtype=np.uint8))
        cases = [
            ("bitmap", 0, 1, [bitmap], {}),
            ("grey", 1, 8, [noise()], {}),
            ("grey flat", 1, 8, [flat()], {}),
            ("grey 0 mode", 0, 8, [noise()], {}),
            ("grey + alpha channel", 1, 8, [noise(), noise()], {}),
            ("indexed", 2, 8, [noise()], {"colour": pal}),
            ("indexed short palette", 2, 8, [noise()], {"colour": pal[:300]}),
            ("indexed no palette", 2, 8, [noise()], {}),
            ("rgb", 3, 8, [noise(), noise(), flat()], {}),
            ("rgba", 3, 8, [noise(), noise(), flat(), noise()], {}),
            ("rgb 5 channels", 3, 8, [noise(), flat(), noise(), noise(), noise()], {}),
            ("cmyk", 4, 8, [noise(), noise(), flat(), noise()], {}),
            ("cmyk 5 channels", 4, 8, [noise(), flat(), noise(), noise(), noise()], {}),
            ("multichannel", 7, 8, [noise(), noise()], {}),
            ("duotone", 8, 8, [noise()], {"colour": bytes(40)}),
            ("lab", 9, 8, [noise(), noise(), noise()], {}),
            ("grey 16", 1, 16, [noise() + noise()], {}),
            ("rgb 16", 3, 16, [noise() + noise() for _ in range(3)], {}),
        ]
        for name, mode, bits, planes, kw in cases:
            for comp in (0, 1):
                label = f"{name} {w}x{h} c{comp}"
                out.append((label, psd_file(planes, w, h, mode=mode, bits=bits, compression=comp, **kw)))
                out.append((label + " resources", psd_file(planes, w, h, mode=mode, bits=bits, compression=comp, resources=res, **kw)))
                out.append((label + " layers", psd_file(planes, w, h, mode=mode, bits=bits, compression=comp, layers=struct.pack(">I", 10) + bytes(10), **kw)))
        rgb = [noise(), noise(), noise()]
        full = psd_file(rgb, w, h, mode=3, compression=1)
        for cut in (20, 30, 40, len(full) // 2, len(full) - 1):
            out.append((f"rgb {w}x{h} cut {cut}", full[:cut]))
        out.append((f"rgb {w}x{h} raw cut", psd_file(rgb, w, h, mode=3)[:-1]))
        out.append((f"rgb {w}x{h} zip", psd_file(rgb, w, h, mode=3, compression=2)))
        out.append((f"rgb {w}x{h} too few channels", psd_file(rgb[:2], w, h, mode=3)))
        out.append((f"rgb {w}x{h} psb", psd_file(rgb, w, h, mode=3, version=2)))
        out.append((f"rgb {w}x{h} two ICC", psd_file(rgb, w, h, mode=3, resources=res + psd_resource(1039, b"second"))))
        out.append((f"rgb {w}x{h} empty ICC", psd_file(rgb, w, h, mode=3, resources=psd_resource(1039, b""))))
        out.append((f"rgb {w}x{h} resource overrun", psd_file(rgb, w, h, mode=3, resources=psd_resource(1039, icc)[:-20])))
    out.append(("zero size", psd_file([b""], 0, 0, mode=1)))
    out.append(("header only", b"8BPS\0\1"))
    return out


@unittest.skipIf(_test_module() is None, "tmi_accel with raster_decode is not available")
class PsdTests(unittest.TestCase):
    def test_generated(self) -> None:
        tally = Tally()
        for label, data in generated_psds(np.random.default_rng(5)):
            compare(self, label, data, tally)
        _report(f"PSD generated: {tally}")
        self.assertGreater(tally.get("psd", "same"), 250)

    def test_pillow_images(self) -> None:
        files = _corpus("pillow", suffixes=(".psd",))
        if not files:
            self.skipTest("no test images (python tools/fetch_test_images.py)")
        tally = Tally()
        for p in files:
            compare(self, p.name, p.read_bytes(), tally)
        _report(f"PSD Pillow images: {tally}")
        self.assertGreater(tally.get("psd", "same"), 8)


if __name__ == "__main__":
    unittest.main()
