"""Per-layer styling CSP applies on top of a layer's stored pixels.

This module reads that metadata; tmi_accel applies it when it renders (the
Python transcription, the reference, is ``tmiref.clip_style``). Recovered by
black-box comparison with CanvasPreview (miura-ja, lolita page0001/0002):

1. レイヤーカラー (layer colour): ``Layer.DrawColorEnable = 1`` on a non-paper
   layer recolours its pixels: dark → DrawColorMain*, light → DrawColorSub*
   (u32 per channel, 0xFFFFFFFF = 255). miura's magenta べた塗り fills (MainId 36,
   173, GradationSettingAdd0001 = (255,0,128)) and the black balloon body
   (MainId 235) all have main = sub = white and come out white in CSP.

2. 表現色 (expression colour) of 1-channel rasters: an alpha-only
   ("single-channel") offscreen carries no colour; CSP paints it black when
   ``LayerColorTypeBlackChecked`` is set, white when only
   ``LayerColorTypeWhiteChecked`` is (mono / gray layers). tmi painted every
   such tile white (ベタフラッシュ01, ウニフラ縦 spikes). Gray (index 2) layers
   with colour data are converted to luma, mono (index 1) layers to black /
   white with the unchecked colour dropped.

3. 画像素材 (image material) layers: CSP does not save their render mipmap
   (the Offscreen's Exta is absent); the pixels live in
   ``ResizableOriginalMipmap`` at the original size and
   ``ResizableImageInfo`` holds the placement. Its last 64 bytes are the four
   destination corners as f64 BE (TL, TR, BL, BR) in canvas pixels; header
   u32[8], u32[9] are the original width / height; u32[4] = 1 marks a tiled
   material (repeat the original over the canvas). Resampling: see
   ``place_material`` / ``_sample_tiled``.

Plus the layer effect フチ (EffectEdge in LayerEffectInfo): u32 enabled, f64
width (px), u32 R, G, B — an outline of that width under the layer.

CSP does not save the render mipmap of anything it can regenerate (fills,
vectors, text, materials), so those layers come back without pixels.
"""
from __future__ import annotations

import struct
from typing import Any

_EXPR_COLUMNS = (
    "LayerType", "LayerFolder",
    "LayerColorTypeIndex", "LayerColorTypeBlackChecked", "LayerColorTypeWhiteChecked",
    "DrawColorEnable", "DrawColorMainRed", "DrawColorMainGreen", "DrawColorMainBlue",
    "DrawColorSubRed", "DrawColorSubGreen", "DrawColorSubBlue",
    "ResizableImageInfo", "ResizableOriginalMipmap",
    "LayerEffectAttached", "LayerEffectInfo",
)


def layer_style(clip: Any, main_id: int) -> dict[str, Any]:
    """Expression-colour / layer-colour / material / effect columns of one layer
    (missing columns read as None). Read for every layer at once, cached per clip."""
    cache = getattr(clip, "style_cache", None)
    if cache is None or getattr(clip, "db", None) is None:
        return {}
    if not cache:
        present = {r[1] for r in clip.db.execute("PRAGMA table_info(Layer)")}
        cols = [c for c in _EXPR_COLUMNS if c in present]
        cache[None] = {}
        if cols:
            for row in clip.db.execute(f"SELECT MainId, {', '.join(cols)} FROM Layer"):
                cache[row[0]] = dict(zip(cols, row[1:]))
    return cache.get(main_id, {})


def _u32_to_u8(value: int | None) -> int:
    value = value or 0
    return min(255, (value * 255 + 0x7FFFFFFF) // 0xFFFFFFFF)


def layer_colour(style: dict[str, Any]) -> tuple[tuple[int, int, int], tuple[int, int, int]] | None:
    """(main, sub) RGB when レイヤーカラー is on, else None. The paper layer
    (LayerType 1584) uses DrawColorEnable for its own fill colour instead."""
    if not style.get("DrawColorEnable") or style.get("LayerType") == 1584:
        return None
    main = tuple(_u32_to_u8(style.get(f"DrawColorMain{c}")) for c in ("Red", "Green", "Blue"))
    if style.get("DrawColorSubRed") is None:
        sub = (255, 255, 255)
    else:
        sub = tuple(_u32_to_u8(style.get(f"DrawColorSub{c}")) for c in ("Red", "Green", "Blue"))
    return main, sub


def needs_profile(style: dict[str, Any]) -> bool:
    """Whether ``apply_layer_style`` can change anything for this layer."""
    return bool(layer_colour(style)) or (style.get("LayerColorTypeIndex") or 0) in (1, 2)


# ───────────────────────────── image material ─────────────────────────────

def parse_resizable_image_info(blob: bytes) -> dict[str, Any]:
    """Decode ``ResizableImageInfo`` (112..184 bytes seen). Layout, big-endian:
    u32 size, u32 16, u32 4, u32 0, u32 tile_flag, u32 tile_mode, u32 2, u32 2,
    u32 orig_w, u32 orig_h, f64 scale_x, f64 scale_y, f64 angle, f64 centre_x,
    f64 centre_y, f64 anchor_x, f64 anchor_y, <16 or 24 bytes of u32 flags>,
    then 4 × (f64 x, f64 y) destination corners TL, TR, BL, BR (always the last
    64 bytes). The first flag word is 3 on every material placed in CSP 5
    (the probes, lolita's 斑雲1) and 0 on miura-ja's; it may be the
    interpolation method (``interp``), which no probe has varied yet. Header
    u32[3] was 1 only on the rotated probe material."""
    ints = struct.unpack(">10I", blob[:40])
    sx, sy, angle, cx, cy, ax, ay = struct.unpack(">7d", blob[40:96])
    corners = struct.unpack(">8d", blob[-64:])
    return {
        "orig_size": (ints[8], ints[9]),
        "tile": bool(ints[4]),
        "tile_mode": ints[5],
        "scale": (sx, sy),
        "angle": angle,
        "centre": (cx, cy),
        "corners": [(corners[i], corners[i + 1]) for i in range(0, 8, 2)],
        "interp": struct.unpack(">I", blob[96:100])[0] if len(blob) >= 164 else None,
    }


# ───────────────────────────── layer effect: edge (フチ) ─────────────────────────────

def parse_edge_effect(style: dict[str, Any]) -> tuple[float, tuple[int, int, int]] | None:
    """(width_px, rgb) of an enabled フチ effect, else None."""
    blob = style.get("LayerEffectInfo")
    if not blob or not style.get("LayerEffectAttached"):
        return None
    b = bytes(blob)
    key = struct.pack(">I", len("EffectEdge")) + "EffectEdge".encode("utf-16-be")
    i = b.find(key)
    if i < 0:
        return None
    p = i + len(key)
    enabled = struct.unpack(">I", b[p:p + 4])[0]
    width = struct.unpack(">d", b[p + 4:p + 12])[0]
    r, g, bl = struct.unpack(">3I", b[p + 12:p + 24])
    if not enabled or width <= 0:
        return None
    return width, (_u32_to_u8(r), _u32_to_u8(g), _u32_to_u8(bl))
