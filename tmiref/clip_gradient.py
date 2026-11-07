"""Colour model of CLIP STUDIO PAINT gradient (グラデーション) fill layers.

Recovered from gradient layers saved by CSP 5.1.5 (example/csp515/grad_*.clip:
black→white in every 混色モード and 輝度の補正 level, plus red→white); the
rendering below matches each of them within 2/255.

Every segment between adjacent stops is eased with the same S-curve ``f`` on
its local parameter, and the two stop colours are mixed with weight ``f(t)``
in a space chosen by the GradationData header's mixing mode:

  0 通常                  8-bit sRGB values, per channel
  1 知覚的                PQ (SMPTE ST 2084) of a fitted cone (LMS) space on
                          linear sRGB, white scaled to a level-dependent
                          luminance; the result is clipped to the sRGB gamut
  2 知覚的(Photoshop互換)  OKLab
  3 リニア(Photoshop互換)  linear-light sRGB

Mode 1 is the best-fitting model, not a known CSP formula: the probes pin it
for greys at every level and for seven chromatic ramps at level 3. Alpha is mixed with the same
weight in every mode (assumed; no transparent-stop probe yet). CSP quantises
the result with a 4x4 ordered dither, reproduced by ``dither_to_u8``.
"""
from __future__ import annotations

import os

import numpy as np

# f(t) at k/64, measured from the 1200 px black→white probes and symmetrised
# (the raw curve sits ~2.5 px late at t = 0.5; whether that is intrinsic is
# unknown). Close analytic forms: t - 0.49·sin(2πt)/(2π) (0.9/255) or the
# symmetric cubic Bézier (0.474, 0.207, 0.526, 0.793) (0.66/255).
_EASE = np.array((
    0.00000, 0.00631, 0.01338, 0.02097, 0.02931, 0.03813, 0.04753, 0.05759,
    0.06825, 0.07948, 0.09116, 0.10374, 0.11671, 0.13015, 0.14431, 0.15895,
    0.17474, 0.19058, 0.20698, 0.22367, 0.24174, 0.26022, 0.27903, 0.29854,
    0.31867, 0.33921, 0.36035, 0.38184, 0.40465, 0.42790, 0.45124, 0.47527,
    0.50000, 0.52473, 0.54876, 0.57210, 0.59535, 0.61816, 0.63965, 0.66079,
    0.68133, 0.70146, 0.72097, 0.73978, 0.75826, 0.77633, 0.79302, 0.80942,
    0.82526, 0.84105, 0.85569, 0.86985, 0.88329, 0.89626, 0.90884, 0.92052,
    0.93175, 0.94241, 0.95247, 0.96187, 0.97069, 0.97903, 0.98662, 0.99369,
    1.00000,
))
_EASE_X = np.linspace(0.0, 1.0, len(_EASE))


def ease(t: np.ndarray) -> np.ndarray:
    """CSP's per-segment S-curve."""
    return np.interp(t, _EASE_X, _EASE)


def _to_lin(c: np.ndarray) -> np.ndarray:
    return np.where(c <= 0.04045, c / 12.92, ((np.maximum(c, 0) + 0.055) / 1.055) ** 2.4)


def _from_lin(c: np.ndarray) -> np.ndarray:
    return np.where(c <= 0.0031308, c * 12.92, 1.055 * np.maximum(c, 0) ** (1 / 2.4) - 0.055)


# SMPTE ST 2084 (PQ), argument in units of 10 000 cd/m², sign-preserving.
_C1, _C2, _C3 = 3424 / 4096, 2413 / 128, 2392 / 128
_M1, _M2 = 2610 / 16384, 2523 / 32


def _pq(y: np.ndarray) -> np.ndarray:
    a = np.abs(y) ** _M1
    return np.sign(y) * ((_C1 + _C2 * a) / (1 + _C3 * a)) ** _M2


