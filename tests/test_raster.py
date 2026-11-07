"""tmi's image operations on tmi_accel (``tmi.raster``, RUST_MIGRATION.md 2b)
against what tmi did with Pillow before: the Pillow code is kept here as the
reference, and every result must be the same bytes.

  - decode of PNG / JPEG (pixels, format, the dpi and ICC presence tmi
    records) over the PngSuite, Pillow's test images and generated files
    (dpi from JFIF and from EXIF in its forms, ICC split over markers, MPO,
    Photoshop and Adobe markers);
  - thumbnail, the diff's bbox, side by side, overlay, outlined preview and
    the composite previews of ``diff --html``, Pillow's rectangle outline;
  - the issue #21 write-back: the JPEG bytes Pillow wrote, and a PNG that
    Pillow reads back with the same mode, pixels, palette and metadata.

The PngSuite and Pillow's own test images are read from ``TMI_TEST_IMAGES``
(default ``.test-images``; ``python tools/fetch_test_images.py``).
"""
from __future__ import annotations

import io
import math
import os
import random
import struct
import sys
import unittest
import warnings
from pathlib import Path

import numpy as np
from PIL import Image, ImageChops, ImageCms, ImageDraw, JpegImagePlugin
from PIL.PngImagePlugin import PngInfo

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))

from tmi import diff as diff_mod  # noqa: E402
from tmi import image_ops, raster  # noqa: E402


def _report(line: str) -> None:
    if os.environ.get("TMI_IMAGE_REPORT"):
        print(f"\n  {line}")


