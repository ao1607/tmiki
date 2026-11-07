"""Vector (ベクター) layer rasterisation for tmi's CLIP compositor.

CSP saves no render pixels for a vector layer (its Offscreen row has no Exta
chunk), so the layer is re-stroked here from its vector data. Everything below
was recovered by black-box comparison of saved files with CanvasPreview.

Storage
-------
``Layer.VectorNormalStrokeIndex`` -> ``VectorObjectList.MainId`` ->
``VectorData`` (40-byte ``extrnlid…`` key) -> Exta inner stream (big-endian)
= strokes back to back:

  stroke header (``hsize`` = 88 / 92 / 96; read the tail relative to hsize)
    +0x00 u32 hsize   +0x04 u32 72|76   +0x08 u32 psize (88)   +0x0c u32 psize
    +0x10 u32 n_points  +0x14 u32 flags (0x2081 pen stroke, 0x11 集中線/流線,
          0x3081, 0x181, 0x81 seen)
    +0x18 i32[4] bbox (loose: padded by the brush's maximum radius)
    +0x28 u32[3] main RGB (0..0xFFFFFFFF)   +0x34 u32[3] sub RGB
    +0x40 f64 opacity
    (hsize 92 has an extra u32 0 here)
    hsize-16 u32 BrushStyle.MainId     hsize-12 f64 brush size (px)
    hsize-4  u32 stroke id / random seed
  n_points x point (88 bytes)
    +0x00 f64 x, f64 y  (canvas px, before LayerOffset)
    +0x10 i32[4] bbox of the stroked span to the next point (loose)
    +0x20 u32 point flags: 0x1 = corner (spline tangent breaks here);
          0x1000 with f5 != 1 (線幅修正), 0x201 on 集中線, 0x10/0x20/0x43 rare
    +0x24 f32 x7: f0 pen pressure, f1 tilt(?), f2 speed(?), f3 rotation (deg),
          f4 taper (入り抜き; per-point, only 集中線/流線 use it), f5 width
          multiplier (線幅修正), f6 = 1
    +0x40 u32 x3 = 0 (no Bézier handles)
    +0x4c f32 dab phase: arc length from this point to the next brush dab.
          It equals (-arc length from the stroke start) mod (0.16 * dab size)
          to 0.01 px on the probe strokes, so CSP stamps dabs every
          DAB_SPACING x size along the curve, starting at the first point.
    +0x50 u32 seed (= the header's)   +0x54 u32 (0 on the probes)

線幅修正 (probe_vector_mouse vs _senhaba, same strokes edited in CSP):
指定倍に拡大 x1.5 rewrites the header size (10 -> 15) and the bboxes; 指定幅で
太らせる 2 px leaves the size, sets point flag 0x1000 on every point and
f5 = 1 + 1/size (20 -> 21, i.e. +1 px of size = +2 px of width). Both
recompute the dab phase. So f5 is a per-point size multiplier on top of
pressure, and size is the brush size (radius-like: UI size 40 is stored as 20).

Geometry and width (measured)
-----------------------------
The probes are pen strokes (header flag 0x2000). The model below applies to
those only; 集中線 / 流線 (0x11) and the other kinds keep the earlier model
(uniform Catmull-Rom, attributes interpolated in the spline parameter,
diameter = 1.77 * dab size, no end trim or edge blur), because the new one
made miura's 集中線 layers worse and no probe covers them yet.

* The centre line is a *centripetal Catmull-Rom spline* through the points
  (knot spacing sqrt(chord)); a duplicated end point / corner point gives a
  zero tangent there. On the mouse zigzag it fits the preview better than
  uniform / chordal / B-spline / polyline (coverage error 0.40 vs 0.65 / 0.42
  / 0.58 / 0.50), it does not overshoot when the last two points are close
  (uniform did, by 13 px), and the dab phases follow its arc length best.
  vector_simple is equally good with either.
* Body diameter = max(1.98 * w - 1, 0), w = size * S(p) * thickness * f4 *
  f5 (the dab size). Measured on probe_vector_mouse (mouse = constant
  pressure): sizes 10 / 20 / 40 are 18.76 / 38.48 / 78.17 px wide, and
  vector_simple's size 5 is 8.85 px (the old single factor 1.77 fitted only
  that one). CSP really stamps soft dabs (see the phase above), so this is
  the swept width of the dabs, not their radius.
  On a mono layer the radius never drops below min(HAIR_K * w, HAIR_R): on
  page0001 a w = 0.5 stroke is still a solid 1 px line (the dabs are dense
  enough to saturate), and only w < ~0.3 turns dotted and vanishes. The
  antialiased probes do not want the floor (probe_vector_thin's size-1
  stroke got 0.3 -> 0.4 worse with it), so it is applied to mono only.
* A stroke whose points carry flag 0x1000 (線幅修正) takes its size factor
  from f5 alone: the pressure curve S(p) is not applied again (applying
  both made lolita's lines vanish), and the thickness t is applied on
  top of f5 once more: w = size * f5 * t * t. The probe (t = 1, p = 1, f5
  = 1.05) is exact either way; page0001's 線幅修正 layers (iso85 / iso88 /
  iso182, t = 0.85, f5 ≈ 0.85..1.0 x S(p)) are 0.8 x what size * f5 * t
  gives (fitted per stroke: 0.7 / 0.8 / 0.9 / 1.0 -> 10.4k / 9.3k / 10.9k /
  15.2k error pixels on iso85), i.e. t again within the fit's resolution.
  Whether 線幅修正 bakes the thickness into f5 or CSP double-applies it
  is not known.
* Pressure curve S(p): the graph's control points are the control polygon
  of a clamped uniform quadratic B-spline (a 3-point graph = the quadratic
  Bézier with the middle point as handle), NOT points on the curve. With
  the default pen curve (0,0) (0.745,0.2545) (1,1) the piecewise-linear
  reading drew page0001's pressure strokes 1.3-1.6 x too thin; under the
  Bézier the best per-stroke width scale is 1.00 +- 0.05 from p = 0.1 to
  0.45 on four layers (see fable_vec notes, 2026-10). Graphs with > 3
  points are read as the same B-spline (untested: no such pen brush in
  the corpus).
* Thickness: a brush with ThicknessBase t < 1 draws t times as wide (the
  tip's short axis lies across the stroke: RotationEffector 0x80 = 向き
  follows the direction of travel), not (1 + t) / 2 as fitted before with
  the wrong pressure curve. 1.98 * 0.85 = 1.68 matches the mono fit
  (D = 1.6..1.7 w' - 1.1 with w' = size * S(p)).
* Mono layers (LayerColorTypeIndex 2, drawn without antialiasing and
  thresholded by clip_style): the ink lies (-0.5, +0.5) px from the
  stored coordinates. Fitted from the centroid of ~1600 cross sections on
  iso61 / iso85 / iso88 (shift (-0.49,+0.47), (-0.56,+0.48), (-0.54,+0.46);
  residual sd 0.6 -> 0.4 px); the antialiased probes want no shift
  (probe_vector_mouse 0.32 -> 0.96 with it), so it is CSP's binary
  rasteriser sampling the pixel grid at a corner, applied here to mono
  layers only.
* Caps are round. The end cap is cut back to the last dab: the tail is
  shortened by spacing - phase(last point) (about 1 px on the probes; the
  start cap sits on the first point). Both caps are still ~0.3 px smaller
  than half the body width, which the model does not reproduce.
* Edges: the box-filtered coverage is softened with a [0.08, 0.84, 0.08]
  blur in x and y (the preview's edge ramps span ~1.4 px).
  - S(p) is the brush's 筆圧 size curve: ``BrushStyle.SizeEffector`` = u32
    flags; when bit 0x10 is set the next words are f32 minimum and u32
    ``BrushEffectorGraphData.MainId``; that graph's (x, y) f64 pairs are the
    B-spline control polygon above, S = min + (1 - min) * curve(p). Without
    bit 0x10 the size does not follow pressure (集中線 brushes). The other
    effector bits are not applied: the default pen's blob is 0x131 with a
    second (1.0, graph 16) pair for 0x20 (速度, a decreasing curve) and 0.8
    / 1.5 after it; reading graph 16 as a size factor of the stored speed
    f2 made page0001 worse (iso61 68k -> 117k error pixels), and the
    per-stroke width fit shows no speed dependence.
  - thickness: brushes with ``ThicknessBase`` t < 1 (flattened tip) draw
    t times as wide (see above).
  - S(p) is not pinned by a probe (mouse probes have p = 1). In the real
    files f5 != 1 occurs only with point flag 0x1000 (see the rule above).
* ``BrushStyle.CompositeMode`` 27 = 透明色: the stroke erases what earlier
  strokes of the same layer drew.
* Strokes are composited in file order with "over", in the stroke's main
  colour times its opacity. The layer's expression colour / layer colour is
  applied afterwards by clip_style (treat the result as colour data).
"""
from __future__ import annotations

