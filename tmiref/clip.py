"""The Python CLIP compositor: the reference ``tmi_accel.render_clip`` is
tested against (byte for byte), and the place to fit rendering formulas to
CSP's output (the probe workflow of research/clip-format.md).

This is the renderer tmi used before RUST_MIGRATION.md stage 1e, as it ran
without tmi_accel: numpy blends and regrades, Pillow for the Pillow-exact
steps, the Python tile decoder. It renders the ``ClipFile`` objects
``tmi.formats.clip.open_clip_bytes`` builds, and reuses tmi's CLIP parser,
warnings and layer buffers; tmi never imports it. Entry points mirror
tmi's: ``render_layered_composite_rgba``, ``render_layered_regions``,
``render_folder``, ``render_full``; ``TMI_RENDER_TRACE`` traces it the same
way (tools/render_diff.py compares the two).
"""
from __future__ import annotations

import math
import os
import sqlite3
import struct
import threading
from dataclasses import dataclass
from typing import Any

import numpy as np
from PIL import Image

# The CLIP model, its parser and the warnings come from tmi; the names below
# are re-exported so that ``from tmiref import clip as C`` reaches them too.
from tmi.formats.clip import (  # noqa: F401
    BEGIN_NAME,
    END_NAME,
    TILE_SIZE,
    Chunk,
    ClipFile,
    ClipLayerPixels,
    LayerNode,
    OffscreenAttribute,
    _COMPOSITE_MAP,
    _composite_name,
    _edge_margin,
    _FILTER_NAMES,
    _offscreen_tile_index,
    _parsed_attribute,
    _warn_offscreen_failure,
    _warn_unknown_composite,
    _warn_unknown_tile_size,
    _warn_unsupported_filter,
    _warned_composites,
    _warned_filters,
    _warned_offscreens,
    _warned_tile_sizes,
    find_layer_node,
    iter_tiles,
    layer_offscreen_info,
    open_clip,
    open_clip_bytes,
    parse_container,
    parse_offscreen_attribute,
    render_canvas_preview_rgba,
    walk_layer_nodes,
)
from tmiref.clip_io import (  # noqa: F401
    _clone_sqlite_connection,
    _emit_container_with_sqli_payload,
    _serialize_sqlite,
)
from tmiref.composite import (  # noqa: F401
    BLEND_NORMAL,
    BLEND_PASS_THROUGH,
    _canvas_size,
    _clip_buffer_to_bounds,
    _CompositeSurface,
    _dilate_bounds,
    _EMPTY_LAYER_BUFFER,
    _EmptyLayerBuffer,
    _intersect_bounds,
    _is_empty_buffer,
    _LayerBuffer,
    _mask_buffer_by_base_alpha,
    _mask_by_alpha,
    _trim_buffer,
    _union_bounds,
)

# ``TMI_RENDER_TRACE=<path>``: append a JSON line per pre-rendered sibling and
# per sibling composited (see render_trace.py). Read once at import, so an
# unset variable costs one flag test per group.
_TRACE = bool(os.environ.get("TMI_RENDER_TRACE"))


# ───────────────────────────── offscreen decode ─────────────────────────────

def render_offscreen_from_info(clip: Any, info: tuple[int, bytes, str] | None) -> ClipLayerPixels | None:
    """Render an Offscreen from a prefetched ``layer_offscreen_info`` result.

    Split from the SQLite lookup so callers can fetch ``info`` once on the main
    thread and run this pure ``clip.raw`` → image work on a worker thread (the
    embedded SQLite connection is single-thread only). This path touches no
    database state.

    Returns None for missing/unparseable offscreens or when something in the
    tile payload chain raises; the caller treats None as "no pixels", which is
    the right thing for text/vector/3D/gradient layers that don't have a
    rasterised cache, and for masks whose data is malformed."""
    if not info:
        return None
    offscreen_id, attr_blob, outer_id = info
    if outer_id not in clip.extas:
        return None
    try:
        attr = parse_offscreen_attribute(attr_blob)
        image = Image.new("RGBA", (attr.tile_cols * TILE_SIZE, attr.tile_rows * TILE_SIZE), (0, 0, 0, 0))
        for tile in iter_tiles(clip.raw, clip.extas[outer_id]):
            if tile.data is None:
                continue
            col = tile.block_index % attr.tile_cols
            row = tile.block_index // attr.tile_cols
            tile_image = _tile_to_image(tile)
            if tile_image.getchannel("A").getbbox() is None:
                continue
            image.alpha_composite(tile_image, (col * TILE_SIZE, row * TILE_SIZE))
        image = image.crop((0, 0, attr.pixel_width, attr.pixel_height))
        return ClipLayerPixels(
            image=image,
            pixel_width=attr.pixel_width,
            pixel_height=attr.pixel_height,
            tile_cols=attr.tile_cols,
            tile_rows=attr.tile_rows,
            channel_profile=attr.channel_profile,
            outer_id=outer_id,
            offscreen_id=offscreen_id,
        )
    except Exception as exc:
        _warn_offscreen_failure(outer_id, exc)
        return None


def _render_offscreen_buffer(clip: Any, mipmap_id: int | None) -> _LayerBuffer | _EmptyLayerBuffer | None:
    prefetched = getattr(clip, "offscreen_prefetch", None)
    job = prefetched.pop(mipmap_id, None) if prefetched else None
    if job is not None:
        future, outer_id, _pool = job
        try:
            return future.result()
        except Exception as exc:
            _warn_offscreen_failure(outer_id, exc)
            return None
    return _render_offscreen_buffer_from_info(clip, layer_offscreen_info(clip, mipmap_id))


def _render_offscreen_buffer_from_info(
    clip: Any,
    info: tuple[int, bytes, str] | None,
    box: tuple[int, int, int, int] | None = None,
) -> _LayerBuffer | _EmptyLayerBuffer | None:
    """Render an offscreen into the smallest tile-bounded RGBA buffer.

    ``render_offscreen_from_info`` intentionally preserves the historical public
    shape (an image sized to the full offscreen). Composite rendering only needs
    the painted rectangle, so this path avoids allocating and blending a full
    canvas-sized image for every CLIP layer.

    ``box`` (offscreen pixel coordinates) decodes only the tiles that overlap
    it. Tiles decode independently, so inside ``box`` the pixels are those of
    the whole decode; outside it the buffer may hold other tiles or nothing.
    """
    if not info:
        return None
    if info[2] not in clip.extas:
        return None
    try:
        return _decode_offscreen_buffer(clip, info, box)
    except Exception as exc:
        _warn_offscreen_failure(info[2], exc)
        return None


def _decode_offscreen_buffer(
    clip: Any,
    info: tuple[int, bytes, str],
    box: tuple[int, int, int, int] | None = None,
) -> _LayerBuffer | _EmptyLayerBuffer:
    """``_render_offscreen_buffer_from_info`` for an offscreen that exists,
    raising where that warns. No database access (thread-safe)."""
    _offscreen_id, attr_blob, outer_id = info
    attr = _parsed_attribute(attr_blob)
    if box is not None:
        clip = _offscreen_tiles_in_box(clip, outer_id, attr, box)
        if clip is None:
            return _EMPTY_LAYER_BUFFER
    return _python_offscreen_buffer(clip, outer_id, attr)


def _prefetch_offscreen_buffers(clip: Any, nodes: list[Any]):
    """Start decoding the stored render offscreens of ``nodes`` (leaf layers
    about to be composited) on a thread pool. ``_render_offscreen_buffer``
    then takes each result instead of decoding on the calling thread; a
    decode error is warned about there, in the usual order. Returns the pool
    (shut down with ``_end_offscreen_prefetch``), or None when nothing is
    prefetched.

    The database lookups run here, on the calling thread (the connection is
    single-thread only); the tile decode itself touches no database state."""
    pending = getattr(clip, "offscreen_prefetch", None)
    if pending is None or getattr(clip, "decodable_layers", None):
        # (A windowed render decodes only some tiles of these layers.)
        return None
    jobs: dict[int, tuple[int, bytes, str]] = {}
    for node in nodes:
        mipmap = getattr(node, "render_mipmap", None)
        if not mipmap or mipmap in pending or mipmap in jobs:
            continue
        info = layer_offscreen_info(clip, mipmap)
        if info and info[2] in clip.extas:
            jobs[mipmap] = info
    if len(jobs) < 2:
        return None
    from concurrent.futures import ThreadPoolExecutor

    pool = ThreadPoolExecutor(max_workers=min(len(jobs), 16, os.cpu_count() or 1))
    for mipmap, info in jobs.items():
        pending[mipmap] = (pool.submit(_decode_offscreen_buffer, clip, info), info[2], pool)
    return pool


def _end_offscreen_prefetch(clip: Any, pool: Any) -> None:
    """Drop what ``pool`` prefetched and was not taken, and shut it down."""
    if pool is None:
        return
    pending = clip.offscreen_prefetch
    for mipmap, (future, _outer_id, owner) in list(pending.items()):
        if owner is pool:
            future.cancel()
            del pending[mipmap]
    pool.shutdown(wait=True)


@dataclass
class _ExtaView:
    """Just enough of a ``ClipFile`` (``raw`` + ``extas``) for the tile decoders."""

    raw: bytes
    extas: dict[str, Chunk]


