"""Per-layer styling CSP applies on top of a layer's stored pixels, in Python
(the reference for tmi_accel; tmi reads the metadata, tmi/formats/clip_style.py).

Recovered by black-box comparison with CanvasPreview (miura-ja, lolita
page0001/0002):

1. レイヤーカラー (layer colour): ``Layer.DrawColorEnable = 1`` on a non-paper
   layer recolours its pixels: dark → DrawColorMain*, light → DrawColorSub*
   (u32 per channel, 0xFFFFFFFF = 255). miura's magenta べた塗り fills (MainId 36,
   173, GradationSettingAdd0001 = (255,0,128)) and the black balloon body
   (MainId 235) all have main = sub = white and come out white in CSP.

2. 表現色 (expression colour, LayerColorTypeIndex 0 colour, 1 グレー,
   2 モノクロ): an alpha-only ("single-channel") offscreen carries no colour;
   CSP paints it black when ``LayerColorTypeBlackChecked`` is set, white
   when only ``LayerColorTypeWhiteChecked`` is (ベタフラッシュ01, ウニフラ縦
   spikes). グレー layers with colour data become their luma (with one box
   ticked, that colour with the luma as coverage); モノクロ layers black /
   white with the unticked colour dropped and no antialiasing
   (``_style_colours``; fitted to CSP's full-size export, research §15.7).

3. 画像素材 (image material) layers: CSP does not save their render mipmap
   (the Offscreen's Exta is absent); the pixels live in
   ``ResizableOriginalMipmap`` at the original size and
   ``ResizableImageInfo`` holds the placement. Its last 64 bytes are the four
   destination corners as f64 BE (TL, TR, BL, BR) in canvas pixels; header
   u32[8], u32[9] are the original width / height; u32[4] = 1 marks a tiled
   material (repeat the original over the canvas). Resampling: see
   ``place_material`` / ``_sample_tiled``.

4. 画像素材 drawn in モノクロ (``material_mono_conversion``): a material placed
   on a モノクロ canvas carries ``UsePreviewColorType = 1`` and
   ``PreviewColorTypeIndex = 2`` (with both colours ticked and u32 thresholds
   0x80000000 = 128) next to its グレー ``LayerColorTypeIndex``; CSP's
   full-size export then holds only 0 / 255 for it (miura-ja's flash
   materials ウニフラ縦 233 / 234, ベタフラッシュ01 461; a material without
   those columns, page0001's 斑雲1, stays antialiased). Fitted on the
   isolated exports of 233 and 461 (Fable 5.1, 2026-10-13):
   - the resampled alpha is cut at >= 128 (``PreviewColorTypeThresholdAlpha``:
     127 / 129 are 105 / 98 error pixels against 76 on 461's ~13.5k black
     pixels), the mipmap level is the usual one (233: level 1 beats 0 / 2 by
     8.9k vs 10.2k / 12.7k, 461: level 3 beats 2 / 4 by 1.3k vs 2.7k / 1.8k);
   - the level's floor(W / 2^k) x floor(H / 2^k) pixels are stretched over the
     whole-pixel rectangle from floor(tl + ½) to floor(br) (233: 4202..5190 x
     896..2260 for corners 4201.58..5190.75 x 896.0..2260.12, 461: 1568..1871
     x 2394..2823 for 1568.0..1872.0 x 2394.03..2823.97), sampled bilinearly
     as a colour placement is. The usual corner snapping (989 x 1364 from
     4202) leaves 22.1k error pixels, the exact corners 8.9k, this 57 (233)
     and 1265 -> 33 (461); a free four-edge fit finds nothing better (452 /
     35). Rotated and tiled placements keep their geometry (no sample).
   The threshold on the colour (``ThresholdImage``) is applied the same way
   but is unpinned: both sources are single-channel.

Plus the layer effect フチ (EffectEdge in LayerEffectInfo): u32 enabled, f64
width (px), u32 R, G, B — an outline of that width under the layer.

CSP does not save the render mipmap of anything it can regenerate (fills,
vectors, text, materials), so those layers come back without pixels.
"""
from __future__ import annotations

from typing import Any

import numpy as np
from PIL import Image

# The layer metadata is read by tmi (re-exported here for the renderer).
from tmi.formats.clip_style import (  # noqa: F401
    _EXPR_COLUMNS,
    _u32_to_u8,
    layer_colour,
    layer_style,
    needs_profile,
    parse_edge_effect,
    parse_resizable_image_info,
)
from tmiref import clip as C

