"""Comic-frame (コマ枠) folder rendering for tmi's CLIP compositor.

Black-box findings (CSP-saved files, compared against CanvasPreview):

* A comic-frame folder (``Layer.ComicFrameLineMipmap`` set) saves *no pixel
  data* for its render / layer-mask / frame-line offscreens: the Offscreen rows
  exist but their Exta chunks are absent. CSP regenerates all three from the
  frame's vector shape.
* The shape is the vector object list referenced by
  ``Layer.VectorNormalBalloonIndex`` (``VectorNormalType = 3``):
  ``VectorObjectList.MainId = index`` -> ``VectorData`` (40-byte
  ``extrnlid…`` key) -> Exta chunk -> inner stream (big-endian) holding one
  record per frame border (a folder can own several panels):

    header (``hsize`` = 0x60 bytes)
      +0x00 u32 hsize (96)      +0x04 u32 72        +0x08 u32 point_size (88)
      +0x0c u32 point_size      +0x10 u32 n_points  +0x14 u32 flags (0x1118)
      +0x18 i32[4] bbox (x0, y0, x1, y1) of the stroked outline
      +0x28 u32[3] main colour R,G,B (0..0xFFFFFFFF; 0 = black)  <- line colour
      +0x34 u32[3] sub colour R,G,B
      +0x40 f64 opacity (1.0)
      +0x48 u32[3] brush/style ids (varies per file)
      +0x54 f64 w  = line half-width in canvas px (e.g. 10.6294 = 0.45 mm @600dpi)
      +0x5c u32 ?
    n_points x point record (``point_size`` = 88 bytes)
      +0x00 f64 x, f64 y         vertex in canvas px (pixel i spans [i, i+1))
      +0x10 i32[4] bbox of the stroked edge starting here
      +0x20 u32 3, f32 x7 (1,1,0,0,1,1,1)
      +0x40 f32 miter_len, f32 miter_nx, f32 miter_ny   (outward miter: w/cos(θ/2))
      +0x4c u32 0, u32 1, u32 0
    Records follow back to back; the polygon is closed.

* Rendering (measured on the 1/4-scale CanvasPreview, ±0.5 px):
    - children are clipped to the polygon interior (union of the folder's
      polygons); outside the frame the folder contributes nothing;
    - the border is a closed, miter-joined stroke *centred on the polygon
      edge* whose total width is ``2 * w``, opaque, in the main colour;
    - the border is drawn above the clipped children (it is part of the
      folder's own output, so the folder's opacity/blend applies to it).
"""
from __future__ import annotations

import math
import struct
from typing import Any

import numpy as np
from PIL import Image

from tmiref import clip as C

SUBROWS = 4          # vertical supersampling of the scanline rasteriser
MITER_LIMIT = 10.0   # in units of w; beyond it the corner is bevelled
BORDER_ALPHA_THRESHOLD = 128  # the border is drawn without antialiasing
_ROW_CHUNK = 512


# ───────────────────────────── vector decoding ──────────────────────────────


def _vector_inner(clip: Any, ext: bytes) -> bytes | None:
    key = bytes(ext).decode("ascii", "replace")
    chunk = clip.extas.get(key)
    if chunk is None:
        return None
    payload = clip.raw[chunk.payload_off:chunk.payload_off + chunk.payload_size]
    if payload[8:16] != b"extrnlid":
        return None
    inner_size = struct.unpack(">Q", payload[48:56])[0]
    return bytes(payload[56:56 + inner_size])