def _pq_inv(e: np.ndarray) -> np.ndarray:
    p = np.abs(e) ** (1 / _M2)
    return np.sign(e) * (np.maximum(p - _C1, 0) / (_C2 - _C3 * p)) ** (1 / _M1)


# White luminance (cd/m²) for 輝度の補正 なし/低/中/高/最高. Levels 0, 1, 2, 4 are
# fitted to the grey probes; level 3 jointly with the chromatic probes.
_PQ_WHITE_NITS = (46533.8, 5822.3, 1124.1, 275.2, 84.67)

# Linear sRGB → cone-like LMS, rows normalised so white → (1, 1, 1). Fitted to
# seven mode-1 chromatic probes (red / blue / yellow / green / magenta → white,
# yellow → blue, red → black) plus grey; least squares started from CAT02, HPE,
# CIE 2006, OKLab and Smith–Pokorny all converge here. As XYZ→LMS it is a
# sharpened cone space near, but equal to none of, the published matrices.
_LMS = np.array([[0.56682, 0.42818, 0.00500], [0.06216, 0.84360, 0.09424], [0.00933, 0.07502, 0.91565]])
_LMS = _LMS / _LMS.sum(1, keepdims=True)
_LMS_INV = np.linalg.inv(_LMS)

_OK_M1 = np.array([
    [0.4122214708, 0.5363325363, 0.0514459929],
    [0.2119034982, 0.6806995451, 0.1073969566],
    [0.0883024619, 0.2817188376, 0.6299787005],
])
_OK_M2 = np.array([
    [0.2104542553, 0.7936177850, -0.0040720468],
    [1.9779984951, -2.4285922050, 0.4505937099],
    [0.0259040371, 0.7827717662, -0.8086757660],
])
_OK_M1_INV, _OK_M2_INV = np.linalg.inv(_OK_M1), np.linalg.inv(_OK_M2)


def _white_scale(level: int) -> float:
    return _PQ_WHITE_NITS[min(max(int(level), 0), 4)] / 10000.0


def _encode(rgb8: np.ndarray, mode: int, level: int) -> np.ndarray:
    """(..., 3) 0..255 sRGB -> mixing space."""
    c = rgb8 / 255.0
    if mode == 0:
        return c
    lin = _to_lin(c)
    if mode == 3:
        return lin
    if mode == 2:
        return np.cbrt(lin @ _OK_M1.T) @ _OK_M2.T
    return _pq(lin @ _LMS.T * _white_scale(level))


def _decode(x: np.ndarray, mode: int, level: int) -> np.ndarray:
    """Mixing space -> (..., 3) 0..255 sRGB float, gamut-clipped."""
    if mode == 0:
        out = x
    else:
        if mode == 3:
            lin = x
        elif mode == 2:
            lin = ((x @ _OK_M2_INV.T) ** 3) @ _OK_M1_INV.T
        else:
            lin = (_pq_inv(x) / _white_scale(level)) @ _LMS_INV.T
        out = _from_lin(np.clip(lin, 0.0, 1.0))
    return np.clip(out, 0.0, 1.0) * 255.0


def gradient_mix(stops, t: np.ndarray, mode: int = 0, level: int = 3) -> np.ndarray:
    """Straight-alpha RGBA (float, 0..255) of ``stops`` at positions ``t``.

    ``stops`` is a list of ``(pos 0..1, (r, g, b, a) 0..255)`` sorted by pos.
    """
    t = np.clip(np.asarray(t, np.float64), 0.0, 1.0)
    pos = np.array([p for p, _ in stops], np.float64)
    col = np.array([c for _, c in stops], np.float64).reshape(len(stops), 4)
    if len(stops) == 1:
        return np.broadcast_to(col[0], t.shape + (4,)).copy()
    if mode not in (0, 1, 2, 3):
        mode = 0
    enc = _encode(col[:, :3], mode, level)
    seg = np.clip(np.searchsorted(pos, t, side="right") - 1, 0, len(stops) - 2)
    p0, p1 = pos[seg], pos[seg + 1]
    span = p1 - p0
    local = np.where(span > 0, (t - p0) / np.where(span > 0, span, 1.0), 1.0)
    w = ease(np.clip(local, 0.0, 1.0))[..., None]
    out = np.empty(t.shape + (4,), np.float64)
    out[..., :3] = _decode(enc[seg] + (enc[seg + 1] - enc[seg]) * w, mode, level)
    out[..., 3] = col[seg, 3] + (col[seg + 1, 3] - col[seg, 3]) * w[..., 0]
    return out