def _tile_rows(clip: Any, outer_id: str, cols: int) -> dict[int, list[int]]:
    """Tile row -> indices into ``_offscreen_tile_index``'s records."""
    cache = getattr(clip, "tile_index_cache", None)
    key = ("rows", outer_id, cols)
    if cache is not None and key in cache:
        return cache[key]
    rows: dict[int, list[int]] = {}
    for k, (block, _off, _size) in enumerate(_offscreen_tile_index(clip, outer_id)[2]):
        rows.setdefault(block // cols, []).append(k)
    if cache is not None:
        cache[key] = rows
    return rows


def _offscreen_tiles_in_box(
    clip: Any,
    outer_id: str,
    attr: OffscreenAttribute,
    box: tuple[int, int, int, int],
    *,
    invert: bool = False,
) -> _ExtaView | None:
    """A stand-in clip whose ``outer_id`` payload keeps only the tile records
    overlapping ``box`` (with ``invert``: those not overlapping it), in file
    order, duplicates included; None when no tile does."""
    header, inner, records = _offscreen_tile_index(clip, outer_id)
    cols = max(1, attr.tile_cols)
    if invert:
        candidates = range(len(records))
    else:
        # Only the tile rows the box spans (a band is a few rows of a tall
        # layer), back in file order.
        by_row = _tile_rows(clip, outer_id, cols)
        first, last = box[1] // TILE_SIZE - 1, (box[3] - 1) // TILE_SIZE
        candidates = sorted(k for row, ks in by_row.items() if first <= row <= last for k in ks)
    picked = []
    for k in candidates:
        block, off, size = records[k]
        tx, ty = (block % cols) * TILE_SIZE, (block // cols) * TILE_SIZE
        if (tx < box[2] and ty < box[3] and tx + TILE_SIZE > box[0] and ty + TILE_SIZE > box[1]) != invert:
            picked.append(inner[off:off + size])
    if not picked:
        return None
    body = b"".join(picked)
    payload = header + struct.pack(">Q", len(body)) + body
    return _ExtaView(payload, {outer_id: Chunk(b"Exta", 0, 0, len(payload))})


def _python_offscreen_buffer(clip: Any, outer_id: str, attr: Any) -> _LayerBuffer | _EmptyLayerBuffer:
    offscreen_bounds = (0, 0, attr.pixel_width, attr.pixel_height)
    union: tuple[int, int, int, int] | None = None
    for tile_bounds in _iter_filled_tile_bounds(clip.raw, clip.extas[outer_id], attr.tile_cols):
        tile_bounds = _intersect_bounds(tile_bounds, offscreen_bounds)
        if tile_bounds is None:
            continue
        union = tile_bounds if union is None else _union_bounds(union, tile_bounds)

    if union is None:
        return _EMPTY_LAYER_BUFFER

    image = Image.new("RGBA", (union[2] - union[0], union[3] - union[1]), (0, 0, 0, 0))
    for tile in iter_tiles(clip.raw, clip.extas[outer_id]):
        if tile.data is None:
            continue
        col = tile.block_index % attr.tile_cols
        row = tile.block_index // attr.tile_cols
        tile_x = col * TILE_SIZE
        tile_y = row * TILE_SIZE
        tile_buf = _LayerBuffer(_tile_to_image(tile), tile_x, tile_y)
        tile_buf = _clip_buffer_to_bounds(tile_buf, offscreen_bounds)
        if tile_buf is None:
            continue
        tile_buf = _trim_buffer(tile_buf)
        if tile_buf is None:
            continue
        image.alpha_composite(tile_buf.image, (tile_buf.x - union[0], tile_buf.y - union[1]))
    return _trim_buffer(_LayerBuffer(image, union[0], union[1])) or _EMPTY_LAYER_BUFFER


def _iter_filled_tile_bounds(data: bytes, exta: Any, tile_cols: int):
    payload = data[exta.payload_off:exta.payload_off + exta.payload_size]
    if payload[8:16] != b"extrnlid":
        raise ValueError("Exta payload missing extrnlid tag")
    inner_size = struct.unpack(">Q", payload[48:56])[0]
    inner = payload[56:56 + inner_size]
    p = 0
    while p + 8 <= len(inner):
        rec_size = struct.unpack(">I", inner[p:p + 4])[0]
        name_len = struct.unpack(">I", inner[p + 4:p + 8])[0]
        if name_len * 2 + 8 > rec_size or p + rec_size > len(inner):
            break
        name = inner[p + 8:p + 8 + 2 * name_len].decode("utf-16-be")
        if name != BEGIN_NAME:
            p += rec_size
            continue
        body = inner[p + 46:p + rec_size]
        block_index = struct.unpack(">I", body[0:4])[0]
        tile_w = struct.unpack(">I", body[8:12])[0]
        tile_h = struct.unpack(">I", body[12:16])[0]
        has_data = struct.unpack(">I", body[16:20])[0]
        if has_data:
            col = block_index % tile_cols
            row = block_index // tile_cols
            yield (
                col * TILE_SIZE,
                row * TILE_SIZE,
                col * TILE_SIZE + tile_w,
                row * TILE_SIZE + tile_h,
            )
        p += rec_size


def _tile_to_image(tile: Any) -> Image.Image:
    """Convert one decompressed CLIP tile into a Pillow RGBA image.

    Supported encodings:
      - ``n/8``  bytes — 1 bit/pixel binary mask (used by some mask/selection layers)
      - ``n``    bytes — single-channel 8-bit alpha; painted as opaque white
      - ``2*n``  bytes — planar ``[alpha][gray]`` (8-bit each); used by line-art mono layers
      - ``5*n``  bytes — planar ``[alpha][BGRA pixels with padding]``; standard colour layer

    Anything else degrades gracefully to a transparent tile with a one-time warning.
    Returning a transparent tile preserves the surrounding composite instead of
    aborting the whole render for one unrecognised raster.
    """
    size = (tile.tile_w, tile.tile_h)
    if tile.data is None:
        return Image.new("RGBA", size, (0, 0, 0, 0))
    pixels = tile.tile_w * tile.tile_h
    bpt = tile.bytes_per_tile
    if bpt == pixels // 8:
        # 1 bit per pixel — Pillow's "1" mode unpacks to L (0 or 255)
        mono = Image.new("1", size)
        mono.frombytes(tile.data)
        mono = mono.convert("L")
        image = Image.new("RGBA", size, (255, 255, 255, 0))
        image.putalpha(mono)
        return image
    if bpt == pixels // 4:
        # planar [alpha-1bpp (n/8 bytes)][gray-1bpp (n/8 bytes)] — used by tone
        # / text-heavy mono layers where neither alpha nor gray needs grayscale
        # interpolation.
        half = pixels // 8
        alpha_bit = Image.new("1", size)
        alpha_bit.frombytes(tile.data[:half])
        gray_bit = Image.new("1", size)
        gray_bit.frombytes(tile.data[half:half * 2])
        alpha = alpha_bit.convert("L")
        gray = gray_bit.convert("L")
        return Image.merge("RGBA", (gray, gray, gray, alpha))
    if bpt == pixels:
        image = Image.new("RGBA", size, (255, 255, 255, 0))
        image.putalpha(Image.frombytes("L", size, tile.data))
        return image
    if bpt == pixels * 2:
        alpha = Image.frombytes("L", size, tile.data[:pixels])
        gray = Image.frombytes("L", size, tile.data[pixels:pixels * 2])
        return Image.merge("RGBA", (gray, gray, gray, alpha))
    if bpt == pixels * 5:
        alpha = Image.frombytes("L", size, tile.data[:pixels])
        image = Image.frombytes("RGBA", size, tile.data[pixels:], "raw", "BGRA")
        image.putalpha(alpha)
        return image
    _warn_unknown_tile_size(bpt, pixels)
    return Image.new("RGBA", size, (0, 0, 0, 0))


# ───────────────────────────── compositing ─────────────────────────────
#
# Blend ids map to names in tmi (``_COMPOSITE_MAP``). 10 覆い焼き(発光) and
# 12 加算(発光) are CSP-only "glow" variants (see ``_blend_pixels``).
_GLOW_MODES = {"glow_dodge", "glow_add"}


def render_layered_composite_rgba(
    clip: Any,
    background=(255, 255, 255, 255),
    *,
    region: tuple[int, int, int, int] | None = None,
) -> Image.Image:
    """Composite raster-backed CLIP layers without using CanvasPreview.

    ``region`` (canvas pixels, x0 y0 x1 y1) renders just that window and
    returns an image of its size holding exactly the pixels the whole-canvas
    render has there; the whole canvas is the special case ``region=None``.

    That is exact because every compositing step is a function of the pixel's
    own layer stack at its absolute position: blend modes, opacity, layer
    masks, clipping, pass-through mixing, correction layers, トーン (screened
    on an absolute lattice) and ordered-dither fills (absolute phase). Only
    fully transparent pixels can carry stray colour, and no step lets the
    colour of a transparent pixel reach a visible one; a blend leaves the
    backdrop exactly alone under a transparent source pixel. The steps that
    do look beyond the pixel are handled explicitly:
      * フチ (EffectEdge) reads neighbours, so a layer or folder with one is
        rendered over the window grown by the outline width (see
        ``_edge_margin``) before its outline is drawn;
      * layers whose pixels are generated from their own placement are
        generated as for the whole canvas (``full_bounds``): vector strokes
        and comic-frame masks and borders are drawn whole (once per render,
        see ``render_session``) and cropped, or, for a vector layer in a
        single window, only the strokes that reach the window are drawn;
        image materials are resampled per destination pixel from absolute
        coordinates, so they are resampled over the window alone.

    A large whole-canvas render is done as horizontal bands of exactly such
    windows, on a thread pool (``_render_banded``), and stitched.
    """
    full = (0, 0, clip.canvas.width, clip.canvas.height)
    window = full if region is None else _intersect_bounds(tuple(region), full)
    if window is None:
        raise ValueError(f"render region {region} lies outside the {full[2]}x{full[3]} canvas")
    return _render_layered_python(clip, background, window, full)


def render_layered_regions(
    clip: Any,
    regions: list[tuple[int, int, int, int]],
    background=(255, 255, 255, 255),
) -> list[Image.Image]:
    """``render_layered_composite_rgba(clip, background, region=r)`` for each
    of ``regions`` (inside the canvas), sharing one render session."""
    full = (0, 0, clip.canvas.width, clip.canvas.height)
    windows = []
    for region in regions:
        window = _intersect_bounds(tuple(region), full)
        if window is None:
            raise ValueError(f"render region {region} lies outside the {full[2]}x{full[3]} canvas")
        windows.append(window)
    if not windows:
        return []
    out = []
    for window in windows:
        if window == full:
            out.append(render_layered_composite_rgba(clip, background))
        else:
            with render_session(clip):
                out.append(_render_window(clip, background, window, full))
    return out


def _render_layered_python(clip: Any, background, window, full) -> Image.Image:
    if window == full:
        # (One pass draws each generated layer once anyway: no memo needed.)
        banded = _render_banded(clip, background)
        return banded if banded is not None else _render_window(clip, background, window, full)
    with render_session(clip):
        return _render_window(clip, background, window, full)


def render_folder(clip: Any, main_id: int) -> Image.Image:
    """A folder's contents composited alone on a transparent canvas
    (``tmi.formats.clip.render_folder``)."""
    node = next((n for n in walk_layer_nodes(clip.root.children) if n.main_id == main_id), None)
    if node is None or not node.folder:
        raise ValueError(f"not a folder: clip:{main_id}")
    surface = _CompositeSurface.fixed_canvas(Image.new("RGBA", (clip.canvas.width, clip.canvas.height), (0, 0, 0, 0)))
    _composite_group_surface(clip, surface, node.children)
    return surface.image


def render_full(clip: Any) -> tuple[Image.Image, bool]:
    """The whole-canvas composite, and whether every layer's stored pixels
    decoded (``tmi.formats.clip_partial.render_full``)."""
    previous = getattr(clip, "decode_failures", None)
    clip.decode_failures = failures = set()
    try:
        image = render_layered_composite_rgba(clip)
    finally:
        clip.decode_failures = previous
    return image, not failures


def _render_window(
    clip: Any,
    background,
    window: tuple[int, int, int, int],
    full: tuple[int, int, int, int],
) -> Image.Image:
    canvas = Image.new("RGBA", _canvas_size(window), background)
    surface = _CompositeSurface(
        window, image=canvas, x=window[0], y=window[1], fixed=True, full_bounds=full, owned=True
    )
    _composite_group_surface(clip, surface, clip.root.children)
    return surface.image


# ───────────────────────────── render sessions ─────────────────────────────


class _RenderMemo:
    """Values computed once per render and shared by all of its windows and
    bands: whole-canvas vector layers and comic-frame shapes, decoded masks
    and material originals. ``get`` is thread-safe and computes each key
    once; concurrent callers of a key being computed wait for it, and an
    exception reaches every caller of that key (as each would have raised
    it computing on its own). Values are read-only once stored."""

    def __init__(self, banded: bool = False, band_rows: int = 0) -> None:
        self.banded = banded
        self.band_rows = band_rows
        self._lock = threading.Lock()
        self._slots: dict = {}
        # ``TMI_RENDER_TRACE`` in a banded render: each band's records, keyed
        # by band, written in band order once all bands are done.
        self.trace_blocks: dict | None = None

    def get(self, key, compute):
        with self._lock:
            slot = self._slots.get(key)
            owner = slot is None
            if owner:
                slot = self._slots[key] = [threading.Event(), None, None]
        if owner:
            try:
                slot[1] = compute()
            except BaseException as exc:
                slot[2] = exc
            finally:
                slot[0].set()
        else:
            slot[0].wait()
        if slot[2] is not None:
            raise slot[2]
        return slot[1]


def _memoized(clip: Any, key, compute):
    """``compute()``, shared through the running render's memo if any."""
    memo = getattr(clip, "render_memo", None)
    if memo is None:
        return compute()
    return memo.get(key, compute)


class render_session:
    """Context manager: renders of ``clip`` inside share one ``_RenderMemo``
    (e.g. the windows of an incremental composite). Nested sessions reuse
    the outer one; the memo is dropped when the outermost session ends."""

    def __init__(self, clip: Any, banded: bool = False, band_rows: int = 0) -> None:
        self.clip = clip
        self.banded = banded
        self.band_rows = band_rows
        self.created = False

    def __enter__(self):
        if self.clip is not None and getattr(self.clip, "render_memo", None) is None:
            try:
                self.clip.render_memo = _RenderMemo(self.banded, self.band_rows)
                self.created = True
            except AttributeError:
                pass  # a stand-in clip without the field
        return self

    def __exit__(self, *exc) -> None:
        if self.created:
            self.clip.render_memo = None


# ───────────────────────────── banded renders ─────────────────────────────
#
# A whole-canvas composite is rendered as horizontal bands, each an exact
# window (see ``render_layered_composite_rgba``), on a thread pool, and pasted
# together. The heavy steps release the GIL (tile decode, blend / regrade
# kernels, numpy), and every intermediate buffer is the size of a band rather
# than of the canvas, so the peak memory follows the band size. Bands are
# whole multiples of the 256-pixel tile, so a layer at a tile-aligned offset
# decodes each tile once.

_BAND_MIN_PIXELS = 4 << 20      # smaller canvases: one pass
_BAND_TARGET_PIXELS = 1 << 21   # about this many pixels per band
_BAND_WORKERS = 8

# ``active`` in band worker threads: the per-call numpy thread pools stay
# serial there (the bands already keep the cores busy).
_band_thread = threading.local()


def _in_band_worker() -> bool:
    return getattr(_band_thread, "active", False)


def _band_layout(width: int, height: int, workers: int) -> list[tuple[int, int, int, int]]:
    rows = max(1, _BAND_TARGET_PIXELS // max(1, width))
    # Enough bands to keep the workers busy when the canvas allows it.
    rows = min(rows, -(-height // (2 * workers)))
    rows = max(TILE_SIZE, rows // TILE_SIZE * TILE_SIZE)
    return [(0, y, width, min(height, y + rows)) for y in range(0, height, rows)]


def _render_banded(clip: Any, background) -> Image.Image | None:
    """The whole-canvas composite rendered as bands, or None when the canvas
    is small or the bands cannot be shown to equal the one-pass render (a
    layer's stored pixels do not decode: the one-pass render then drops the
    layer whole, and records it in ``decode_failures``)."""
    if not isinstance(clip, ClipFile) or clip.render_memo is not None:
        return None
    width, height = clip.canvas.width, clip.canvas.height
    if width * height < _BAND_MIN_PIXELS:
        return None
    workers = min(_BAND_WORKERS, os.cpu_count() or 1)
    bands = _band_layout(width, height, workers)
    if len(bands) < 2:
        return None
    leaves = [n for n in walk_layer_nodes(clip.root.children) if not n.folder]
    if not _stored_pixels_outside_canvas_decode(clip, leaves):
        return None

    from tmiref import clip_style

    clip_style.layer_style(clip, -1)  # fills the per-layer style cache up front
    saved = (clip.db, clip.decodable_layers, clip.window_decode_failures)
    shared_db = _SharedConnection(clip.db) if workers > 1 else None
    full = (0, 0, width, height)
    canvas = Image.new("RGBA", (width, height), background)
    try:
        if shared_db is not None:
            clip.db = shared_db
        clip.decodable_layers = frozenset(n.main_id for n in leaves)
        clip.window_decode_failures = failures = set()
        with render_session(clip, banded=True, band_rows=bands[0][3] - bands[0][1]):
            trace_blocks = None
            if _TRACE:
                trace_blocks = clip.render_memo.trace_blocks = {}
            if workers == 1:
                for band in bands:
                    canvas.paste(_render_window(clip, background, band, full), band[:2])
            else:
                _render_bands_on_pool(clip, background, bands, full, canvas, workers)
    finally:
        clip.db, clip.decodable_layers, clip.window_decode_failures = saved
        if shared_db is not None:
            shared_db.close()
    if failures:
        return None  # (the one-pass render that follows traces itself)
    if trace_blocks:
        from tmiref import render_trace

        render_trace.write_blocks(trace_blocks)
    return canvas


def _render_bands_on_pool(clip, background, bands, full, canvas: Image.Image, workers: int) -> None:
    from concurrent.futures import ThreadPoolExecutor, as_completed

    def render(band):
        _band_thread.active = True
        try:
            return band, _render_window(clip, background, band, full)
        finally:
            _band_thread.active = False

    def warm(compute):
        _band_thread.active = True
        try:
            compute()
        except Exception:
            pass  # (stored in the memo; the band that needs it reports it)
        finally:
            _band_thread.active = False

    with ThreadPoolExecutor(max_workers=workers) as pool:
        for compute in _band_warmups(clip, full):
            pool.submit(warm, compute)
        for future in as_completed([pool.submit(render, band) for band in bands]):
            band, image = future.result()
            canvas.paste(image, band[:2])


def _band_warmups(clip: Any, full: tuple[int, int, int, int]) -> list:
    """What the bands will share and is slow to make, as thunks for the pool
    to start on before the bands reach it: every band needs a comic frame's
    shape or a long-stroked vector layer from its first layers on, and would
    otherwise wait at it in turn (the memo computes each key once)."""
    from tmiref import clip_frame, clip_style, clip_vector

    out = []

    def walk(nodes):
        for node in nodes:
            if not getattr(node, "visible", True):
                continue
            if node.folder:
                if getattr(node, "comic_frame", False):
                    out.append(lambda n=node: clip_frame.shared_frame_border(clip, n, full))
                    out.append(lambda n=node: clip_frame.frame_mask_crop(clip, n, (0, 0, 1, 1), full))
                walk(node.children)
                continue
            info = layer_offscreen_info(clip, node.render_mipmap)
            if info and info[2] in clip.extas:
                continue  # stored pixels
            if clip_style.layer_style(clip, node.main_id).get("ResizableOriginalMipmap"):
                continue  # materials are resampled per band
            if clip_vector.has_vector_strokes(clip, node):
                out.append(lambda n=node: _warm_vector_layer(clip, n, full))

    walk(clip.root.children)
    return out


def _warm_vector_layer(clip: Any, node: Any, full: tuple[int, int, int, int]) -> None:
    if _vector_layer_shared(clip, node, full):
        _shared_vector_layer(clip, node, full)


def _stored_pixels_outside_canvas_decode(clip: Any, leaves: list[Any]) -> bool:
    """Whether the tiles of ``leaves``' render offscreens that lie outside
    the canvas (which no band decodes, but a whole decode does) all decode.
    Usually there are none."""
    width, height = clip.canvas.width, clip.canvas.height
    for node in leaves:
        info = layer_offscreen_info(clip, node.render_mipmap)
        if not info or info[2] not in clip.extas:
            continue
        try:
            attr = parse_offscreen_attribute(info[1])
            dx = node.offset_x + (getattr(node, "render_offset_x", 0) or 0)
            dy = node.offset_y + (getattr(node, "render_offset_y", 0) or 0)
            outside = _offscreen_tiles_in_box(clip, info[2], attr, (-dx, -dy, width - dx, height - dy), invert=True)
            if outside is not None:
                _decode_offscreen_buffer(outside, info)
        except Exception:
            return False
    return True


class _FetchedRows:
    def __init__(self, rows: list) -> None:
        self._rows = rows

    def fetchone(self):
        return self._rows[0] if self._rows else None

    def fetchall(self) -> list:
        return self._rows

    def __iter__(self):
        return iter(self._rows)


class _SharedConnection:
    """A ClipFile database usable from the band threads: a copy of it opened
    without sqlite3's same-thread check, every query run under one lock and
    fetched whole (sqlite3 connections are not safe for concurrent cursors).
    Rendering only reads."""

    def __init__(self, source: sqlite3.Connection) -> None:
        self._con = sqlite3.connect(":memory:", check_same_thread=False)
        source.backup(self._con)
        self._lock = threading.Lock()

    def execute(self, sql: str, params=()) -> _FetchedRows:
        with self._lock:
            return _FetchedRows(self._con.execute(sql, params).fetchall())

    def cursor(self) -> "_SharedConnection":
        return self

    def close(self) -> None:
        self._con.close()


def _composite_group(clip: Any, target: Image.Image, layers: list[Any]) -> None:
    surface = _CompositeSurface.fixed_canvas(target)
    _composite_group_surface(clip, surface, layers)


def _composite_group_surface(clip: Any, target: _CompositeSurface, layers: list[Any]) -> None:
    """Composite a sibling group onto ``target`` (bottom-first sibling order).

    Each sibling is one of:
      - leaf raster layer  → render its pixels, apply layer-mask, blend onto target
      - folder (normal)    → composite children into an isolated buffer, mask &
                             opacity-attenuate the buffer, blend onto target
      - folder (pass-through, composite=30) → composite children directly onto
                             target; if the folder has its own opacity/mask, mix
                             the post-folder canvas with the pre-folder snapshot
                             using opacity * mask as the mix weight.

    Clipping (``LayerClip``): the nearest earlier non-clipped sibling and the
    layers clipped to it form one group, composited in the base's place with
    the base's mode and opacity (see ``_composite_group_body``).
    """
    if _TRACE:
        from tmiref import render_trace

        with render_trace.group(clip, target) as trace:
            _composite_group_body(clip, target, layers, trace)
        return
    _composite_group_body(clip, target, layers, None)


def _composite_group_body(clip: Any, target: _CompositeSurface, layers: list[Any], trace: Any) -> None:
    """``_composite_group_surface``; ``trace`` (a ``render_trace.Group``)
    records each step when ``TMI_RENDER_TRACE`` is set, else None."""
    n = len(layers)
    # Pre-render each visible node into a tight RGBA rectangle. None means
    # invisible/unrenderable; _EMPTY_LAYER_BUFFER means renderable but fully
    # transparent, which still matters as a clipping base. Pass-through folders
    # cannot be pre-rendered because they need to mutate the target surface in
    # place, so they are handled in the second pass below.
    rendered: list[_LayerBuffer | _EmptyLayerBuffer | None] = [None] * n
    pass_through_indices: set[int] = set()
    adjustments: dict[int, Any] = {}

    # The visible leaf layers' stored pixels decode on a pool meanwhile.
    prefetch = _prefetch_offscreen_buffers(clip, [
        node for node in layers
        if getattr(node, "visible", True) and not node.folder and getattr(node, "type", 0) != 4098
    ])
    try:
        _prerender_group(clip, target, layers, rendered, pass_through_indices, adjustments)
    finally:
        _end_offscreen_prefetch(clip, prefetch)
    if trace is not None:
        for i, node in enumerate(layers):
            if rendered[i] is not None:
                trace.buffer("prerender", node, rendered[i])

    # Clipping: CSP draws a clipping base (the nearest earlier non-clipped
    # sibling, its own mask applied) and the layers clipped to it as one
    # isolated group (fitted on probe_clip_*, §15.7): each clipped layer
    # blends onto the base's colour as if the base were opaque, a clipped
    # correction layer re-grades it, the base's alpha stays, and the group
    # is composited in the base's place with the base's mode and opacity.
    # The clipped layers themselves are then not composited.
    for i, node in enumerate(layers):
        if not getattr(node, "clip", 0) or (rendered[i] is None and i not in adjustments):
            continue
        base_idx = _clipping_base_index(layers, i)
        if base_idx is None:
            # Nothing below it in this folder to clip to: CSP 5.1.5 then draws
            # the layer unclipped (verified on a clipping layer at the bottom of
            # a folder in example/csp515/structure.clip).
            continue
        base = rendered[base_idx]
        if i in adjustments:
            filter_fn = adjustments.pop(i)
            if base is not None and not _is_empty_buffer(base):
                rendered[base_idx] = base = _fold_adjustment(clip, node, filter_fn, base, target)
        else:
            if base is not None and not _is_empty_buffer(base) and not _is_empty_buffer(rendered[i]):
                mode = _composite_name(node.composite or 0)
                rendered[base_idx] = base = _fold_clipped(base, rendered[i], mode, (node.opacity or 0) / 256.0)
            rendered[i] = None
        if trace is not None:
            trace.buffer("clip", node, _EMPTY_LAYER_BUFFER if base is None else base)

    # Composite onto target in sibling order (bottom first).
    for i, node in enumerate(layers):
        applied = False
        try:
            if i in adjustments:
                applied = True
                _apply_adjustment_to_surface(clip, target, node, adjustments[i], rendered, layers, i)
                continue
            if i in pass_through_indices:
                applied = True
                _composite_pass_through_folder(clip, target, node)
                continue
            buf = rendered[i]
            opacity = (node.opacity or 0) / 256.0
            mode = _composite_name(node.composite or 0)
            if buf is None or _is_empty_buffer(buf):
                continue
            applied = True
            _blend_buffer_onto_surface(target, buf, mode, opacity)
        except Exception as exc:
            _warn_layer_failure(node, exc)
        finally:
            if trace is not None and applied:
                trace.surface("apply", node, target)
    target.trim()


def _fold_clipped(base: _LayerBuffer, source: _LayerBuffer, mode: str, opacity: float) -> _LayerBuffer:
    """The clipping group ``base`` with ``source`` blended onto its colour as
    over an opaque backdrop (CSP's clipping, see ``_composite_group_body``);
    the base's alpha and rectangle stay."""
    overlap = _intersect_bounds(source.bounds, base.bounds)
    if overlap is None:
        return base
    x0, y0, x1, y1 = overlap
    group = np.array(_rgba_array(base.image))
    region = group[y0 - base.y:y1 - base.y, x0 - base.x:x1 - base.x]
    backdrop = region.copy()
    backdrop[..., 3] = 255
    src = _rgba_array(source.image)[y0 - source.y:y1 - source.y, x0 - source.x:x1 - source.x]
    region[..., :3] = _blend_pixels(backdrop, src, mode, opacity)[..., :3]
    return _LayerBuffer(Image.fromarray(group, "RGBA"), base.x, base.y)


def _fold_adjustment(clip: Any, node: Any, filter_fn, base: _LayerBuffer, target: _CompositeSurface) -> _LayerBuffer:
    """The clipping group ``base`` with its colour re-graded by the clipped
    correction layer ``node`` (its opacity and mask; the base's alpha stays)."""
    opacity = (node.opacity or 0) / 256.0
    if opacity <= 0:
        return base
    raw = _rgba_array(base.image)
    h, w = raw.shape[:2]
    mask = _layer_mask_crop(clip, node, base.bounds, target.canvas_bounds)
    mask_plane = np.asarray(mask) if mask is not None and mask.size == (w, h) else None
    out = _regrade_pixels(filter_fn, raw, opacity, mask_plane, None)
    out[..., 3] = raw[..., 3]
    return _LayerBuffer(Image.fromarray(out, "RGBA"), base.x, base.y)


def _rgba_array(image: Image.Image):
    """A read-only (h, w, 4) uint8 array of an RGBA image's pixels."""
    return np.frombuffer(image.tobytes(), np.uint8).reshape(image.height, image.width, 4)


def _prerender_group(
    clip: Any,
    target: _CompositeSurface,
    layers: list[Any],
    rendered: list,
    pass_through_indices: set[int],
    adjustments: dict[int, Any],
) -> None:
    """The pre-render pass of ``_composite_group_surface``: fills ``rendered``
    (and the pass-through / adjustment indices) for each sibling."""
    full = target.full
    for i, node in enumerate(layers):
        if not getattr(node, "visible", True):
            continue
        adjustment = _decode_filter_layer(clip, node)
        if adjustment is not None:
            # Correction layer: it re-grades the backdrop at composite time
            # rather than contributing pixels, so it gets no pre-rendered buffer.
            adjustments[i] = adjustment
            continue
        mode = _composite_name(node.composite or 0)
        # The window this node's own pixels are needed over: the target's,
        # grown by a フチ outline's reach (whole-canvas renders stay whole).
        window = _dilate_bounds(target.canvas_bounds, _edge_margin(clip, node), full)
        try:
            if node.folder:
                if mode == BLEND_PASS_THROUGH:
                    pass_through_indices.add(i)
                    continue
                group = _CompositeSurface.empty(window, target.full_bounds)
                _composite_group_surface(clip, group, node.children)
                buf = group.as_buffer()
                if getattr(node, "comic_frame", False) and clip is not None:
                    # A コマ枠 saves no pixels for its mask or border lines; both
                    # are rebuilt from the frame's vector shape. An empty frame
                    # still draws its border.
                    from tmiref.clip_frame import apply_frame_to_group_buffer

                    rendered[i] = apply_frame_to_group_buffer(clip, node, buf, full, window)
                    continue
                if buf is None:
                    rendered[i] = _EMPTY_LAYER_BUFFER
                    continue
                rendered[i] = _apply_layer_mask_to_buffer(clip, node, buf, full)
            else:
                buf = _render_layer_buffer(clip, node, full, window)
                if buf is None or _is_empty_buffer(buf):
                    # No rasterised offscreen — try to synthesise from metadata
                    # (paper / fill / gradient layers).
                    if buf is None:
                        synthesized = _synthesize_layer(clip, node, _canvas_size(full), window)
                        # A fill transparent here (or everywhere) still exists:
                        # empty, not missing, as for a window's stored pixels.
                        buf = (_trim_buffer(synthesized) or _EMPTY_LAYER_BUFFER) if synthesized is not None else None
                    if buf is None:
                        continue
                    if _is_empty_buffer(buf):
                        rendered[i] = buf
                        continue
                rendered[i] = _apply_layer_mask_to_buffer(clip, node, buf, full)
        except Exception as exc:
            _warn_layer_failure(node, exc)
            rendered[i] = None


_warned_layers: set[int] = set()


def _warn_layer_failure(node: Any, exc: Exception) -> None:
    key = getattr(node, "main_id", id(node))
    if key in _warned_layers:
        return
    _warned_layers.add(key)
    import sys

    print(
        f"tmiki: skipping CLIP layer {key} ({getattr(node, 'name', '?')!r}): "
        f"{type(exc).__name__}: {exc}",
        file=sys.stderr,
    )


def _render_layer_buffer(
    clip: Any,
    node: Any,
    canvas_bounds: tuple[int, int, int, int],
    region: tuple[int, int, int, int] | None = None,
) -> _LayerBuffer | _EmptyLayerBuffer | None:
    """A leaf layer's pixels as CSP shows them: the stored render offscreen (or,
    for a 画像素材 / ベクター layer whose render CSP did not save, the placed
    original / the re-stroked vector) with expression / layer colour applied.

    ``region`` (default: the canvas) limits the result to that window; inside
    it the pixels are the whole-canvas ones. Generated layers are generated
    as for ``canvas_bounds`` (see ``render_layered_composite_rgba``) and
    cropped. None (no pixels at all) stays None, so the caller's synthesis
    fallback runs exactly as for the whole canvas."""
    window = canvas_bounds if region is None else region
    buf = _render_stored_buffer(clip, node, canvas_bounds, window)
    if clip is None:
        return buf
    from tmiref import clip_style

    style = clip_style.layer_style(clip, node.main_id)
    profile = None
    if buf is None and style.get("ResizableOriginalMipmap"):
        buf, profile = clip_style.render_image_material(clip, node, canvas_bounds, window)
        buf = _clip_layer_to_window(buf, window)
    elif buf is None:
        from tmiref import clip_vector

        if clip_vector.has_vector_strokes(clip, node):
            buf, profile = _render_vector_buffer(clip, node, canvas_bounds, window)
            buf = _clip_layer_to_window(buf, window)
    elif buf is not None and not _is_empty_buffer(buf) and clip_style.needs_profile(style):
        profile = clip_style.offscreen_profile(clip, node.render_mipmap)
    if buf is None or _is_empty_buffer(buf) or not clip_style.needs_profile(style):
        return buf
    image = clip_style.apply_layer_style(buf.image, style, profile)
    return buf if image is buf.image else _LayerBuffer(image, buf.x, buf.y)


def _render_vector_buffer(
    clip: Any,
    node: Any,
    canvas_bounds: tuple[int, int, int, int],
    window: tuple[int, int, int, int],
):
    """A vector layer's ``(buffer, profile)`` holding at least ``window``.

    A window draws only the strokes that reach it. The bands of a banded
    render share one drawing of the whole layer instead: drawing is mostly
    Python per stroke, so bands drawing their own strokes side by side
    mainly wait for the GIL (lolita page0001: 29 s against 23 s)."""
    from tmiref import clip_vector

    if window != canvas_bounds and _vector_layer_shared(clip, node, canvas_bounds):
        sparse, profile = _shared_vector_layer(clip, node, canvas_bounds)
        if sparse is None or _is_empty_buffer(sparse):
            return sparse, profile
        # As the whole layer cropped to the window and trimmed would be.
        part = sparse.buffer(window)
        return (_EMPTY_LAYER_BUFFER if part is None else _trim_buffer(part) or _EMPTY_LAYER_BUFFER), profile
    return clip_vector.render_vector_layer_buffer(
        clip, node, canvas_bounds, None if window == canvas_bounds else window
    )


def _vector_layer_shared(clip: Any, node: Any, canvas_bounds: tuple[int, int, int, int]) -> bool:
    """Whether the running render draws this vector layer whole once."""
    memo = getattr(clip, "render_memo", None)
    return memo is not None and memo.banded


def _shared_vector_layer(clip: Any, node: Any, canvas_bounds: tuple[int, int, int, int]):
    """The banded render's one drawing of a vector layer: ``(_SparseImage |
    empty | None, profile)``."""
    from tmiref import clip_vector

    def draw():
        buf, profile = clip_vector.render_vector_layer_buffer(clip, node, canvas_bounds)
        if buf is None or _is_empty_buffer(buf):
            return buf, profile
        return _SparseImage(buf.image, buf.x, buf.y), profile

    return clip.render_memo.get(("vector", node.main_id, tuple(canvas_bounds)), draw)


class _SparseImage:
    """An RGBA image at (x, y) kept as its 256x256 tiles that hold any
    non-zero byte: what a banded render shares of a whole-canvas vector
    layer or comic-frame border, which are thin lines over most of the page
    (lolita page0001: 0.6 GB as whole images). ``crop`` rebuilds any part
    byte for byte, zeros included."""

    TILE = 256

    def __init__(self, image: Image.Image, x: int, y: int) -> None:
        self.x, self.y = x, y
        self.width, self.height = image.size
        arr = _rgba_array(image)
        self.tiles: list[tuple[int, int, Image.Image]] = []
        t = self.TILE
        for ty in range(0, self.height, t):
            for tx in range(0, self.width, t):
                block = arr[ty:ty + t, tx:tx + t]
                if block.any():
                    self.tiles.append((tx, ty, Image.fromarray(np.ascontiguousarray(block), "RGBA")))

    @property
    def bounds(self) -> tuple[int, int, int, int]:
        return (self.x, self.y, self.x + self.width, self.y + self.height)

    def buffer(self, window: tuple[int, int, int, int]) -> _LayerBuffer | None:
        """The image cropped to ``window`` (canvas coordinates), or None
        where they do not overlap."""
        box = _intersect_bounds(self.bounds, window)
        if box is None:
            return None
        out = Image.new("RGBA", _canvas_size(box), (0, 0, 0, 0))
        x0, y0 = box[0] - self.x, box[1] - self.y
        x1, y1 = box[2] - self.x, box[3] - self.y
        for tx, ty, tile in self.tiles:
            if tx < x1 and ty < y1 and tx + tile.width > x0 and ty + tile.height > y0:
                out.paste(tile, (tx - x0, ty - y0))
        return _LayerBuffer(out, box[0], box[1])


def _clip_layer_to_window(
    buf: _LayerBuffer | _EmptyLayerBuffer | None,
    window: tuple[int, int, int, int],
) -> _LayerBuffer | _EmptyLayerBuffer | None:
    if buf is None or _is_empty_buffer(buf):
        return buf
    clipped = _clip_buffer_to_bounds(buf, window)
    if clipped is None:
        return _EMPTY_LAYER_BUFFER
    if clipped is buf:
        return buf
    return _trim_buffer(clipped) or _EMPTY_LAYER_BUFFER


def _render_stored_buffer(
    clip: Any,
    node: Any,
    canvas_bounds: tuple[int, int, int, int],
    region: tuple[int, int, int, int] | None = None,
) -> _LayerBuffer | _EmptyLayerBuffer | None:
    window = canvas_bounds if region is None else region
    dx = node.offset_x + (getattr(node, "render_offset_x", 0) or 0)
    dy = node.offset_y + (getattr(node, "render_offset_y", 0) or 0)
    if window != canvas_bounds and node.main_id in (getattr(clip, "decodable_layers", None) or ()):
        info = layer_offscreen_info(clip, node.render_mipmap)
        box = (window[0] - dx, window[1] - dy, window[2] - dx, window[3] - dy)
        buf = _render_offscreen_buffer_from_info(clip, info, box)
        failures = getattr(clip, "window_decode_failures", None)
        if buf is None and failures is not None and info and info[2] in clip.extas:
            failures.add(node.main_id)
    else:
        buf = _render_offscreen_buffer(clip, node.render_mipmap)
        failures = getattr(clip, "decode_failures", None)
        if buf is None and failures is not None:
            info = layer_offscreen_info(clip, node.render_mipmap)
            if info and info[2] in clip.extas:
                failures.add(node.main_id)
    if buf is None:
        return None
    if _is_empty_buffer(buf):
        return _EMPTY_LAYER_BUFFER
    shifted = _LayerBuffer(buf.image, buf.x + dx, buf.y + dy)
    buf = _clip_buffer_to_bounds(shifted, window)
    if buf is None:
        return _EMPTY_LAYER_BUFFER
    if buf is shifted:
        return buf  # the offscreen buffer is already trimmed to its alpha bbox
    return _trim_buffer(buf) or _EMPTY_LAYER_BUFFER


def _composite_pass_through_folder(clip: Any, target: _CompositeSurface, node: Any) -> None:
    """Pass-through folder: children blend with whatever sits beneath them in
    ``target``, exactly as if the folder did not exist. If the folder has its
    own opacity or layer-mask, the post-folder canvas is mixed back with the
    pre-folder snapshot using ``opacity * mask`` as the mix weight."""
    opacity = (node.opacity or 0) / 256.0
    has_mask = _mask_is_enabled(node)

    if opacity >= 0.999 and not has_mask:
        _composite_group_surface(clip, target, node.children)
        return

    before = target.copy()
    _composite_group_surface(clip, target, node.children)

    _mix_pass_through_result(clip, node, target, before, opacity)


def _mix_pass_through_result(
    clip: Any,
    node: Any,
    target: _CompositeSurface,
    before: _CompositeSurface,
    opacity: float,
) -> None:
    before_bounds = before.bounds
    after_bounds = target.bounds
    if before_bounds is None and after_bounds is None:
        return
    if before_bounds is None:
        union = after_bounds
    elif after_bounds is None:
        union = before_bounds
    else:
        union = _union_bounds(before_bounds, after_bounds)
    assert union is not None
    union = _intersect_bounds(union, target.canvas_bounds)
    if union is None:
        if not target.fixed:
            target.image = None
        return

    size = (union[2] - union[0], union[3] - union[1])
    before_image = Image.new("RGBA", size, (0, 0, 0, 0))
    after_image = Image.new("RGBA", size, (0, 0, 0, 0))
    if before.image is not None:
        before_crop = _crop_surface_to_bounds(before, union)
        if before_crop is not None:
            before_image.alpha_composite(before_crop.image, (before_crop.x - union[0], before_crop.y - union[1]))
    if target.image is not None:
        after_crop = _crop_surface_to_bounds(target, union)
        if after_crop is not None:
            after_image.alpha_composite(after_crop.image, (after_crop.x - union[0], after_crop.y - union[1]))

    weight = Image.new("L", size, round(255 * opacity))
    mask = _layer_mask_crop(clip, node, union, target.canvas_bounds)
    if mask is not None:
        from PIL import ImageChops

        weight = ImageChops.multiply(weight, mask)
    mixed = Image.composite(after_image, before_image, weight)
    if target.fixed:
        target.image.paste(mixed, (union[0] - target.x, union[1] - target.y))
        return
    mixed_buf = _trim_buffer(_LayerBuffer(mixed, union[0], union[1]))
    if mixed_buf is None:
        target.image = None
        target.x = 0
        target.y = 0
    else:
        target.image = mixed_buf.image
        target.x = mixed_buf.x
        target.y = mixed_buf.y


def _crop_surface_to_bounds(surface: _CompositeSurface, bounds: tuple[int, int, int, int]) -> _LayerBuffer | None:
    if surface.image is None:
        return None
    return _clip_buffer_to_bounds(_LayerBuffer(surface.image, surface.x, surface.y), bounds)


def _synthesize_layer(
    clip: Any,
    node: Any,
    canvas_size: tuple[int, int],
    region: tuple[int, int, int, int] | None = None,
) -> _LayerBuffer | None:
    """Build a layer buffer for nodes that have no rasterised offscreen.

    Handles LayerType 1584 (the CSP "Paper" layer — a flat colour spanning the
    canvas; the draw colour is 32-bit unsigned RGB on the Layer row,
    0xFFFFFFFF = full intensity) and ``べた塗り`` / ``グラデーション`` layers,
    which keep only their GradationFillInfo record (a solid fill is told apart
    by the GradationSettingAdd0001 flag, not by GradationFillColorMixingInfo).

    The buffer covers ``region`` (default: the whole canvas); every pixel is
    computed from its absolute position, so a window gets the canvas's bytes.
    """
    window = (0, 0, canvas_size[0], canvas_size[1]) if region is None else region
    if getattr(node, "type", 0) == 1584:
        image = _render_paper_layer(clip, node, _canvas_size(window))
        return None if image is None else _LayerBuffer(image, window[0], window[1])
    blob = _safe_layer_blob(clip, node.main_id, "GradationFillInfo") if clip is not None else None
    if blob:
        try:
            image = _render_gradation_fill(_parse_gradation_fill(blob), canvas_size, window)
        except Exception as exc:
            _warn_layer_failure(node, exc)
            return None
        if image is None:
            return None
        from tmiref import clip_style

        # A fill keeps its own colour record, but レイヤーカラー still recolours it.
        image = clip_style.apply_layer_style(image, clip_style.layer_style(clip, node.main_id), None)
        return _LayerBuffer(image, window[0], window[1])
    return None


def _gradation_section(blob: bytes, name: str) -> int:
    """Offset just past ``u32 len + UTF-16BE name`` of a GradationFillInfo section, or -1."""
    key = struct.pack(">I", len(name)) + name.encode("utf-16-be")
    i = blob.find(key)
    return -1 if i < 0 else i + len(key)


def _parse_gradation_fill(blob: bytes) -> dict[str, Any]:
    """Decode a ``べた塗り`` / ``グラデーション`` layer's GradationFillInfo.

    Layout (big-endian), recovered from files saved by CSP 5.1.5 and older:
      GradationData: u32 size, then u32 header_size (16 or 24), u32 stop_size
        (28), u32 stop_count, … ; each stop is R,G,B,A as u32 (0xFFFFFFFF =
        full), u32 colour kind (1 main / 2 sub / 0 custom), u32 position
        (0..0x8000), u32 reserved.
      GradationSetting: u32, u32, u32, then f64 ×3 (unknown: 100, 100, 0) and
        the line's start / end points as f64 (x0, y0, x1, y1) in canvas pixels.
      GradationSettingAdd0001 (newer files): u32 size, u32 solid flag (1 for a
        べた塗り fill), u32 R, G, B.
    """
    p = _gradation_section(blob, "GradationData")
    if p < 0:
        raise ValueError("GradationFillInfo without GradationData")
    header_size, stop_size, count = struct.unpack(">III", blob[p + 4:p + 16])
    # 24-byte headers (CSP 5.x) add the 混色モード and 輝度の補正 level; older
    # files predate both and render like 通常.
    mode, level = 0, 3
    if header_size >= 24:
        mode, level = struct.unpack(">II", blob[p + 20:p + 28])
    stops = []
    for i in range(count):
        s = p + 4 + header_size + i * stop_size
        r, g, b, a, kind, pos = struct.unpack(">6I", blob[s:s + 24])
        stops.append((min(pos, 0x8000) / 0x8000, (r >> 24, g >> 24, b >> 24, a >> 24)))
    stops.sort(key=lambda item: item[0])
    q = _gradation_section(blob, "GradationSetting")
    points = None
    if q >= 0:
        doubles = struct.unpack(">7d", blob[q + 12:q + 68])
        points = doubles[3:7]
    solid = None
    a = _gradation_section(blob, "GradationSettingAdd0001")
    if a >= 0:
        flag, r, g, b = struct.unpack(">4I", blob[a + 4:a + 20])
        if flag == 1:
            solid = (r >> 24, g >> 24, b >> 24, 255)
    return {"stops": stops, "points": points, "solid": solid, "mode": mode, "level": level}


def _render_gradation_fill(
    info: dict[str, Any],
    canvas_size: tuple[int, int],
    region: tuple[int, int, int, int] | None = None,
) -> Image.Image | None:
    """The fill over ``region`` of a ``canvas_size`` canvas (default: all of it)."""
    window = (0, 0, canvas_size[0], canvas_size[1]) if region is None else region
    if info["solid"] is not None:
        return Image.new("RGBA", _canvas_size(window), info["solid"])
    stops, points = info["stops"], info["points"]
    if not stops or points is None or np is None:
        return None
    x0, y0, x1, y1 = points
    if (x1 - x0) ** 2 + (y1 - y0) ** 2 <= 0:
        return Image.new("RGBA", _canvas_size(window), stops[-1][1])
    from tmiref.clip_gradient import render_linear_gradient

    pixels = render_linear_gradient(
        stops, points, canvas_size, info.get("mode", 0), info.get("level", 3),
        region=None if region is None else window,
    )
    return Image.fromarray(pixels, "RGBA")


def _u32_to_u8(value: int) -> int:
    return min(255, (value * 255 + 0x7FFFFFFF) // 0xFFFFFFFF)


def _render_paper_layer(clip: Any, node: Any, canvas_size: tuple[int, int]) -> Image.Image | None:
    try:
        cur = clip.db.cursor()
        row = cur.execute(
            "SELECT DrawColorMainRed, DrawColorMainGreen, DrawColorMainBlue, DrawColorEnable "
            "FROM Layer WHERE MainId = ?",
            (node.main_id,),
        ).fetchone()
    except Exception:
        return None
    if not row or not row[3]:
        return None
    raw_r, raw_g, raw_b = row[0] or 0, row[1] or 0, row[2] or 0
    # DrawColorMain* are u32 in CSP's schema; ``_u32_to_u8`` round-to-nearest
    # avoids the 1-LSB bias floor division gave (mid-grey landed on 127 not 128).
    return Image.new("RGBA", canvas_size, (_u32_to_u8(raw_r), _u32_to_u8(raw_g), _u32_to_u8(raw_b), 255))


def _clipping_base_index(layers: list[Any], start: int) -> int | None:
    """The nearest earlier sibling that is not clipped; None when there is
    none or it is a correction layer (CSP then draws the clipped layers
    unclipped: probe_clip_tonecurve, whose tone curve clips to a hidden
    brightness / contrast layer and re-grades the whole canvas)."""
    for j in range(start - 1, -1, -1):
        if not getattr(layers[j], "clip", 0):
            return None if getattr(layers[j], "type", 0) == 4098 else j
    return None


def _safe_alpha_composite(target: Image.Image, layer: Image.Image, offset: tuple[int, int]) -> None:
    """Alpha-composite ``layer`` onto ``target`` at ``offset``, clipping to
    target bounds. Allows negative offsets and oversize layers."""
    tw, th = target.size
    lw, lh = layer.size
    ox, oy = offset
    sx0 = max(0, -ox)
    sy0 = max(0, -oy)
    sx1 = min(lw, tw - ox)
    sy1 = min(lh, th - oy)
    if sx0 >= sx1 or sy0 >= sy1:
        return
    dx = max(0, ox)
    dy = max(0, oy)
    cropped = layer.crop((sx0, sy0, sx1, sy1))
    target.alpha_composite(cropped, (dx, dy))


def _apply_layer_mask_to_buffer(
    clip: Any,
    node: Any,
    buf: _LayerBuffer | _EmptyLayerBuffer,
    canvas_bounds: tuple[int, int, int, int],
) -> _LayerBuffer | _EmptyLayerBuffer:
    if _is_empty_buffer(buf):
        return _EMPTY_LAYER_BUFFER
    if clip is not None:
        from tmiref import clip_style, clip_tone

        # トーン screens the layer's own pixels (before its mask and フチ).
        tone = clip_tone.parse_tone_effect(clip_style.layer_style(clip, node.main_id))
        if tone is not None:
            screened = clip_tone.apply_tone(buf.image, buf.x, buf.y, tone, clip.canvas.resolution)
            buf = _trim_buffer(_LayerBuffer(screened, buf.x, buf.y)) or _EMPTY_LAYER_BUFFER
            if _is_empty_buffer(buf):
                return buf
    mask = _layer_mask_crop(clip, node, buf.bounds, canvas_bounds)
    if mask is not None:
        image = buf.image.copy()
        _mask_by_alpha(image, mask)
        buf = _trim_buffer(_LayerBuffer(image, buf.x, buf.y)) or _EMPTY_LAYER_BUFFER
        if _is_empty_buffer(buf):
            return buf
    if clip is None:
        return buf
    from tmiref import clip_style

    edge = clip_style.parse_edge_effect(clip_style.layer_style(clip, node.main_id))
    if edge is None:
        return buf
    return clip_style.apply_edge(buf, edge[0], edge[1], canvas_bounds)


def _mask_init_alpha(attr: OffscreenAttribute) -> int:
    """Default attenuation (0..255) for mask pixels outside the painted tiles.

    A mask Offscreen's ``InitColor`` record says whether the mask starts hidden
    or shown: word 0 = ``DoInitialize``, word 1 = ``AlphaValue`` (u32). The
    common "hide all, paint to reveal" mask stores ``(0, 0, …)`` and defaults to
    0; an enabled "show all, paint to hide" mask stores ``(1, 0xFFFFFFFF, …)``
    and defaults to 255. Unpainted tiles and the canvas area beyond the mask
    bounds take this value."""
    words = attr.init_color_words
    if len(words) < 2 or not words[0]:
        return 0
    return min(255, (words[1] * 255 + 0x7FFFFFFF) // 0xFFFFFFFF)


def _render_mask_attenuation(
    clip: Any,
    info: tuple[int, bytes, str] | None,
) -> tuple[Image.Image, int] | None:
    """Render a layer-mask Offscreen into an ``L`` attenuation plane.

    A CSP layer mask is single-channel: one byte per pixel giving visibility
    0..255 (0 = fully hidden, 255 = fully visible). ``_tile_to_image`` decodes
    that byte into the **alpha** channel (it fills R=G=B with a constant), so the
    mask value is read from ``A`` — not from ``R``, which only marks where tiles
    happen to exist. Tiles the user never painted (``has_data = 0`` or absent
    from the grid) fall back to the Offscreen's ``InitColor`` default, so the
    plane is pre-filled with ``init_alpha`` before painted tiles are pasted in.

    Returns ``(plane, init_alpha)`` with ``plane`` a ``_MaskPlane`` of the
    mask's pixel dimensions, or ``None`` if the mask can't be decoded
    (treated as no mask)."""
    if not info:
        return None
    _offscreen_id, attr_blob, outer_id = info
    try:
        attr = parse_offscreen_attribute(attr_blob)
        init_alpha = _mask_init_alpha(attr)
        tiles = []
        if outer_id in clip.extas:
            for tile in iter_tiles(clip.raw, clip.extas[outer_id]):
                if tile.data is None:
                    continue
                col = tile.block_index % attr.tile_cols
                row = tile.block_index // attr.tile_cols
                tiles.append((col * TILE_SIZE, row * TILE_SIZE, _tile_alpha(tile)))
        return _MaskPlane((attr.pixel_width, attr.pixel_height), init_alpha, tiles), init_alpha
    except Exception as exc:
        _warn_offscreen_failure(outer_id, exc)
        return None


class _MaskPlane:
    """A decoded mask: ``init`` everywhere except the painted tiles, pasted
    in file order (a later tile wins where two overlap). It stands for the
    whole ``size`` plane without holding it: a 46-megapixel page carried a
    dozen masks, mostly untouched "show all" ones, that took half a gigabyte
    as full planes."""

    def __init__(self, size: tuple[int, int], init: int, tiles: list) -> None:
        self.size = size
        self.width, self.height = size
        self.init = init
        self.tiles = tiles

    def crop(self, box: tuple[int, int, int, int]) -> Image.Image:
        """The plane's pixels over ``box`` (inside the plane), as an L image."""
        out = Image.new("L", (box[2] - box[0], box[3] - box[1]), self.init)
        for x, y, tile in self.tiles:
            if x < box[2] and y < box[3] and x + tile.width > box[0] and y + tile.height > box[1]:
                out.paste(tile, (x - box[0], y - box[1]))
        return out


def _tile_alpha(tile: Any) -> Image.Image:
    """``_tile_to_image(tile).getchannel("A")`` without building the RGBA."""
    size = (tile.tile_w, tile.tile_h)
    pixels = tile.tile_w * tile.tile_h
    bpt = tile.bytes_per_tile
    if bpt == pixels // 8:
        mono = Image.new("1", size)
        mono.frombytes(tile.data)
        return mono.convert("L")
    if bpt == pixels // 4:
        alpha_bit = Image.new("1", size)
        alpha_bit.frombytes(tile.data[:pixels // 8])
        gray_bit = Image.new("1", size)
        gray_bit.frombytes(tile.data[pixels // 8:pixels // 8 * 2])
        return alpha_bit.convert("L")
    if bpt in (pixels, pixels * 2, pixels * 5):
        return Image.frombytes("L", size, tile.data[:pixels])
    return _tile_to_image(tile).getchannel("A")  # (warns once)


def _mask_is_enabled(node: Any) -> bool:
    """A layer mask applies when the layer has a mask offscreen and its
    LayerVisibility mask bit is set. LayerMasking is *not* that flag: CSP 5.1.5
    writes 0 there for a raster layer with an active mask (and 32/33 for folders
    and correction layers regardless), so gating on it dropped every raster
    layer mask. Nodes built without ``mask_enabled`` keep the old rule."""
    if not getattr(node, "mask_mipmap", None):
        return False
    if getattr(node, "comic_frame", False):
        # The saved mask raster of a comic-frame folder is empty (CSP draws it
        # from the frame's vector shape); applying it would blank the panel.
        # Showing the folder unmasked is the closer approximation until frame
        # shapes are rasterised.
        return False
    enabled = getattr(node, "mask_enabled", None)
    if enabled is None:
        return bool(getattr(node, "masking", 0) or 0)
    return bool(enabled)


def _layer_mask_crop(
    clip: Any,
    node: Any,
    crop_bounds: tuple[int, int, int, int],
    canvas_bounds: tuple[int, int, int, int],
) -> Image.Image | None:
    """Build the attenuation plane for ``node``'s layer mask over ``crop_bounds``.

    Returns an ``L`` image the size of ``crop_bounds`` (0 = hide, 255 = show), or
    ``None`` when the node has no active mask. The mask value comes from the
    Offscreen's alpha channel (see ``_render_mask_attenuation``); the region
    outside the mask's painted bounds takes its ``InitColor`` default, so a
    hide-all mask correctly hides the layer everywhere it wasn't revealed."""
    if not _mask_is_enabled(node):
        return None
    mask_mipmap = node.mask_mipmap
    crop_bounds = _intersect_bounds(crop_bounds, canvas_bounds)
    if crop_bounds is None:
        return None
    info = layer_offscreen_info(clip, mask_mipmap)
    # _render_mask_attenuation re-decodes the whole mask offscreen on every call,
    # which is wasteful for masks referenced by multiple layers (pass-through
    # folders, correction layers). Cache the decoded plane per ClipFile, keyed by
    # the mask's outer_id. The cached (plane, init_alpha) is read-only — only
    # .crop() copies are taken below — so this is safe to share.
    mask_cache = getattr(clip, "mask_cache", None)
    cache_key = info[2] if info else None
    if mask_cache is not None and cache_key is not None and cache_key in mask_cache:
        rendered = mask_cache[cache_key]
    else:
        # (Once per render even when several bands ask at the same time.)
        if cache_key is None:
            rendered = _render_mask_attenuation(clip, info)
        else:
            rendered = _memoized(clip, ("mask", cache_key), lambda: _render_mask_attenuation(clip, info))
        if mask_cache is not None and cache_key is not None:
            mask_cache[cache_key] = rendered
    if rendered is None:
        return None
    plane, init_alpha = rendered

    mask = Image.new("L", (crop_bounds[2] - crop_bounds[0], crop_bounds[3] - crop_bounds[1]), init_alpha)
    mx = node.offset_x + (getattr(node, "mask_offset_x", 0) or 0)
    my = node.offset_y + (getattr(node, "mask_offset_y", 0) or 0)
    source_bounds = (mx, my, mx + plane.width, my + plane.height)
    overlap = _intersect_bounds(crop_bounds, source_bounds)
    if overlap is None:
        # Mask data lies entirely outside this crop; the whole crop takes the
        # InitColor default (hide-all masks correctly attenuate to 0 here).
        return mask
    value_crop = plane.crop((overlap[0] - mx, overlap[1] - my, overlap[2] - mx, overlap[3] - my))
    mask.paste(value_crop, (overlap[0] - crop_bounds[0], overlap[1] - crop_bounds[1]))
    return mask


# ─────────────────────────── blend mode implementations ───────────────────────
#
# Compositing follows the W3C / PSD-compatible Porter-Duff source-over with a
# per-mode RGB blend function. All math is done in float32 with [0,1] colour
# values, then quantised back to 8-bit. Numpy keeps this fast even on
# multi-megapixel canvases.


def _blend_buffer_onto_surface(
    target: _CompositeSurface,
    source: _LayerBuffer | _EmptyLayerBuffer,
    mode: str,
    opacity: float,
) -> None:
    if opacity <= 0 or _is_empty_buffer(source):
        return
    source = _clip_buffer_to_bounds(source, target.canvas_bounds)
    if source is None:
        return
    if not target.ensure(source.bounds) or target.image is None:
        return
    _blend_onto_at(target.image, source.image, mode, opacity, (source.x - target.x, source.y - target.y))
    # No trim here: re-scanning the whole accumulated surface after every layer
    # was ~1s on tama.clip. _composite_group_surface trims once per group.


def _blend_onto(target: Image.Image, source: Image.Image, mode: str, opacity: float, *, csp: bool = True) -> None:
    """Composite ``source`` (RGBA) onto ``target`` (RGBA, same size) using the
    given blend mode and layer opacity. Mutates ``target`` in place. ``csp``:
    see ``_blend_pixels``."""
    _blend_onto_at(target, source, mode, opacity, (0, 0), csp=csp)


def _blend_onto_at(
    target: Image.Image,
    source: Image.Image,
    mode: str,
    opacity: float,
    offset: tuple[int, int],
    *,
    csp: bool = True,
) -> None:
    """Composite ``source`` onto ``target`` at ``offset`` using ``mode``."""
    if opacity <= 0:
        return
    tw, th = target.size
    lw, lh = source.size
    ox, oy = offset
    sx0 = max(0, -ox)
    sy0 = max(0, -oy)
    sx1 = min(lw, tw - ox)
    sy1 = min(lh, th - oy)
    if sx0 >= sx1 or sy0 >= sy1:
        return
    dx = max(0, ox)
    dy = max(0, oy)
    if sx0 or sy0 or sx1 != lw or sy1 != lh:
        source = source.crop((sx0, sy0, sx1, sy1))
    if (mode in (BLEND_NORMAL, BLEND_PASS_THROUGH)) and opacity >= 0.999:
        if source.getchannel("A").getextrema() == (255, 255):
            # Pillow's alpha_composite gives a source pixel of alpha 255
            # back unchanged, whatever lies under it (the backdrop's weight
            # is exactly 0; test_opaque_alpha_composite_is_a_paste), so an
            # opaque source is a paste, at a fraction of the cost.
            target.paste(source, (dx, dy))
            return
        target.alpha_composite(source, (dx, dy))
        return

    # Wherever the source is fully transparent the blend leaves the backdrop
    # untouched (the αs factors below all vanish), so restrict the expensive
    # per-pixel numpy work to the source's non-transparent bounding box. Inside
    # the box the result is identical to processing the whole canvas; outside it
    # ``target`` keeps its existing pixels. Layers typically paint a small region
    # of a multi-megapixel canvas, so this avoids converting the full canvas to
    # float arrays once per blended layer.
    bbox = source.getchannel("A").getbbox()
    if bbox is None:
        return
    if bbox == (0, 0, source.width, source.height):
        target_box = (dx, dy, dx + source.width, dy + source.height)
        region_target, region_source = target.crop(target_box), source
        paste_at = (dx, dy)
    else:
        target_box = (dx + bbox[0], dy + bbox[1], dx + bbox[2], dy + bbox[3])
        region_target, region_source = target.crop(target_box), source.crop(bbox)
        paste_at = (target_box[0], target_box[1])

    backdrop = np.asarray(region_target)
    source_px = np.asarray(region_source)
    out = np.empty_like(backdrop)

    def blend_rows(y0: int, y1: int) -> None:
        out[y0:y1] = _blend_pixels(backdrop[y0:y1], source_px[y0:y1], mode, opacity, csp=csp)

    _for_row_bands(backdrop.shape[0], backdrop.shape[1], blend_rows)
    target.paste(Image.fromarray(out, "RGBA"), paste_at)


# The numpy blend and regrade paths run over row bands of about this many
# pixels on a thread pool: a band's float32 temporaries stay cache-sized and
# numpy releases the GIL inside each step (~5x on a 12-megapixel layer). Every
# step is a function of the pixel's own values, so the bytes are exactly those
# of one pass over the whole region.
_NUMPY_BAND_PIXELS = 1 << 15
_NUMPY_BAND_WORKERS = 8


def _for_row_bands(height: int, width: int, fn) -> None:
    """Call ``fn(y0, y1)`` for row bands covering ``[0, height)``, in parallel
    when there is more than one band. ``fn`` must write disjoint rows only."""
    rows = max(1, _NUMPY_BAND_PIXELS // max(1, width))
    bands = [(y, min(height, y + rows)) for y in range(0, height, rows)]
    workers = min(len(bands), _NUMPY_BAND_WORKERS, os.cpu_count() or 1)
    if workers <= 1 or _in_band_worker():
        for y0, y1 in bands:
            fn(y0, y1)
        return
    from concurrent.futures import ThreadPoolExecutor

    with ThreadPoolExecutor(max_workers=workers) as pool:
        for _ in pool.map(lambda band: fn(*band), bands):
            pass


def _blend_pixels(backdrop, source_px, mode: str, opacity: float, *, csp: bool = True):
    """Source-over of ``source_px`` onto ``backdrop`` (uint8 RGBA arrays of one
    shape) in blend mode ``mode``: the uint8 RGBA result. Per pixel.

    ``csp`` (CLIP STUDIO PAINT, fitted to its full-size PNG export,
    research/clip-format.md §15.7): the layer opacity (0..256) is folded
    into the source alpha in 8 bit, a' = (a · opacity) >> 8; the blended
    colour B is rounded to 8 bit (not for multiply); the colour result is
    rounded instead of truncated, and so is 加算(発光). Without it: the
    plain W3C maths truncated, which ``tmi_accel.blend_rgba`` (MDP2)
    reproduces."""
    base = backdrop.astype(np.float32) / 255.0
    src = source_px.astype(np.float32) / 255.0

    αb = base[..., 3:4]
    if csp:
        op256 = int(round(float(opacity) * 256.0))
        αs = ((source_px[..., 3:4].astype(np.uint32) * op256) >> 8).astype(np.float32) / 255.0
    else:
        αs = src[..., 3:4] * float(opacity)
    # A fully transparent source pixel leaves the backdrop exactly as it was.
    # The maths below would not: αb·Cb/αo comes back a hair under Cb and is
    # truncated, so a backdrop byte could drop by one.
    untouched = αs[..., 0] == 0
    if csp and mode == "multiply":
        # CSP's multiply applies its [ref > s] correction even where a'
        # rounds to 0 (seen over an opaque backdrop only)
        untouched &= (source_px[..., 3] == 0) | (backdrop[..., 3] != 255)

    Cb = base[..., :3]
    Cs = src[..., :3]

    if mode in _GLOW_MODES:
        # CSP's "glow" (発光) modes apply the source pre-multiplied by its alpha
        # and are not mixed back by alpha afterwards (measured on CSP 5.1.5,
        # example/csp515/glow_*.clip: max error 0 / 2 levels):
        #   加算(発光)       G = Cb + Cs·αs
        #   覆い焼き(発光)   G = Cb / (1 − Cs·αs)
        # Over a backdrop that is itself (semi-)transparent the behaviour is
        # not measured; we blend G with plain source-over by the backdrop alpha,
        # which is exact for an opaque backdrop and degrades to normal over
        # transparency.
        x = Cs * αs
        if mode == "glow_add":
            G = np.minimum(1.0, Cb + x)
        else:
            G = np.where(x >= 1.0, 1.0, np.minimum(1.0, Cb / np.maximum(1e-6, 1.0 - x)))
        αo = αs + αb * (1.0 - αs)
        safe_αo = np.where(αo > 0, αo, 1.0)
        Co = np.where(αo > 0, (αb * G + (1.0 - αb) * x) / safe_αo, 0.0)
        out = np.clip(np.concatenate([Co, αo], axis=-1) * 255.0, 0.0, 255.0).astype(np.uint8)
        if csp and mode == "glow_add":
            out[..., :3] = np.clip(np.floor(Co * 255.0 + 0.5), 0.0, 255.0).astype(np.uint8)
        out[untouched] = backdrop[untouched]
        return out

    blend_fn = _BLEND_FNS.get(mode)
    if blend_fn is None:
        # Unmapped mode (defensive) — degrade to normal.
        Bc = Cs
    else:
        Bc = blend_fn(Cb, Cs)
        # Where the base is transparent, the blend formula has no defined
        # backdrop; the W3C formula handles this via the (1-αb)*Cs term below.
        Bc = np.clip(Bc, 0.0, 1.0)
        if csp and mode in ("overlay", "hard_light", "divide"):
            # CSP's overlay in 8 bit: the bright half halves the rounded
            # screen product, so it is always odd; hard light is overlay with
            # the operands swapped; divide rounds 255·b / s, 255 where s = 0
            # but 0 where b = 0 (§15.7)
            bi = backdrop[..., :3].astype(np.int64)
            si = source_px[..., :3].astype(np.int64)
            if mode == "divide":
                q = np.minimum(255, (510 * bi + si) // np.maximum(2 * si, 1))
                Bi = np.where(bi == 0, 0, np.where(si == 0, 255, q))
            else:
                if mode == "hard_light":
                    bi, si = si, bi
                low = (2 * bi * si + 127) // 255
                high = 255 - 2 * (((255 - bi) * (255 - si) + 127) // 255)
                Bi = np.where(bi < 128, low, high)
            Bc = (Bi / 255.0).astype(np.float32)
        if csp and mode != "multiply":
            Bc = np.floor(Bc * 255.0 + 0.5) / 255.0

    # W3C compositing: replace the source colour with (1-αb)·Cs + αb·B(Cb, Cs)
    # before running standard source-over.
    Cs_eff = (1.0 - αb) * Cs + αb * Bc

    αo = αs + αb * (1.0 - αs)
    # Premultiplied output colour, demultiplied to straight RGB.
    safe_αo = np.where(αo > 0, αo, 1.0)
    Co = (αs * Cs_eff + (1.0 - αs) * αb * Cb) / safe_αo
    Co = np.where(αo > 0, Co, 0.0)

    out = np.concatenate([Co, αo], axis=-1)
    out = np.clip(out * 255.0, 0.0, 255.0).astype(np.uint8)
    if csp:
        out[..., :3] = np.clip(np.floor(Co * 255.0 + 0.5), 0.0, 255.0).astype(np.uint8)
        opaque = backdrop[..., 3] == 255
        if mode != "multiply" and opaque.any():
            # over an opaque backdrop CSP adds round((B − b)·a'/255) to b,
            # halves away from zero (§15.7): exactly, in integers
            bi = backdrop[..., :3].astype(np.int64)
            Bi = np.floor(np.asarray(Bc, np.float64) * 255.0 + 0.5).astype(np.int64)
            a1 = ((source_px[..., 3:4].astype(np.int64) * int(round(float(opacity) * 256.0))) >> 8)
            num = (Bi - bi) * a1
            r = bi + np.sign(num) * ((2 * np.abs(num) + 255) // 510)
            out[..., :3] = np.where(opaque[..., None], np.clip(r, 0, 255), out[..., :3]).astype(np.uint8)
        if mode == "multiply":
            # CSP's multiply: the unrounded product mixed by a' and rounded
            # once (exactly, in integers, over an opaque backdrop), then one
            # lower wherever that lands above the source byte (§15.7)
            si = source_px[..., :3].astype(np.int64)
            r = out[..., :3].astype(np.int64)
            opaque = backdrop[..., 3] == 255
            if opaque.any():
                bi = backdrop[..., :3].astype(np.int64)
                a1 = ((source_px[..., 3:4].astype(np.int64) * int(round(float(opacity) * 256.0))) >> 8)
                exact = (bi * (255 - a1) * 255 + bi * si * a1 + 32512) // 65025
                r = np.where(opaque[..., None], exact, r)
            r = r - (r > si)
            out[..., :3] = np.clip(r, 0, 255).astype(np.uint8)
    out[untouched] = backdrop[untouched]
    return out


# Per-mode separable / non-separable blend functions operating on numpy arrays
# of shape (..., 3) with values in [0, 1].

def _blend_normal(Cb, Cs):
    return Cs


def _blend_multiply(Cb, Cs):
    return Cb * Cs


def _blend_screen(Cb, Cs):
    return Cb + Cs - Cb * Cs


def _blend_darken(Cb, Cs):
    return np.minimum(Cb, Cs)


def _blend_lighten(Cb, Cs):
    return np.maximum(Cb, Cs)


def _blend_difference(Cb, Cs):
    return np.abs(Cb - Cs)


def _blend_subtract(Cb, Cs):
    return np.clip(Cb - Cs, 0.0, 1.0)


def _blend_linear_dodge(Cb, Cs):
    return np.clip(Cb + Cs, 0.0, 1.0)


def _blend_linear_burn(Cb, Cs):
    return np.clip(Cb + Cs - 1.0, 0.0, 1.0)


def _blend_color_dodge(Cb, Cs):
    out = np.where(Cs >= 1.0, 1.0, np.minimum(1.0, Cb / np.maximum(1.0 - Cs, 1e-6)))
    return np.where(Cb <= 0.0, 0.0, out)


def _blend_color_burn(Cb, Cs):
    out = np.where(Cs <= 0.0, 0.0, 1.0 - np.minimum(1.0, (1.0 - Cb) / np.maximum(Cs, 1e-6)))
    return np.where(Cb >= 1.0, 1.0, out)


def _blend_overlay(Cb, Cs):
    return np.where(Cb < 0.5, 2.0 * Cb * Cs, 1.0 - 2.0 * (1.0 - Cb) * (1.0 - Cs))


def _blend_hard_light(Cb, Cs):
    # Hard light = overlay with the two operands swapped.
    return _blend_overlay(Cs, Cb)


def _blend_soft_light(Cb, Cs):
    # W3C soft-light (PSD-compatible).
    d = np.where(Cb <= 0.25, ((16.0 * Cb - 12.0) * Cb + 4.0) * Cb, np.sqrt(np.clip(Cb, 0.0, 1.0)))
    return np.where(
        Cs <= 0.5,
        Cb - (1.0 - 2.0 * Cs) * Cb * (1.0 - Cb),
        Cb + (2.0 * Cs - 1.0) * (d - Cb),
    )


def _blend_exclusion(Cb, Cs):
    return Cb + Cs - 2.0 * Cb * Cs


def _blend_divide(Cb, Cs):
    return np.where(Cs <= 0.0, 1.0, np.clip(Cb / np.maximum(Cs, 1e-6), 0.0, 1.0))


def _luma(rgb):
    # Rec. 709 luma; matches CSP's reported colour-channel weighting closely.
    return 0.2126 * rgb[..., 0:1] + 0.7152 * rgb[..., 1:2] + 0.0722 * rgb[..., 2:3]


def _blend_darker_color(Cb, Cs):
    return np.where(_luma(Cb) <= _luma(Cs), Cb, Cs)


def _blend_lighter_color(Cb, Cs):
    return np.where(_luma(Cb) >= _luma(Cs), Cb, Cs)


def _set_lum(C, target_lum):
    L = _luma(C)
    out = C + (target_lum - L)
    # Clip to gamut while preserving the requested luma.
    n = np.amin(out, axis=-1, keepdims=True)
    x = np.amax(out, axis=-1, keepdims=True)
    out = np.where(n < 0.0, target_lum + (out - target_lum) * target_lum / np.maximum(target_lum - n, 1e-6), out)
    out = np.where(x > 1.0, target_lum + (out - target_lum) * (1.0 - target_lum) / np.maximum(x - target_lum, 1e-6), out)
    return np.clip(out, 0.0, 1.0)


def _saturation(rgb):
    return np.amax(rgb, axis=-1, keepdims=True) - np.amin(rgb, axis=-1, keepdims=True)


def _set_sat(C, target_sat):
    # Standard PSD set-sat: rank channels by value, stretch the middle.
    mn = np.amin(C, axis=-1, keepdims=True)
    mx = np.amax(C, axis=-1, keepdims=True)
    span = np.maximum(mx - mn, 1e-6)
    scale = np.where(mx > mn, target_sat / span, 0.0)
    out = (C - mn) * scale
    return out  # mn re-set to 0; mx to target_sat


def _blend_hue(Cb, Cs):
    return _set_lum(_set_sat(Cs, _saturation(Cb)), _luma(Cb))


def _blend_saturation(Cb, Cs):
    return _set_lum(_set_sat(Cb, _saturation(Cs)), _luma(Cb))


def _blend_color(Cb, Cs):
    return _set_lum(Cs, _luma(Cb))


def _blend_luminosity(Cb, Cs):
    return _set_lum(Cb, _luma(Cs))


def _blend_hard_mix(Cb, Cs):
    return np.where(Cb + Cs < 1.0, 0.0, 1.0)


def _blend_linear_light(Cb, Cs):
    return np.clip(Cb + 2.0 * Cs - 1.0, 0.0, 1.0)


def _blend_vivid_light(Cb, Cs):
    burn = _blend_color_burn(Cb, 2.0 * Cs)
    dodge = _blend_color_dodge(Cb, 2.0 * (Cs - 0.5))
    return np.where(Cs < 0.5, burn, dodge)


def _blend_pin_light(Cb, Cs):
    return np.where(Cs < 0.5, np.minimum(Cb, 2.0 * Cs), np.maximum(Cb, 2.0 * (Cs - 0.5)))


_BLEND_FNS = {
    "normal": _blend_normal,
    "multiply": _blend_multiply,
    "screen": _blend_screen,
    "darken": _blend_darken,
    "lighten": _blend_lighten,
    "difference": _blend_difference,
    "subtract": _blend_subtract,
    "linear_dodge": _blend_linear_dodge,
    "linear_burn": _blend_linear_burn,
    "color_dodge": _blend_color_dodge,
    "color_burn": _blend_color_burn,
    "overlay": _blend_overlay,
    "hard_light": _blend_hard_light,
    "soft_light": _blend_soft_light,
    "exclusion": _blend_exclusion,
    "divide": _blend_divide,
    "darker_color": _blend_darker_color,
    "lighter_color": _blend_lighter_color,
    "hue": _blend_hue,
    "saturation": _blend_saturation,
    "color": _blend_color,
    "luminosity": _blend_luminosity,
    "hard_mix": _blend_hard_mix,
    "linear_light": _blend_linear_light,
    "vivid_light": _blend_vivid_light,
    "pin_light": _blend_pin_light,
}


def _safe_layer_blob(clip: Any, main_id: int, column: str) -> bytes | None:
    try:
        row = clip.db.execute(f"SELECT {column} FROM Layer WHERE MainId = ?", (main_id,)).fetchone()
    except sqlite3.Error:
        return None
    if not row or row[0] is None:
        return None
    return bytes(row[0])


def _filter_invert(data: bytes):
    return lambda rgb: 1.0 - rgb


def _filter_posterize(data: bytes):
    levels = struct.unpack(">I", data[:4])[0] if len(data) >= 4 else 0
    if levels < 2:
        return None
    span = float(levels - 1)
    return lambda rgb: np.round(rgb * span) / span


def _filter_binarize(data: bytes):
    threshold = (struct.unpack(">I", data[:4])[0] if len(data) >= 4 else 128) / 255.0

    def apply(rgb):
        on = (_luma(rgb) >= threshold).astype(np.float32)
        return np.repeat(on, 3, axis=-1)

    apply.per_channel = False  # luma mixes channels: no per-channel LUT
    return apply


def _filter_brightness_contrast(data: bytes):
    if len(data) < 8:
        return None
    brightness, contrast = struct.unpack(">ii", data[:8])
    if brightness == 0 and contrast == 0:
        return None
    # Validated against CSP-rendered files (probes + CSP 5.1.5 saves): brightness
    # is an additive 8-bit shift applied first (+50 -> in+50); contrast is then a
    # slope about mid-grey, out = (in-0.5)*slope + 0.5, with
    #   slope = 100/(100-c) for c > 0  (+20 -> 1.25, +50 -> 2.0)
    #   slope = (100+c)/100 for c < 0  (-30 -> 0.7)
    # An earlier 1 + c/50 matched only at c = +50 (13 levels off at b=30, c=20).
    bf = brightness / 255.0
    if contrast > 0:
        cf = 100.0 / (100.0 - contrast) if contrast < 100 else 1e9
    else:
        cf = max(0.0, (100.0 + contrast) / 100.0)

    def apply(rgb):
        out = rgb + bf
        if contrast:
            out = (out - 0.5) * cf + 0.5
        return out

    return apply


TONE_CURVE_SAMPLES = 33  # samples per quadratic span (32 chords)


def _tone_curve_lut(points: list[tuple[float, float]]) -> "np.ndarray":
    """Build a 256-entry [0,1] LUT from CSP tone-curve control points.

    CSP draws the curve as a *quadratic spline* (TrueType-style): the endpoints
    are on-curve anchors, every interior point is an off-curve quadratic control,
    and an implied on-curve anchor sits at the midpoint of each consecutive pair
    of controls. Validated to <1 LSB against CSP-rendered probe clips for both a
    3-point curve (a single quadratic Bézier, = 1-(1-x)²) and a 4-point S-curve —
    where a single degree-(n-1) Bézier is off by ~28 and Catmull-Rom by ~52. x is
    monotonic (control x-coords increase) so the curve resamples onto 0..255."""
    P = [np.asarray(p, dtype=np.float64) for p in points]
    xs = np.arange(256) / 255.0
    if len(P) < 2:
        return xs.copy()
    if len(P) == 2:
        return np.clip(np.interp(xs, [P[0][0], P[1][0]], [P[0][1], P[1][1]]), 0.0, 1.0)
    interior = P[1:-1]
    # CSP flattens each span into 32 chords and interpolates linearly between
    # them: a concave curve comes out up to ~12/65535 lower than the exact
    # spline, which decides 9 of 256 values of example/csp515/adjust_tc's curve
    # (probe_clip_tonecurve2) and lifts probe_cube_tc_tama from 98.7 to
    # 99.4 % exact (§15.5); ryan's and ricardo's stay exact.
    t = np.linspace(0.0, 1.0, TONE_CURVE_SAMPLES)
    anchor = P[0]
    bx, by = [], []
    for i, ctrl in enumerate(interior):
        nxt = (ctrl + interior[i + 1]) / 2.0 if i < len(interior) - 1 else P[-1]
        bx.append((1 - t) ** 2 * anchor[0] + 2 * (1 - t) * t * ctrl[0] + t**2 * nxt[0])
        by.append((1 - t) ** 2 * anchor[1] + 2 * (1 - t) * t * ctrl[1] + t**2 * nxt[1])
        anchor = nxt
    bx = np.concatenate(bx)
    by = np.concatenate(by)
    order = np.argsort(bx, kind="stable")
    return np.clip(np.interp(xs, bx[order], by[order]), 0.0, 1.0)


def _csp16_to_8(y):
    """CSP's 16-bit corrections back to 8 bit: round(y·65535) >> 8 for y in
    [0, 1] (float64 array) -> int64 array."""
    return np.floor(np.clip(y, 0.0, 1.0) * 65535.0 + 0.5).astype(np.int64) >> 8


def _filter_tone_curve(data: bytes):
    """Tone curve: four 130-byte channel sections (master / R / G / B), each
    ``[u16 count][32 x (u16 x, u16 y)]`` with coords in 0..65535, each reduced
    to a 256-entry LUT via the quadratic-spline evaluation in
    ``_tone_curve_lut``.

    Sections 1/2/3 are R/G/B and run first; the master (section 0) is then
    applied to every channel's result: out_c = master[curve_c[in_c]], the
    intermediate rounded to 8 bit. Pinned by a probe with one point per
    channel (R 128->191, G 128->63, B 64->159): with an extra master point
    (128->159), master-after fits within 1/255 while master-before is off by
    2.

    CSP evaluates the curves in 16 bit and keeps the high byte:
    out = round(y·65535) >> 8 (``_csp16_to_8``), for the channel curve and
    again for the master. Fitted to the cube probes with the curves of tama,
    ryan and ricardo: every value but one of 2448 (tama's 204, model 209.013,
    CSP 208) matches (research §15.5)."""

    def channel_lut(ch: int):
        off = ch * 130
        if off + 2 > len(data):
            return None
        count = struct.unpack(">H", data[off:off + 2])[0]
        if count < 2 or count > 32 or off + 2 + count * 4 > len(data):
            return None
        pts = [
            (x / 65535.0, y / 65535.0)
            for x, y in struct.iter_unpack(">2H", data[off + 2:off + 2 + count * 4])
        ]
        if all(abs(x - y) < 1e-6 for x, y in pts):
            return None  # identity
        return _tone_curve_lut(pts)

    master = channel_lut(0)
    channels = [channel_lut(c) for c in (1, 2, 3)]
    if master is None and all(c is None for c in channels):
        return None
    luts = []
    for curve in channels:
        lut8 = np.arange(256, dtype=np.int64) if curve is None else _csp16_to_8(curve)
        if master is not None:
            lut8 = _csp16_to_8(master[lut8])
        luts.append((lut8 / 255.0).astype(np.float32))

    def apply(rgb):
        idx = np.clip((rgb * 255.0).round().astype(np.int32), 0, 255)
        out = np.empty_like(rgb)
        for c in range(3):
            out[..., c] = luts[c][idx[..., c]]
        return out

    return apply


# 色相・彩度・明度 in CSP's integers (fitted with Fable 5.1, research §15.7):
# hue in 1/32768 of a 60° sextant, saturation and the parameters in 1/65536,
# V the 8-bit max, every division a floor.
_HSV_HK = 32768
_HSV_SK = 65536


def _hsv_int(rgb):
    """(n, 3) int64 RGB -> (H in [0, 6·HK), S in [0, SK], V in [0, 255])."""
    r, g, b = rgb[:, 0], rgb[:, 1], rgb[:, 2]
    mx = rgb.max(1)
    ch = mx - rgb.min(1)
    S = np.where(mx > 0, (ch * _HSV_SK) // np.maximum(mx, 1), 0)
    rmax = r == mx
    gmax = (g == mx) & ~rmax
    base = np.where(rmax, 0, np.where(gmax, 2, 4))
    num = np.where(rmax, g - b, np.where(gmax, b - r, r - g))
    # the hue fraction divides towards zero (a C division)
    H = base * _HSV_HK + np.sign(num) * ((np.abs(num) * _HSV_HK) // np.maximum(ch, 1))
    H = np.where(H < 0, H + 6 * _HSV_HK, H)
    return np.where(ch == 0, 0, H), S, mx


def _rgb_int(H, S, V):
    H = np.mod(H, 6 * _HSV_HK)
    i = H // _HSV_HK
    F = H - i * _HSV_HK
    p = (V * (_HSV_SK - S)) // _HSV_SK
    q = (V * (_HSV_SK - (S * F) // _HSV_HK)) // _HSV_SK
    t = (V * (_HSV_SK - (S * (_HSV_HK - F)) // _HSV_HK)) // _HSV_SK
    sel = [i == 0, i == 1, i == 2, i == 3, i == 4]
    r = np.select(sel, [V, q, p, p, t], V)
    g = np.select(sel, [t, V, V, q, p], p)
    b = np.select(sel, [p, p, t, V, V], q)
    return np.stack([r, g, b], 1)


def _filter_hue_saturation(data: bytes):
    """色相・彩度・明度: ``>iii hue (deg), saturation, lightness`` (-100..100).

    CSP works in integer HSV (``_hsv_int``: H in 1/32768 sextants, S in
    1/65536, V the 8-bit max; every division floors, the hue fraction's
    towards zero) and back (``_rgb_int``, p / q / t), so even an all-zero
    layer moves the middle channel of some colours down by one. Each
    parameter becomes P = |p|·65536 // 100, applied in the order lightness,
    saturation, hue:

    - lightness > 0: V += (255 − V)·L // SK, S −= S·L // SK; < 0: V −= V·L // SK.
    - saturation < 0: X = S·A // SK, S −= X, V −= V·X // (2·SK).
    - saturation > 0 (not on greys, S = 0): dS = (SK − S)·A // SK,
      dV = V·dS // SK; S += dS, V += dV, or, where V + dV > 255,
      S += dS·(255 − V) // dV and V = 255.
    - hue: H += hue·HK // 60.

    Every value of 16 cube probes (each parameter alone at four values, three
    real files' settings, identity) is exact."""
    if len(data) < 12:
        return None
    hue, sat, lum = struct.unpack(">iii", data[:12])
    SK = _HSV_SK
    L = (abs(lum) * SK) // 100
    A = (abs(sat) * SK) // 100
    dH = (hue * _HSV_HK) // 60

    def apply(rgb):
        rgb = np.asarray(rgb)
        shape = rgb.shape
        v8 = np.clip(np.rint(rgb.reshape(-1, 3).astype(np.float64) * 255.0), 0, 255).astype(np.int64)
        H, S, V = _hsv_int(v8)
        if lum > 0:
            V = V + ((255 - V) * L) // SK
            S = S - (S * L) // SK
        elif lum < 0:
            V = V - (V * L) // SK
        if sat < 0:
            X = (S * A) // SK
            V = V - (V * X) // (2 * SK)
            S = S - X
        elif sat > 0:
            dS = ((SK - S) * A) // SK
            dV = (V * dS) // SK
            over = (V + dV) > 255
            S_new = np.where(over, S + (dS * (255 - V)) // np.maximum(dV, 1), S + dS)
            V_new = np.where(over, 255, V + dV)
            grey = S == 0
            S = np.where(grey, S, S_new)
            V = np.where(grey, V, V_new)
        out = _rgb_int(H + dH, S, V)
        return (out / 255.0).astype(np.float32).reshape(shape)

    apply.per_channel = False  # HSV mixes channels: no per-channel LUT
    return apply


def _filter_color_balance(data: bytes):
    """カラーバランス: ``>10i`` preserve-luminosity flag, then shadows, midtones,
    highlights, each (cyan-red, magenta-green, yellow-blue) in -100..100.

    CSP works in 16 bit (v·257) and gives each channel one levels-and-gamma
    curve, out = round(clip((x − b)/(w − b), 0, 1)^g · 65535) >> 8, with
    b, w in 1/65535 steps and g = ln ½ / ln m held to 1/4096
    (round(g·4096)/4096).

    With preserve-luminosity on: b_c = (max of the three shadow values − the
    channel's own)·255, w_c = 65535 − (own highlight value − the smallest of
    the three)·255 (so shadows R+50 lowers G and B, R+50 G+50 only B), and the
    mid point m = ½ − 0.0015·d for the net shift d = own − (others)/2.

    With it off each channel uses its own values only: shadows s > 0 give
    m −= 0.0008·s; s < 0 raise b by 0.96·|s|·255 and give m += 0.0012·|s|;
    highlights h > 0 lower w by 0.96·h·255 and give m −= 0.0012·h; h < 0 give
    m += 0.0008·|h|; midtones m −= 0.002·v.

    Fitted to cube probes (every channel value): preserve on, each range alone
    at ±5..100, pairs, and the three real files' settings (tama, tsuaii,
    ricardo) exact; preserve off, each range alone at 25..100 and −50, all but
    4 of 6432 values. Unprobed: several ranges at once with preserve off
    (taken to combine like preserve on: one curve)."""
    if len(data) < 40:
        return None
    vals = struct.unpack(">10i", data[:40])
    preserve, shadows, mids, highs = vals[0], vals[1:4], vals[4:7], vals[7:10]
    if not any(vals[1:]):
        return None
    x = np.arange(256, dtype=np.float64) * 257.0 / 65535.0
    luts = []
    for c in range(3):
        b, w, m = 0.0, 1.0, 0.5
        if preserve:
            b = (max(shadows) - shadows[c]) * 255 / 65535.0
            w = 1.0 - (highs[c] - min(highs)) * 255 / 65535.0
            m = 0.5 - 0.0015 * (mids[c] - sum(mids[o] for o in range(3) if o != c) / 2.0)
        else:
            s, h = shadows[c], highs[c]
            if s > 0:
                m -= 0.0008 * s
            elif s < 0:
                b = 0.96 * -s * 255 / 65535.0
                m += 0.0012 * -s
            if h > 0:
                w = 1.0 - 0.96 * h * 255 / 65535.0
                m -= 0.0012 * h
            elif h < 0:
                m += 0.0008 * -h
            m -= 0.002 * mids[c]
        m = min(max(m, 1e-6), 1.0 - 1e-6)
        g = math.floor(math.log(0.5) / math.log(m) * 4096.0 + 0.5) / 4096.0
        y = np.clip((x - b) / max(w - b, 1e-9), 0.0, 1.0) ** g
        luts.append((_csp16_to_8(y) / 255.0).astype(np.float32))

    def apply(rgb):
        idx = np.clip((rgb * 255.0).round().astype(np.int32), 0, 255)
        out = np.empty_like(rgb)
        for c in range(3):
            out[..., c] = luts[c][idx[..., c]]
        return out

    return apply


def _levels_lut(rec: tuple[int, ...]) -> "np.ndarray":
    """``(in_black, mid, in_white, out_black, out_white)`` (u16) -> 256-entry LUT."""
    ib, mid, iw, ob, ow = (v / 65535.0 for v in rec)
    span = max(iw - ib, 1e-6)
    t = np.clip((np.arange(256) / 255.0 - ib) / span, 0.0, 1.0)
    p = min(max((mid - ib) / span, 1e-6), 1.0 - 1e-6)
    gamma = min(10.0, max(0.1, math.log(0.5) / math.log(p)))
    return ob + t**gamma * (ow - ob)


_LEVELS_IDENTITY = (0, 0x7FFF, 0xFFFF, 0, 0xFFFF)


def _filter_levels(data: bytes):
    """レベル補正: 32 records of ``>5H (in_black, mid, in_white, out_black,
    out_white)``; record 0 is the master, 1/2/3 are taken as R/G/B.

    Only the master's mid slider is probe-verified (mid at 0 → out = in^0.1);
    the field order of the two black points, the mid→gamma map and the channel
    records are inferred."""
    if len(data) < 10:
        return None
    n = min(4, len(data) // 10)
    recs = [struct.unpack(">5H", data[i * 10:i * 10 + 10]) for i in range(n)]
    if all(r == _LEVELS_IDENTITY for r in recs):
        return None
    master = _levels_lut(recs[0])
    luts = []
    for c in range(3):
        if c + 1 < n and recs[c + 1] != _LEVELS_IDENTITY:
            luts.append(_levels_lut(recs[c + 1])[np.clip(np.round(master * 255.0).astype(np.int32), 0, 255)])
        else:
            luts.append(master)
    lut = np.stack(luts, -1).astype(np.float32)

    def apply(rgb):
        idx = np.clip((rgb * 255.0).round().astype(np.int32), 0, 255)
        out = np.empty_like(rgb)
        for c in range(3):
            out[..., c] = lut[idx[..., c], c]
        return out

    return apply


# Filters wired into the compositor, each fitted against CSP-rendered probe
# clips (research §15.5). Not wired: the gradient map (9; no CSP-rendered
# sample yet).
_FILTER_BUILDERS = {
    1: _filter_brightness_contrast,
    2: _filter_levels,
    3: _filter_tone_curve,
    4: _filter_hue_saturation,
    5: _filter_color_balance,
    6: _filter_invert,
    7: _filter_posterize,
    8: _filter_binarize,
}


def _decode_filter_layer(clip: Any, node: Any):
    """Return a callable ``rgb -> rgb`` (numpy float arrays in [0,1]) for a
    correction layer, or None when the node is not a renderable, non-identity
    adjustment layer. None means "treat as a transparent pass-through layer",
    which is the existing behaviour, so unsupported filters never regress."""
    if np is None or getattr(node, "type", 0) != 4098:
        return None
    blob = _safe_layer_blob(clip, node.main_id, "FilterLayerInfo")
    if not blob or len(blob) < 8:
        return None
    filter_type, size = struct.unpack(">II", blob[:8])
    builder = _FILTER_BUILDERS.get(filter_type)
    if builder is None:
        _warn_unsupported_filter(filter_type)
        return None
    try:
        return builder(blob[8:8 + size])
    except Exception:
        return None


def _alpha_plane_in_target(base: _LayerBuffer, target: _CompositeSurface, shape: tuple[int, int]):
    overlap = _intersect_bounds(base.bounds, target.bounds)
    if overlap is None:
        return None
    plane = np.zeros((shape[0], shape[1], 1), np.float32)
    ba = np.asarray(base.image.getchannel("A"), np.float32) / 255.0
    by0, bx0 = overlap[1] - base.y, overlap[0] - base.x
    ty0, tx0 = overlap[1] - target.y, overlap[0] - target.x
    h, w = overlap[3] - overlap[1], overlap[2] - overlap[0]
    plane[ty0:ty0 + h, tx0:tx0 + w, 0] = ba[by0:by0 + h, bx0:bx0 + w]
    return plane


def _filter_lut(filter_fn) -> list[float]:
    """Run a per-channel filter over the 256 possible 8-bit inputs.

    The inputs are built exactly like the full-image path builds ``rgb``
    (uint8 -> float32, / 255.0), so every LUT entry is bit-identical to what
    the filter yields for a pixel holding that value."""
    values = np.arange(256, dtype=np.uint8).astype(np.float32) / 255.0
    rgb = np.repeat(values[:, None, None], 3, axis=2)  # (256, 1, 3)
    filtered = np.clip(filter_fn(rgb), 0.0, 1.0).astype(np.float32)
    return filtered[:, 0, :].T.reshape(-1).tolist()  # [R0..R255, G0.., B0..]


# Below this many pixels a colour-mixing filter runs directly on the image.
_DISTINCT_COLOUR_MIN_PIXELS = 1 << 18


def _run_filter(filter_fn, rgb, raw):
    """``filter_fn(rgb)`` for an (h, w, 3) float32 image of 8-bit values
    (``raw`` is the uint8 RGBA it came from), bit-identical but faster.

    A filter that mixes channels (Hue/Saturation's HSV round trip, Binarise's
    luma) cannot use the per-channel LUT and costs about a microsecond per
    pixel in numpy, ~20 s for the 20-megapixel canvases CSP users paint on.
    It is a pure function of the pixel's 8-bit colour, though, and paintings
    hold far fewer distinct colours than pixels: evaluate it once per
    distinct colour (built exactly like ``rgb``: uint8 -> float32 / 255) and
    gather. Every filter here is elementwise numpy without reductions across
    pixels, so the shape of the input does not change the result."""
    h, w = rgb.shape[:2]
    if not _use_distinct_colours(filter_fn, h * w):
        return filter_fn(rgb)
    distinct, index = _distinct_colours(raw)
    out = np.asarray(filter_fn(distinct.astype(np.float32) / 255.0))
    return out[index].reshape(h, w, 3)


def _use_distinct_colours(filter_fn, pixels: int) -> bool:
    return not getattr(filter_fn, "per_channel", True) and pixels >= _DISTINCT_COLOUR_MIN_PIXELS


def _distinct_colours(raw):
    """``(distinct, index)`` for an (h, w, ≥3) uint8 image: the distinct RGB
    triples as an (n, 3) uint8 array and, per pixel (flattened), the row of
    its colour — ``distinct[index]`` rebuilds the RGB planes. Linear time via
    a 2^24-entry table instead of a sort."""
    keys = (raw[..., 0].astype(np.uint32) << 16) | (raw[..., 1].astype(np.uint32) << 8) | raw[..., 2]
    keys = keys.ravel()
    present = np.zeros(1 << 24, dtype=bool)
    present[keys] = True
    colours = np.flatnonzero(present).astype(np.uint32)
    slot = np.zeros(1 << 24, dtype=np.int32)
    slot[colours] = np.arange(colours.size, dtype=np.int32)
    distinct = np.stack([(colours >> 16) & 0xFF, (colours >> 8) & 0xFF, colours & 0xFF], axis=-1).astype(np.uint8)
    return distinct, slot[keys]


def _regrade_uniform_by_colour(filter_fn, raw, opacity: float):
    """The numpy regrade below for a weight that is the same at every pixel
    (no mask, or a fully open one, and no clipping base): the output byte is
    then a function of the input colour alone, so the whole float pipeline
    runs once per distinct colour. Same float32 operations, same bytes."""
    distinct, index = _distinct_colours(raw)
    rgb = distinct.astype(np.float32) / 255.0
    filtered = np.clip(filter_fn(rgb), 0.0, 1.0)
    a = (255 * int(round(float(opacity) * 256.0))) >> 8
    graded = _mix_filtered(distinct, filtered, a)[..., :3]
    out = raw.copy()
    out[..., :3] = graded[index].reshape(raw.shape[0], raw.shape[1], 3)
    return out


def _apply_adjustment_to_surface(
    clip: Any,
    target: _CompositeSurface,
    node: Any,
    filter_fn,
    rendered: list,
    layers: list,
    idx: int,
) -> None:
    """Re-grade the backdrop already accumulated in ``target`` with a correction
    layer's filter, attenuated by the layer's opacity, its own mask, and — when
    the layer is a clipping layer — the alpha of its clipping base."""
    if np is None or target.image is None:
        return
    opacity = (node.opacity or 0) / 256.0
    if opacity <= 0:
        return
    raw = _rgba_array(target.image)
    h, w = raw.shape[:2]

    mask = _layer_mask_crop(clip, node, target.bounds, target.canvas_bounds)
    mask_plane = None
    if mask is not None and mask.size == (w, h):
        mask_plane = np.asarray(mask)
        if mask_plane.min() == 255:
            mask_plane = None  # weight * (255 / 255) is the weight itself

    base_alpha = None
    if getattr(node, "clip", 0) and _clipping_base_index(layers, idx) is not None:
        # (No base in the folder: unclipped, as for raster layers above.)
        base = rendered[_clipping_base_index(layers, idx)]
        if base is None or _is_empty_buffer(base):
            return
        base_alpha = _alpha_plane_in_target(base, target, (h, w))
        if base_alpha is None:
            return

    if mask_plane is None and base_alpha is None and _use_distinct_colours(filter_fn, h * w):
        regraded = Image.fromarray(_regrade_uniform_by_colour(filter_fn, raw, opacity), "RGBA")
        target.image.paste(regraded)
        return

    if not getattr(filter_fn, "per_channel", True):
        # One pass: a colour-mixing filter on a large image is evaluated once
        # per distinct colour of the whole image (``_run_filter``).
        out = _regrade_pixels(filter_fn, raw, opacity, mask_plane, base_alpha)
    else:
        # A per-channel filter runs on the pixels themselves: in row bands.
        out = np.empty_like(raw)

        def regrade_rows(y0: int, y1: int) -> None:
            out[y0:y1] = _regrade_pixels(
                filter_fn,
                raw[y0:y1],
                opacity,
                None if mask_plane is None else mask_plane[y0:y1],
                None if base_alpha is None else base_alpha[y0:y1],
            )

        _for_row_bands(h, w, regrade_rows)
    # Paste in place rather than rebinding ``target.image``: a fixed-canvas
    # surface aliases the caller's returned canvas, so a rebind would be lost.
    target.image.paste(Image.fromarray(out, "RGBA"))


def _mix_filtered(raw, filtered, weight):
    """CSP mixes a correction layer in like a normal layer over an opaque
    backdrop (§15.7): the filtered colour rounded to 8 bit,
    out = b + round((F − b)·a'/255) halves away from zero, exactly in
    integers. ``weight`` is a' (int64, broadcastable to the pixels)."""
    F = np.floor(np.asarray(filtered, np.float64) * 255.0 + 0.5).astype(np.int64)
    b = raw[..., :3].astype(np.int64)
    n = (F - b) * weight
    out = raw.copy()
    out[..., :3] = np.clip(b + np.sign(n) * ((2 * np.abs(n) + 255) // 510), 0, 255).astype(np.uint8)
    return out


def _regrade_pixels(filter_fn, raw, opacity: float, mask_plane, base_alpha):
    """``raw`` (uint8 RGBA) with its colour re-graded by ``filter_fn``, mixed
    in by a' = (mask · opacity) >> 8 (mask 255 when absent; times the
    clipping-base alpha // 255 when given, a float32 plane in [0, 1]): the
    uint8 RGBA result. The integer opacity took ricardo's masked correction
    layers at opacities 113 / 133 / 102 from 95.9 to 99.6 % exact."""
    rgb = raw[..., :3].astype(np.float32) / 255.0
    filtered = np.clip(_run_filter(filter_fn, rgb, raw), 0.0, 1.0)
    op256 = int(round(float(opacity) * 256.0))
    m = np.int64(255) if mask_plane is None else mask_plane.astype(np.int64)
    a = (m * op256) >> 8
    if base_alpha is not None:
        a = a * np.rint(base_alpha[..., 0] * 255.0).astype(np.int64) // 255
    return _mix_filtered(raw, filtered, np.asarray(a)[..., None] if np.ndim(a) else a)