def parse_frame_objects(inner: bytes) -> list[dict]:
    """Decode the frame-border records of a frame folder's vector stream."""
    out: list[dict] = []
    p = 0
    while p + 0x60 <= len(inner):
        hsize, _f4, psize, _ps2, count, flags = struct.unpack(">IIIIII", inner[p:p + 24])
        if hsize < 0x60 or psize < 0x4c or count == 0 or p + hsize + count * psize > len(inner):
            break
        main_rgb = struct.unpack(">III", inner[p + 0x28:p + 0x34])
        opacity = struct.unpack(">d", inner[p + 0x40:p + 0x48])[0]
        # Like vector strokes, the header's tail is fixed relative to hsize
        # (88 / 92 / 96 seen): the width sits 12 bytes before its end.
        width = struct.unpack(">d", inner[p + hsize - 12:p + hsize - 4])[0]
        pts = []
        miters = []
        for i in range(count):
            q = p + hsize + i * psize
            x, y = struct.unpack(">dd", inner[q:q + 16])
            pts.append((x, y))
            ml, mx, my = struct.unpack(">fff", inner[q + 0x40:q + 0x4c])
            miters.append((ml * mx, ml * my))
        out.append({
            "points": pts,
            "stored_miters": miters,
            "half_width": width,
            "color": tuple(min(255, (c * 255 + 0x7FFFFFFF) // 0xFFFFFFFF) for c in main_rgb),
            "opacity": max(0.0, min(1.0, opacity)),
            "flags": flags,
        })
        p += hsize + count * psize
    return out


def frame_objects(clip: Any, node: Any) -> list[dict]:
    """Frame-border polygons of a comic-frame folder, in canvas coordinates."""
    cache = clip.frame_cache
    key = ("objects", node.main_id)
    if key in cache:
        return cache[key]
    objs: list[dict] = []
    try:
        row = clip.db.execute(
            "SELECT VectorNormalBalloonIndex FROM Layer WHERE MainId = ?", (node.main_id,)
        ).fetchone()
        idx = row[0] if row else None
        if idx:
            vrow = clip.db.execute(
                "SELECT VectorData FROM VectorObjectList WHERE MainId = ?", (idx,)
            ).fetchone()
            if vrow and vrow[0] is not None:
                inner = _vector_inner(clip, vrow[0])
                if inner:
                    objs = parse_frame_objects(inner)
    except Exception:
        objs = []
    ox = getattr(node, "offset_x", 0) or 0
    oy = getattr(node, "offset_y", 0) or 0
    if ox or oy:
        for o in objs:
            o["points"] = [(x + ox, y + oy) for x, y in o["points"]]
    cache[key] = objs
    return objs


# ───────────────────────────── rasteriser ───────────────────────────────────


def _rasterize(polys: list[list[tuple[float, float]]], bounds: tuple[int, int, int, int]) -> np.ndarray:
    """Anti-aliased non-zero-winding fill of ``polys`` over ``bounds``.

    Exact horizontal coverage, ``SUBROWS`` vertical samples per pixel row.
    Pixel (i, j) covers [i, i+1) x [j, j+1). Returns float32 coverage 0..1."""
    x0, y0, x1, y1 = bounds
    W, H = x1 - x0, y1 - y0
    out = np.zeros((H, W), dtype=np.float32)
    if W <= 0 or H <= 0:
        return out
    # Edges as arrays
    ex0, ey0, ex1, ey1 = [], [], [], []
    for poly in polys:
        n = len(poly)
        for k in range(n):
            ax, ay = poly[k]
            bx, by = poly[(k + 1) % n]
            if ay == by:
                continue
            ex0.append(ax - x0); ey0.append(ay - y0); ex1.append(bx - x0); ey1.append(by - y0)
    if not ex0:
        return out
    ex0 = np.array(ex0); ey0 = np.array(ey0); ex1 = np.array(ex1); ey1 = np.array(ey1)
    dirs = np.where(ey1 > ey0, 1, -1).astype(np.int32)
    ylo = np.minimum(ey0, ey1); yhi = np.maximum(ey0, ey1)
    step = 1.0 / SUBROWS
    for r0 in range(0, H, _ROW_CHUNK):
        r1 = min(H, r0 + _ROW_CHUNK)
        ys = (np.arange(r0 * SUBROWS, r1 * SUBROWS) + 0.5) * step  # subrow sample y
        # edges overlapping this chunk
        sel = (yhi > ys[0]) & (ylo <= ys[-1])
        if not sel.any():
            continue
        sx0, sy0, sx1, sy1, sd = ex0[sel], ey0[sel], ex1[sel], ey1[sel], dirs[sel]
        slo, shi = ylo[sel], yhi[sel]
        # half-open rule: edge crosses sample y if lo <= y < hi
        S, E = np.nonzero((ys[:, None] >= slo[None, :]) & (ys[:, None] < shi[None, :]))
        if S.size == 0:
            continue
        t = (ys[S] - sy0[E]) / (sy1[E] - sy0[E])
        xs = sx0[E] + t * (sx1[E] - sx0[E])
        d = sd[E]
        order = np.lexsort((xs, S))
        S, xs, d = S[order], xs[order], d[order]
        wind = np.cumsum(d)
        # span k: from xs[k] to xs[k+1] when same subrow and winding != 0
        same = S[:-1] == S[1:]
        inside = same & (wind[:-1] != 0)
        a = np.clip(xs[:-1][inside], 0, W)
        b = np.clip(xs[1:][inside], 0, W)
        rows = S[:-1][inside] // SUBROWS
        keep = b > a
        a, b, rows = a[keep], b[keep], rows[keep]
        if a.size == 0:
            continue
        nr = r1 - r0
        acc = np.zeros((nr, W + 2), dtype=np.float64)
        ia = np.floor(a).astype(np.int64); fa = a - ia
        ib = np.floor(b).astype(np.int64); fb = b - ib
        np.add.at(acc, (rows, ia), (1 - fa) * step)
        np.add.at(acc, (rows, ia + 1), fa * step)
        np.add.at(acc, (rows, ib), -(1 - fb) * step)
        np.add.at(acc, (rows, ib + 1), -fb * step)
        cov = np.cumsum(acc, axis=1)[:, :W]
        out[r0:r1] = np.clip(cov, 0.0, 1.0)
    return out


def _signed_area(poly):
    s = 0.0
    n = len(poly)
    for k in range(n):
        ax, ay = poly[k]; bx, by = poly[(k + 1) % n]
        s += ax * by - bx * ay
    return s / 2


def _miter_offsets(pts: list[tuple[float, float]], w: float):
    """Per-vertex outward miter offset vectors for a closed polygon (len = w/cos(θ/2))."""
    n = len(pts)
    # y-down canvas: positive shoelace area == clockwise on screen, for which
    # the normal (ey, -ex) points outward.
    flip = _signed_area(pts) < 0
    out = []
    for k in range(n):
        px, py = pts[k - 1]
        cx, cy = pts[k]
        nx_, ny_ = pts[(k + 1) % n]
        e0 = (cx - px, cy - py); e1 = (nx_ - cx, ny_ - cy)
        l0 = math.hypot(*e0) or 1.0; l1 = math.hypot(*e1) or 1.0
        # left normals (y-down); flip to point outward
        n0 = (e0[1] / l0, -e0[0] / l0); n1 = (e1[1] / l1, -e1[0] / l1)
        if flip:
            n0 = (-n0[0], -n0[1]); n1 = (-n1[0], -n1[1])
        mx, my = n0[0] + n1[0], n0[1] + n1[1]
        denom = 1.0 + n0[0] * n1[0] + n0[1] * n1[1]
        if denom < 1e-6:
            out.append((n0[0] * w, n0[1] * w))
            continue
        mx, my = mx * w / denom, my * w / denom
        ml = math.hypot(mx, my)
        if ml > MITER_LIMIT * w:
            mx, my = mx * MITER_LIMIT * w / ml, my * MITER_LIMIT * w / ml
        out.append((mx, my))
    return out


def _stroke_quads(pts, w):
    m = _miter_offsets(pts, w)
    n = len(pts)
    quads = []
    for k in range(n):
        a = pts[k]; b = pts[(k + 1) % n]
        ma = m[k]; mb = m[(k + 1) % n]
        q = [(a[0] + ma[0], a[1] + ma[1]), (b[0] + mb[0], b[1] + mb[1]),
             (b[0] - mb[0], b[1] - mb[1]), (a[0] - ma[0], a[1] - ma[1])]
        if _signed_area(q) < 0:
            q = q[::-1]
        quads.append(q)
    return quads


def _poly_bounds(polys, pad=1.0):
    xs = [x for p in polys for x, _ in p]; ys = [y for p in polys for _, y in p]
    return (math.floor(min(xs) - pad), math.floor(min(ys) - pad), math.ceil(max(xs) + pad), math.ceil(max(ys) + pad))


# ───────────────────────────── public API ───────────────────────────────────


def frame_mask(clip: Any, node: Any, canvas_bounds=None):
    """(L image, x, y) clip mask of the frame interior, or None if no shape.

    Union of the folder's frame polygons, anti-aliased; 255 inside."""
    objs = frame_objects(clip, node)
    if not objs:
        return None
    canvas_bounds = canvas_bounds or (0, 0, clip.canvas.width, clip.canvas.height)
    polys = []
    for o in objs:
        poly = o["points"]
        polys.append(poly if _signed_area(poly) > 0 else poly[::-1])  # same winding → union
    b = C._intersect_bounds(_poly_bounds(polys), canvas_bounds)
    if b is None:
        return Image.new("L", (1, 1), 0), 0, 0
    cov = _rasterize(polys, b)
    return Image.fromarray(np.round(cov * 255).astype(np.uint8), "L"), b[0], b[1]


def frame_border(clip: Any, node: Any, canvas_bounds=None):
    """(RGBA image, x, y) of the frame border lines, or None."""
    objs = frame_objects(clip, node)
    if not objs:
        return None
    canvas_bounds = canvas_bounds or (0, 0, clip.canvas.width, clip.canvas.height)
    layers = []
    for o in objs:
        w = o["half_width"]
        if w <= 0:
            continue
        quads = _stroke_quads(o["points"], w)
        b = C._intersect_bounds(_poly_bounds(quads), canvas_bounds)
        if b is None:
            continue
        # one rasterisation per straight edge keeps the bbox tight on long
        # diagonals; the shared miter edges sum to full coverage.
        cov = np.zeros((b[3] - b[1], b[2] - b[0]), dtype=np.float32)
        for q in quads:
            qb = C._intersect_bounds(_poly_bounds([q]), b)
            if qb is None:
                continue
            part = _rasterize([q], qb)
            sl = cov[qb[1] - b[1]:qb[3] - b[1], qb[0] - b[0]:qb[2] - b[0]]
            np.add(sl, part, out=sl)
        np.clip(cov, 0, 1, out=cov)
        layers.append((cov * o["opacity"], o["color"], b, o["points"]))
    if not layers:
        return None
    ub = layers[0][2]
    for _, _, b, _ in layers[1:]:
        ub = C._union_bounds(ub, b)
    W, H = ub[2] - ub[0], ub[3] - ub[1]
    # Painter's order (measured on lolita page0002, where panel 4 overlaps
    # panel 3): each panel first covers the border lines drawn so far inside
    # its own interior, then draws its own border. So a later panel hides an
    # earlier panel's line where they overlap, but keeps its own outline.
    from PIL import ImageChops

    img = Image.new("RGBA", (W, H), (0, 0, 0, 0))
    for k, (cov, color, b, pts) in enumerate(layers):
        pb = C._intersect_bounds(_poly_bounds([pts]), ub)
        if pb is not None and k > 0:
            keep = Image.fromarray(np.round((1.0 - _rasterize([pts], pb)) * 255).astype(np.uint8), "L")
            box = (pb[0] - ub[0], pb[1] - ub[1], pb[2] - ub[0], pb[3] - ub[1])
            region = img.crop(box)
            region.putalpha(ImageChops.multiply(region.getchannel("A"), keep))
            img.paste(region, box[:2])
        a = Image.fromarray(np.round(cov * 255).astype(np.uint8), "L")
        solid = Image.new("RGBA", a.size, tuple(color) + (255,))
        solid.putalpha(a)
        img.alpha_composite(solid, (b[0] - ub[0], b[1] - ub[1]))
    # CSP draws the border without antialiasing, on colour canvases too: the
    # full-size exports of page0001 / page0002 / miura / haruse hold only 0 /
    # 255 across the lines, and this cut of the drawn coverage reproduces them
    # (page0001 0.838 -> 0.798, haruse 0.010 -> 0.001; research §15.7).
    arr = np.array(img)
    arr[..., 3] = np.where(arr[..., 3] >= BORDER_ALPHA_THRESHOLD, 255, 0).astype(np.uint8)
    return Image.fromarray(arr, "RGBA"), ub[0], ub[1]


def frame_mask_crop(clip: Any, node: Any, crop_bounds, canvas_bounds):
    """Drop-in for ``clip._layer_mask_crop`` on a comic-frame folder: an ``L``
    image the size of ``crop_bounds`` (0 = hide, 255 = show)."""
    crop_bounds = C._intersect_bounds(crop_bounds, canvas_bounds)
    if crop_bounds is None:
        return None
    cache = clip.frame_cache
    key = ("mask", node.main_id, tuple(canvas_bounds))
    if key not in cache:
        cache[key] = C._memoized(clip, ("frame", key), lambda: frame_mask(clip, node, canvas_bounds))
    m = cache[key]
    out = Image.new("L", (crop_bounds[2] - crop_bounds[0], crop_bounds[3] - crop_bounds[1]), 0)
    if m is None:
        return out
    img, mx, my = m
    out.paste(img, (mx - crop_bounds[0], my - crop_bounds[1]))
    return out


def shared_frame_border(clip: Any, node: Any, canvas_bounds):
    """A banded render's one drawing of a frame border, kept as its
    non-blank tiles (``clip._SparseImage``), or None."""

    def draw():
        border = frame_border(clip, node, canvas_bounds)
        return None if border is None else C._SparseImage(*border)

    return clip.render_memo.get(("frame-border", node.main_id, tuple(canvas_bounds)), draw)


def apply_frame_to_group_buffer(clip: Any, node: Any, buf, canvas_bounds, window=None):
    """Clip a frame folder's composited children to the frame interior and
    draw the border on top. ``buf`` is the folder's ``_LayerBuffer`` (or None
    / empty when it has no visible children). Returns a ``_LayerBuffer`` or
    ``_EMPTY_LAYER_BUFFER``.

    ``window`` (the window the folder was composited over) leaves out the
    border outside it; inside it every pixel is the same. The border itself
    is drawn whole, once per render."""
    windowed = window is not None and tuple(window) != tuple(canvas_bounds)
    memo = getattr(clip, "render_memo", None)
    if windowed and memo is not None and memo.banded:
        sparse = shared_frame_border(clip, node, canvas_bounds)
        part = None if sparse is None else sparse.buffer(window)
        border = None if part is None else (part.image, part.x, part.y)
    else:
        border = C._memoized(
            clip, ("frame-border", node.main_id, tuple(canvas_bounds)), lambda: frame_border(clip, node, canvas_bounds)
        )
        if border is not None and windowed:
            clipped = C._clip_buffer_to_bounds(C._LayerBuffer(border[0], border[1], border[2]), window)
            border = None if clipped is None else (clipped.image, clipped.x, clipped.y)
    if not frame_objects(clip, node):
        # Unknown shape: keep tmi's previous behaviour (unmasked children).
        return buf if buf is not None else C._EMPTY_LAYER_BUFFER
    parts = []
    if buf is not None and not C._is_empty_buffer(buf):
        mask = frame_mask_crop(clip, node, buf.bounds, canvas_bounds)
        if mask is not None:
            img = buf.image.copy()
            from PIL import ImageChops
            img.putalpha(ImageChops.multiply(img.getchannel("A"), mask))
            parts.append(C._LayerBuffer(img, buf.x, buf.y))
    if border is not None:
        parts.append(C._LayerBuffer(border[0], border[1], border[2]))
    if not parts:
        return C._EMPTY_LAYER_BUFFER
    ub = parts[0].bounds
    for p in parts[1:]:
        ub = C._union_bounds(ub, p.bounds)
    out = Image.new("RGBA", (ub[2] - ub[0], ub[3] - ub[1]), (0, 0, 0, 0))
    for p in parts:
        out.alpha_composite(p.image, (p.x - ub[0], p.y - ub[1]))
    res = C._trim_buffer(C._LayerBuffer(out, ub[0], ub[1]))
    return res if res is not None else C._EMPTY_LAYER_BUFFER
