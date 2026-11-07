"""Differential tests of the Rust renderer's building blocks against the
installed Pillow and numpy (RUST_MIGRATION.md 1a, done criterion 3).

Each kernel is reached through ``tmi_accel._test`` and compared with the
Python operation it reproduces (Pillow, numpy, or the reference package tmiref), exhaustively where the input space allows
(multiply: all 65,536 pairs; the L-mask paste: all 16.7 M triples).

Stage 1b / 1c add the C-runtime calls over each call site's input domain
(pow, cbrt, cos, sin, hypot, sqrt), numpy's OpenBLAS 2x2 inverse and
(n, 3) @ (3, 3) product, and the gradient, tone, edge and material code on
random inputs."""
from __future__ import annotations

import unittest

import numpy as np
from PIL import Image, ImageChops

from tmi import accel


def _test_module():
    return getattr(accel.native(), "_test", None)


@unittest.skipIf(_test_module() is None, "tmi_accel with _test is not available")
class PillowKernelTests(unittest.TestCase):
    def setUp(self) -> None:
        self.t = _test_module()

    def test_multiply_all_pairs(self) -> None:
        a = np.repeat(np.arange(256, dtype=np.uint8), 256)
        b = np.tile(np.arange(256, dtype=np.uint8), 256)
        want = ImageChops.multiply(Image.frombytes("L", (256, 256), a.tobytes()),
                                   Image.frombytes("L", (256, 256), b.tobytes())).tobytes()
        self.assertEqual(self.t.multiply(a.tobytes(), b.tobytes()), want)

    def test_paste_mask_l_all_triples(self) -> None:
        """``Image.composite(src, dst, mask)`` (Pillow's paste_mask_L) for every
        (dst, src, mask) byte triple; channel 0 enumerates them, the other
        channels shuffle the same values."""
        p = np.arange(1 << 24, dtype=np.uint32)
        d = (p >> 16).astype(np.uint8)
        s = ((p >> 8) & 0xFF).astype(np.uint8)
        m = (p & 0xFF).astype(np.uint8)
        dst = np.stack([d, d ^ 0x55, 255 - d, s], -1)
        src = np.stack([s, 255 - s, s ^ 0xA5, d], -1)
        size = (4096, 4096)
        dst_img = Image.frombytes("RGBA", size, dst.tobytes())
        src_img = Image.frombytes("RGBA", size, src.tobytes())
        mask_img = Image.frombytes("L", size, m.tobytes())
        want = Image.composite(src_img, dst_img, mask_img).tobytes()
        self.assertEqual(self.t.paste_mask_l(dst.tobytes(), src.tobytes(), m.tobytes()), want)

    def test_alpha_bbox(self) -> None:
        rng = np.random.default_rng(11)
        for _ in range(300):
            h, w = rng.integers(1, 40, size=2)
            a = np.zeros((h, w, 4), np.uint8)
            for _ in range(int(rng.integers(0, 4))):
                y, x = rng.integers(0, h), rng.integers(0, w)
                a[y, x, 3] = rng.integers(1, 256)
                a[y, x, 0] = 9
            want = Image.fromarray(a, "RGBA").getchannel("A").getbbox()
            self.assertEqual(self.t.alpha_bbox(a.tobytes(), int(w)), want)


def _draw(size, polygons=(), ellipses=()):
    from PIL import ImageDraw

    im = Image.new("L", size, 0)
    d = ImageDraw.Draw(im)
    for p in polygons:
        d.polygon(p, fill=255)
    for e in ellipses:
        d.ellipse(e, fill=255)
    return im.tobytes()


def _random_polygon(rng):
    """A polygon as clip_vector draws them (a stroke segment's quad, often
    tiny or degenerate after truncation) or an arbitrary one, at the scales
    of the supersampled stroke images."""
    import math

    scale = float(rng.choice([3, 8, 30, 120, 600]))
    w, h = (int(v) for v in rng.integers(1, int(min(scale, 200)) + 8, 2))
    kind = rng.random()
    if kind < 0.45:
        p0 = rng.uniform(-scale * 0.1, scale, 2)
        p1 = p0 + rng.uniform(-scale / 4, scale / 4, 2)
        ra, rb = rng.uniform(0, scale / 8, 2) if rng.random() < 0.7 else rng.uniform(0, 1.0, 2)
        d = p1 - p0
        length = math.hypot(*d) or 1.0
        n = np.array([-d[1], d[0]]) / length
        pts = [p0 + n * ra, p1 + n * rb, p1 - n * rb, p0 - n * ra]
    elif kind < 0.7:
        pts = list(rng.uniform(-scale * 0.1, scale * 1.1, (int(rng.integers(3, 9)), 2)))
    elif kind < 0.9:  # integer vertices: corners, collinear and retraced edges
        pts = list(rng.integers(-3, int(scale) + 3, (int(rng.integers(3, 8)), 2)).astype(float))
    else:
        c = rng.uniform(0, scale, 2)
        pts = [c + rng.uniform(-1.5, 1.5, 2) for _ in range(int(rng.integers(3, 6)))]
    return (w, h), [(float(x), float(y)) for x, y in pts]


