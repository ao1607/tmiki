"""Differential tests of the stage-2 image kernels and codecs (RUST_MIGRATION.md
2a) against the installed Pillow 12.3, through ``tmi_accel._test``.

Exact (byte for byte) unless a test says otherwise:
  - ``Image.resize`` (LANCZOS, BICUBIC, BILINEAR, BOX, HAMMING; ``box=``,
    ``reducing_gap``; every integer factor 1-16 and odd sizes), ``reduce``,
    ``thumbnail``;
  - ``Image.blend``, ``ImageChops.difference`` / ``lighter``,
    ``getbbox(alpha_only=False)``, ``getextrema``;
  - ``Image.convert`` from every mode the PNG / JPEG decoders produce;
  - PNG decode (the PngSuite, Pillow- and mtpng-written PNGs) and JPEG
    decode (a corpus Pillow writes) followed by ``convert("RGBA")``.
Encoders (JPEG, WebP) are checked by decoding: size and PSNR near Pillow's.

The PngSuite and Pillow's own test images are read from ``TMI_TEST_IMAGES``
(default ``.test-images``; ``python tools/fetch_test_images.py`` fetches
them); without them those tests skip.
"""
from __future__ import annotations

import io
import os
import unittest
import warnings
from pathlib import Path

import numpy as np
from PIL import Image, ImageChops

try:
    import tmi_accel as _native
except ImportError:  # pragma: no cover - tmi_accel not built
    _native = None

R = Image.Resampling
FILTERS = [R.LANCZOS, R.BICUBIC, R.BILINEAR, R.BOX, R.HAMMING]


def _test_module():
    t = getattr(_native, "_test", None) if _native is not None else None
    return t if t is not None and hasattr(t, "pil_resize") else None


def _report(line: str) -> None:
    """The corpus counts, with TMI_IMAGE_REPORT=1."""
    if os.environ.get("TMI_IMAGE_REPORT"):
        print(f"\n  {line}")