import math
import struct
from typing import Any

import numpy as np
from PIL import Image, ImageChops, ImageDraw

from tmiref import clip as C
from tmiref.clip_frame import _vector_inner

WIDTH_K = 1.98        # diameter = WIDTH_K * dab size + WIDTH_B, see module doc
WIDTH_B = -1.0
HAIR_K = 1.18         # hairlines: the radius never drops below min(HAIR_K * w, HAIR_R)
HAIR_R = 0.8          # (page0001 mono layers: a w = 0.5 stroke is still a solid 1 px line)
LEGACY_K = 1.77       # diameter / dab size for strokes that are not pen strokes
PEN_FLAG = 0x2000     # stroke header flag of pen-tool strokes (0x2081 / 0x3081)
EDGE_BLUR = 0.08      # [k, 1-2k, k] blur of each stroke's coverage, both axes
DAB_SPACING = 0.16    # brush dab interval / dab size (from the +0x4c dab phase)
ERASE_MODE = 27       # BrushStyle.CompositeMode of 透明色 strokes
CORNER_FLAG = 0x1
WIDTH_FIXED_FLAG = 0x1000  # 線幅修正 point: f5 is the whole size factor (times thickness)
MONO_COLOR_TYPE = 2   # LayerColorTypeIndex of a モノクロ layer
MONO_SHIFT = (-0.5, 0.5)  # a mono layer's strokes land this much off the stored coordinates
CURVE_SAMPLES = 256   # resolution of the evaluated pressure curve
SS = 4               # base supersampling of the per-stroke raster (x2 up to 16 for hairlines)
TOL = 0.05            # max spline-to-chord deviation (px) per sub-segment
MAX_STEP = 0.75       # sub-segment cap where the radius changes (px of radius)
JOIN_GAP = 0.09       # draw a round join where quads would leave a gap (output px)


