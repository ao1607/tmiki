"""CLIP STUDIO PAINT レイヤープロパティ「トーン」 (EffectTone): halftone screening.

Recovered only from saved files (example/csp515/tone_*.clip, CSP 5.1.5)
compared with their CanvasPreview; no CSP code was inspected.

EffectTone body (big-endian, 72 bytes; offsets from the end of the section name):
   0 u32  enabled
   4 f64  resolution the tone was set up at (72 on the 72-dpi probes; 300 / 600
          on miura's 600-dpi page) — which dpi defines the cell is untested
  12 u32  網点の種類: 1 円, 2 四角, 4 線 (others not probed)
  16 u32  ? (0 on the probes; 2 on layer_types / some miura layers)
  20 u32  ? (20 on the probes; 40 layer_types; 15 miura)
  24 f64  線数 (lpi)
  32 u32  ? (1 on the probes / layer_types, 0 on miura — all of whose tones are
          disabled)
  36 u32  ? (0)
  40 u32  角度 (degrees)
  44 u32  ? (30 on the probes and most miura layers; 10 on layer_types and
          the miura layers whose u16 = 2)
  48 ..   zeros (one miura layer: u32 0x303, 0xF8C at 52 / 56 — position?)

Screening — an ordered-dither threshold matrix (99.80-99.88 % of pixels
identical on all six probes; the rest are single threshold steps):
  * cell: P = dpi / lpi, clamped to P >= 2√2 (60 lpi @ 72 dpi renders exactly
    like 30 lpi); lattice v1 = round(P·(cos θ, sin θ)) (y down), v2 = v1
    rotated +90° = (-v1y, v1x). 45° / 10 lpi @ 72 dpi -> (5, 5) (horizontal
    period 10, not 10.18); 30 lpi -> 0°: (3, 0), 45°: (2, 2). Cells are the
    half-open parallelograms {i·v1 + j·v2 + (s, t)·V, 0 <= s, t < 1}, anchored
    at the canvas origin; pixels are sampled at their integer (corner)
    coordinates.
  * spot, u = (s, t) - 0.5: 円 cos 2πu1 + cos 2πu2 (its contours are diamonds
    in cell space, i.e. axis-aligned squares at 45°), 四角 -max(|u1|, |u2|),
    線 cos 2πu2 (lines along v1). Exact ties are split by evaluating at a
    centre nudged by a tiny pixel vector — (1, 0.8)·ε for rotated screens,
    (1, 1)·ε for axis-aligned ones — and then by cell raster order (t, s).
  * the cell's pixels are ranked r = 0 … area-1 by that order; cells carry an
    n×n Bayer index b = B[j mod n][i mod n] (n = smallest power of 2 with
    area·n² >= 128: 4 for 8- and 9-pixel cells, 2 for 50-pixel cells), and the
    threshold index is k = r·n² + b (N = area·n² levels).
  * coverage v = (255 - grey) / 255 of the layer's own 8-bit pixel (the
    gradient's ordered dither shows through); inked iff k < K with
    K = floor(N·v + 1 - n²/2), K = N at v = 1.
  * the clamped rotated screen (P < 2√2 at 45°) is the one exception found:
    it samples at pixel centres, with cell-edge pixels pushed along the nudge.
  * output is binary: inked pixels black, the rest transparent (unverified:
    the probes are opaque over white paper).

A second screen variant, fitted on layer_types MainId 17 against the full-size
CSP export (the one tone layer with EffectTone u16 = 2, u20 = 40, u44 = 10;
same 60 lpi / 45° / 72 dpi, flat grey 153 over the whole canvas; 100 % of the
pixels outside the 3D layer match). It differs from the probes' screen in two
ways: whole-cell levels (no Bayer refinement, n = 1: 3 of the 8 pixels of a
cell for grey 153, K = floor(8·0.4 + 0.5) = 3) and pixel-corner sampling even
though the period is clamped (the probes' clamped screen samples at pixel
centres). Which of the three differing fields (u16 / u20 / u44) selects it is
not known (no other sample has them); u16 is used here. The pattern is then a
4×4 tile, inked at (x mod 4, y mod 4) = (2,0) (3,0) (2,1) (0,2) (1,2) (0,3).
"""
from __future__ import annotations

import math
import struct
from typing import Any

import numpy as np
from PIL import Image

TAU = 2.0 * math.pi
SHAPES = {1: "circle", 2: "square", 4: "line"}
_EPS = (1e-7, 0.8e-7)  # tie-break nudge of the spot centre, pixel units
_EPS_AXIS = (1e-7, 1e-7)
_MIN_LEVELS = 128
_MIN_PERIOD = 2.0 * math.sqrt(2.0)


# ───────────────────────────── settings ─────────────────────────────

def _section(blob: bytes, name: str) -> int:
    key = struct.pack(">I", len(name)) + name.encode("utf-16-be")
    i = blob.find(key)
    return -1 if i < 0 else i + len(key)