# ───────────────────────────── layer metadata ─────────────────────────────

def offscreen_profile(clip: Any, mipmap_id: int | None) -> str | None:
    info = C.layer_offscreen_info(clip, mipmap_id)
    if not info:
        return None
    try:
        return C.parse_offscreen_attribute(info[1]).channel_profile
    except Exception:
        return None


# ───────────────────────────── colour transforms ─────────────────────────────

def _luma(rgb: np.ndarray) -> np.ndarray:
    return rgb[..., 0] * 0.299 + rgb[..., 1] * 0.587 + rgb[..., 2] * 0.114


def apply_layer_style(image: Image.Image, style: dict[str, Any], profile: str | None) -> Image.Image:
    """Return ``image`` (straight RGBA) with CSP's expression colour and layer
    colour applied. ``profile`` is the source offscreen's channel profile
    ("single-channel", "gray-alpha", "color-alpha-bgra" or None for synthesised
    fills). Returns the input object unchanged when nothing applies."""
    ct = style.get("LayerColorTypeIndex") or 0
    black = bool(style.get("LayerColorTypeBlackChecked"))
    white = bool(style.get("LayerColorTypeWhiteChecked"))
    lc = layer_colour(style)
    single = profile == "single-channel"
    if not lc and ct not in (1, 2):
        return image
    a = C._rgba_array(image.convert("RGBA") if image.mode != "RGBA" else image).copy()
    if ct == MONO and not single:
        # モノクロ draws without antialiasing: the alpha is cut to 0 / 255
        a[..., 3] = np.where(a[..., 3] >= MONO_ALPHA_THRESHOLD, 255, 0).astype(np.uint8)
    if ct in (1, 2) and single:
        # Every pixel gets the same colour: run the float steps below on one
        # pixel (they are elementwise) and fill. Alpha is kept.
        one = np.full((1, 1, 3), 0.0 if (black or not white) else 255.0, dtype=np.float32)
        if lc:
            main, sub = (np.array(c, dtype=np.float32) for c in lc)
            t = (_luma(one) / 255.0)[..., None]
            one = main * (1.0 - t) + sub * t
        a[..., :3] = np.clip(np.rint(one), 0, 255).astype(np.uint8)
        return Image.fromarray(a, "RGBA")
    h, w = a.shape[:2]
    if h * w >= C._DISTINCT_COLOUR_MIN_PIXELS:
        # A pure function of the pixel's colour, made of elementwise float32
        # steps: run it once per distinct colour (a page holds a few hundred)
        # and gather, as ``clip._run_filter`` does. Same bytes.
        distinct, index = C._distinct_colours(a)
        rgb_out, keep = _style_colours(distinct.astype(np.float32), ct, black, white, lc, profile)
        a[..., :3] = rgb_out[index].reshape(h, w, 3)
        if keep is not None:
            a[..., 3] = _scale_alpha(a[..., 3], keep[index].reshape(h, w))
        return Image.fromarray(a, "RGBA")
    rgb_out, keep = _style_colours(a[..., :3].astype(np.float32), ct, black, white, lc, profile)
    a[..., :3] = rgb_out
    if keep is not None:
        a[..., 3] = _scale_alpha(a[..., 3], keep)
    return Image.fromarray(a, "RGBA")


# LayerColorTypeIndex: 0 colour, 1 グレー, 2 モノクロ (probe_material_madara_*,
# whose 表現色 the names give; page0001's line art is モノクロ and CSP draws
# it in pure black without antialiasing).
GRAY, MONO = 1, 2
MONO_ALPHA_THRESHOLD = 128