@unittest.skipIf(_test_module() is None, "tmi_accel with _test is not available")
class PillowDrawTests(unittest.TestCase):
    """``ImageDraw.polygon`` / ``ellipse`` and ``Image.reduce`` as clip_vector
    uses them (stage 1d). The rules in tmi-pillow/src/draw.rs were derived
    from Pillow's behaviour; these keep them pinned (the same generators ran
    1.5 M polygons and every ellipse up to 300 x 300 when they were written)."""

    def setUp(self) -> None:
        self.t = _test_module()

    def test_polygons(self) -> None:
        rng = np.random.default_rng(1607)
        for i in range(30000):
            size, pts = _random_polygon(rng)
            want = _draw(size, [pts])
            self.assertEqual(self.t.draw_l(size[0], size[1], [pts], []), want, (size, pts))
            if i % 5 == 0:  # a band / window of the same drawing
                w, h = size
                x0, y0 = int(rng.integers(0, w)), int(rng.integers(0, h))
                region = (x0, y0, int(rng.integers(x0 + 1, w + 1)), int(rng.integers(y0 + 1, h + 1)))
                crop = np.frombuffer(want, np.uint8).reshape(h, w)[region[1]:region[3], region[0]:region[2]]
                self.assertEqual(self.t.draw_l(w, h, [pts], [], region), crop.tobytes(), (size, pts, region))

    def test_ellipses_every_box(self) -> None:
        for a in range(80):
            for b in range(80):
                box = (1.0, 2.0, 1.0 + a, 2.0 + b)
                self.assertEqual(self.t.draw_l(a + 3, b + 4, [], [box]), _draw((a + 3, b + 4), (), [box]), (a, b))

    def test_ellipses_as_round_caps(self) -> None:
        rng = np.random.default_rng(5)
        for i in range(3000):
            r = float(rng.uniform(0.5, 400.0 if i % 10 == 0 else 40.0))
            cx, cy = rng.uniform(-r, 3 * r, 2)
            w, h = (int(min(v, 1200)) for v in rng.integers(1, int(3 * r) + 4, 2))
            box = (cx - r, cy - r, cx + r, cy + r)
            self.assertEqual(self.t.draw_l(w, h, [], [box]), _draw((w, h), (), [box]), box)

    def test_vector_strokes(self) -> None:
        """clip_vector's whole per-stroke drawing: quads and round caps on the
        supersampled image (4x, or 8x / 16x for hairlines), reduce, the
        float32 blur and the opacity table."""
        from tmiref import clip_vector as V

        rng = np.random.default_rng(23)
        for i in range(400):
            n = int(rng.integers(1, 30))
            steps = rng.normal(0, float(rng.choice([0.3, 2.0, 12.0])), (n, 2))
            if rng.random() < 0.2:
                steps[rng.integers(0, n, 3) % n] = 0.0  # repeated samples
            xy = np.cumsum(steps, axis=0) + rng.uniform(-20, 120, 2)
            scale = float(rng.choice([0.03, 0.3, 1.5, 6.0]))
            r = np.abs(rng.normal(scale, scale / 2, n))
            if rng.random() < 0.3:
                r[rng.integers(0, n)] = 0.0
            bounds = (0, 0, int(rng.integers(20, 140)), int(rng.integers(20, 140)))
            blur = rng.random() < 0.7
            opacity = 1.0 if rng.random() < 0.5 else float(rng.uniform(0, 1))
            got = V._draw_stroke(xy, r, bounds, V.EDGE_BLUR if blur else 0.0)
            mine = self.t.vector_stroke([tuple(p) for p in xy.tolist()], r.tolist(), list(bounds), blur, opacity)
            if got is None:
                self.assertIsNone(mine, i)
                continue
            cov, x0, y0 = got
            if opacity < 1.0:
                cov = cov.point(lambda v, o=opacity: int(round(v * o)))
            self.assertIsNotNone(mine, i)
            data, mx0, my0, mx1, my1 = mine
            self.assertEqual((mx0, my0, mx1 - mx0, my1 - my0), (x0, y0, cov.width, cov.height), i)
            self.assertEqual(data, cov.tobytes(), i)

    def test_frame_rasterizer(self) -> None:
        """clip_frame's scanline fill (float32 coverage, exactly)."""
        from tmiref import clip_frame as F

        rng = np.random.default_rng(29)
        for i in range(300):
            polys = []
            for _ in range(int(rng.integers(1, 4))):
                k = int(rng.integers(3, 9))
                c = rng.uniform(0, 60, 2)
                pts = c + rng.normal(0, float(rng.choice([2.0, 15.0, 40.0])), (k, 2))
                if rng.random() < 0.3:
                    pts = np.round(pts * 4) / 4
                polys.append([tuple(p) for p in pts.tolist()])
            x0, y0 = (int(v) for v in rng.integers(-10, 30, 2))
            bounds = (x0, y0, x0 + int(rng.integers(1, 90)), y0 + int(rng.integers(1, 90)))
            want = F._rasterize(polys, bounds)
            got = np.array(self.t.frame_rasterize(polys, list(bounds)), np.float32).reshape(want.shape)
            self.assertTrue(np.array_equal(want, got), i)  # (the sign of a zero may differ; no step reads it)

    def test_reduce_every_block_sum(self) -> None:
        rng = np.random.default_rng(3)
        for n in (2, 3, 4, 8, 16):
            total = 255 * n * n + 1
            side = int(np.ceil(np.sqrt(total)))
            sums = np.arange(side * side) % total
            q, r = np.divmod(sums, n * n)
            blocks = np.repeat(q[:, None], n * n, 1) + (np.arange(n * n)[None, :] < r[:, None])
            blocks = rng.permuted(np.clip(blocks, 0, 255), axis=1)
            img = blocks.reshape(side, side, n, n).transpose(0, 2, 1, 3).reshape(side * n, side * n).astype(np.uint8)
            want = Image.fromarray(img, "L").reduce(n).tobytes()
            self.assertEqual(self.t.reduce_l(img.tobytes(), side * n, side * n, n), want, n)