def _rgba(rng: np.random.Generator, w: int, h: int, *, opaque: bool = False) -> Image.Image:
    yy, xx = np.mgrid[0:h, 0:w]
    rgb = np.stack([(xx * 7 + yy * 3) % 256, (xx * 2 + yy * 11) % 256, (xx * yy) % 256], -1)
    a = rng.integers(0, 256, size=(h, w))
    a[(xx // 5 + yy // 4) % 3 == 0] = 0
    a[(xx // 3 + yy // 6) % 4 == 1] = 255
    if opaque:
        a[:] = 255
    return Image.fromarray(np.concatenate([rgb, a[..., None]], -1).astype(np.uint8), "RGBA")


def _pil(image) -> Image.Image:
    return image if isinstance(image, Image.Image) else image.to_pil()


def _icc() -> bytes:
    return ImageCms.ImageCmsProfile(ImageCms.createProfile("sRGB")).tobytes()


# ── the Pillow code tmi used before 2b (the reference) ─────────────────────


def ref_same_size(left, right):
    width, height = max(left.width, right.width), max(left.height, right.height)
    if left.size == (width, height) and right.size == (width, height):
        return left, right
    a = Image.new("RGBA", (width, height), (0, 0, 0, 0))
    b = Image.new("RGBA", (width, height), (0, 0, 0, 0))
    a.alpha_composite(left, (0, 0))
    b.alpha_composite(right, (0, 0))
    return a, b


def ref_diff_bbox(left, right):
    a, b = ref_same_size(left.convert("RGBA"), right.convert("RGBA"))
    diff = ImageChops.difference(a, b)
    alpha_box = diff.getchannel("A").getbbox()
    visible_alpha = ImageChops.lighter(a.getchannel("A"), b.getchannel("A"))
    boxes = [alpha_box] + [ImageChops.multiply(band, visible_alpha).getbbox() for band in diff.split()[:3]]
    boxes = [box for box in boxes if box is not None]
    if not boxes:
        return None
    return [min(b[0] for b in boxes), min(b[1] for b in boxes), max(b[2] for b in boxes), max(b[3] for b in boxes)]


def ref_side_by_side(left, right):
    left, right = ref_same_size(left, right)
    out = Image.new("RGBA", (left.width * 2 + 16, left.height), (32, 32, 32, 255))
    for image, at in ((left, (0, 0)), (right, (left.width + 16, 0))):
        if image.getchannel("A").getextrema()[0] == 255:
            out.paste(image, at)
        else:
            out.alpha_composite(image, at)
    return out


def ref_overlay(left, right):
    left, right = ref_same_size(left, right)
    return Image.blend(left, right, 0.5)


def ref_diff_preview(left, right, bbox=None):
    left, right = ref_same_size(left, right)
    if bbox is None:
        bbox = ref_diff_bbox(left, right)
    base = right.copy()
    if bbox:
        ImageDraw.Draw(base).rectangle(tuple(bbox), outline=(255, 0, 0, 255), width=max(2, base.width // 300))
    return base


def ref_thumbnail(image, size=384):
    thumb = image.copy() if image.mode == "RGBA" else image.convert("RGBA")
    thumb.thumbnail((size, size), Image.Resampling.LANCZOS)
    canvas = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    canvas.alpha_composite(thumb, ((size - thumb.width) // 2, (size - thumb.height) // 2))
    return canvas


def _fit(size, edge):
    scale = edge / max(size[0], size[1], 1)
    if scale >= 1:
        return size
    return max(1, round(size[0] * scale)), max(1, round(size[1] * scale))


def _shrink(image, edge):
    size = _fit(image.size, edge)
    return image if size == image.size else image.resize(size, Image.Resampling.LANCZOS)


def ref_composite_previews(old, new, bbox, edge):
    """diff.py's ``_composite_preview_images`` before 2b."""
    if old.size != new.size:
        return {
            "old_png": _shrink(old, edge), "new_png": _shrink(new, edge),
            "side_by_side_png": _shrink(ref_side_by_side(old, new), edge),
            "overlay_png": _shrink(ref_overlay(old, new), edge),
            "diff_preview_png": _shrink(ref_diff_preview(old, new, bbox), edge),
        }
    width, height = new.size
    small = _fit(new.size, edge)
    jobs = [(old, small, None), (new, small, None)]
    gutter = 16
    pair = (2 * width + gutter, height)
    pair_size = _fit(pair, edge)
    if pair_size != pair:
        step = pair[0] / pair_size[0]
        left_w = math.floor(width / step)
        right_x = math.ceil((width + gutter) / step)
        right_w = pair_size[0] - right_x
        if left_w < 1 or right_w < 1:
            pair_size = None
        else:
            jobs.append((old, (left_w, pair_size[1]), (0, 0, min(width, left_w * step), height)))
            jobs.append((new, (right_w, pair_size[1]), (max(0.0, right_x * step - width - gutter), 0, width, height)))
    small_old, small_new, *halves = [
        image if box is None and image.size == size else image.resize(size, Image.Resampling.LANCZOS, box=box)
        for image, size, box in jobs
    ]
    if pair_size is None:
        pair_image = _shrink(ref_side_by_side(old, new), edge)
    elif pair_size == pair:
        pair_image = ref_side_by_side(small_old, small_new)
    else:
        pair_image = Image.new("RGBA", pair_size, (32, 32, 32, 255))
        for image, x in zip(halves, (0, right_x)):
            if image.getchannel("A").getextrema()[0] == 255:
                pair_image.paste(image, (x, 0))
            else:
                pair_image.alpha_composite(image, (x, 0))
        c0, c1 = max(0, left_w - 3), min(pair_size[0], right_x + 3)
        x0, x1 = max(0, math.floor((c0 - 4) * step)), min(pair[0], math.ceil((c1 + 4) * step))
        strip = Image.new("RGBA", (x1 - x0, height), (32, 32, 32, 255))
        if x0 < width:
            strip.alpha_composite(old.crop((x0, 0, width, height)), (0, 0))
        if x1 > width + gutter:
            at = max(0, width + gutter - x0)
            strip.alpha_composite(new.crop((x0 + at - width - gutter, 0, x1 - width - gutter, height)), (at, 0))
        box = (c0 * step - x0, 0, min(x1 - x0, c1 * step - x0), height)
        pair_image.paste(strip.resize((c1 - c0, pair_size[1]), Image.Resampling.LANCZOS, box=box), (c0, 0))
    outline = None
    if bbox:
        sx, sy = small_new.width / width, small_new.height / height
        outline = (math.floor(bbox[0] * sx), math.floor(bbox[1] * sy), math.ceil(bbox[2] * sx), math.ceil(bbox[3] * sy))
    return {
        "old_png": small_old, "new_png": small_new, "side_by_side_png": pair_image,
        "overlay_png": ref_overlay(small_old, small_new),
        "diff_preview_png": ref_diff_preview(small_old, small_new, outline or ()),
    }


_GRAY = {"1", "L", "LA", "La", "I", "I;16", "I;16B", "I;16L", "I;16N", "F"}
_ALPHA = {"RGBA", "LA", "PA", "RGBa", "La"}
_COPY = {b"gAMA", b"cHRM", b"sRGB", b"cICP", b"tEXt", b"zTXt", b"iTXt"}


def _icc_fits(profile, mode):
    space = profile[16:20] if len(profile) >= 20 else b""
    if mode in {"1", "L", "LA"}:
        return space == b"GRAY"
    if mode in {"RGB", "RGBA", "P"}:
        return space == b"RGB "
    return False


def _family(mode):
    return "gray" if mode in _GRAY else "cmyk" if mode == "CMYK" else "rgb"


def _is_gray(rgba):
    r, g, b, _ = rgba.split()
    return ImageChops.difference(r, g).getbbox() is None and ImageChops.difference(g, b).getbbox() is None


def _to_palette(rgba, src):
    palette = src.getpalette() or []
    count = len(palette) // 3
    if not count:
        return None
    alpha = [255] * count
    t = src.info.get("transparency")
    if isinstance(t, int) and t < count:
        alpha[t] = 0
    elif isinstance(t, bytes):
        for i, a in enumerate(t[:count]):
            alpha[i] = a
    entries = np.array([palette[3 * i] | palette[3 * i + 1] << 8 | palette[3 * i + 2] << 16 | alpha[i] << 24
                        for i in range(count)], dtype="<u4")
    order = np.argsort(entries, kind="stable")
    ranked = entries[order]
    colours, inverse = np.unique(np.frombuffer(rgba.tobytes(), dtype="<u4"), return_inverse=True)
    pos = np.minimum(np.searchsorted(ranked, colours), count - 1)
    if not np.array_equal(ranked[pos], colours):
        return None
    out = Image.frombytes("P", rgba.size, order[pos].astype(np.uint8)[inverse.reshape(-1)].tobytes())
    out.putpalette(palette)
    out.info = {} if t is None else {"transparency": t}
    return out


def _fit_mode(rgba, src, jpeg):
    mode = src.mode if src is not None else None
    if jpeg:
        out = rgba.getchannel("R") if mode in _GRAY and _is_gray(rgba) else rgba.convert("RGB")
        out.info = {}
        return out
    if src is None:
        out = rgba.copy()
        out.info = {}
        return out
    key = src.info.get("transparency")
    has_alpha = mode in _ALPHA or key is not None
    candidates = []
    if mode == "P":
        candidates.append(lambda: _to_palette(rgba, src))
    if mode in _GRAY and _is_gray(rgba):
        gray = rgba.getchannel("R")
        if mode in _ALPHA:
            candidates.append(lambda: Image.merge("LA", (gray, rgba.getchannel("A"))))
        if mode == "1":
            candidates.append(lambda: gray.convert("1", dither=Image.Dither.NONE))
        if mode == "L" and isinstance(key, int):
            candidates.append(lambda: _keyed(gray, key))
        if not has_alpha:
            candidates.append(lambda: gray)
    if mode == "RGB" and isinstance(key, tuple):
        candidates.append(lambda: _keyed(rgba.convert("RGB"), key))
    if not has_alpha:
        candidates.append(lambda: rgba.convert("RGB"))
    for make in candidates:
        out = make()
        if out is None:
            continue
        if out.mode != "P" and "transparency" not in out.info:
            out.info = {}
        if ImageChops.difference(out.convert("RGBA"), rgba).getbbox() is None:
            return out
    out = rgba.copy()
    out.info = {}
    return out


def _keyed(image, key):
    out = image.copy()
    out.info = {"transparency": key}
    return out


def _png_ancillary_chunks(png):
    out = []
    pos = 8
    while pos + 8 <= len(png):
        length, kind = struct.unpack(">I4s", png[pos:pos + 8])
        if kind == b"IEND":
            break
        if kind in _COPY:
            out.append((kind, png[pos + 8:pos + 8 + length]))
        pos += 12 + length
    return out


def ref_encode_raster_like(rgba, original, source_format):
    """image_ops.encode_raster_like before 2b."""
    try:
        src = Image.open(io.BytesIO(original)) if original else None
    except Exception:
        src = None
    is_jpeg = source_format.lower() in {"jpeg", "jpg"}
    out = _fit_mode(rgba, src, is_jpeg)
    info = src.info if src is not None else {}
    params: dict = {}
    if info.get("dpi"):
        params["dpi"] = tuple(info["dpi"])
    icc = info.get("icc_profile")
    if icc and (_icc_fits(icc, out.mode) or _family(out.mode) == _family(src.mode)):
        params["icc_profile"] = icc
    if info.get("exif"):
        params["exif"] = info["exif"]
    buf = io.BytesIO()
    if is_jpeg:
        if src is not None and src.format == "JPEG" and getattr(src, "quantization", None):
            params["qtables"] = src.quantization
            sampling = JpegImagePlugin.get_sampling(src)
            if sampling != -1:
                params["subsampling"] = sampling
        else:
            params["quality"] = 95
        for key in ("progressive", "comment", "xmp"):
            if info.get(key):
                params[key] = info[key]
        if out.mode == "RGB" and src is not None and src.mode == "RGB" and info.get("adobe_transform") == 0:
            params["keep_rgb"] = True
        out.save(buf, format="JPEG", **params)
        return buf.getvalue()
    if "transparency" in out.info:
        params["transparency"] = out.info["transparency"]
    extra = _png_ancillary_chunks(original) if src is not None and src.format == "PNG" else []
    if extra:
        pnginfo = PngInfo()
        for kind, data in extra:
            pnginfo.add(kind, data)
        params["pnginfo"] = pnginfo
    out.save(buf, format="PNG", **params)
    return buf.getvalue()


# ── corpora ────────────────────────────────────────────────────────────────


def _external(name: str) -> Path | None:
    base = Path(os.environ.get("TMI_TEST_IMAGES") or ROOT / ".test-images")
    path = base / name
    return path if path.is_dir() else None


def _exif(order: str, entries: list[tuple[int, int, int, bytes]]) -> bytes:
    """An EXIF block (b"Exif\\0\\0" + TIFF) with one directory of (tag,
    type, count, value bytes) entries; values over 4 bytes go after it."""
    end = "<" if order == "II" else ">"
    head = order.encode() + struct.pack(end + "HL", 42, 8)
    ifd_size = 2 + 12 * len(entries) + 4
    data = b""
    table = struct.pack(end + "H", len(entries))
    for tag, typ, count, value in entries:
        if len(value) <= 4:
            table += struct.pack(end + "HHL", tag, typ, count) + value.ljust(4, b"\0")
        else:
            table += struct.pack(end + "HHLL", tag, typ, count, 8 + ifd_size + len(data))
            data += value
    return b"Exif\0\0" + head + table + struct.pack(end + "L", 0) + data


def _rational(order, num, den, signed=False):
    end = "<" if order == "II" else ">"
    return struct.pack(end + ("ll" if signed else "LL"), num, den)


def _short(order, v):
    return struct.pack(("<" if order == "II" else ">") + "H", v)


def _without_jfif(data: bytes) -> bytes:
    assert data[2:4] == b"\xff\xe0"
    n = int.from_bytes(data[4:6], "big")
    return data[:2] + data[4 + n:]


def _with_marker(data: bytes, code: int, body: bytes) -> bytes:
    """A marker inserted after SOI."""
    return data[:2] + bytes([0xFF, code]) + struct.pack(">H", len(body) + 2) + body + data[2:]


def generated_jpegs(rng: np.random.Generator) -> list[tuple[str, bytes]]:
    img = _rgba(rng, 41, 23, opaque=True).convert("RGB")
    out: list[tuple[str, bytes]] = []

    def save(image, **kw) -> bytes:
        buf = io.BytesIO()
        image.save(buf, "JPEG", **kw)
        return buf.getvalue()

    out.append(("plain", save(img)))
    out.append(("dpi 300", save(img, dpi=(300, 300))))
    out.append(("dpi 0", save(img, dpi=(0, 0))))
    d = bytearray(save(img, dpi=(100, 120)))
    d[13] = 2
    out.append(("dpcm", bytes(d)))
    d[13] = 0
    out.append(("aspect only", bytes(d)))
    out.append(("grey", save(img.convert("L"), quality=80)))
    out.append(("cmyk", save(img.convert("CMYK"), quality=80, icc_profile=bytes(16) + b"CMYK" + bytes(108))))
    out.append(("progressive", save(img, progressive=True, quality=60)))
    out.append(("4:4:4", save(img, subsampling=0)))
    out.append(("4:2:2", save(img, subsampling=1)))
    out.append(("keep_rgb", save(img, keep_rgb=True, subsampling=0)))
    out.append(("icc", save(img, icc_profile=_icc())))
    big = _icc() + bytes(70000)
    out.append(("icc in 2 markers", save(img, icc_profile=big)))
    out.append(("comment", save(img, comment=b"hello")))
    out.append(("xmp", save(img, xmp=b"<x:xmpmeta/>")))
    plain = save(img)
    for order in ("II", "MM"):
        cases = {
            "rational inch": [(0x0128, 3, 1, _short(order, 2)), (0x011A, 5, 1, _rational(order, 600, 2))],
            "rational cm": [(0x0128, 3, 1, _short(order, 3)), (0x011A, 5, 1, _rational(order, 118, 1))],
            "short": [(0x0128, 3, 1, _short(order, 2)), (0x011A, 3, 1, _short(order, 144))],
            "long": [(0x0128, 4, 1, struct.pack(("<" if order == "II" else ">") + "L", 2)),
                     (0x011A, 4, 1, struct.pack(("<" if order == "II" else ">") + "L", 99))],
            "srational": [(0x0128, 3, 1, _short(order, 2)), (0x011A, 10, 1, _rational(order, 7, 3, True))],
            "denominator 0": [(0x0128, 3, 1, _short(order, 2)), (0x011A, 5, 1, _rational(order, 7, 0))],
            "no unit": [(0x011A, 5, 1, _rational(order, 300, 1))],
            "no resolution": [(0x0128, 3, 1, _short(order, 2))],
            "two rationals": [(0x0128, 3, 1, _short(order, 2)),
                              (0x011A, 5, 2, _rational(order, 300, 1) + _rational(order, 5, 1))],
            "ascii resolution": [(0x0128, 3, 1, _short(order, 2)), (0x011A, 2, 4, b"300\0")],
            "duplicate": [(0x0128, 3, 1, _short(order, 2)), (0x011A, 5, 1, _rational(order, 1, 1)),
                          (0x011A, 5, 1, _rational(order, 250, 1))],
            "unknown type": [(0x0128, 3, 1, _short(order, 2)), (0x011A, 99, 1, b"abcd"),
                             (0x0131, 2, 3, b"ab\0")],
            "empty": [],
        }
        for label, entries in cases.items():
            exif = _exif(order, entries)
            out.append((f"exif {order} {label}", _with_marker(_without_jfif(plain), 0xE1, exif)))
    bad = _exif("II", [(0x0128, 3, 1, _short("II", 2)), (0x011A, 5, 1, _rational("II", 600, 2))])
    out.append(("exif truncated", _with_marker(_without_jfif(plain), 0xE1, bad[:-6])))
    out.append(("exif not tiff", _with_marker(_without_jfif(plain), 0xE1, b"Exif\0\0XXXXXXXX")))
    out.append(("photoshop", _with_marker(plain, 0xED, b"Photoshop 3.0\0" + b"8BIM\x03\xed\0\0\0\0\0\x10" + bytes(16))))
    out.append(("photoshop broken", _with_marker(plain, 0xED, b"Photoshop 3.0\0" + b"8BIM\x03\xed")))
    out.append(("adobe", _with_marker(plain, 0xEE, b"Adobe\0\x64\0\0\0\0\x01")))
    out.append(("adobe short", _with_marker(plain, 0xEE, b"Adobe\0")))
    out.append(("jfif short", _with_marker(_without_jfif(plain), 0xE0, b"JFIF\0\x01")))
    out.append(("mpf", _with_marker(plain, 0xE2, b"MPF\0" + bytes(8))))
    out.append(("junk before marker", plain[:2] + b"\x00\x00" + plain[2:]))
    out.append(("padding ff", plain[:2] + b"\xff" + plain[2:]))
    out.append(("truncated", plain[: len(plain) // 2]))
    return out


def generated_pngs(rng: np.random.Generator) -> list[tuple[str, bytes]]:
    img = _rgba(rng, 37, 19)
    out = []

    def save(image, **kw) -> bytes:
        buf = io.BytesIO()
        image.save(buf, "PNG", **kw)
        return buf.getvalue()

    out.append(("dpi", save(img, dpi=(350, 350))))
    out.append(("icc", save(img, icc_profile=_icc())))
    info = PngInfo()
    info.add(b"pHYs", struct.pack(">IIB", 1000, 2000, 0))
    out.append(("aspect", save(img, pnginfo=info)))
    png = save(img, dpi=(72, 72))
    # pHYs (and iCCP) after the image data: counted after load(), as Pillow does.
    phys = struct.pack(">IIB", 11811, 11811, 1)
    chunk = struct.pack(">I", 9) + b"pHYs" + phys + struct.pack(">I", __import__("zlib").crc32(b"pHYs" + phys))
    out.append(("pHYs after IDAT", png[:-12] + chunk + png[-12:]))
    short = struct.pack(">I", 4) + b"pHYs" + phys[:4] + struct.pack(">I", __import__("zlib").crc32(b"pHYs" + phys[:4]))
    out.append(("short pHYs", png[:33] + short + png[33:]))
    out.append(("short pHYs after IDAT", png[:-12] + short + png[-12:]))
    gama = struct.pack(">I", 2) + b"gAMA" + b"\0\1" + struct.pack(">I", __import__("zlib").crc32(b"gAMA\0\1"))
    out.append(("short gAMA", png[:33] + gama + png[33:]))
    trns = struct.pack(">I", 2) + b"tRNS" + b"\0\1" + struct.pack(">I", __import__("zlib").crc32(b"tRNS\0\1"))
    rgb = save(img.convert("RGB"))
    out.append(("short RGB tRNS", rgb[:33] + trns + rgb[33:]))
    out.append(("garbage after IDAT", png[:-12] + b"\0\0\0\1\xff\xff\xff\xff\0\0\0\0\0" + png[-12:]))
    out.append(("cut after IDAT", png[:-12] + struct.pack(">I", 100) + b"tEXt" + b"abc"))
    return out


# ── tests ──────────────────────────────────────────────────────────────────


def _pillow_decode(data: bytes):
    """What import_raster_image read with Pillow: (rgba, size, format, dpi,
    icc) or the exception type's name."""
    try:
        with warnings.catch_warnings():
            warnings.simplefilter("ignore")
            opened = Image.open(io.BytesIO(data))
            fmt = (opened.format or "").lower()
            image = opened.convert("RGBA")
    except Exception as e:  # noqa: BLE001
        return type(e).__name__
    dpi = image.info.get("dpi")
    if dpi:
        dpi = float(dpi[0]) if isinstance(dpi, tuple) else float(dpi)
    else:
        dpi = None
    return image.tobytes(), image.size, fmt, dpi, "icc_profile" in image.info


def _route(data: bytes) -> str:
    """Who decodes ``data`` in ``raster.decode`` (for the report)."""
    from tmi import accel

    try:
        header = raster._header(data, True)
    except raster._Unsure:
        return "via Pillow: header"
    except Exception:  # noqa: BLE001
        return "via Pillow: header error"
    if header is None:
        return "via Pillow: format"
    try:
        accel.native().image_decode(data)
    except ValueError:
        return "via Pillow: declined"
    return "via tmi_accel"


class DecodeTests(unittest.TestCase):
    def check(self, cases, counts):
        for label, data in cases:
            route = _route(data)
            counts[route] = counts.get(route, 0) + 1
            want = _pillow_decode(data)
            try:
                with warnings.catch_warnings():
                    warnings.simplefilter("ignore")
                    got = raster.decode(data, meta=True)
            except Exception as e:  # noqa: BLE001
                self.assertIsInstance(want, str, f"{label}: tmi raises {e!r}, Pillow decodes")
                counts["both refuse"] = counts.get("both refuse", 0) + 1
                continue
            self.assertNotIsInstance(want, str, f"{label}: Pillow refuses ({want}), tmi decodes")
            self.assertEqual(want, (got.image.tobytes(), got.image.size, got.format, got.dpi, got.icc), label)
            counts["same"] = counts.get("same", 0) + 1

    def test_generated(self) -> None:
        rng = np.random.default_rng(5)
        counts: dict = {}
        self.check(generated_jpegs(rng) + generated_pngs(rng), counts)
        _report(f"generated: {counts}")

    def test_corpora(self) -> None:
        files = []
        for name in ("pngsuite", "pillow"):
            base = _external(name)
            if base is not None:
                files += sorted(p for p in base.rglob("*") if p.is_file())
        if not files:
            self.skipTest("no test images (python tools/fetch_test_images.py)")
        counts: dict = {}
        self.check([(p.name, p.read_bytes()) for p in files], counts)
        _report(f"corpora: {counts}")

    def test_headers_read_without_pillow(self) -> None:
        """The common files never fall back to Pillow."""
        rng = np.random.default_rng(6)
        img = _rgba(rng, 20, 10)
        for fmt, kw in (("PNG", {"dpi": (300, 300), "icc_profile": _icc()}), ("JPEG", {"dpi": (350, 350)})):
            buf = io.BytesIO()
            (img if fmt == "PNG" else img.convert("RGB")).save(buf, fmt, **kw)
            header = raster._header(buf.getvalue(), True)
            self.assertIsNotNone(header)


class OpsTests(unittest.TestCase):
    def test_diff_ops_match_pillow(self) -> None:
        rng = np.random.default_rng(11)
        pyrng = random.Random(3)
        for i in range(60):
            w, h = pyrng.randint(1, 70), pyrng.randint(1, 50)
            a = _rgba(rng, w, h, opaque=i % 3 == 0)
            b = a.copy()
            draw = ImageDraw.Draw(b)
            for _ in range(pyrng.randint(0, 3)):
                x, y = pyrng.randrange(w), pyrng.randrange(h)
                draw.rectangle((x, y, x + pyrng.randint(0, 9), y + pyrng.randint(0, 9)),
                               fill=tuple(pyrng.randrange(256) for _ in range(4)))
            if i % 7 == 1:
                b = b.crop((0, 0, max(1, w - 3), h + 2))  # sizes differ
            if i % 11 == 2:
                b.putpixel((0, 0), (255, 0, 0, 0))  # colour under alpha 0 only
            label = f"case {i} {a.size} {b.size}"
            self.assertEqual(ref_diff_bbox(a, b), image_ops.diff_bbox(a, b), label)
            self.assertEqual(ref_side_by_side(a, b).tobytes(), image_ops.side_by_side(a, b).tobytes(), label)
            self.assertEqual(ref_overlay(a, b).tobytes(), image_ops.overlay(a, b).tobytes(), label)
            self.assertEqual(ref_diff_preview(a, b).tobytes(), image_ops.diff_preview(a, b).tobytes(), label)

    def test_sparse_layers_compare_as_their_full_images(self) -> None:
        rng = np.random.default_rng(12)
        pyrng = random.Random(4)
        for i in range(80):
            size = (pyrng.randint(1, 60), pyrng.randint(1, 40))

            def layer():
                if pyrng.random() < 0.15:
                    return raster.Sparse(size), Image.new("RGBA", size)
                x0, y0 = pyrng.randrange(size[0]), pyrng.randrange(size[1])
                w, h = pyrng.randint(1, size[0] - x0), pyrng.randint(1, size[1] - y0)
                crop = _rgba(rng, w, h)
                full = Image.new("RGBA", size)
                full.paste(crop, (x0, y0))
                return raster.Sparse(size, x0, y0, raster.as_rgba(crop)), full

            (sa, fa), (sb, fb) = layer(), layer()
            self.assertEqual(ref_diff_bbox(fa, fb), diff_mod._diff_bbox(sa, sb), i)
            self.assertEqual(fa.tobytes(), sa.tobytes(), i)
            box = (pyrng.randint(-5, size[0]), pyrng.randint(-5, size[1]), size[0] + pyrng.randint(0, 5), size[1] + 3)
            self.assertEqual(fa.crop(box).tobytes(), sa.crop(box).tobytes(), (i, box))

    def test_rectangle_outline_matches_imagedraw(self) -> None:
        pyrng = random.Random(9)
        for i in range(400):
            w, h = pyrng.randint(1, 40), pyrng.randint(1, 40)
            base = Image.new("RGBA", (w, h), (10, 20, 30, 40))
            x0, y0 = pyrng.randint(-5, w + 3), pyrng.randint(-5, h + 3)
            box = (x0, y0, x0 + pyrng.randint(0, 25), y0 + pyrng.randint(0, 25))
            width = pyrng.choice([0, 1, 2, 3, 5, 9])
            want = base.copy()
            ImageDraw.Draw(want).rectangle(box, outline=(255, 0, 0, 255), width=width)
            got = raster.outline(raster.as_rgba(base), box, (255, 0, 0, 255), width)
            self.assertEqual(want.tobytes(), got.tobytes(), (i, (w, h), box, width))

    def test_thumbnail_matches_pillow(self) -> None:
        rng = np.random.default_rng(13)
        for size, mode in [((1, 1), "RGBA"), ((900, 7), "RGBA"), ((385, 384), "RGBA"), ((383, 1000), "RGB"),
                           ((1200, 700), "RGBA"), ((50, 30), "L"), ((2000, 2001), "RGBA")]:
            image = _rgba(rng, *size)
            image = image if mode == "RGBA" else image.convert(mode)
            want = ref_thumbnail(image)
            self.assertEqual(want.tobytes(), image_ops.make_thumbnail(image).tobytes(), (size, mode))
            self.assertEqual(want.tobytes(), raster.thumbnail(raster.as_rgba(image)).tobytes(), (size, mode))

    def test_composite_previews_match_pillow(self) -> None:
        rng = np.random.default_rng(14)
        from unittest import mock

        for size, edge, opaque in (((300, 200), 2048, False), ((200, 120), 300, False), ((640, 480), 256, False),
                                   ((120, 500), 200, False), ((640, 480), 256, True), ((1001, 333), 777, True),
                                   ((97, 61), 64, False), ((5, 300), 40, False)):
            old = _rgba(rng, *size, opaque=opaque)
            new = old.copy()
            ImageDraw.Draw(new).line([(size[0] // 4, size[1] // 3), (size[0] // 2, size[1] // 2)],
                                     fill=(200, 30, 60, 255), width=3)
            bbox = ref_diff_bbox(old, new)
            want = ref_composite_previews(old, new, bbox, edge)
            with mock.patch.object(diff_mod, "HTML_COMPOSITE_EDGE", edge):
                got = diff_mod._composite_preview_images(old, new, bbox)
            for key in want:
                self.assertEqual((want[key].size, want[key].tobytes()), (got[key].size, got[key].tobytes()),
                                 f"{size} {edge}: {key}")
        old, new = _rgba(rng, 300, 200), _rgba(rng, 320, 180)
        bbox = ref_diff_bbox(old, new)
        want = ref_composite_previews(old, new, bbox, 256)
        with mock.patch.object(diff_mod, "HTML_COMPOSITE_EDGE", 256):
            got = diff_mod._composite_preview_images(old, new, bbox)
        for key in want:
            self.assertEqual(want[key].tobytes(), got[key].tobytes(), key)

    def test_webp_preview_is_pillows(self) -> None:
        rng = np.random.default_rng(15)
        for size in ((1, 1), (64, 33), (300, 200)):
            image = _rgba(rng, *size)
            buf = io.BytesIO()
            image.save(buf, "WEBP", quality=82, method=2)
            self.assertEqual(buf.getvalue(), diff_mod._preview_bytes(image), size)


def _span(pyrng: random.Random, n: int) -> tuple[int, int]:
    """A crop's extent along a side of ``n``: touching the start, the end,
    covering all, 1 px, or anywhere."""
    kind = pyrng.randrange(6)
    if kind == 0:
        return 0, pyrng.randint(1, n)
    if kind == 1:
        return pyrng.randrange(n), n
    if kind == 2:
        return 0, n
    if kind == 3:
        a = pyrng.randrange(n)
        return a, a + 1
    a = pyrng.randrange(n)
    return a, pyrng.randint(a + 1, n)


def _sparse_layer(rng: np.random.Generator, pyrng: random.Random, size: tuple[int, int]):
    """(tmi layer, the same as a full PIL image): empty, dense, or a crop
    (line-art-like half the time: runs of transparent black)."""
    kind = pyrng.random()
    if kind < 0.1:
        return raster.Sparse(size), Image.new("RGBA", size)
    if kind < 0.2:
        full = _rgba(rng, *size)
        return raster.as_rgba(full), full
    (x0, x1), (y0, y1) = _span(pyrng, size[0]), _span(pyrng, size[1])
    crop = _rgba(rng, x1 - x0, y1 - y0)
    if pyrng.random() < 0.5:
        arr = np.asarray(crop).copy()
        arr[rng.random(arr.shape[:2]) < 0.7] = 0
        crop = Image.fromarray(arr, "RGBA")
    full = Image.new("RGBA", size)
    full.paste(crop, (x0, y0))
    return raster.Sparse(size, x0, y0, raster.as_rgba(crop)), full


class CropAwareTests(unittest.TestCase):
    """The crop-aware paths of diff's layer previews against Pillow on the
    full-size layers (and tmi's full-size kernels), byte for byte."""

    def test_resize_region_is_the_region_resized(self) -> None:
        rng = np.random.default_rng(21)
        pyrng = random.Random(21)
        for i in range(250):
            size = (pyrng.randint(1, 90), pyrng.randint(1, 90))
            layer, full = _sparse_layer(rng, pyrng, size)
            box = (pyrng.randint(-3, size[0] - 1), pyrng.randint(-3, size[1] - 1), 0, 0)
            box = (box[0], box[1], pyrng.randint(box[0] + 1, size[0] + 3), pyrng.randint(box[1] + 1, size[1] + 3))
            if i % 5 == 0 and layer.rect is not None:
                box = layer.rect
            region = full.crop(box)
            for edge in (1024, 61, 17, 5, 1):
                out = _fit(region.size, edge)
                want = region.resize(out, Image.Resampling.LANCZOS) if out != region.size else region
                got = raster.resize_region(layer, box, out)
                self.assertEqual(want.tobytes(), got.tobytes(), (i, size, box, edge))
                self.assertEqual(layer.crop(box).resize(out).tobytes(), got.tobytes(), (i, size, box, edge))

    def test_sparse_resize_kernel_with_boxes(self) -> None:
        # The box= resamples of the composite previews (the side by side's
        # halves on the pair's grid), on odd sizes.
        rng = np.random.default_rng(22)
        pyrng = random.Random(22)
        native = raster._native()
        for i in range(120):
            size = (pyrng.randint(3, 120) | 1, pyrng.randint(3, 120) | 1)
            layer, full = _sparse_layer(rng, pyrng, size)
            if not isinstance(layer, raster.Sparse) or layer.crop_image is None:
                continue
            w, h = size
            out = (pyrng.randint(1, w), pyrng.randint(1, h))
            box = sorted((pyrng.uniform(0, w), pyrng.uniform(0, w)))
            box = (box[0], 0.0, max(box[1], box[0] + 0.5), float(h))
            want = full.resize(out, Image.Resampling.LANCZOS, box=box).tobytes()
            c = layer.crop_image
            got = native.rgba_resize_sparse(c.data, c.width, c.height, layer.x0, layer.y0, w, h, out, box)
            self.assertEqual(want, got, (i, size, out, box))

    def test_layer_previews_and_bbox_are_the_full_images(self) -> None:
        rng = np.random.default_rng(23)
        pyrng = random.Random(23)
        for i in range(250):
            size = (pyrng.randint(1, 80), pyrng.randint(1, 60))
            (sa, fa), (sb, fb) = _sparse_layer(rng, pyrng, size), _sparse_layer(rng, pyrng, size)
            bbox = diff_mod._diff_bbox(sa, sb)
            self.assertEqual(ref_diff_bbox(fa, fb), bbox, i)
            self.assertEqual(ref_side_by_side(fa, fb).tobytes(), raster.side_by_side_painted(sa, sb).tobytes(), i)
            self.assertEqual(ref_overlay(fa, fb).tobytes(), raster.overlay_painted(sa, sb).tobytes(), i)
            self.assertEqual(ref_diff_preview(fa, fb).tobytes(), raster.diff_preview_painted(sa, sb, bbox).tobytes(), i)

    def test_far_apart_crops_and_an_empty_side(self) -> None:
        rng = np.random.default_rng(24)
        size = (400, 300)
        for (ax, ay), (bx, by) in (((0, 0), (380, 280)), ((380, 0), (0, 280)), ((10, 150), (390, 150))):
            ca, cb = _rgba(rng, 20, 20), _rgba(rng, 10, 20)
            sa, sb = raster.Sparse(size, ax, ay, raster.as_rgba(ca)), raster.Sparse(size, bx, by, raster.as_rgba(cb))
            fa, fb = Image.new("RGBA", size), Image.new("RGBA", size)
            fa.paste(ca, (ax, ay))
            fb.paste(cb, (bx, by))
            self.assertEqual(ref_diff_bbox(fa, fb), raster.diff_bbox(sa, sb))
            self.assertEqual(ref_diff_bbox(fa, Image.new("RGBA", size)), raster.diff_bbox(sa, raster.Sparse(size)))
            self.assertEqual(ref_diff_bbox(Image.new("RGBA", size), fb), raster.diff_bbox(raster.Sparse(size), sb))


def _originals(rng: np.random.Generator):
    """(label, original bytes, format) for the write-back."""
    img = _rgba(rng, 33, 21)
    grey = img.convert("L")
    out = []

    def png(image, **kw):
        buf = io.BytesIO()
        image.save(buf, "PNG", **kw)
        return buf.getvalue()

    info = PngInfo()
    info.add_text("Comment", "hello")
    info.add_text("Comment", "again")
    info.add(b"gAMA", struct.pack(">I", 45455))
    info.add(b"gAMA", struct.pack(">I", 50000))
    info.add(b"sRGB", b"\0")
    info.add_itxt("XML:com.adobe.xmp", "<x/>", zip=True)
    exif = Image.Exif()
    exif[0x0131] = "painter"
    out += [
        ("png rgba", png(img), "png"),
        ("png rgb + everything", png(img.convert("RGB"), pnginfo=info, dpi=(300, 300), icc_profile=_icc(), exif=exif.tobytes()), "png"),
        ("png rgb + srgb, no icc", png(img.convert("RGB"), pnginfo=info), "png"),
        ("png rgb key", png(img.convert("RGB"), transparency=(1, 2, 3)), "png"),
        ("png l", png(grey, dpi=(72, 72)), "png"),
        ("png l key", png(grey, transparency=7), "png"),
        ("png la", png(grey.convert("LA")), "png"),
        ("png 1", png(grey.convert("1")), "png"),
        ("png i;16", png(grey.convert("I;16")), "png"),
        ("png grey icc", png(grey, icc_profile=bytes(16) + b"GRAY" + bytes(108)), "png"),
    ]
    pal = img.convert("RGB").convert("P", palette=Image.Palette.ADAPTIVE, colors=5)
    out.append(("png p", png(pal), "png"))
    out.append(("png p index", png(pal, transparency=2), "png"))
    out.append(("png p bytes", png(pal, transparency=bytes([255, 0, 128, 255, 9])), "png"))
    out.append(("png p 200", png(img.convert("RGB").convert("P", palette=Image.Palette.ADAPTIVE, colors=200)), "png"))
    for label, data in generated_jpegs(rng):
        if label not in ("truncated", "photoshop broken", "adobe short", "jfif short", "exif truncated"):
            out.append((f"jpeg {label}", data, "jpeg"))
    out.append(("png as jpeg", png(img.convert("RGB"), dpi=(96, 96)), "jpeg"))
    out.append(("jpeg as png", dict(generated_jpegs(rng))["icc"], "png"))
    out.append(("nothing", b"", "png"))
    out.append(("not an image", b"hello", "jpeg"))
    return out


def _pixels_for(original: bytes, rng: np.random.Generator):
    """RGBA pixel sets for an original: what it decodes to (fits), the same
    with one pixel changed, and a translucent one."""
    try:
        with warnings.catch_warnings():
            warnings.simplefilter("ignore")
            base = Image.open(io.BytesIO(original)).convert("RGBA")
    except Exception:
        base = _rgba(rng, 33, 21)
    edited = base.copy()
    edited.putpixel((0, 0), (9, 200, 9, 255))
    translucent = base.copy()
    translucent.putpixel((1, 1), (1, 2, 3, 100))
    return {"fits": base, "edited": edited, "translucent": translucent}


def _malformed_mpo(original: bytes) -> bool:
    """Whether Pillow warns "malformed MPO" opening ``original``."""
    with warnings.catch_warnings(record=True) as caught:
        warnings.simplefilter("always")
        try:
            Image.open(io.BytesIO(original))
        except Exception:  # noqa: BLE001
            return False
    return any("malformed MPO" in str(w.message) for w in caught)


class WriteBackTests(unittest.TestCase):
    def test_matches_pillow(self) -> None:
        rng = np.random.default_rng(21)
        for label, original, fmt in _originals(rng):
            for variant, rgba in _pixels_for(original, rng).items():
                name = f"{label} / {variant}"
                with warnings.catch_warnings():
                    warnings.simplefilter("ignore")
                    # (a malformed MPO is an opaque blob to tmi, PLUGINS.md P0e: the
                    # write-back keeps none of its metadata, where Pillow's old code did)
                    want = ref_encode_raster_like(rgba, b"" if _malformed_mpo(original) else original, fmt)
                with warnings.catch_warnings():
                    warnings.simplefilter("ignore")  # Pillow reads the headers tmi.raster leaves to it
                    got = image_ops.encode_raster_like(raster.as_rgba(rgba), original, fmt)
                if fmt == "jpeg":
                    self.assertEqual(want, got, name)
                    continue
                a, b = Image.open(io.BytesIO(want)), Image.open(io.BytesIO(got))
                a.load()
                b.load()
                self.assertEqual((a.format, a.mode, a.size), (b.format, b.mode, b.size), name)
                self.assertEqual(a.tobytes(), b.tobytes(), name)
                self.assertEqual(a.getpalette(), b.getpalette(), name)
                self.assertEqual(a.info, b.info, name)
                self.assertEqual(a.getexif(), b.getexif(), name)
                self.assertEqual(_chunk_types(want), _chunk_types(got), name)


def _chunk_types(png: bytes) -> list[bytes]:
    out, pos = [], 8
    while pos + 8 <= len(png):
        length, kind = struct.unpack(">I4s", png[pos:pos + 8])
        if kind != b"IDAT" or not out or out[-1] != b"IDAT":
            out.append(kind)
        pos += 12 + length
    return out


if __name__ == "__main__":
    unittest.main()