def _scale_alpha(alpha, keep):
    """``alpha`` (uint8) times ``keep`` (uint8, 255 = unchanged), rounded."""
    return ((alpha.astype(np.uint32) * keep + 127) // 255).astype(np.uint8)


def _style_colours(rgb, ct: int, black: bool, white: bool, lc, profile: str | None):
    """The colour steps of ``apply_layer_style`` on float32 ``rgb`` (..., 3):
    ``(uint8 colours, uint8 alpha factor or None)``, elementwise.

    グレー: the luma; with only black (white) ticked the layer is black (white)
    and the luma becomes its coverage. モノクロ: black or white by the luma, a
    colour whose box is not ticked becomes transparent."""
    keep = None
    if ct == GRAY:
        l = _luma(rgb)
        if black and not white:
            rgb[...] = 0.0
            keep = np.clip(np.rint(255.0 - l), 0, 255).astype(np.uint32)
        elif white and not black:
            rgb[...] = 255.0
            keep = np.clip(np.rint(l), 0, 255).astype(np.uint32)
        else:
            rgb[...] = l[..., None]
    elif ct == MONO:
        is_white = _luma(rgb) >= 127.5
        rgb[...] = np.where(is_white, 255.0, 0.0)[..., None]
        drop = (is_white & (not white)) | (~is_white & (not black))
        keep = np.where(drop, 0, 255).astype(np.uint32)
    if lc:
        main, sub = (np.array(c, dtype=np.float32) for c in lc)
        t = (_luma(rgb) / 255.0)[..., None]
        rgb = main * (1.0 - t) + sub * t
    return np.clip(np.rint(rgb), 0, 255).astype(np.uint8), keep


# ───────────────────────────── image material ─────────────────────────────


def _full_offscreen(clip: Any, mipmap_id: int) -> tuple[Image.Image, str | None] | None:
    info = C.layer_offscreen_info(clip, mipmap_id)
    if not info or info[2] not in clip.extas:
        return None
    attr = C.parse_offscreen_attribute(info[1])
    buf = C._render_offscreen_buffer_from_info(clip, info)
    if buf is None or C._is_empty_buffer(buf):
        return None
    full = Image.new("RGBA", (attr.pixel_width, attr.pixel_height), (0, 0, 0, 0))
    full.paste(buf.image, (buf.x, buf.y))
    return full, attr.channel_profile


def _material_original(clip: Any, mipmap_id: int):
    """``(image, uint8 array, profile)`` of a material's original, or None."""
    got = _full_offscreen(clip, mipmap_id)
    if got is None:
        return None
    image, profile = got
    return image, np.asarray(image), profile


def render_image_material(
    clip: Any,
    node: Any,
    canvas_bounds: tuple[int, int, int, int],
    window: tuple[int, int, int, int] | None = None,
):
    """Rasterise a 画像素材 layer from its original mipmap. Returns
    ``(_LayerBuffer | None, profile)``.

    ``window`` (inside ``canvas_bounds``) limits the result to that window,
    with the whole-canvas pixels there: every destination pixel is sampled
    from its own absolute coordinates, elementwise (see ``place_material``
    and ``_sample_tiled``)."""
    style = layer_style(clip, node.main_id)
    blob, orig_mm = style.get("ResizableImageInfo"), style.get("ResizableOriginalMipmap")
    if not blob or not orig_mm:
        return None, None
    # The original decodes once per render, however many windows sample it.
    got = C._memoized(clip, ("material-original", orig_mm), lambda: _material_original(clip, orig_mm))
    if got is None:
        return None, None
    src, src_array, profile = got
    dst = canvas_bounds if window is None else window
    info = parse_resizable_image_info(bytes(blob))
    W, H = src.size
    ox, oy = node.offset_x or 0, node.offset_y or 0
    corners = [np.array(p, dtype=np.float64) + (ox, oy) for p in info["corners"][:3]]
    mono = material_mono_conversion(clip, node.main_id)
    if info["tile"]:
        tl, tr, bl = corners
        ex, ey = (tr - tl) / W, (bl - tl) / H
        Mi = np.linalg.inv(np.array([[ex[0], ey[0]], [ex[1], ey[1]]]))
        image = _sample_tiled(src_array, Mi, tl, dst)
        if mono is not None:
            image = _binarise_material(image, mono)
        buf = C._trim_buffer(C._LayerBuffer(image, dst[0], dst[1]))
        return (buf if buf is not None else C._EMPTY_LAYER_BUFFER), profile
    got = place_material(src_array, corners, dst, mono=mono is not None)
    if got is None:
        return C._EMPTY_LAYER_BUFFER, profile
    image, x, y = got
    if mono is not None:
        image = _binarise_material(image, mono)
    buf = C._trim_buffer(C._LayerBuffer(image, x, y))
    return (buf if buf is not None else C._EMPTY_LAYER_BUFFER), profile


# ───────────────── material drawn in モノクロ (PreviewColorType) ─────────────────

_PREVIEW_COLUMNS = (
    "UsePreviewColorType", "PreviewColorTypeIndex",
    "PreviewColorTypeBlackChecked", "PreviewColorTypeWhiteChecked",
    "PreviewColorTypeThresholdImage", "PreviewColorTypeThresholdAlpha",
)


def material_mono_conversion(clip: Any, main_id: int) -> dict[str, Any] | None:
    """The モノクロ conversion CSP draws an image material with, or None.

    A material placed on a モノクロ canvas carries ``UsePreviewColorType = 1``
    with ``PreviewColorTypeIndex = 2`` (miura-ja's flash materials: ウニフラ縦
    233 / 234, ベタフラッシュ01 461, ...): CSP draws its resampled pixels
    thresholded, not antialiased. ``PreviewColorTypeThresholdAlpha`` /
    ``ThresholdImage`` are u32 (0x80000000 = 128); only the alpha threshold
    is pinned (single-channel sources). Read once per layer, cached per clip."""
    if clip is None:
        return None
    cache = clip.frame_cache
    key = ("material-mono", main_id)
    if key in cache:
        return cache[key]
    out = None
    try:
        present = {r[1] for r in clip.db.execute("PRAGMA table_info(Layer)")}
        if all(c in present for c in _PREVIEW_COLUMNS):
            row = clip.db.execute(
                f"SELECT {', '.join(_PREVIEW_COLUMNS)} FROM Layer WHERE MainId = ?", (main_id,)
            ).fetchone()
            if row and row[0] and row[1] == MONO:
                out = {
                    "black": bool(row[2]),
                    "white": bool(row[3]),
                    "threshold_image": _u32_to_u8(row[4]),
                    "threshold_alpha": _u32_to_u8(row[5]),
                }
    except Exception:
        out = None
    cache[key] = out
    return out


def _binarise_material(image: Image.Image, mono: dict[str, Any]) -> Image.Image:
    """Cut a placed material's alpha to 0 / 255 at the alpha threshold and its
    colour to black / white at the image threshold (an unticked colour is
    dropped), as CSP draws a material in モノクロ."""
    a = np.asarray(image).copy()
    keep = a[..., 3] >= mono["threshold_alpha"]
    is_white = _luma(a[..., :3].astype(np.float32)) >= np.float32(mono["threshold_image"] - 0.5)
    drop = (is_white & (not mono["white"])) | (~is_white & (not mono["black"]))
    a[..., 3] = np.where(keep & ~drop, 255, 0).astype(np.uint8)
    a[..., :3] = np.where(is_white[..., None], 255, 0).astype(np.uint8)
    return Image.fromarray(a, "RGBA")


# Placement of a non-tiled material, fitted pixel-exact on
# probe_material_scale.clip (a 64×64 checkerboard at 300 %, 250 %, 30 % and
# 170 % rotated 30°):
#
# * Destination pixel centre (x + ½, y + ½) maps to the source by the inverse
#   of the corner affine, and the source is read at that point with bilinear
#   interpolation on pixel centres (premultiplied). The weights are 8-bit:
#   floor(frac · 256) / 256, and a point that lands exactly on a source centre
#   reads 1/256 of the previous pixel (CSP's coordinates come out a hair low).
# * Downscaling first halves the source (2×2 premultiplied mean) while the
#   scale stays ≤ ½, i.e. it samples the power-of-two mipmap level that is
#   still at least as large as the destination (30 % reads the ½ level).
# * Axis-aligned placement snaps the corners to whole pixels (30 % of 64 at
#   790.4..809.6 is drawn on 790..810) and clamps at the source edge, so the
#   edges are hard. A rotated placement keeps the exact corners and treats the
#   outside of the source as transparent, so its edges ramp over one source
#   pixel (alpha ½ exactly on the edge).
# * A material drawn in モノクロ (``mono``) stretches the mipmap level over the
#   whole-pixel rectangle floor(tl + ½) .. floor(br) instead (module doc, 4).
_SAMPLE_EPS = 1e-4


def _mip_level(src: np.ndarray, scale: float) -> tuple[np.ndarray, int]:
    level = 0
    while scale * (2 << level) <= 1.0 + 1e-9 and min(src.shape[:2]) >= 2:
        H, W = src.shape[:2]
        s = src[: H // 2 * 2, : W // 2 * 2]
        src = (s[0::2, 0::2] + s[1::2, 0::2] + s[0::2, 1::2] + s[1::2, 1::2]) * 0.25
        level += 1
    return src, level


def _bilinear(src: np.ndarray, u: np.ndarray, v: np.ndarray) -> np.ndarray:
    """Sample premultiplied float ``src`` (H, W, 4) at pixel-centre coordinates
    (u, v), clamped to the edge, with CSP's 8-bit weights."""
    H, W = src.shape[:2]
    u = np.clip(u - _SAMPLE_EPS, 0.0, W - 1)
    v = np.clip(v - _SAMPLE_EPS, 0.0, H - 1)
    i0 = u.astype(np.int64)
    j0 = v.astype(np.int64)
    fu = (np.floor((u - i0) * 256.0) / 256.0)[..., None]
    fv = (np.floor((v - j0) * 256.0) / 256.0)[..., None]
    i1 = np.minimum(i0 + 1, W - 1)
    j1 = np.minimum(j0 + 1, H - 1)
    top = src[j0, i0] * (1.0 - fu) + src[j0, i1] * fu
    bot = src[j1, i0] * (1.0 - fu) + src[j1, i1] * fu
    return top * (1.0 - fv) + bot * fv


def place_material(src: np.ndarray, corners, canvas_bounds, rows: int = 256, mono: bool = False):
    """Resample straight-alpha RGBA ``src`` (H, W, 4 uint8) onto the canvas so
    that its top-left, top-right and bottom-left corners land on ``corners``
    (canvas px). Returns (straight RGBA image, x, y) or None if off-canvas.

    ``mono``: the material is drawn in モノクロ (``material_mono_conversion``).
    An axis-aligned placement then stretches the mipmap level it samples over
    the whole-pixel rectangle floor(tl + ½) .. floor(br) instead of snapping
    the corners (fitted on miura-ja's flash materials, see the module doc)."""
    H, W = src.shape[:2]
    tl, tr, bl = (np.asarray(p, dtype=np.float64) for p in corners)
    ex, ey = (tr - tl) / W, (bl - tl) / H
    axis = abs(ex[1]) < 1e-6 and abs(ey[0]) < 1e-6
    level_rect = None
    if axis and mono:
        x0i, y0i = int(np.floor(tl[0] + 0.5)), int(np.floor(tl[1] + 0.5))
        x1i, y1i = int(np.floor(tr[0])), int(np.floor(bl[1]))
        if x1i <= x0i or y1i <= y0i:
            return None
        level_rect = (x0i, y0i, x1i, y1i)
        tl = np.array([x0i, y0i], dtype=np.float64)
        tr = np.array([x1i, y0i], dtype=np.float64)
        bl = np.array([x0i, y1i], dtype=np.float64)
    elif axis:
        tl, tr, bl = np.round(tl), np.round(tr), np.round(bl)
        ex, ey = (tr - tl) / W, (bl - tl) / H
        if not ex[0] or not ey[1]:
            return None
    pts = np.array([tl, tr, bl, tr + bl - tl])
    lo, hi = pts.min(axis=0), pts.max(axis=0)
    pad_px = 0 if axis else int(np.ceil(0.5 * max(np.hypot(*ex), np.hypot(*ey)))) + 1
    dst = C._intersect_bounds(
        (int(np.floor(lo[0])) - pad_px, int(np.floor(lo[1])) - pad_px, int(np.ceil(hi[0])) + pad_px, int(np.ceil(hi[1])) + pad_px),
        canvas_bounds,
    )
    if dst is None:
        return None
    a = src.astype(np.float32)
    a[..., :3] *= a[..., 3:4] / 255.0
    a, level = _mip_level(a, min(np.hypot(*ex), np.hypot(*ey)))
    if not axis:
        a = np.pad(a, ((1, 1), (1, 1), (0, 0)))
    f = float(1 << level)
    if level_rect is not None:
        # the level's floor(W / 2^k) x floor(H / 2^k) pixels span the rectangle
        x0i, y0i, x1i, y1i = level_rect
        Hl, Wl = a.shape[:2]
        ex = np.array([(x1i - x0i) / (Wl * f), 0.0])
        ey = np.array([0.0, (y1i - y0i) / (Hl * f)])
    Mi = np.linalg.inv(np.array([[ex[0], ey[0]], [ex[1], ey[1]]]) * f)
    shift = 0.5 if axis else -0.5  # pixel-centre offset, minus the transparent border
    x0, y0, x1, y1 = dst
    out = np.empty((y1 - y0, x1 - x0, 4), dtype=np.float32)
    px = np.arange(x0, x1, dtype=np.float64)[None, :] + 0.5 - tl[0]
    for r0 in range(0, y1 - y0, rows):
        py = np.arange(y0 + r0, min(y1, y0 + r0 + rows), dtype=np.float64)[:, None] + 0.5 - tl[1]
        u = Mi[0, 0] * px + Mi[0, 1] * py - shift
        v = Mi[1, 0] * px + Mi[1, 1] * py - shift
        out[r0:r0 + py.shape[0]] = _bilinear(a, u, v)
    alpha = out[..., 3:4]
    out[..., :3] = np.where(alpha > 0, out[..., :3] * 255.0 / np.maximum(alpha, 1e-6), 0.0)
    image = Image.fromarray(np.clip(np.rint(out), 0, 255).astype(np.uint8), "RGBA")
    return image, x0, y0


def _sample_tiled(src: np.ndarray, Mi: np.ndarray, tl: np.ndarray, dst) -> Image.Image:
    """Nearest-neighbour wrap-around sampling of a tiled material over ``dst``.

    Pixel x reads source index floor(M^-1 (x - tl)), from the pixel's corner:
    斑雲1 placed with its corner at (452.5, 437.5) shows source pixel 0 at
    canvas pixel 453 (probe_material_madara_*.clip match exactly)."""
    a = np.asarray(src)
    H, W = a.shape[:2]
    out = np.zeros((dst[3] - dst[1], dst[2] - dst[0], 4), dtype=np.uint8)
    xs = np.arange(dst[0], dst[2], dtype=np.float64) - tl[0]
    for row0 in range(0, out.shape[0], 512):
        ys = np.arange(dst[1] + row0, min(dst[3], dst[1] + row0 + 512), dtype=np.float64) - tl[1]
        u = Mi[0, 0] * xs[None, :] + Mi[0, 1] * ys[:, None]
        v = Mi[1, 0] * xs[None, :] + Mi[1, 1] * ys[:, None]
        ui = np.floor(u).astype(np.int64) % W
        vi = np.floor(v).astype(np.int64) % H
        out[row0:row0 + ys.size] = a[vi, ui]
    return Image.fromarray(out, "RGBA")


# ───────────────────────────── layer effect: edge (フチ) ─────────────────────────────


def _hmax(a: np.ndarray, hw: int) -> np.ndarray:
    """Horizontal running max over [x-hw, x+hw] (zero padded)."""
    if hw <= 0:
        return a
    L = 2 * hw + 1
    pad = np.pad(a, ((0, 0), (hw, hw)))
    m = pad
    p = 1
    while p * 2 <= L:
        nxt = m.copy()
        nxt[:, :-p] = np.maximum(m[:, :-p], m[:, p:])
        m = nxt
        p *= 2
    n = a.shape[1]
    return np.maximum(m[:, 0:n], m[:, L - p:L - p + n])


def _disk_max(a: np.ndarray, R: int) -> np.ndarray:
    if R <= 0:
        return a
    out = np.zeros_like(a)
    rows = {}
    H = a.shape[0]
    for dy in range(-R, R + 1):
        hw = int(np.floor(np.sqrt(R * R - dy * dy) + 1e-9))
        if hw not in rows:
            rows[hw] = _hmax(a, hw)
        src = rows[hw]
        if dy >= 0:
            out[:H - dy] = np.maximum(out[:H - dy], src[dy:])
        else:
            out[-dy:] = np.maximum(out[-dy:], src[:H + dy])
    return out


def apply_edge(buf: Any, width: float, rgb, canvas_bounds):
    """Return a new _LayerBuffer: ``buf`` over a ``width``-px outline of ``rgb``."""
    R = int(np.ceil(width))
    bounds = C._intersect_bounds((buf.x - R, buf.y - R, buf.x + buf.image.width + R, buf.y + buf.image.height + R), canvas_bounds)
    if bounds is None:
        return buf
    w, h = bounds[2] - bounds[0], bounds[3] - bounds[1]
    canvas = Image.new("RGBA", (w, h), (0, 0, 0, 0))
    canvas.paste(buf.image, (buf.x - bounds[0], buf.y - bounds[1]))
    alpha = np.asarray(canvas.getchannel("A"))
    lo = int(np.floor(width))
    frac = width - lo
    edge = _disk_max(alpha, lo).astype(np.float32)
    if frac > 1e-3:
        edge = np.maximum(edge, _disk_max(alpha, lo + 1).astype(np.float32) * frac)
    edge_img = Image.new("RGBA", (w, h), tuple(rgb) + (0,))
    edge_img.putalpha(Image.fromarray(np.clip(np.rint(edge), 0, 255).astype(np.uint8), "L"))
    edge_img.alpha_composite(canvas)
    return C._trim_buffer(C._LayerBuffer(edge_img, bounds[0], bounds[1])) or buf