def parse_tone_effect(style_or_blob: Any) -> dict | None:
    """EffectTone settings of an attached, enabled トーン, else None.

    Accepts a clip_style.layer_style() dict (needs LayerEffectAttached and
    LayerEffectInfo) or a raw LayerEffectInfo blob."""
    if isinstance(style_or_blob, dict):
        if not style_or_blob.get("LayerEffectAttached"):
            return None
        blob = style_or_blob.get("LayerEffectInfo")
    else:
        blob = style_or_blob
    if not blob:
        return None
    b = bytes(blob)
    p = _section(b, "EffectTone")
    if p < 0 or len(b) < p + 56:
        return None
    enabled = struct.unpack(">I", b[p:p + 4])[0]
    if not enabled:
        return None
    res = struct.unpack(">d", b[p + 4:p + 12])[0]
    shape_id, u16, u20 = struct.unpack(">3I", b[p + 12:p + 24])
    lpi = struct.unpack(">d", b[p + 24:p + 32])[0]
    u32_, u36, angle, u44 = struct.unpack(">4I", b[p + 32:p + 48])
    if not (lpi > 0 and math.isfinite(lpi)):
        return None
    return {
        "lpi": lpi,
        "angle": float(angle),
        "shape_id": shape_id,
        "shape": SHAPES.get(shape_id, "circle"),
        "resolution": res,
        "unknown": {"u16": u16, "u20": u20, "u32": u32_, "u36": u36, "u44": u44},
        "tail": b[p + 48:p + 68].hex(),
    }


# ───────────────────────────── threshold matrix ─────────────────────────────

def screen_vectors(lpi: float, angle_deg: float, dpi: float) -> tuple[tuple[int, int], tuple[int, int], bool]:
    """(v1, v2, clamped): integer cell vectors and whether the requested period
    fell under the minimum."""
    P0 = dpi / lpi
    P = max(P0, _MIN_PERIOD)
    th = math.radians(angle_deg)
    v1 = (int(math.floor(P * math.cos(th) + 0.5)), int(math.floor(P * math.sin(th) + 0.5)))
    if v1 == (0, 0):
        v1 = (1, 0)
    v2 = (-v1[1], v1[0])
    return v1, v2, P0 < _MIN_PERIOD


def _spot(u1: np.ndarray, u2: np.ndarray, shape: str) -> np.ndarray:
    if shape == "square":
        return -np.maximum(np.abs(u1), np.abs(u2))
    if shape == "line":
        return np.cos(TAU * u2)
    return np.cos(TAU * u1) + np.cos(TAU * u2)


def _bayer(n: int) -> np.ndarray:
    m = np.array([[0]], np.int64)
    while m.shape[0] < n:
        m = np.block([[4 * m, 4 * m + 2], [4 * m + 3, 4 * m + 1]])
    return m