def _rgba(rng: np.random.Generator, w: int, h: int) -> Image.Image:
    """Smooth colour with noise, and an alpha that has fully transparent,
    fully opaque and partial areas (premultiplication matters)."""
    yy, xx = np.mgrid[0:h, 0:w]
    base = np.stack([(xx * 7 + yy * 3) % 256, (xx * 2 + yy * 11) % 256, (xx * yy) % 256], -1)
    noise = rng.integers(-40, 41, size=(h, w, 3))
    rgb = np.clip(base + noise, 0, 255)
    a = rng.integers(0, 256, size=(h, w))
    a[(xx // 5 + yy // 4) % 3 == 0] = 0
    a[(xx // 3 + yy // 6) % 4 == 1] = 255
    px = np.concatenate([rgb, a[..., None]], -1).astype(np.uint8)
    return Image.fromarray(px, "RGBA")


def _bands(rng: np.random.Generator, mode: str, w: int, h: int) -> Image.Image:
    img = _rgba(rng, w, h)
    return img if mode == "RGBA" else img.convert(mode)


class _Base(unittest.TestCase):
    def setUp(self) -> None:
        t = _test_module()
        if t is None:
            self.skipTest("tmi_accel with the stage-2 _test hooks is not available")
        self.t = t

    def resize(self, img: Image.Image, size, resample, box=None, reducing_gap=None) -> bytes:
        data, mode, w, h = self.t.pil_resize(img.mode, img.tobytes(), img.width, img.height, tuple(size),
                                             int(resample), box, reducing_gap)
        self.assertEqual((mode, w, h), (img.mode, *size))
        return data


@unittest.skipIf(_test_module() is None, "tmi_accel with the stage-2 _test hooks is not available")
class ResampleTests(_Base):
    def test_every_factor_1_to_16(self) -> None:
        """Downscales by every integer factor 1-16 (and the sizes either side
        of it), each filter, RGBA / RGB / L / LA."""
        rng = np.random.default_rng(1)
        count = 0
        for mode in ("RGBA", "RGB", "L", "LA"):
            src = _bands(rng, mode, 97, 83)
            for f in range(1, 17):
                for dw, dh in ((0, 0), (1, 0), (0, -1)):
                    size = (max(1, src.width // f + dw), max(1, src.height // f + dh))
                    for flt in FILTERS:
                        want = src.resize(size, flt).tobytes()
                        self.assertEqual(self.resize(src, size, flt), want, (mode, size, flt))
                        count += 1
        self.assertGreater(count, 900)

    def test_odd_sizes_up_and_down(self) -> None:
        rng = np.random.default_rng(2)
        for _ in range(300):
            w, h = (int(v) for v in rng.integers(1, 70, size=2))
            src = _rgba(rng, w, h)
            size = tuple(int(v) for v in rng.integers(1, 90, size=2))
            flt = FILTERS[int(rng.integers(0, len(FILTERS)))]
            self.assertEqual(self.resize(src, size, flt), src.resize(size, flt).tobytes(), (w, h, size, flt))

    def test_boxes(self) -> None:
        """Float boxes (float32 in Pillow), integer boxes (the crop shortcut
        when the box is the output size), boxes reaching the edges."""
        rng = np.random.default_rng(3)
        for i in range(400):
            w, h = (int(v) for v in rng.integers(2, 80, size=2))
            src = _bands(rng, ("RGBA", "RGB", "L")[i % 3], w, h)
            x0, x1 = sorted(rng.uniform(0, w, size=2))
            y0, y1 = sorted(rng.uniform(0, h, size=2))
            if i % 4 == 0:
                x0, y0 = float(int(x0)), float(int(y0))
                box = (x0, y0, min(w, x0 + max(1, int(x1 - x0))), min(h, y0 + max(1, int(y1 - y0))))
                size = (int(box[2] - box[0]), int(box[3] - box[1]))
            else:
                box = (x0, y0, x1, y1)
                size = tuple(int(v) for v in rng.integers(1, 60, size=2))
            if i % 7 == 0:
                box = (0.0, box[1], float(w), box[3])
            flt = FILTERS[i % len(FILTERS)]
            want = src.resize(size, flt, box=box).tobytes()
            self.assertEqual(self.resize(src, size, flt, box=box), want, (src.mode, w, h, box, size, flt))

    def test_reducing_gap(self) -> None:
        """resize(..., reducing_gap=g): RGB / L reduce first; RGBA / LA
        ignore it (Pillow drops it on the premultiplied call)."""
        rng = np.random.default_rng(4)
        for i in range(240):
            mode = ("RGB", "L", "RGBA", "LA")[i % 4]
            w, h = (int(v) for v in rng.integers(20, 300, size=2))
            src = _bands(rng, mode, w, h)
            size = (max(1, w // int(rng.integers(2, 20))), max(1, h // int(rng.integers(2, 20))))
            gap = float(rng.choice([1.0, 1.5, 2.0, 2.5, 3.0]))
            box = None
            if i % 3 == 0:
                box = (float(rng.uniform(0, w / 4)), float(rng.uniform(0, h / 4)),
                       float(rng.uniform(w * 3 / 4, w)), float(rng.uniform(h * 3 / 4, h)))
            flt = FILTERS[i % len(FILTERS)]
            want = src.resize(size, flt, box=box, reducing_gap=gap).tobytes()
            self.assertEqual(self.resize(src, size, flt, box=box, reducing_gap=gap), want, (mode, w, h, size, gap, box))

    def test_reduce(self) -> None:
        rng = np.random.default_rng(5)
        for i in range(300):
            mode = ("RGB", "L", "RGBA", "LA")[i % 4]
            w, h = (int(v) for v in rng.integers(1, 60, size=2))
            src = _bands(rng, mode, w, h)
            factor = (int(rng.integers(1, 9)), int(rng.integers(1, 9)))
            box = None
            if i % 2:
                x0, y0 = int(rng.integers(0, w)), int(rng.integers(0, h))
                box = (x0, y0, int(rng.integers(x0 + 1, w + 1)), int(rng.integers(y0 + 1, h + 1)))
            want = src.reduce(factor, box=box)
            data, m, ow, oh = self.t.pil_reduce(mode, src.tobytes(), w, h, factor, box)
            self.assertEqual((m, ow, oh), (want.mode, *want.size))
            self.assertEqual(data, want.tobytes(), (mode, w, h, factor, box))

    def test_tall_images(self) -> None:
        """More than 100 times as tall as wide: Pillow resizes height first."""
        rng = np.random.default_rng(6)
        for w, h, size in ((3, 400, (2, 50)), (2, 333, (5, 40)), (1, 250, (1, 7)), (4, 401, (3, 401))):
            for mode in ("RGBA", "RGB"):
                src = _bands(rng, mode, w, h)
                for flt in FILTERS:
                    self.assertEqual(self.resize(src, size, flt), src.resize(size, flt).tobytes(), (mode, w, h, size, flt))

    def test_thumbnail(self) -> None:
        """``thumbnail()`` (default reducing_gap 2.0): sizes and pixels, as
        make_thumbnail uses it (RGBA, LANCZOS, 384) and on RGB / L."""
        rng = np.random.default_rng(7)
        for i in range(160):
            mode = ("RGBA", "RGB", "L")[i % 3]
            w, h = (int(v) for v in rng.integers(1, 1300, size=2))
            if i % 5 == 0:
                w, h = int(rng.integers(385, 3000)), int(rng.integers(1, 40))
            src = _bands(rng, mode, w, h)
            edge = (384, 384) if i % 2 == 0 else (float(rng.uniform(1, 500)), float(rng.uniform(1, 500)))
            want = src.copy()
            want.thumbnail(edge, R.LANCZOS)
            data, m, ow, oh = self.t.pil_thumbnail(mode, src.tobytes(), w, h, edge, int(R.LANCZOS))
            self.assertEqual((ow, oh), want.size, (w, h, edge))
            self.assertEqual(data, want.tobytes(), (mode, w, h, edge))

    def test_thumbnail_sizes(self) -> None:
        for w in list(range(1, 60)) + [383, 384, 385, 1000, 4093]:
            for h in (1, 2, 3, 7, 50, 383, 384, 385, 999, 4096):
                for edge in ((384, 384), (100, 37), (37.9, 100.2), (1, 1)):
                    probe = Image.new("L", (w, h))
                    probe.thumbnail(edge)
                    got = self.t.pil_thumbnail_size(w, h, edge)
                    self.assertEqual(got or (w, h), probe.size, (w, h, edge))

    def test_diff_preview_resizes(self) -> None:
        """The resizes of diff.py's ``_composite_preview_images`` (both
        canvases to the preview size, and each onto the side-by-side's grid
        with ``box=``) and ``_shrink``."""
        import math

        rng = np.random.default_rng(8)
        for width, height, edge in ((700, 300, 256), (513, 511, 256), (300, 900, 256), (1000, 77, 256),
                                    (260, 260, 256), (4100, 2900, 2048)):
            for side in range(2):
                img = _rgba(rng, width, height)
                scale = edge / max(width, height)
                small = (max(1, round(width * scale)), max(1, round(height * scale)))
                self.assertEqual(self.resize(img, small, R.LANCZOS), img.resize(small, R.LANCZOS).tobytes())
                gutter = 16
                pair = (2 * width + gutter, height)
                pscale = edge / max(pair)
                pair_size = (max(1, round(pair[0] * pscale)), max(1, round(pair[1] * pscale)))
                step = pair[0] / pair_size[0]
                left_w = math.floor(width / step)
                right_x = math.ceil((width + gutter) / step)
                right_w = pair_size[0] - right_x
                jobs = [((left_w, pair_size[1]), (0, 0, min(width, left_w * step), height)),
                        ((right_w, pair_size[1]), (max(0.0, right_x * step - width - gutter), 0, width, height))]
                for size, box in jobs:
                    want = img.resize(size, R.LANCZOS, box=box).tobytes()
                    self.assertEqual(self.resize(img, size, R.LANCZOS, box=box), want, (width, height, size, box))


@unittest.skipIf(_test_module() is None, "tmi_accel with the stage-2 _test hooks is not available")
class BlendChopsBBoxTests(_Base):
    def test_blend_all_pairs(self) -> None:
        """``Image.blend`` for every (a, b) byte pair, at the alphas tmi and
        Pillow's shortcuts use and a spread of others (including outside
        0..1, which clips)."""
        a = np.repeat(np.arange(256, dtype=np.uint8), 256)
        b = np.tile(np.arange(256, dtype=np.uint8), 256)
        ia = Image.frombytes("L", (256, 256), a.tobytes())
        ib = Image.frombytes("L", (256, 256), b.tobytes())
        for alpha in (0.5, 0.0, 1.0, 0.25, 0.1, 0.3333333, 0.75, 0.999, 1e-7, -0.5, 1.5, 2.0):
            want = Image.blend(ia, ib, alpha).tobytes()
            self.assertEqual(self.t.pil_blend(a.tobytes(), b.tobytes(), alpha), want, alpha)

    def test_blend_rgba(self) -> None:
        rng = np.random.default_rng(9)
        x, y = _rgba(rng, 64, 48), _rgba(rng, 64, 48)
        self.assertEqual(self.t.pil_blend(x.tobytes(), y.tobytes(), 0.5), Image.blend(x, y, 0.5).tobytes())

    def test_chops_all_pairs(self) -> None:
        a = np.repeat(np.arange(256, dtype=np.uint8), 256)
        b = np.tile(np.arange(256, dtype=np.uint8), 256)
        ia = Image.frombytes("L", (256, 256), a.tobytes())
        ib = Image.frombytes("L", (256, 256), b.tobytes())
        for op in ("difference", "lighter", "darker", "multiply"):
            want = getattr(ImageChops, op)(ia, ib).tobytes()
            self.assertEqual(self.t.pil_chop(op, a.tobytes(), b.tobytes()), want, op)
        rng = np.random.default_rng(10)
        x, y = _rgba(rng, 40, 30), _rgba(rng, 40, 30)
        self.assertEqual(self.t.pil_chop("difference", x.tobytes(), y.tobytes()), ImageChops.difference(x, y).tobytes())

    def test_bbox_and_extrema(self) -> None:
        rng = np.random.default_rng(11)
        for i in range(500):
            h, w = (int(v) for v in rng.integers(1, 30, size=2))
            mode = ("RGBA", "RGB", "L", "LA")[i % 4]
            bands = len(mode)
            a = np.zeros((h, w, bands), np.uint8)
            for _ in range(int(rng.integers(0, 4))):
                y, x, c = int(rng.integers(0, h)), int(rng.integers(0, w)), int(rng.integers(0, bands))
                a[y, x, c] = rng.integers(1, 256)
            img = Image.frombytes(mode, (w, h), a.tobytes())
            want = img.getbbox(alpha_only=False)
            self.assertEqual(self.t.pil_bbox_any(a.tobytes(), w, bands), want, (mode, w, h))
            ext = img.getextrema()
            if bands == 1:
                ext = (ext,)
            self.assertEqual([tuple(e) for e in self.t.pil_extrema(a.tobytes(), bands)], [tuple(e) for e in ext])


# ── convert ─────────────────────────────────────────────────────────────────

def _all_bytes_image(mode: str, w: int, h: int, rng: np.random.Generator) -> Image.Image:
    n = w * h
    if mode == "1":
        return Image.frombytes("1", (w, h), rng.integers(0, 256, size=(w + 7) // 8 * h, dtype=np.uint8).tobytes())
    if mode in ("I;16", "I;16B"):
        v = rng.integers(0, 65536, size=n, dtype=np.uint16)
        v[: min(n, 512)] = np.arange(min(n, 512))  # every low value, both bytes
        return Image.frombytes(mode, (w, h), v.astype("<u2" if mode == "I;16" else ">u2").tobytes())
    if mode == "I":
        v = rng.integers(-300, 70000, size=n).astype("<i4")
        return Image.frombytes("I", (w, h), v.tobytes())
    bands = {"L": 1, "P": 1, "LA": 2, "La": 2, "RGB": 3, "RGBA": 4, "RGBa": 4, "CMYK": 4}[mode]
    data = rng.integers(0, 256, size=n * bands, dtype=np.uint8)
    if bands == 4 or mode in ("LA", "La"):
        # every (value, alpha) pair once
        pairs = np.stack(np.meshgrid(np.arange(256), np.arange(256)), -1).reshape(-1, 2).astype(np.uint8)
        k = min(n, len(pairs))
        flat = data.reshape(n, bands)
        flat[:k, 0] = pairs[:k, 0]
        flat[:k, -1] = pairs[:k, 1]
        data = flat.reshape(-1)
    return Image.frombytes(mode, (w, h), data.tobytes())


@unittest.skipIf(_test_module() is None, "tmi_accel with the stage-2 _test hooks is not available")
class ConvertTests(_Base):
    def convert(self, img: Image.Image, to: str, palette=None, transparency=None):
        return self.t.pil_convert(img.mode, img.tobytes(), img.width, img.height, to, palette, transparency)

    def test_every_mode_to_rgba(self) -> None:
        rng = np.random.default_rng(12)
        for mode in ("1", "L", "LA", "La", "RGB", "RGBa", "CMYK", "I", "I;16", "I;16B"):
            img = _all_bytes_image(mode, 256, 300, rng)
            for to in ("RGBA", "RGB", "L", "LA", "RGBa", "La"):
                try:
                    want = img.convert(to)
                except ValueError:
                    with self.assertRaises(ValueError):
                        self.convert(img, to)
                    continue
                data, m, w, h = self.convert(img, to)
                self.assertEqual(m, want.mode, (mode, to))
                self.assertEqual(data, want.tobytes(), (mode, to))

    def test_palette_with_and_without_trns(self) -> None:
        rng = np.random.default_rng(13)
        for n_colours in (1, 2, 16, 100, 256):
            pal = rng.integers(0, 256, size=n_colours * 3, dtype=np.uint8).tobytes()
            idx = rng.integers(0, 256, size=64 * 64, dtype=np.uint8).tobytes()
            for trns in (None, 0, 5, 255, bytes(rng.integers(0, 256, size=min(n_colours, 77), dtype=np.uint8)), b"\x00" * 300):
                img = Image.frombytes("P", (64, 64), idx)
                img.putpalette(pal)
                if trns is not None:
                    img.info["transparency"] = trns
                for to in ("RGBA", "RGB", "L", "LA"):
                    try:
                        with warnings.catch_warnings():
                            warnings.simplefilter("ignore")
                            want = img.copy().convert(to)
                    except ValueError:  # more than 256 alphas
                        with self.assertRaises(ValueError):
                            self.convert(img, to, palette=pal, transparency=trns)
                        continue
                    data, m, w, h = self.convert(img, to, palette=pal, transparency=trns)
                    self.assertEqual((m, data), (want.mode, want.tobytes()), (n_colours, trns, to))

    def test_transparent_colour_keys(self) -> None:
        """``info["transparency"]`` on "1" / "L" / "I" / "I;16" / "RGB":
        convert_transparent, keyed on the low byte of the value."""
        rng = np.random.default_rng(14)
        cases = [("1", [0, 255, 1]), ("L", [0, 7, 255, 256 + 7]), ("I", [0, 300, 44]),
                 ("I;16", [0, 255, 300, 65535, 7]), ("RGB", [(1, 2, 3), (300, 2, 3), (0, 0, 0)])]
        for mode, keys in cases:
            img = _all_bytes_image(mode, 64, 64, rng)
            if mode == "RGB":
                arr = np.frombuffer(img.tobytes(), np.uint8).reshape(-1, 3).copy()
                arr[::5] = (1, 2, 3)
                arr[1::7] = (44, 2, 3)
                arr[2::9] = (0, 0, 0)
                img = Image.frombytes("RGB", img.size, arr.tobytes())
            for key in keys:
                img.info["transparency"] = key
                for to in ("RGBA", "LA") + (("RGBa", "La") if mode == "RGB" else ()):
                    want = img.convert(to)
                    data, m, w, h = self.convert(img, to, transparency=key)
                    self.assertEqual((m, data), (want.mode, want.tobytes()), (mode, key, to))

    def test_one_bit_to_l(self) -> None:
        """clip.py's 1-bit tiles: ``frombytes("1")`` then ``convert("L")``."""
        rng = np.random.default_rng(15)
        for w, h in ((256, 256), (13, 7), (8, 1)):
            img = _all_bytes_image("1", w, h, rng)
            data, m, _, _ = self.convert(img, "L")
            self.assertEqual((m, data), ("L", img.convert("L").tobytes()))


# ── codecs ──────────────────────────────────────────────────────────────────

ROOT = Path(__file__).resolve().parents[1]


def _external(name: str) -> Path | None:
    base = Path(os.environ.get("TMI_TEST_IMAGES") or ROOT / ".test-images")
    path = base / name
    return path if path.is_dir() else None


def _pillow_open(data: bytes):
    """(mode, size, tobytes, rgba bytes) or the exception type's name."""
    try:
        with warnings.catch_warnings():
            warnings.simplefilter("ignore")
            im = Image.open(io.BytesIO(data))
            im.load()
            return im.mode, im.size, im.tobytes(), im.convert("RGBA").tobytes()
    except Exception as e:  # noqa: BLE001 - any failure is "Pillow refuses"
        return type(e).__name__


class _CodecBase(_Base):
    def compare_decode(self, data: bytes, label: str, counts: dict) -> None:
        """decode_rgba against Image.open(...).convert("RGBA"): exact, both
        refuse, or Rust declines (an error where Pillow decodes: the caller
        then uses Pillow). Rust decoding what Pillow refuses fails."""
        want = _pillow_open(data)
        try:
            got, w, h, _fmt = self.t.decode_rgba(data)
        except ValueError as e:
            key = "both refuse" if isinstance(want, str) else "declined"
            counts[key] = counts.get(key, 0) + 1
            if key == "declined":
                counts.setdefault("declined files", []).append(label)
            return
        self.assertNotIsInstance(want, str, f"{label}: Pillow refuses ({want}), Rust decodes")
        self.assertEqual(((w, h), got), (want[1], want[3]), label)
        counts["exact"] = counts.get("exact", 0) + 1


def _pillow_pngs(rng: np.random.Generator):
    """(label, PNG bytes) written by Pillow in every mode it writes, with
    and without transparency, palettes of 1-8 bits."""
    out = []
    for w, h in ((1, 1), (7, 5), (33, 17), (64, 3)):
        rgba = _rgba(rng, w, h)
        for mode in ("1", "L", "LA", "RGB", "RGBA", "I;16", "I", "P"):
            img = rgba.convert(mode) if mode not in ("I;16", "I") else Image.frombytes(
                mode, (w, h), rng.integers(0, 65536, size=w * h).astype("<u2" if mode == "I;16" else "<i4").tobytes())
            variants = [({}, None)]
            if mode in ("L", "I;16"):
                variants.append(({"transparency": int(np.asarray(img).flat[0])}, "key"))
            if mode == "1":
                variants.append(({"transparency": 255}, "key"))
            if mode == "RGB":
                variants.append(({"transparency": tuple(int(v) for v in np.asarray(img)[0, 0])}, "key"))
            if mode == "P":
                variants.append(({"transparency": 3}, "index"))
                variants.append(({"transparency": bytes(range(0, 256, 3))}, "bytes"))
                for bits in (1, 2, 4):
                    variants.append(({"bits": bits}, f"{bits}-bit"))
            for kw, tag in variants:
                buf = io.BytesIO()
                with warnings.catch_warnings():
                    warnings.simplefilter("ignore")  # mode "I" as PNG is deprecated
                    img.save(buf, "PNG", **kw)
                out.append((f"{mode} {w}x{h} {tag}", buf.getvalue()))
    return out


@unittest.skipIf(_test_module() is None, "tmi_accel with the stage-2 _test hooks is not available")
class PngDecodeTests(_CodecBase):
    def test_pillow_written(self) -> None:
        counts: dict = {}
        for label, data in _pillow_pngs(np.random.default_rng(20)):
            mode, w, h, raw, pal, trns, _anim = self.t.png_open(data)
            im = Image.open(io.BytesIO(data))
            im.load()
            self.assertEqual((mode, (w, h)), (im.mode, im.size), label)
            self.assertEqual(raw, im.tobytes(), label)
            self.assertEqual(trns, im.info.get("transparency"), label)
            self.compare_decode(data, label, counts)
        self.assertNotIn("declined", counts)
        _report(f"Pillow-written PNGs: {counts}")

    def test_mtpng_and_banded(self) -> None:
        """tmi's PNG writer, mtpng (tmi_accel.encode_png_rgba), and the
        numpy band encoder tmi had before 1e (tmiref.png)."""
        from tmiref.png import encode_png_banded

        rng = np.random.default_rng(21)
        counts: dict = {}
        for w, h in ((1, 1), (300, 7), (129, 1100), (1500, 400)):
            img = _rgba(rng, w, h)
            raw = img.tobytes()
            for label, data in (("mtpng", _native.encode_png_rgba(raw, w, h, ("k", "v"))),
                                ("banded", encode_png_banded(raw, w, h))):
                self.compare_decode(data, f"{label} {w}x{h}", counts)
        self.assertEqual(counts, {"exact": 8})

    def test_pngsuite(self) -> None:
        root = _external("pngsuite")
        if root is None:
            self.skipTest("no PngSuite (python tools/fetch_test_images.py)")
        counts: dict = {}
        files = sorted(root.glob("*.png"))
        for p in files:
            self.compare_decode(p.read_bytes(), p.name, counts)
        self.assertEqual(len(files), 176)
        self.assertNotIn("declined", counts)
        _report(f"PngSuite: {counts}")

    def test_pillow_test_images(self) -> None:
        root = _external("pillow")
        if root is None:
            self.skipTest("no Pillow test images (python tools/fetch_test_images.py)")
        counts: dict = {}
        for p in sorted(root.rglob("*.png")):
            self.compare_decode(p.read_bytes(), str(p.relative_to(root)), counts)
        declined = counts.pop("declined files", [])
        # Rust leaves animated PNGs to Pillow.
        for name in declined:
            with warnings.catch_warnings(), Image.open(root / name) as probe:
                warnings.simplefilter("ignore")
                self.assertTrue(getattr(probe, "is_animated", False) or "apng" in name, name)
        _report(f"Pillow's test PNGs: {counts}")


def _jpeg_corpus(rng: np.random.Generator):
    """(label, JPEG bytes) written by Pillow: L / RGB / CMYK, qualities 1-100,
    4:4:4 / 4:2:2 / 4:2:0, baseline / progressive, optimised Huffman or not,
    sizes that leave partial MCUs."""
    out = []
    for w, h in ((1, 1), (7, 13), (17, 9), (64, 64), (129, 77), (300, 5), (33, 200)):
        rgba = _rgba(rng, w, h)
        cmyk = Image.frombytes("CMYK", (w, h), rng.integers(0, 256, size=w * h * 4, dtype=np.uint8).tobytes())
        for mode, src in (("L", rgba.convert("L")), ("RGB", rgba.convert("RGB")), ("CMYK", cmyk)):
            for q in (1, 10, 50, 75, 90, 95, 100):
                for sub in ((0, 1, 2) if mode == "RGB" else (-1,)):
                    for prog in (False, True):
                        for opt in (False, True):
                            kw = {"quality": q, "progressive": prog, "optimize": opt}
                            if sub != -1:
                                kw["subsampling"] = sub
                            buf = io.BytesIO()
                            src.save(buf, "JPEG", **kw)
                            out.append((f"{mode} {w}x{h} q{q} s{sub} p{prog} o{opt}", buf.getvalue()))
    return out


@unittest.skipIf(_test_module() is None, "tmi_accel with the stage-2 _test hooks is not available")
class JpegDecodeTests(_CodecBase):
    def compare_open(self, data: bytes, label: str) -> None:
        im = Image.open(io.BytesIO(data))
        im.load()
        mode, w, h, raw = self.t.jpeg_open(data)
        self.assertEqual((mode, (w, h)), (im.mode, im.size), label)
        self.assertEqual(raw, im.tobytes(), label)

    def test_pillow_written(self) -> None:
        corpus = _jpeg_corpus(np.random.default_rng(22))
        counts: dict = {}
        for label, data in corpus:
            self.compare_open(data, label)
            self.compare_decode(data, label, counts)
        self.assertEqual(counts, {"exact": len(corpus)})
        _report(f"Pillow-written JPEGs: {len(corpus)} exact")

    def test_other_samplings_arithmetic_restarts(self) -> None:
        """Files Pillow cannot write, written with libjpeg's own encoder:
        4:4:0, 4:1:1, 4:1:0, 3x1, h2v2 chroma at 2x1 and other factors,
        arithmetic coding, restart markers."""
        rng = np.random.default_rng(23)
        samplings = [[(1, 2)], [(4, 1)], [(4, 2)], [(2, 2), (2, 1)], [(3, 1)], [(1, 3)], [(2, 4)], [(4, 4)],
                     [(2, 2), (1, 2), (2, 1)], [(1, 1)], [(2, 1)], [(2, 2)]]
        n = 0
        for w, h in ((1, 1), (5, 3), (17, 9), (64, 64), (129, 77), (31, 202)):
            for mode, c in (("RGB", 3), ("CMYK", 4), ("L", 1)):
                a = np.asarray(_rgba(rng, w, h))[..., :c] if c < 4 else rng.integers(0, 256, size=(h, w, 4), dtype=np.uint8)
                for samp in samplings if c > 1 else [[(1, 1)], [(2, 2)], [(1, 2)]]:
                    factors = (samp + [(1, 1)] * c)[:c]
                    for prog, arith, restart in ((False, False, 0), (True, False, 0), (False, True, 0), (False, False, 2), (True, True, 1)):
                        try:
                            data = self.t.jpeg_encode(mode, np.ascontiguousarray(a).tobytes(), w, h, 85, None, -1, False,
                                                      prog, factors, restart, arith)
                        except ValueError:  # libjpeg refuses the combination (too many blocks per MCU)
                            continue
                        self.compare_open(data, f"{mode} {w}x{h} {factors} p{prog} a{arith} r{restart}")
                        n += 1
        self.assertGreater(n, 600)
        _report(f"libjpeg-written JPEGs (other samplings, arithmetic, restarts): {n} exact")

    def test_truncated_is_an_error(self) -> None:
        buf = io.BytesIO()
        _rgba(np.random.default_rng(24), 64, 64).convert("RGB").save(buf, "JPEG", quality=90)
        data = buf.getvalue()
        for cut in (len(data) // 2, len(data) - 2, 400):
            self.assertIsInstance(_pillow_open(data[:cut]), str)
            with self.assertRaises(ValueError):
                self.t.decode_rgba(data[:cut])

    def test_pillow_test_images(self) -> None:
        root = _external("pillow")
        if root is None:
            self.skipTest("no Pillow test images (python tools/fetch_test_images.py)")
        counts: dict = {}
        for p in sorted(root.rglob("*")):
            if p.suffix.lower() in (".jpg", ".jpeg", ".mpo"):
                self.compare_decode(p.read_bytes(), str(p.relative_to(root)), counts)
        declined = counts.pop("declined files", [])
        # Declined: files libjpeg warns about (extraneous bytes, corrupt data).
        self.assertLessEqual(len(declined), 5, declined)
        _report(f"Pillow's test JPEGs: {counts}, declined {declined}")


def _psnr(a: bytes, b: bytes) -> float:
    x = np.frombuffer(a, np.uint8).astype(np.float64)
    y = np.frombuffer(b, np.uint8).astype(np.float64)
    mse = float(np.mean((x - y) ** 2))
    return float("inf") if mse == 0 else 10 * np.log10(255.0 ** 2 / mse)


@unittest.skipIf(_test_module() is None, "tmi_accel with the stage-2 _test hooks is not available")
class EncoderTests(_Base):
    def test_jpeg_quality_95(self) -> None:
        """native.py's write-back (``convert("RGB").save(JPEG, quality=95)``)
        and other qualities / subsamplings: the same bytes as Pillow."""
        rng = np.random.default_rng(30)
        n = 0
        for w, h in ((1, 1), (16, 16), (101, 37), (640, 480)):
            rgb = _rgba(rng, w, h).convert("RGB")
            for q in (95, 75, 50, 100):
                for sub in (-1, 0, 1, 2):
                    buf = io.BytesIO()
                    kw = {"quality": q} | ({"subsampling": sub} if sub != -1 else {})
                    rgb.save(buf, "JPEG", **kw)
                    got = self.t.jpeg_encode("RGB", rgb.tobytes(), w, h, q, None, sub)
                    self.assertEqual(got, buf.getvalue(), (w, h, q, sub))
                    n += 1
            grey = rgb.convert("L")
            buf = io.BytesIO()
            grey.save(buf, "JPEG", quality=95)
            self.assertEqual(self.t.jpeg_encode("L", grey.tobytes(), w, h, 95), buf.getvalue())
        _report(f"JPEG encode: {n + 4} byte-identical")

    def test_jpeg_keep_source_tables(self) -> None:
        """Write-back keeping the source JPEG's quantisation tables and
        subsampling (``qtables=src.quantization, subsampling=get_sampling``,
        what ``quality="keep"`` uses): the same bytes as Pillow, and the
        tables read back from the source as Pillow reads them."""
        from PIL import JpegImagePlugin

        rng = np.random.default_rng(31)
        for q, sub in ((95, 2), (80, 0), (60, 1), (30, 2)):
            src_buf = io.BytesIO()
            _rgba(rng, 120, 90).convert("RGB").save(src_buf, "JPEG", quality=q, subsampling=sub)
            source = Image.open(io.BytesIO(src_buf.getvalue()))
            tables = self.t.jpeg_quantization(src_buf.getvalue())
            self.assertEqual(tables, {k: list(v) for k, v in source.quantization.items()})
            sampling = JpegImagePlugin.get_sampling(source)
            edited = _rgba(rng, 120, 90).convert("RGB")
            want = io.BytesIO()
            edited.save(want, "JPEG", qtables=source.quantization, subsampling=sampling)
            got = self.t.jpeg_encode("RGB", edited.tobytes(), 120, 90, -1, [tables[k] for k in sorted(tables)], sampling)
            self.assertEqual(got, want.getvalue(), (q, sub))

    def test_webp_preview(self) -> None:
        """diff.py's previews (quality 82, method 2, RGBA): decodes, keeps
        alpha, and here even the bytes are Pillow's (same libwebp 1.6.0)."""
        rng = np.random.default_rng(32)
        for w, h in ((1, 1), (63, 17), (300, 200), (1024, 700)):
            img = _rgba(rng, w, h)
            buf = io.BytesIO()
            img.save(buf, "WEBP", quality=82, method=2)
            got = self.t.webp_encode(img.tobytes(), w, h, 4, 82.0, 2)
            back = Image.open(io.BytesIO(got))
            back.load()
            self.assertEqual((back.mode, back.size), ("RGBA", (w, h)))
            mine = np.frombuffer(back.tobytes(), np.uint8).reshape(-1, 4)
            src = np.frombuffer(img.tobytes(), np.uint8).reshape(-1, 4)
            self.assertGreater(_psnr(mine[:, 3].tobytes(), src[:, 3].tobytes()), 40)
            theirs = Image.open(io.BytesIO(buf.getvalue())).convert("RGBA").tobytes()
            self.assertAlmostEqual(len(got), len(buf.getvalue()), delta=len(buf.getvalue()) * 0.02 + 64)
            self.assertGreater(_psnr(back.tobytes(), img.tobytes()), _psnr(theirs, img.tobytes()) - 0.5)
            self.assertEqual(got, buf.getvalue(), (w, h))
        opaque = _rgba(rng, 50, 40).convert("RGB")
        buf = io.BytesIO()
        opaque.save(buf, "WEBP", quality=82, method=2)
        self.assertEqual(self.t.webp_encode(opaque.tobytes(), 50, 40, 3, 82.0, 2), buf.getvalue())


if __name__ == "__main__":
    unittest.main()