# 4x4 Bayer matrix indexed [(H-1-y) % 4][x % 4] (CSP counts rows bottom-up).
_BAYER = np.array([[0, 8, 2, 10], [12, 4, 14, 6], [3, 11, 1, 9], [15, 7, 13, 5]], np.float64)


def dither_to_u8(values: np.ndarray, row0: int = 0, height: int | None = None, col0: int = 0) -> np.ndarray:
    """Quantise an (H, W, C) 0..255 float image with CSP's ordered dither.

    ``values`` may be the band of rows ``row0 ..`` (columns ``col0 ..``) of an
    image ``height`` tall (the dither phase is counted from the image's bottom
    row and left column)."""
    h, w = values.shape[:2]
    total = h if height is None else height
    ys = (total - 1 - (row0 + np.arange(h))) % 4
    xs = (col0 + np.arange(w)) % 4
    thr = (_BAYER[ys[:, None], xs[None, :]] + 0.5) / 16.0
    thr = thr.reshape(thr.shape + (1,) * (values.ndim - 2))
    return np.clip(np.floor(values + 1.0 - thr), 0, 255).astype(np.uint8)


def render_linear_gradient(
    stops,
    points,
    size: tuple[int, int],
    mode: int = 0,
    level: int = 3,
    region: tuple[int, int, int, int] | None = None,
) -> np.ndarray:
    """(H, W, 4) uint8 render of a linear gradient from ``points`` (x0, y0, x1, y1).

    ``region`` (x0, y0, x1, y1 within ``size``) renders only that window of the
    canvas; every pixel is computed from its absolute position, so the window
    holds the same bytes as the matching crop of the whole render."""
    cw, ch = size
    rx0, ry0, rx1, ry1 = (0, 0, cw, ch) if region is None else region
    w, h = rx1 - rx0, ry1 - ry0
    x0, y0, x1, y1 = points
    dx, dy = x1 - x0, y1 - y0
    length2 = dx * dx + dy * dy or 1.0
    xs = np.arange(rx0, rx1) + 0.5
    # Evaluate the colour model once on a dense 1-D grid, then interpolate
    # (error < 0.01/255 away from a hard stop).
    n = 16384
    lut = gradient_mix(stops, np.linspace(0.0, 1.0, n + 1), mode, level)
    out = np.empty((h, w, 4), np.uint8)

    def band(row0: int) -> None:
        # Every step is per pixel, so bands of rows give the same bytes as one
        # whole-canvas pass; numpy releases the GIL, so bands run in parallel.
        ys = np.arange(ry0 + row0, ry0 + min(h, row0 + _BAND_ROWS)) + 0.5
        t = np.clip(((xs[None, :] - x0) * dx + (ys[:, None] - y0) * dy) / length2, 0.0, 1.0)
        idx = t * n
        i0 = np.minimum(idx.astype(np.int64), n - 1)
        fr = (idx - i0)[..., None]
        out[row0:row0 + ys.size] = dither_to_u8(lut[i0] * (1.0 - fr) + lut[i0 + 1] * fr, ry0 + row0, ch, rx0)

    starts = range(0, h, _BAND_ROWS)
    if h * w < (1 << 20) or len(starts) == 1:
        for row0 in starts:
            band(row0)
    else:
        from concurrent.futures import ThreadPoolExecutor

        with ThreadPoolExecutor(max_workers=min(8, os.cpu_count() or 1)) as pool:
            list(pool.map(band, starts))
    return out


_BAND_ROWS = 128