class ToneScreen:
    def __init__(self, v1, v2, shape: str, half: bool = False, eps=_EPS, min_levels: int = _MIN_LEVELS):
        """``half``: sample at pixel centres instead of pixel corners.
        ``min_levels``: 1 gives whole-cell levels (no Bayer refinement)."""
        self.v1, self.v2, self.shape = v1, v2, shape
        self.half = 1 if half else 0
        self.eps = eps
        det = v1[0] * v2[1] - v1[1] * v2[0]
        self.det = det
        self.area = abs(det)
        n = 1
        while self.area * n * n < min_levels:
            n *= 2
        self.n = n
        self.N = self.area * n * n
        self.bayer = _bayer(n)
        self._build_template()

    def _cell_coords(self, x, y):
        """Cell indices and the integer numerators of the in-cell coordinates
        (s, t) = (sn, tn) / d, exact. Works on doubled coordinates so a
        half-pixel sampling offset stays integral."""
        v1, v2, det = self.v1, self.v2, self.det
        X = 2 * x + self.half
        Y = 2 * y + self.half
        sn = v2[1] * X - v2[0] * Y
        tn = -v1[1] * X + v1[0] * Y
        d = 2 * det
        if d < 0:
            sn, tn, d = -sn, -tn, -d
        if self.half:
            # The pixel-centre (minimum-size) screen also moves the sampling
            # point by the tie-break nudge, so pixels on a cell edge fall to
            # the side the nudge points at (fits tone_default / tone_lpi30).
            a, b = self.eps
            sg = 1 if det > 0 else -1
            ds = sg * (v2[1] * a - v2[0] * b)
            dt = sg * (-v1[1] * a + v1[0] * b)
            sn = sn * 2 - (1 if ds > 0 else -1 if ds < 0 else 0)
            tn = tn * 2 - (1 if dt > 0 else -1 if dt < 0 else 0)
            d = d * 2
            i = np.floor_divide(sn, d)
            j = np.floor_divide(tn, d)
            return i, j, (sn - i * d), (tn - j * d), d
        i = np.floor_divide(sn, d)
        j = np.floor_divide(tn, d)
        return i, j, sn - i * d, tn - j * d, d

    def _build_template(self):
        v1, v2 = self.v1, self.v2
        cx = [0, v1[0], v2[0], v1[0] + v2[0]]
        cy = [0, v1[1], v2[1], v1[1] + v2[1]]
        ys, xs = np.mgrid[min(cy) - 1:max(cy) + 2, min(cx) - 1:max(cx) + 2]
        i, j, sr, tr, d = self._cell_coords(xs, ys)
        m = (i == 0) & (j == 0)
        X, Y, S, T = xs[m], ys[m], sr[m], tr[m]
        assert X.size == self.area
        # spot evaluated at a centre nudged by a tiny pixel vector
        Vi = np.linalg.inv(np.array([[v1[0], v2[0]], [v1[1], v2[1]]], np.float64))
        e1 = Vi[0, 0] * self.eps[0] + Vi[0, 1] * self.eps[1]
        e2 = Vi[1, 0] * self.eps[0] + Vi[1, 1] * self.eps[1]
        u1 = S / d - 0.5 - e1
        u2 = T / d - 0.5 - e2
        s = _spot(u1, u2, self.shape)
        order = np.lexsort((S, T, -s))
        rank = np.empty(self.area, np.int64)
        rank[order] = np.arange(self.area)
        self.tx0, self.ty0 = int(X.min()), int(Y.min())
        lut = np.full((int(Y.max()) - self.ty0 + 1, int(X.max()) - self.tx0 + 1), -1, np.int64)
        lut[Y - self.ty0, X - self.tx0] = rank
        self.lut = lut

    def index(self, x0: int, y0: int, w: int, h: int) -> np.ndarray:
        """Threshold index k for the canvas pixels [x0, x0+w) × [y0, y0+h)."""
        ys, xs = np.mgrid[y0:y0 + h, x0:x0 + w]
        i, j, _, _, _ = self._cell_coords(xs.astype(np.int64), ys.astype(np.int64))
        rx = xs - (i * self.v1[0] + j * self.v2[0]) - self.tx0
        ry = ys - (i * self.v1[1] + j * self.v2[1]) - self.ty0
        r = self.lut[ry, rx]
        n = self.n
        return r * (n * n) + self.bayer[j % n, i % n]


_SCREENS: dict = {}


def get_screen(tone: dict, dpi: float) -> ToneScreen:
    v1, v2, clamped = screen_vectors(tone["lpi"], tone["angle"], dpi)
    axis = v1[0] == 0 or v1[1] == 0
    # Empirical (see module docstring): axis-aligned screens split ties
    # symmetrically, rotated ones lean on +x; the clamped (minimum-size)
    # rotated screen samples at pixel centres.
    eps = _EPS_AXIS if axis else _EPS
    half = clamped and not axis
    min_levels = _MIN_LEVELS
    if tone.get("unknown", {}).get("u16"):
        # layer_types MainId 17 (EffectTone u16 = 2): whole-cell levels, sampled
        # at pixel corners even though the screen is clamped (module docstring).
        half, min_levels = False, 1
    key = (v1, v2, tone["shape"], half, eps, min_levels)
    if key not in _SCREENS:
        _SCREENS[key] = ToneScreen(v1, v2, tone["shape"], half, eps, min_levels)
    return _SCREENS[key]


# ───────────────────────────── apply ─────────────────────────────

def ink_levels(grey: np.ndarray, N: int, n: int) -> np.ndarray:
    v = (255.0 - grey.astype(np.float64)) / 255.0
    K = np.floor(N * v + 1.0 - n * n / 2.0)
    K = np.where(v >= 1.0, N, K)
    return np.clip(K, 0, N).astype(np.int64)


def apply_tone(image_rgba: Image.Image, x: int, y: int, tone: dict, dpi: float) -> Image.Image:
    """Halftone a straight-RGBA layer buffer placed at canvas (x, y)."""
    a = np.asarray(image_rgba.convert("RGBA"))
    h, w = a.shape[:2]
    rgb = a[..., :3].astype(np.float64)
    alpha = a[..., 3].astype(np.float64)
    luma = rgb[..., 0] * 0.299 + rgb[..., 1] * 0.587 + rgb[..., 2] * 0.114
    # ink density: darkness × opacity (opacity handling unverified)
    dens = (255.0 - luma) * (alpha / 255.0)
    grey = np.rint(255.0 - dens)
    scr = get_screen(tone, dpi)
    k = scr.index(x, y, w, h)
    ink = (k < ink_levels(grey, scr.N, scr.n)) & (alpha > 0)
    out = np.zeros((h, w, 4), np.uint8)
    out[ink] = (0, 0, 0, 255)
    return Image.fromarray(out, "RGBA")