# ───────────────────────────── decoding ─────────────────────────────


def parse_strokes(inner: bytes) -> list[dict]:
    """Decode a vector layer's stroke stream."""
    out = []
    p = 0
    n_inner = len(inner)
    while p + 24 <= n_inner:
        hsize, _f4, psize, _ps2, count, flags = struct.unpack_from(">IIIIII", inner, p)
        if hsize < 0x58 or psize < 0x40 or count == 0 or p + hsize + count * psize > n_inner:
            break
        main = struct.unpack_from(">3I", inner, p + 0x28)
        opacity = struct.unpack_from(">d", inner, p + 0x40)[0]
        brush = struct.unpack_from(">I", inner, p + hsize - 16)[0]
        size = struct.unpack_from(">d", inner, p + hsize - 12)[0]
        base = p + hsize
        raw = np.frombuffer(inner, dtype=np.uint8, count=count * psize, offset=base).reshape(count, psize)
        out.append({
            "flags": flags,
            "brush": brush,
            "size": size if math.isfinite(size) and size > 0 else 0.0,
            "color": tuple(min(255, (c * 255 + 0x7FFFFFFF) // 0xFFFFFFFF) for c in main),
            "opacity": max(0.0, min(1.0, opacity)) if math.isfinite(opacity) else 1.0,
            "xy": raw[:, 0:16].copy().view(">f8").astype(np.float64),
            "pflags": raw[:, 32:36].copy().view(">u4").astype(np.int64)[:, 0],
            "f": raw[:, 36:64].copy().view(">f4").astype(np.float64),
            "phase": raw[:, 76:80].copy().view(">f4").astype(np.float64)[:, 0] if psize >= 80 else None,
        })
        p = base + count * psize
    return out


def layer_strokes(clip: Any, node: Any) -> list[dict]:
    """Strokes of a vector layer (empty for any other layer). Cached per clip."""
    cache = clip.frame_cache
    key = ("vstrokes", node.main_id)
    if key in cache:
        return cache[key]
    strokes: list[dict] = []
    try:
        row = clip.db.execute(
            "SELECT VectorNormalStrokeIndex FROM Layer WHERE MainId = ?", (node.main_id,)
        ).fetchone()
        if row and row[0]:
            v = clip.db.execute(
                "SELECT VectorData FROM VectorObjectList WHERE MainId = ?", (row[0],)
            ).fetchone()
            if v and v[0] is not None:
                inner = _vector_inner(clip, v[0])
                if inner:
                    strokes = parse_strokes(inner)
    except Exception:
        strokes = []
    cache[key] = strokes
    return strokes


def has_vector_strokes(clip: Any, node: Any) -> bool:
    return bool(layer_strokes(clip, node))


# ───────────────────────────── brush model ─────────────────────────────


def _brush(clip: Any, brush_id: int) -> dict:
    """Width-relevant BrushStyle fields: pressure curve, thickness, composite."""
    cache = clip.frame_cache
    key = ("vbrush", brush_id)
    if key in cache:
        return cache[key]
    info = {"pressure": "linear", "thick": 1.0, "composite": 0}
    try:
        row = clip.db.execute(
            "SELECT SizeEffector, ThicknessBase, CompositeMode FROM BrushStyle WHERE MainId = ?",
            (brush_id,),
        ).fetchone()
    except Exception:
        row = None
    if row:
        eff, thick, comp = row
        info["thick"] = float(thick) if thick is not None else 1.0
        info["composite"] = comp or 0
        if eff is not None and len(bytes(eff)) >= 4:
            b = bytes(eff)
            flags = struct.unpack(">I", b[:4])[0]
            if flags & 0x10 and len(b) >= 12:
                mn = struct.unpack(">f", b[4:8])[0]
                gid = struct.unpack(">I", b[8:12])[0]
                info["pressure"] = (mn if math.isfinite(mn) else 0.0, _graph(clip, gid))
            else:
                info["pressure"] = None  # size does not follow pressure
    cache[key] = info
    return info


def _graph(clip: Any, gid: int):
    if not gid:
        return None
    try:
        row = clip.db.execute(
            "SELECT ControlNumber, ControlPoints FROM BrushEffectorGraphData WHERE MainId = ?", (gid,)
        ).fetchone()
    except Exception:
        return None
    if not row or not row[0] or row[1] is None:
        return None
    n = int(row[0])
    v = struct.unpack(f">{n * 2}d", bytes(row[1])[:n * 16])
    x, y = np.array(v[0::2]), np.array(v[1::2])
    order = np.argsort(x, kind="stable")
    return _curve(x[order], y[order])


def _curve(x: np.ndarray, y: np.ndarray):
    """The effector graph's curve, sampled as (x, y) arrays for ``np.interp``.

    The stored control points are the control polygon of a clamped uniform
    quadratic B-spline, not points on the curve: a three-point graph is the
    quadratic Bézier whose middle point is the handle. (Two points: the
    straight line.) Fitted on page0001's pen strokes, see module doc."""
    n = len(x)
    if n < 3:
        return x, y
    # clamped uniform quadratic B-spline: knots 0,0,0,1,2,..,n-2,n-2,n-2
    k = n - 2  # number of spans
    px, py = [], []
    for s in range(k):
        # control points of span s: P[s], P[s+1], P[s+2] with the end
        # spans clamped (first / last control point on the curve)
        a = np.array([x[s], y[s]]); b = np.array([x[s + 1], y[s + 1]]); c = np.array([x[s + 2], y[s + 2]])
        start = a if s == 0 else (a + b) / 2.0
        end = c if s == k - 1 else (b + c) / 2.0
        t = np.linspace(0.0, 1.0, CURVE_SAMPLES, endpoint=(s == k - 1))[:, None]
        q = (1 - t) ** 2 * start + 2 * (1 - t) * t * b + t * t * end
        px.append(q[:, 0]); py.append(q[:, 1])
    return np.concatenate(px), np.concatenate(py)


def _dab_size(clip: Any, s: dict) -> np.ndarray:
    """Per-point brush size (px): the stored size scaled by the pressure
    curve, tip thickness, taper (f4) and 線幅修正 (f5). A point flagged
    WIDTH_FIXED_FLAG takes f5 instead of the pressure curve."""
    f = s["f"]
    b = _brush(clip, s["brush"])
    p = np.clip(f[:, 0], 0.0, 1.0)
    pr = b["pressure"]
    if pr is None:
        k = np.ones_like(p)
    elif pr == "linear":
        k = p
    else:
        mn, g = pr
        gv = np.interp(p, g[0], g[1]) if g is not None else p
        k = mn + (1.0 - mn) * gv
    t = b["thick"] if 0.0 < b["thick"] < 1.0 else 1.0
    # 線幅修正 points: f5 carries the size factor, and the thickness is in it
    # too (it is applied once more below), see module doc
    k = np.where((s["pflags"] & WIDTH_FIXED_FLAG) != 0, t, k)
    return s["size"] * k * t * f[:, 4] * f[:, 5]


def _radius(w: np.ndarray, pen=True, hair=False) -> np.ndarray:
    """Half width (px) of the stroke body for dab size ``w``: the probe-fitted
    pen width, or the older single factor for other stroke kinds. With
    ``hair`` (mono layers) a pen stroke is never thinner than the hairline
    floor."""
    body = 0.5 * np.maximum(WIDTH_K * w + WIDTH_B, 0.0)
    if hair:
        body = np.maximum(body, np.minimum(HAIR_K * w, HAIR_R))
    return np.where(pen, body, 0.5 * LEGACY_K * w)


def _is_mono(clip: Any, node: Any) -> bool:
    """Whether the layer's 表現色 is モノクロ (its strokes are then drawn without
    antialiasing, half a pixel off: see MONO_SHIFT in the module doc)."""
    try:
        from tmi.formats.clip_style import layer_style

        return (layer_style(clip, node.main_id).get("LayerColorTypeIndex") or 0) == MONO_COLOR_TYPE
    except Exception:
        return False


def is_pen_stroke(s: dict) -> bool:
    """Strokes drawn with a pen tool (header flag 0x2000); 集中線 / 流線
    (0x11) and the other generated kinds keep the pre-probe model."""
    return bool(s["flags"] & PEN_FLAG)


# ───────────────────────────── geometry ─────────────────────────────


def _centripetal_tangents(p0, p1, p2, p3):
    """Hermite tangents (per unit segment parameter) of the centripetal
    Catmull-Rom segment p1 -> p2 (knot spacing sqrt of the chord length).
    A duplicated end / corner point (p0 == p1 or p3 == p2) gives a zero
    tangent there."""
    d0 = np.sqrt(np.hypot(*(p1 - p0).T))[:, None]
    d1 = np.sqrt(np.hypot(*(p2 - p1).T))[:, None]
    d2 = np.sqrt(np.hypot(*(p3 - p2).T))[:, None]
    eps = 1e-9
    s1 = np.maximum(d1, eps)
    # m1 = d1 * [(p1-p0)/d0 - (p2-p0)/(d0+d1) + (p2-p1)/d1]; 0 when d0 = 0
    m1 = np.where(d0 > eps, d1 * ((p1 - p0) / np.maximum(d0, eps) - (p2 - p0) / np.maximum(d0 + d1, eps)) + (p2 - p1) * (d1 / s1), 0.0)
    m2 = np.where(d2 > eps, d1 * ((p3 - p2) / np.maximum(d2, eps) - (p3 - p1) / np.maximum(d1 + d2, eps)) + (p2 - p1) * (d1 / s1), 0.0)
    return m1, m2


def _layer_samples(clip: Any, strokes: list[dict], ox: float, oy: float, hair: bool = False):
    """Dense samples of every stroke's spline, all strokes at once.

    Returns (xy (M,2), r (M,), stroke index (M,), knot (M,) bool)."""
    sizes = np.array([len(s["xy"]) for s in strokes])
    P = np.concatenate([s["xy"] for s in strokes]) + np.array([ox, oy])
    Wd = np.concatenate([_dab_size(clip, s) for s in strokes])
    corner = np.concatenate([(s["pflags"] & CORNER_FLAG) != 0 for s in strokes])
    pen = np.array([is_pen_stroke(s) for s in strokes])
    sid_pt = np.repeat(np.arange(len(strokes)), sizes)
    R = _radius(Wd, pen[sid_pt], hair)
    start = np.cumsum(sizes) - sizes
    end = start + sizes - 1  # last point index of each stroke
    # segments: every point that is not its stroke's last point
    i = np.nonzero(np.arange(len(P)) != end[sid_pt])[0]
    sid = sid_pt[i]
    im1 = np.maximum(i - 1, start[sid])
    ip2 = np.minimum(i + 2, end[sid])
    im1 = np.where(corner[i], i, im1)
    ip2 = np.where(corner[i + 1], i + 1, ip2)
    p0, p1, p2, p3 = P[im1], P[i], P[i + 1], P[ip2]
    m1, m2 = _centripetal_tangents(p0, p1, p2, p3)
    # other strokes: uniform Catmull-Rom (the pre-probe model)
    old = ~pen[sid]
    m1[old] = 0.5 * (p2[old] - p0[old])
    m2[old] = 0.5 * (p3[old] - p1[old])
    c = p2 - p1
    chord = np.hypot(*c.T)
    dev = (np.hypot(*(m1 - c).T) + np.hypot(*(m2 - c).T)) / 8.0
    m = np.ceil(np.sqrt(dev / TOL))
    m = np.maximum(m, np.ceil(np.abs(R[i + 1] - R[i]) / MAX_STEP))
    m = np.clip(m, 1, np.maximum(1, np.ceil(chord / 0.5))).astype(np.int64)
    seg = np.repeat(np.arange(len(i)), m)
    first = np.cumsum(m) - m
    t = ((np.arange(m.sum()) - first[seg]) / m[seg])[:, None]
    t2, t3 = t * t, t * t * t
    xy = ((2 * t3 - 3 * t2 + 1) * p1[seg] + (t3 - 2 * t2 + t) * m1[seg]
          + (-2 * t3 + 3 * t2) * p2[seg] + (t3 - t2) * m2[seg])
    a = i[seg]
    # A pen stroke's dab size runs linearly in arc length between points
    # (the centripetal parameter slows down at a zero end tangent); other
    # strokes keep interpolating in the spline parameter, which fits the
    # tapers of miura's 集中線 better.
    nxt = np.empty_like(xy)
    nxt[:-1] = xy[1:]
    last = np.ones(len(seg), bool)
    last[:-1] = seg[1:] != seg[:-1]
    nxt[last] = p2[seg[last]]
    step = np.hypot(*(nxt - xy).T)
    cum = np.cumsum(step)
    seg_end = cum[last]
    seg_start = seg_end - np.add.reduceat(step, first)
    u = (cum - step - seg_start[seg]) / np.maximum(seg_end - seg_start, 1e-12)[seg]
    pen_s = pen[sid[seg]]
    u = np.where((seg_end[seg] - seg_start[seg] > 1e-12) & pen_s, u, t[:, 0])
    r = _radius(Wd[a] * (1 - u) + Wd[a + 1] * u, pen_s, hair)
    knot = t[:, 0] == 0
    sid_s = sid[seg]
    # append each stroke's last point after its samples
    xy = np.concatenate([xy, P[end]])
    r = np.concatenate([r, R[end]])
    sid_s = np.concatenate([sid_s, np.arange(len(strokes))])
    knot = np.concatenate([knot, np.ones(len(strokes), bool)])
    pos = np.concatenate([np.arange(len(seg)), np.full(len(strokes), 1 << 62)])
    order = np.lexsort((pos, sid_s))
    return xy[order], r[order], sid_s[order], knot[order]


def _trim_end(clip: Any, s: dict, xy: np.ndarray, r: np.ndarray):
    """Cut the stroke's tail back to its last brush dab (see module doc: dabs
    sit every DAB_SPACING * dab size along the curve, the point field at
    +0x4c is the distance to the next one, so the last dab lies
    spacing - phase before the final point)."""
    ph = s.get("phase")
    if ph is None or len(xy) < 2:
        return xy, r
    spacing = DAB_SPACING * float(_dab_size(clip, s)[-1])
    recess = spacing - float(ph[-1])
    if not (0.0 < recess < spacing):
        return xy, r
    seg = np.hypot(*np.diff(xy, axis=0).T)
    cum = np.concatenate([[0.0], np.cumsum(seg)])
    stop = cum[-1] - recess
    if stop <= 0.0:
        return xy, r
    j = int(np.searchsorted(cum, stop, side="right"))  # first sample beyond the cut
    t = (stop - cum[j - 1]) / max(seg[j - 1], 1e-12)
    end_xy = xy[j - 1] + t * (xy[j] - xy[j - 1])
    end_r = r[j - 1] + t * (r[j] - r[j - 1])
    return np.vstack([xy[:j], end_xy]), np.concatenate([r[:j], [end_r]])


def _stroke_bounds(xy: np.ndarray, r: np.ndarray, bounds):
    """The rectangle ``_draw_stroke`` draws the stroke over, or None when it
    draws nothing."""
    if len(xy) == 0 or not (r > 0.02).any():
        return None
    rmax = float(r.max())
    return C._intersect_bounds((
        math.floor(xy[:, 0].min() - rmax - 1), math.floor(xy[:, 1].min() - rmax - 1),
        math.ceil(xy[:, 0].max() + rmax + 1), math.ceil(xy[:, 1].max() + rmax + 1),
    ), bounds)


def _draw_stroke(xy: np.ndarray, r: np.ndarray, bounds, blur: float = 0.0):
    """Anti-aliased coverage of one round-capped variable-width stroke.
    Returns (L image, x0, y0) or None."""
    b = _stroke_bounds(xy, r, bounds)
    if b is None:
        return None
    rmax = float(r.max())
    x0, y0, x1, y1 = b
    # thin strokes need finer supersampling or sliver quads drop out
    ss = SS
    while ss < 16 and rmax * ss < 4 and (x1 - x0) * (y1 - y0) * (2 * ss) ** 2 <= 1 << 26:
        ss *= 2
    img = Image.new("L", ((x1 - x0) * ss, (y1 - y0) * ss), 0)
    d = ImageDraw.Draw(img)
    P = (xy - np.array([x0, y0])) * ss
    R = r * ss
    n = len(P)
    dP = np.diff(P, axis=0)
    L = np.hypot(dP[:, 0], dP[:, 1])
    nz = L > 1e-9
    safe = np.where(nz, L, 1.0)
    nx = np.where(nz, -dP[:, 1] / safe, 0.0)
    ny = np.where(nz, dP[:, 0] / safe, 0.0)
    ra, rb = R[:-1], R[1:]
    quads = np.stack([
        P[:-1, 0] + nx * ra, P[:-1, 1] + ny * ra, P[1:, 0] + nx * rb, P[1:, 1] + ny * rb,
        P[1:, 0] - nx * rb, P[1:, 1] - ny * rb, P[:-1, 0] - nx * ra, P[:-1, 1] - ny * ra,
    ], axis=1)
    draw_q = nz & ((ra > 0.1) | (rb > 0.1))
    for q in quads[draw_q].tolist():
        d.polygon(q, fill=255)
    # round caps and joins, only where the quads leave a gap
    need = np.zeros(n, dtype=bool)
    need[0] = need[-1] = True
    if n > 2:
        cosang = nx[:-1] * nx[1:] + ny[:-1] * ny[1:]
        gap = R[1:-1] * (1 - np.sqrt(np.clip((1 + cosang) / 2, 0, 1)))
        need[1:-1] |= (gap > JOIN_GAP * ss) | ~nz[:-1] | ~nz[1:]
    steep = np.abs(np.diff(R)) > 0.3 * L
    need[:-1] |= steep
    need[1:] |= steep
    for cx, cy, rk in np.column_stack([P, R])[need & (R > 0.5)].tolist():
        d.ellipse((cx - rk, cy - rk, cx + rk, cy + rk), fill=255)
    if ss > 1:
        img = img.reduce(ss)
    if blur > 0:
        img = _soften(img, blur)
    return img, x0, y0


def _soften(img: Image.Image, k: float) -> Image.Image:
    """Separable [k, 1 - 2k, k] blur (the bounds carry a 1 px margin)."""
    a = np.asarray(img, dtype=np.float32)
    b = a * (1.0 - 2.0 * k)
    b[1:] += k * a[:-1]
    b[:-1] += k * a[1:]
    c = b * (1.0 - 2.0 * k)
    c[:, 1:] += k * b[:, :-1]
    c[:, :-1] += k * b[:, 1:]
    return Image.fromarray(np.clip(np.rint(c), 0, 255).astype(np.uint8), "L")


# ───────────────────────────── public API ─────────────────────────────


def vector_plan(clip: Any, node: Any, canvas_bounds) -> list:
    """The strokes a vector layer draws, in order, each as ``(stroke, xy, r,
    blur, rectangle)``: its samples, ready for ``_draw_stroke``, and the
    rectangle that draws over (``_stroke_bounds``). Strokes that draw
    nothing are left out. Computed once per render (``render_session``)."""
    return C._memoized(clip, ("vector-plan", node.main_id, tuple(canvas_bounds)),
                       lambda: _vector_plan(clip, node, canvas_bounds))


def _vector_plan(clip: Any, node: Any, canvas_bounds) -> list:
    strokes = [s for s in layer_strokes(clip, node) if s["size"] > 0 and len(s["xy"])]
    if not strokes:
        return []
    ox = getattr(node, "offset_x", 0) or 0
    oy = getattr(node, "offset_y", 0) or 0
    mono = _is_mono(clip, node)
    xy, r, sid, _knot = _layer_samples(clip, strokes, ox, oy, hair=mono)
    if mono:
        xy = xy + np.array(MONO_SHIFT)
    cuts = np.searchsorted(sid, np.arange(len(strokes) + 1))
    plan = []
    for k, s in enumerate(strokes):
        a, b = cuts[k], cuts[k + 1]
        if is_pen_stroke(s):
            sxy, sr = _trim_end(clip, s, xy[a:b], r[a:b])
            blur = EDGE_BLUR
        else:
            sxy, sr = xy[a:b], r[a:b]
            blur = 0.0
        reach = _stroke_bounds(sxy, sr, canvas_bounds)
        if reach is not None:
            plan.append((s, sxy, sr, blur, reach))
    return plan


def render_vector_layer(clip: Any, node: Any, canvas_bounds, window=None):
    """Rasterise a vector layer. Returns (straight-alpha RGBA image, x, y) in
    canvas coordinates, or None when the layer has no strokes / nothing visible.

    Point coordinates are shifted by the layer offset (LayerOffsetX/Y).

    ``window`` (canvas px) skips the strokes whose drawing misses it: inside
    the window the result is then the whole layer's. Each stroke is drawn
    over its own rectangle of ``canvas_bounds`` either way, and the strokes
    are composited pixel by pixel; the samples of all strokes are still
    computed together, as their arc lengths are summed across strokes (once
    per render: ``vector_plan``)."""
    items = []
    ub = None
    for s, sxy, sr, blur, reach in vector_plan(clip, node, canvas_bounds):
        if window is not None and C._intersect_bounds(reach, window) is None:
            continue
        got = _draw_stroke(sxy, sr, canvas_bounds, blur)
        if got is None:
            continue
        cov, x0, y0 = got
        items.append((cov, x0, y0, s))
        bb = (x0, y0, x0 + cov.width, y0 + cov.height)
        ub = bb if ub is None else C._union_bounds(ub, bb)
    if not items:
        return None
    canvas = Image.new("RGBA", (ub[2] - ub[0], ub[3] - ub[1]), (0, 0, 0, 0))
    for cov, x0, y0, s in items:
        if s["opacity"] < 1.0:
            cov = cov.point(lambda v, o=s["opacity"]: int(round(v * o)))
        dx, dy = x0 - ub[0], y0 - ub[1]
        if _brush(clip, s["brush"])["composite"] == ERASE_MODE:
            box = (dx, dy, dx + cov.width, dy + cov.height)
            region = canvas.crop(box)
            region.putalpha(ImageChops.multiply(region.getchannel("A"), ImageChops.invert(cov)))
            canvas.paste(region, box[:2])
            continue
        solid = Image.new("RGBA", cov.size, s["color"] + (255,))
        solid.putalpha(cov)
        canvas.alpha_composite(solid, (dx, dy))
    return canvas, ub[0], ub[1]


def render_vector_layer_buffer(clip: Any, node: Any, canvas_bounds, window=None):
    """``_render_layer_buffer`` drop-in for a vector layer: (_LayerBuffer or
    _EMPTY_LAYER_BUFFER, profile) with the stroke colours as colour data.
    With ``window``, only the pixels inside it are the layer's (see
    ``render_vector_layer``)."""
    got = render_vector_layer(clip, node, canvas_bounds, window)
    if got is None:
        return C._EMPTY_LAYER_BUFFER, None
    img, x, y = got
    buf = C._trim_buffer(C._LayerBuffer(img, x, y))
    return (buf if buf is not None else C._EMPTY_LAYER_BUFFER), "color-alpha-bgra"