@unittest.skipIf(_test_module() is None, "tmi_accel with _test is not available")
class NumpyKernelTests(unittest.TestCase):
    def setUp(self) -> None:
        self.t = _test_module()

    def test_interp(self) -> None:
        rng = np.random.default_rng(7)
        for n in (2, 3, 5, 40, 2048 * 3):
            for _ in range(20):
                xp = np.sort(rng.random(n))
                if n > 3:
                    xp[n // 2] = xp[n // 2 - 1]  # a repeated knot
                fp = rng.random(n) * 2 - 0.5
                x = np.concatenate([rng.random(300) * 1.4 - 0.2, xp[: min(n, 50)], [xp[0], xp[-1]]])
                want = np.interp(x, xp, fp)
                got = np.array(self.t.interp(x.tolist(), xp.tolist(), fp.tolist()))
                self.assertTrue(np.array_equal(want.view(np.uint64), got.view(np.uint64)))

    def test_linspace(self) -> None:
        for start, stop, num in ((0.0, 1.0, 2048), (0.0, 1.0, 2), (0.25, -3.5, 77), (1.0, 1.0, 5), (0.0, 1.0, 1)):
            want = np.linspace(start, stop, num)
            got = np.array(self.t.linspace(start, stop, num), np.float64)
            self.assertTrue(np.array_equal(want.view(np.uint64), got.view(np.uint64)), (start, stop, num))

    def test_hypot(self) -> None:
        """CPython's own ``math.hypot`` (clip_frame) and libm's (``np.hypot``,
        clip_vector), bit for bit."""
        import math

        rng = np.random.default_rng(13)
        n = 50000
        cases = [
            rng.uniform(-1e4, 1e4, (2, n)),
            rng.normal(0, 1, (2, n)) * 10.0 ** rng.integers(-300, 300, (2, n)),
            np.stack([rng.integers(-5000, 5000, n) + rng.integers(0, 2, n) * 0.5, rng.integers(-5000, 5000, n).astype(float)]),
            np.array([[0.0, 3.0, -0.0, 1e-310, 5e-324, 1.7e308, float("inf"), 1.0], [0.0, 4.0, 2.0, 1e-310, 0.0, 1.7e308, 1.0, 1e-17]]),
        ]
        for x, y in cases:
            want = np.array([math.hypot(a, b) for a, b in zip(x.tolist(), y.tolist())])
            got = np.array(self.t.py_hypot(x.tolist(), y.tolist()))
            self.assertTrue(np.array_equal(want.view(np.uint64), got.view(np.uint64)))
            got = np.array(self.t.libm_hypot(x.tolist(), y.tolist()))
            with np.errstate(over="ignore"):
                want = np.hypot(x, y)
            self.assertTrue(np.array_equal(want.view(np.uint64), got.view(np.uint64)))

    def test_add_reduceat(self) -> None:
        rng = np.random.default_rng(17)
        for _ in range(400):
            n = int(rng.integers(1, 2000))
            a = rng.random(n) * 10.0 ** int(rng.integers(-3, 4))
            idx = np.unique(np.concatenate([[0], rng.integers(0, n, int(rng.integers(0, 40)))]))
            want = np.add.reduceat(a, idx)
            got = np.array(self.t.add_reduceat(a.tolist(), idx.tolist()))
            self.assertTrue(np.array_equal(want.view(np.uint64), got.view(np.uint64)))

    def test_float_mod(self) -> None:
        rng = np.random.default_rng(3)
        a = np.concatenate([rng.random(5000) * 40 - 20, [-0.0, 0.0, -6.0, 6.0, 12.0, -1e-300, 5.999999999999999]])
        for b in (6.0, 1.0, 0.7):
            want = np.asarray(a) % b
            got = np.array(self.t.py_mod(a.tolist(), b))
            self.assertTrue(np.array_equal(want.view(np.uint64), got.view(np.uint64)), b)
            self.assertEqual([v % b for v in a.tolist()], got.tolist())


def _filter_blob(kind: int, data: bytes) -> bytes:
    import struct

    return struct.pack(">II", kind, len(data)) + data


def _random_filters(rng) -> list[bytes]:
    """FilterLayerInfo blobs of every kind with random parameters, plus the
    edge cases (identity, short data, unsupported ids)."""
    import struct

    out = []
    for _ in range(40):
        out.append(_filter_blob(1, struct.pack(">ii", *rng.integers(-255, 256, 2))))
        out.append(_filter_blob(1, struct.pack(">ii", int(rng.integers(-100, 101)), int(rng.choice([100, 150, -100, -150, 99])))))
        recs = []
        for _r in range(int(rng.integers(1, 5))):
            ib, iw = sorted(int(v) for v in rng.integers(0, 65536, 2))
            mid = int(rng.integers(0, 65536))
            ob, ow = (int(v) for v in rng.integers(0, 65536, 2))
            recs.append(struct.pack(">5H", ib, mid, iw, ob, ow) if rng.random() < 0.8 else struct.pack(">5H", 0, 0x7FFF, 0xFFFF, 0, 0xFFFF))
        out.append(_filter_blob(2, b"".join(recs) + bytes(10 * (32 - len(recs)))))
        sections = []
        for _c in range(4):
            n = int(rng.integers(2, 7))
            xs = np.sort(rng.integers(0, 65536, n))
            xs[0], xs[-1] = 0, 65535
            ys = rng.integers(0, 65536, n)
            if rng.random() < 0.3:
                ys = xs.copy()  # identity channel
            body = struct.pack(">H", n) + b"".join(struct.pack(">2H", int(x), int(y)) for x, y in zip(xs, ys))
            sections.append(body + bytes(130 - len(body)))
        out.append(_filter_blob(3, b"".join(sections)))
        out.append(_filter_blob(4, struct.pack(">iii", int(rng.integers(-180, 181)), int(rng.integers(-100, 101)), int(rng.integers(-100, 101)))))
        vals = [int(rng.integers(0, 2))] + [int(v) if rng.random() < 0.5 else 0 for v in rng.integers(-100, 101, 9)]
        out.append(_filter_blob(5, struct.pack(">10i", *vals)))
        out.append(_filter_blob(7, struct.pack(">I", int(rng.integers(0, 40)))))
        out.append(_filter_blob(8, struct.pack(">I", int(rng.integers(0, 256)))))
    out += [_filter_blob(6, b""), _filter_blob(8, b""), _filter_blob(9, b"\0" * 8), _filter_blob(1, b"\0" * 8),
            _filter_blob(4, struct.pack(">iii", 360, 0, 0)), _filter_blob(5, bytes(40)), _filter_blob(2, b"\0" * 5),
            _filter_blob(4, struct.pack(">iii", 0, 0, 100)), _filter_blob(4, struct.pack(">iii", 0, 100, -100))]
    return out


@unittest.skipIf(_test_module() is None, "tmi_accel with _test is not available")
class RenderPieceTests(unittest.TestCase):
    """The renderer's correction filters and layer styles against clip.py."""

    def setUp(self) -> None:
        self.t = _test_module()

    def test_filters(self) -> None:
        from tmiref import clip as C

        rng = np.random.default_rng(1)
        grid = np.stack(np.meshgrid(*[np.arange(0, 256, 15, dtype=np.uint8)] * 3, indexing="ij"), -1).reshape(-1, 3)
        colours = np.concatenate([grid, rng.integers(0, 256, (3000, 3), dtype=np.uint8),
                                  np.repeat(np.arange(256, dtype=np.uint8)[:, None], 3, 1)])
        kinds = set()
        for blob in _random_filters(rng):
            import struct

            kind, size = struct.unpack(">II", blob[:8])
            builder = C._FILTER_BUILDERS.get(kind)
            fn = builder(blob[8:8 + size]) if builder else None
            got_kind, lut, unsupported = self.t.filter_lut(blob)
            if builder is None:
                self.assertEqual((got_kind, unsupported), ("unsupported", kind))
                continue
            if fn is None:
                self.assertEqual(got_kind, "none", blob.hex())
                continue
            kinds.add(kind)
            if getattr(fn, "per_channel", True):
                self.assertEqual(got_kind, "lut", blob.hex())
                want = np.array(C._filter_lut(fn), np.float32)
                self.assertTrue(np.array_equal(want.view(np.uint32), np.array(lut, np.float32).view(np.uint32)), blob.hex())
            else:
                self.assertEqual(got_kind, "colour", blob.hex())
                rgb = colours.astype(np.float32) / 255.0
                want = np.clip(fn(rgb[:, None, :]), 0.0, 1.0).astype(np.float32).reshape(-1)
                got = np.array(self.t.filter_colours(blob, colours.tobytes()), np.float32)
                self.assertTrue(np.array_equal(want.view(np.uint32), got.view(np.uint32)), blob.hex())
        self.assertEqual(kinds, {1, 2, 3, 4, 5, 6, 7, 8})

    def test_layer_styles(self) -> None:
        from tmiref import clip_style

        rng = np.random.default_rng(2)
        colours = np.concatenate([rng.integers(0, 256, (5000, 3), dtype=np.uint8),
                                  np.repeat(np.arange(256, dtype=np.uint8)[:, None], 3, 1)])
        img = np.concatenate([colours, np.full((len(colours), 1), 200, np.uint8)], 1)[None]
        image = Image.fromarray(np.ascontiguousarray(img), "RGBA")
        cases = 0
        for ct in (0, 1, 2):
            for black in (0, 1):
                for white in (0, 1):
                    for profile in (None, "single-channel", "gray-alpha", "color-alpha-bgra"):
                        for lc in (None, ((12, 200, 77), (255, 255, 255)), ((0, 0, 0), (250, 3, 128)), ((9, 9, 9), None)):
                            style = {"LayerColorTypeIndex": ct, "LayerColorTypeBlackChecked": black,
                                     "LayerColorTypeWhiteChecked": white}
                            if lc is not None:
                                style["DrawColorEnable"] = 1
                                for name, v in zip(("Red", "Green", "Blue"), lc[0]):
                                    style[f"DrawColorMain{name}"] = v * 0x01010101
                                if lc[1] is not None:
                                    for name, v in zip(("Red", "Green", "Blue"), lc[1]):
                                        style[f"DrawColorSub{name}"] = v * 0x01010101
                            out = clip_style.apply_layer_style(image, style, profile)
                            if out is image:
                                continue
                            cases += 1
                            arr = np.asarray(out)[0]
                            rgb, keep = self.t.style_colours(colours.tobytes(), ct, bool(black), bool(white),
                                                             lc[0] if lc else None, lc[1] if lc else None, profile)
                            self.assertEqual(arr[:, :3].tobytes(), rgb, (ct, black, white, profile, lc))
                            # モノクロ cuts the alpha (200) to 255 before the factor
                            a = 255 if ct == clip_style.MONO and profile != "single-channel" else 200
                            self.assertEqual(arr[:, 3].tolist(), [(a * k + 127) // 255 for k in keep])
        self.assertGreater(cases, 50)


# ───────────────────── stage 1b / 1c: gradients, tone, edge, materials ─────────────────────


def _same(a, b) -> bool:
    """Bit-for-bit equality of two float64 arrays."""
    a, b = np.asarray(a, np.float64), np.asarray(b, np.float64)
    return a.shape == b.shape and np.array_equal(a.view(np.uint64), b.view(np.uint64))


def _dense(lo: float, hi: float, n: int, rng) -> np.ndarray:
    """``n`` float64 values over [lo, hi]: half uniform, half uniform over the
    bit patterns (so every binade is visited), plus both ends."""
    if lo < 0 < hi:
        return np.concatenate([-_dense(0.0, -lo, n // 2, rng), _dense(0.0, hi, n - n // 2, rng)])
    a, b = sorted(np.array([lo, hi], np.float64).view(np.int64).tolist())
    bits = rng.integers(a, b + 1, n // 2, dtype=np.int64).view(np.float64)
    return np.concatenate([bits, rng.uniform(lo, hi, n - n // 2), [lo, hi]])


@unittest.skipIf(_test_module() is None, "tmi_accel with _test is not available")
class LibmCallSiteTests(unittest.TestCase):
    """The C-runtime functions Rust calls, against numpy (or ``math`` where
    Python calls ``math``) over each call site's input domain: every value
    where the domain is finite, about a million otherwise."""

    N = 1 << 20

    def setUp(self) -> None:
        self.t = _test_module()
        self.rng = np.random.default_rng(1607)

    def check(self, name: str, x: np.ndarray, want: np.ndarray, y=None) -> None:
        got = self.t.libm(name, x.tolist(), None if y is None else np.broadcast_to(y, x.shape).tolist())
        bad = np.flatnonzero(np.asarray(want, np.float64).view(np.uint64) != np.asarray(got, np.float64).view(np.uint64))
        self.assertEqual(bad.size, 0, f"{name}: {bad.size} differ, e.g. x={x[bad[:3]].tolist()}")

    def test_gradient_pow(self) -> None:
        from tmiref import clip_gradient as G

        # _to_lin: every 8-bit channel value (its whole domain).
        c = np.arange(256, dtype=np.float64) / 255.0
        base = (np.maximum(c, 0) + 0.055) / 1.055
        self.check("pow", base, base ** 2.4, 2.4)
        # _from_lin on (0.0031308, 1].
        x = _dense(0.0031308, 1.0, self.N, self.rng)
        self.check("pow", x, x ** (1 / 2.4), 1 / 2.4)
        # _pq: |y| ** M1 with y = linear * white scale (<= 4.66), then the
        # ratio (C1 .. ~1.002) ** M2.
        y = _dense(0.0, 5.0, self.N, self.rng)
        self.check("pow", y, np.abs(y) ** G._M1, G._M1)
        r = _dense(G._C1, 1.01, self.N, self.rng)
        self.check("pow", r, r ** G._M2, G._M2)
        # _pq_inv: |e| ** (1 / M2) (e = mixed PQ values), then ** (1 / M1).
        e = _dense(0.0, 1.2, self.N, self.rng)
        self.check("pow", e, np.abs(e) ** (1 / G._M2), 1 / G._M2)
        q = _dense(0.0, 4.0, self.N, self.rng)
        self.check("pow", q, q ** (1 / G._M1), 1 / G._M1)
        # mode 2 decode: (x @ OK_M2_INV.T) ** 3.
        v = _dense(-1.0, 2.0, self.N, self.rng)
        self.check("pow", v, v ** 3, 3.0)

    def test_gradient_cbrt(self) -> None:
        x = _dense(-0.5, 1.5, self.N, self.rng)
        self.check("cbrt", x, np.cbrt(x))

    def test_tone_cos_and_screen_angles(self) -> None:
        import math

        from tmiref import clip_tone as T

        u = _dense(-0.6, 0.6, self.N, self.rng)
        arg = T.TAU * u
        self.check("cos", arg, np.cos(arg))
        # screen_vectors: math.cos / math.sin of every whole angle up to 2^20
        # degrees (EffectTone stores a u32), plus random u32s.
        angles = np.concatenate([np.arange(1 << 20), self.rng.integers(0, 1 << 32, 1 << 16)]).astype(np.float64)
        th = np.array([math.radians(a) for a in angles.tolist()])
        self.check("cos", th, np.array([math.cos(v) for v in th.tolist()]))
        self.check("sin", th, np.array([math.sin(v) for v in th.tolist()]))

    def test_material_hypot_and_edge_sqrt(self) -> None:
        a = _dense(-1e4, 1e4, self.N, self.rng)
        b = self.rng.permutation(a)
        self.check("hypot", a, np.hypot(a, b), b)
        k = np.arange(1 << 22, dtype=np.float64)  # R*R - dy*dy for R up to 2048
        self.check("sqrt", k, np.sqrt(k))


@unittest.skipIf(_test_module() is None, "tmi_accel with _test is not available")
class LinalgTests(unittest.TestCase):
    """numpy's OpenBLAS arithmetic: the 2x2 ``np.linalg.inv`` and the
    (n, 3) @ (3, 3) matmul."""

    def setUp(self) -> None:
        self.t = _test_module()

    def test_inv2(self) -> None:
        rng = np.random.default_rng(5)
        mats = [rng.standard_normal((2, 2)) for _ in range(20000)]
        mats += [rng.uniform(-1e3, 1e3, (2, 2)) for _ in range(5000)]
        mats += [rng.standard_normal((2, 2)) * 10.0 ** rng.integers(-12, 12) for _ in range(5000)]
        mats += [rng.integers(-12, 13, (2, 2)).astype(np.float64) for _ in range(5000)]
        for _ in range(5000):  # near axis-aligned (materials), pivot ties
            m = rng.standard_normal((2, 2))
            m[0, 1], m[1, 0] = rng.standard_normal(2) * 1e-7
            mats.append(m)
            m = rng.standard_normal((2, 2))
            m[1, 0] = m[0, 0] * rng.choice([1.0, -1.0])
            mats.append(m)
        for vx in range(-40, 41):  # every tone lattice [[v1x, -v1y], [v1y, v1x]]
            for vy in range(-40, 41):
                mats.append(np.array([[vx, -vy], [vy, vx]], np.float64))
        mats += [np.zeros((2, 2)), np.array([[1.0, 2.0], [2.0, 4.0]]), np.array([[0.0, 1.0], [1.0, 0.0]]),
                 np.array([[1e-310, 0.0], [0.0, 1.0]]), np.array([[np.inf, 0.0], [0.0, 1.0]])]
        some = 0
        for m in mats:
            got = self.t.inv2(m.tolist())
            try:
                with np.errstate(all="ignore"):
                    want = np.linalg.inv(m)
            except np.linalg.LinAlgError:
                self.assertIsNone(got, m)
                continue
            if got is None:
                # Declined only off the pinned path (non-finite, subnormal pivots).
                self.assertTrue(not np.isfinite(m).all() or np.abs(m).min() < 1e-300 or not np.isfinite(want).all(), m)
                continue
            some += 1
            self.assertTrue(_same(want, got), (m.tolist(), want.tolist(), got))
        self.assertGreater(some, 40000)

    def test_matmul3(self) -> None:
        from tmiref import clip_gradient as G

        if not self.t.fma_kernels():
            self.skipTest("no FMA kernels: the renderer declines what needs them")
        rng = np.random.default_rng(6)
        mats = [G._LMS, G._LMS_INV, G._OK_M1, G._OK_M2, G._OK_M1_INV, G._OK_M2_INV] + [rng.standard_normal((3, 3)) for _ in range(6)]
        for m in mats:
            # (A single row goes through gemv, which sums differently; the
            # renderer's products have >= 2 stops / 16385 grid points.)
            for n in (2, 3, 4, 5, 7, 8, 9, 100, 16385):
                x = rng.standard_normal((n, 3)) * rng.choice([1e-3, 1.0, 1e3])
                got = np.array(self.t.matmul3(x.reshape(-1).tolist(), m.tolist())).reshape(n, 3)
                self.assertTrue(_same(x @ m.T, got), (n, m.tolist()))

    def test_gradient_matrices(self) -> None:
        from tmiref import clip_gradient as G

        got = self.t.gradient_matrices()
        for want, mine in zip((G._LMS, G._LMS_INV, G._OK_M1, G._OK_M2, G._OK_M1_INV, G._OK_M2_INV), got):
            self.assertTrue(_same(want, mine))


def _random_stops(rng, n: int):
    pos = sorted(int(v) for v in rng.integers(0, 0x8001, n))
    if n > 2 and rng.random() < 0.3:
        pos[1] = pos[0]  # a hard stop
    if rng.random() < 0.5:
        pos[0], pos[-1] = 0, 0x8000
    out = []
    for p in pos:
        rgba = [int(v) for v in rng.integers(0, 256, 4)]
        if rng.random() < 0.6:
            rgba[3] = 255
        out.append((min(p, 0x8000) / 0x8000, tuple(rgba)))
    return out


@unittest.skipIf(_test_module() is None, "tmi_accel with _test is not available")
class GradientToneEdgeMaterialTests(unittest.TestCase):
    """clip_gradient, clip_tone, apply_edge and the material samplers against
    the Python, on random inputs."""

    def setUp(self) -> None:
        self.t = _test_module()

    def test_gradient_mix(self) -> None:
        from tmiref import clip_gradient as G

        rng = np.random.default_rng(8)
        grid = np.linspace(0.0, 1.0, 16385)
        for k in range(400):
            stops = _random_stops(rng, int(rng.integers(1, 7)))
            mode, level = int(rng.integers(0, 5)), int(rng.integers(0, 6))
            if mode in (1, 2) and not self.t.fma_kernels():
                continue
            t = grid if k % 2 == 0 else rng.uniform(-0.2, 1.2, 4000)
            with np.errstate(all="ignore"):
                want = G.gradient_mix(stops, t, mode, level)
            got = np.array(self.t.gradient_mix(stops, t.tolist(), mode, level)).reshape(-1, 4)
            self.assertTrue(_same(want, got), (stops, mode, level))

    def test_linear_gradient(self) -> None:
        from tmiref import clip_gradient as G

        rng = np.random.default_rng(9)
        for _ in range(150):
            cw, ch = (int(v) for v in rng.integers(1, 90, 2))
            stops = _random_stops(rng, int(rng.integers(1, 5)))
            points = [float(v) for v in rng.uniform(-60, 150, 4)]
            if rng.random() < 0.1:
                points[2:] = points[:2]  # zero length
            if rng.random() < 0.2:
                points = [float(round(v)) for v in points]
            mode, level = int(rng.integers(0, 5)), int(rng.integers(0, 5))
            if mode in (1, 2) and not self.t.fma_kernels():
                continue
            x0, y0 = int(rng.integers(0, cw)), int(rng.integers(0, ch))
            region = [x0, y0, int(rng.integers(x0 + 1, cw + 1)), int(rng.integers(y0 + 1, ch + 1))]
            want = G.render_linear_gradient(stops, points, (cw, ch), mode, level, region=tuple(region))
            got = self.t.linear_gradient(stops, points, (cw, ch), mode, level, region)
            self.assertEqual(want.tobytes(), got, (stops, points, (cw, ch), mode, level, region))

    def _tones(self, rng, n: int):
        for _ in range(n):
            lpi = float(rng.choice([rng.uniform(3, 150), 10.0, 30.0, 60.0, 15.0, 25.4, 85.0]))
            angle = float(rng.choice([rng.integers(0, 360), 0, 45, 90, 30, 15, 135, 180, 400]))
            shape = str(rng.choice(["circle", "square", "line"]))
            dpi = float(rng.choice([72.0, 150.0, 300.0, 350.0, 600.0, 72.5]))
            yield {"lpi": lpi, "angle": angle, "shape": shape}, dpi

    def test_tone_screen_index(self) -> None:
        from tmiref import clip_tone as T

        rng = np.random.default_rng(10)
        checked = 0
        for tone, dpi in self._tones(rng, 250):
            x0, y0 = int(rng.integers(-40, 3000)), int(rng.integers(-40, 3000))
            w, h = int(rng.integers(1, 48)), int(rng.integers(1, 48))
            got = self.t.tone_index(tone["lpi"], tone["angle"], tone["shape"], dpi, x0, y0, w, h)
            try:
                scr = T.get_screen(tone, dpi)
                want = scr.index(x0, y0, w, h)
            except Exception:  # noqa: BLE001 - Python cannot draw it: Rust must decline
                self.assertIsNone(got, (tone, dpi))
                continue
            self.assertIsNotNone(got, (tone, dpi))
            checked += 1
            self.assertEqual(want.reshape(-1).tolist(), got[0], (tone, dpi, x0, y0))
            self.assertEqual(scr.N, got[1])
        self.assertGreater(checked, 200)

    def test_apply_tone(self) -> None:
        from tmiref import clip_tone as T

        rng = np.random.default_rng(12)
        for tone, dpi in self._tones(rng, 120):
            h, w = (int(v) for v in rng.integers(1, 50, 2))
            a = rng.integers(0, 256, (h, w, 4), dtype=np.uint8)
            a[..., 3] = np.where(rng.random((h, w)) < 0.2, 0, np.where(rng.random((h, w)) < 0.5, 255, a[..., 3]))
            x, y = int(rng.integers(0, 2000)), int(rng.integers(0, 2000))
            want = T.apply_tone(Image.fromarray(a, "RGBA"), x, y, tone, dpi).tobytes()
            got = self.t.apply_tone(a.tobytes(), w, x, y, tone["lpi"], tone["angle"], tone["shape"], dpi)
            self.assertEqual(want, got, (tone, dpi, x, y))

    def test_tone_unknown_u16_variant(self) -> None:
        # EffectTone u16 != 0 (layer_types MainId 17): whole-cell levels, pixel-corner sampling.
        from tmiref import clip_tone as T

        rng = np.random.default_rng(14)
        checked = 0
        for tone, dpi in self._tones(rng, 120):
            tone = {**tone, "unknown": {"u16": 2}}
            x0, y0 = int(rng.integers(-40, 3000)), int(rng.integers(-40, 3000))
            w, h = int(rng.integers(1, 48)), int(rng.integers(1, 48))
            got = self.t.tone_index(tone["lpi"], tone["angle"], tone["shape"], dpi, x0, y0, w, h, 2)
            try:
                scr = T.get_screen(tone, dpi)
                want = scr.index(x0, y0, w, h)
            except Exception:  # noqa: BLE001 - Python cannot draw it: Rust must decline
                self.assertIsNone(got, (tone, dpi))
                continue
            self.assertIsNotNone(got, (tone, dpi))
            checked += 1
            self.assertEqual(want.reshape(-1).tolist(), got[0], (tone, dpi, x0, y0))
            self.assertEqual(scr.N, got[1])
            a = rng.integers(0, 256, (h, w, 4), dtype=np.uint8)
            a[..., 3] = np.where(rng.random((h, w)) < 0.2, 0, 255)
            want_px = T.apply_tone(Image.fromarray(a, "RGBA"), x0, y0, tone, dpi).tobytes()
            got_px = self.t.apply_tone(a.tobytes(), w, x0, y0, tone["lpi"], tone["angle"], tone["shape"], dpi, 2)
            self.assertEqual(want_px, got_px, (tone, dpi, x0, y0))
        self.assertGreater(checked, 80)

    def test_apply_edge(self) -> None:
        from tmiref import clip as C
        from tmiref import clip_style

        rng = np.random.default_rng(13)
        checked = 0
        for _ in range(400):
            h, w = (int(v) for v in rng.integers(1, 30, 2))
            a = np.zeros((h, w, 4), np.uint8)
            a[..., :3] = rng.integers(0, 256, (h, w, 3))
            blob = rng.random((h, w)) < rng.uniform(0.05, 0.9)
            a[..., 3] = np.where(blob, rng.choice([255, 128, 1, 77], (h, w)), 0)
            cw, ch = (int(v) for v in rng.integers(1, 70, 2))
            x, y = int(rng.integers(-20, cw)), int(rng.integers(-20, ch))
            width = float(rng.choice([rng.uniform(0.05, 9.0), int(rng.integers(1, 8)), 1.0009, 2.001, 0.5, 3.0005]))
            rgb = tuple(int(v) for v in rng.integers(0, 256, 3))
            canvas = (0, 0, cw, ch)
            buf = C._LayerBuffer(Image.fromarray(a, "RGBA"), x, y)
            got = self.t.apply_edge(a.tobytes(), w, x, y, width, rgb, list(canvas))
            try:
                out = clip_style.apply_edge(buf, width, rgb, canvas)
            except Exception:  # noqa: BLE001
                self.assertIsNone(got, (h, w, x, y, width, canvas))
                continue
            self.assertIsNotNone(got, (h, w, x, y, width, canvas))
            checked += 1
            want = (out.image.tobytes(), out.x, out.y, out.image.width, out.image.height)
            self.assertEqual(want, tuple(got), (h, w, x, y, width, rgb, canvas))
        self.assertGreater(checked, 300)

    def _corners(self, rng, w: int, h: int):
        kind = rng.integers(0, 4)
        sx, sy = (float(v) for v in rng.choice([0.05, 0.2, 0.3, 0.5, 0.7, 1.0, 1.5, 2.5, 3.0, rng.uniform(0.03, 4)], 2))
        if rng.random() < 0.2:
            sx = -sx
        tl = rng.uniform(-30, 100, 2)
        if kind == 0:  # axis-aligned, fractional corners
            ex, ey = np.array([sx, 0.0]), np.array([0.0, sy])
        elif kind == 1:  # axis-aligned within the 1e-6 snap
            ex, ey = np.array([sx, 3e-7]), np.array([-2e-7, sy])
        else:
            th = np.radians(float(rng.choice([30.0, 90.0, 180.0, rng.uniform(0, 360)])))
            rot = np.array([[np.cos(th), -np.sin(th)], [np.sin(th), np.cos(th)]])
            ex, ey = rot @ np.array([sx, 0.0]), rot @ np.array([0.0, sy])
        if kind == 3:
            tl = np.round(tl * 2) / 2
        return [tl, tl + ex * w, tl + ey * h]

    def test_material_sampling(self) -> None:
        from tmiref import clip_style

        rng = np.random.default_rng(14)
        placed = 0
        for k in range(600):
            h, w = (int(v) for v in rng.integers(1, 40, 2))
            src = rng.integers(0, 256, (h, w, 4), dtype=np.uint8)
            src[..., 3] = np.where(rng.random((h, w)) < 0.3, 0, np.where(rng.random((h, w)) < 0.5, 255, src[..., 3]))
            corners = self._corners(rng, w, h)
            x0, y0 = int(rng.integers(-10, 100)), int(rng.integers(-10, 100))
            window = [x0, y0, x0 + int(rng.integers(1, 120)), y0 + int(rng.integers(1, 120))]
            tile = k % 3 == 0
            label = (h, w, [c.tolist() for c in corners], window, tile)
            try:
                got = self.t.material(src.tobytes(), w, [c.tolist() for c in corners], window, tile)
            except ValueError:
                got = "none"
            try:
                if tile:
                    tl, tr, bl = corners
                    ex, ey = (tr - tl) / w, (bl - tl) / h
                    mi = np.linalg.inv(np.array([[ex[0], ey[0]], [ex[1], ey[1]]]))
                    image = clip_style._sample_tiled(src, mi, tl, window)
                    want = (image.tobytes(), window[0], window[1], image.width, image.height)
                else:
                    out = clip_style.place_material(src, corners, window)
                    want = "none" if out is None else (out[0].tobytes(), out[1], out[2], out[0].width, out[0].height)
            except Exception:  # noqa: BLE001
                self.assertIsNone(got, label)
                continue
            self.assertIsNotNone(got, label)
            placed += want != "none"
            self.assertEqual(want, got if got == "none" else tuple(got), label)
        self.assertGreater(placed, 250)


if __name__ == "__main__":
    unittest.main()
