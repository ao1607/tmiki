"""Incremental layered composite: re-render only what changed since a base.

tmi has to composite a CLIP file itself whenever CSP's CanvasPreview is
missing — files tmi wrote (merge results, a layer restored with
``tmiki restore path:layer``) carry none. A full composite of a large painting
costs seconds, yet such a file usually differs from the version it came from
in a few tiles of one or two layers. Given that version's source and its
composite, this module finds the canvas rectangles whose composite can have
changed and re-renders only those (``render_layered_composite_rgba``'s
``region``), pasting them into the base composite. The result is byte for
byte the whole-canvas render; ``TMI_VERIFY_PARTIAL=1`` renders both and
raises on any difference.

When it applies — anything else returns None and the caller renders the whole
canvas:

* the base composite is a tmi layered render by this renderer version: its
  PNG carries the ``tmi-composite`` text written by ``encode_layered_composite``
  (a CanvasPreview or a render from another renderer version does not);
* the two files' databases are identical apart from what tmi's pixel
  write-back touches (tile sizes in ``Offscreen.Attribute``, the Exta offsets
  in ``ExternalChunk``, ``CanvasPreview``, the ``*NeedRefresh*`` columns of
  ``LayerThumbnail``): same layer tree, properties,
  masks, effects, fills, vectors, canvas — any such edit re-renders fully;
* every Exta payload that differs is the render offscreen of leaf layers only
  (not a mask, image-material original or vector record), and decodes in
  both files;
* the changed area is at most ``MAX_DIRTY_FRACTION`` of the canvas.

The dirty area is computed per tile: a tile whose record bytes differ (or that
exists in only one file) marks its 256x256 cell, moved to the canvas by the
layer's offsets. Compositing is a function of each pixel's own layer stack
(see ``render_layered_composite_rgba``), so only フチ spreads a change: each
cell grows by the outline reach of the layer and of every enclosing folder.
"""
from __future__ import annotations

import json
import os
import struct
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass
from typing import Any, Callable

from tmi import accel, raster
from tmi.formats import clip as C
from tmi.formats.composite import _intersect_bounds, _union_bounds
from tmi.raster import Rgba

COMPOSITE_TEXT_KEY = "tmi-composite"
MAX_DIRTY_FRACTION = 0.5
# Past this many separate rectangles, render their bounding box instead.
MAX_RECTS = 16

# Tables tmi's pixel write-back rewrites; compared specially, or not at all.
_SKIPPED_TABLES = {"CanvasPreview", "sqlite_sequence"}


@dataclass
class CompositeBase:
    """Where an incremental composite starts from: an earlier version of the
    same file. Both loaders are called only when needed."""

    load_source: Callable[[], bytes]
    load_composite_png: Callable[[], bytes]


def layered_marker() -> str:
    from tmi.render_cache import RENDER_CACHE_VERSION

    return f"layered/v{RENDER_CACHE_VERSION}"


def render_full(clip: Any) -> tuple[Rgba, bool]:
    """The whole-canvas layered composite, and whether a later version can
    build on it: not when a layer's stored pixels failed to decode, since
    windows of a later version decode only some tiles (see
    ``encode_layered_composite``)."""
    previous = getattr(clip, "decode_failures", None)
    clip.decode_failures = failures = set()
    try:
        image = C.render_layered_composite_rgba(clip)
    finally:
        clip.decode_failures = previous
    return image, not failures


def encode_layered_composite(image: Rgba, is_base: bool) -> bytes:
    """PNG of a layered composite, tagged with the renderer version so that a
    later version can build on it (untagged unless ``is_base``)."""
    from tmi.image_ops import image_to_png_bytes

    if not is_base:
        return image_to_png_bytes(image)
    meta = {"renderer": layered_marker()}
    return image_to_png_bytes(image, text=(COMPOSITE_TEXT_KEY, json.dumps(meta, separators=(",", ":"))))


def read_composite_meta(png: bytes) -> dict | None:
    """The ``tmi-composite`` metadata of a composite PNG written by
    ``encode_layered_composite`` with this renderer version, else None."""
    text = png_text(png, COMPOSITE_TEXT_KEY)
    if not text:
        return None
    try:
        meta = json.loads(text)
        if meta.get("renderer") != layered_marker():
            return None
        return meta
    except (ValueError, TypeError, AttributeError):
        return None


def png_text(data: bytes, key: str) -> str | None:
    """The value of the ``key`` tEXt chunk before the image data, or None."""
    if not data.startswith(b"\x89PNG\r\n\x1a\n"):
        return None
    p = 8
    want = key.encode("latin-1") + b"\0"
    while p + 8 <= len(data):
        length = struct.unpack(">I", data[p:p + 4])[0]
        kind = data[p + 4:p + 8]
        if kind in (b"IDAT", b"IEND"):
            return None
        body = data[p + 8:p + 8 + length]
        if kind == b"tEXt" and body.startswith(want):
            return body[len(want):].decode("latin-1")
        p += 12 + length
    return None


# Why the last ``render_incremental`` call fell back to a full render (None
# when it did not); for tests and diagnosis.
last_fallback: str | None = None


def _fallback(reason: str) -> None:
    global last_fallback
    last_fallback = reason
    return None


def verify_enabled() -> bool:
    """``TMI_VERIFY_PARTIAL=1``: check every incremental composite against a
    whole-canvas render (debugging aid; costs the full render it saves)."""
    return os.environ.get("TMI_VERIFY_PARTIAL", "").strip().lower() in {"1", "true", "yes", "on"}


def check_against_full_render(clip: Any, image: Rgba) -> None:
    """Raise RuntimeError unless ``image`` is exactly what a whole-canvas
    render of ``clip`` gives."""
    full, _is_base = render_full(clip)
    if full.tobytes() != image.tobytes():
        where = ""
        try:
            import numpy as np  # only to say where (a debugging aid; not a dependency)
        except ImportError:
            np = None
        if np is not None and full.size == image.size:
            diff = np.argwhere(np.any(np.asarray(full) != np.asarray(image), axis=-1))
            lo, hi = diff.min(axis=0), diff.max(axis=0)
            where = f": {len(diff)} pixels within x {lo[1]}..{hi[1]}, y {lo[0]}..{hi[0]}"
        raise RuntimeError(f"TMI_VERIFY_PARTIAL: incremental composite differs from the full render{where}")


def render_incremental(clip: Any, base: CompositeBase) -> Rgba | None:
    """``clip``'s layered composite built from ``base``, or None when the
    incremental path does not apply (see the module docstring).

    The windows decode only the tiles they need of every layer: a tagged
    base comes from renders in which every layer decoded (``render_full``),
    unchanged layers carry the same bytes, and ``dirty_rects`` decodes each
    changed layer in full on both sides."""
    global last_fallback
    last_fallback = None
    old = None
    try:
        png = base.load_composite_png()
        if not png or read_composite_meta(png) is None:
            return _fallback("base composite is not this renderer's layered render")
    except Exception as exc:
        return _fallback(f"{type(exc).__name__}: {exc}")
    # Inflating the base composite is the largest single cost here (tmi_accel
    # releases the GIL while it decodes); overlap it with the diff and render.
    pool = ThreadPoolExecutor(max_workers=1)
    decoded = pool.submit(_decode_png, png)
    pool.shutdown(wait=False)
    try:
        old = C.open_clip_bytes(base.load_source())
        rects = dirty_rects(old, clip)
        if rects is None:
            return None  # dirty_rects recorded why
    except Exception as exc:
        return _fallback(f"{type(exc).__name__}: {exc}")
    finally:
        if old is not None:
            old.db.close()
    previous = getattr(clip, "decodable_layers", None)
    clip.decodable_layers = frozenset(n.main_id for n in C.walk_layer_nodes(clip.root.children))
    try:
        # One call: the windows share what is drawn whole (vector layers
        # crossing several, comic-frame shapes, masks).
        patches = list(zip(rects, C.render_layered_regions(clip, rects)))
    finally:
        clip.decodable_layers = previous
    try:
        mode, image = decoded.result()
    except Exception as exc:
        return _fallback(f"base composite does not decode: {type(exc).__name__}: {exc}")
    if mode != "RGBA" or image.size != (clip.canvas.width, clip.canvas.height):
        return _fallback("base composite has another size or mode")
    if not patches:
        return image
    return raster.compose(
        image.width, image.height, [("paste", patch, rect[0], rect[1]) for rect, patch in patches], base=image
    )


def _decode_png(png: bytes) -> tuple[str | None, Rgba | None]:
    """``(mode, pixels)`` of the base composite PNG: the mode Pillow opens
    it in, and its pixels when that is RGBA (tmi_accel's decode; tmi wrote
    it, ``encode_layered_composite``). Raises what reading it raises."""
    mode = raster.png_header(png, after_idat=False).get("mode")
    if mode != "RGBA":
        return mode, None
    return mode, raster.decode_rgba(png)


# ───────────────────────────── dirty area ─────────────────────────────


def dirty_rects(
    old: Any,
    new: Any,
    decodable_layers: frozenset | set | None = None,
) -> list[tuple[int, int, int, int]] | None:
    """Canvas rectangles outside which ``new``'s composite equals ``old``'s,
    or None when that cannot be shown cheaply (render everything)."""
    canvas = (0, 0, new.canvas.width, new.canvas.height)
    if (old.canvas.width, old.canvas.height) != canvas[2:]:
        return _fallback("canvas size changed")
    if not _same_database(old.db, new.db):
        return _fallback("layer tree, properties or other metadata changed")
    if set(old.extas) != set(new.extas):
        return _fallback("data blocks added or removed")
    changed = [oid for oid in new.extas if not _same_payload(old, new, oid)]
    if not changed:
        return []
    uses = _offscreen_uses(new)
    decodable = decodable_layers or frozenset()
    rects: list[tuple[int, int, int, int]] = []
    for outer_id in changed:
        users = uses.get(outer_id)
        if not users or any(kind != "render" for kind, _node, _margin in users):
            return _fallback(f"changed data {outer_id} is not only layer pixels")
        cells = _changed_cells(old, new, outer_id, users[0][1].render_mipmap)
        if cells is None:
            return _fallback(f"changed data {outer_id} has no tile grid")
        if not cells:
            continue
        for _kind, node, margin in users:
            # The changed tiles must decode on both sides, or the layer drops
            # out of one composite entirely rather than changing in place.
            if (node.main_id not in decodable and not _decodes(new, node)) or not _decodes(old, node):
                return _fallback(f"layer {node.main_id} does not decode")
            dx =node.offset_x + (getattr(node, "render_offset_x", 0) or 0)
            dy = node.offset_y + (getattr(node, "render_offset_y", 0) or 0)
            for x0, y0, x1, y1 in cells:
                cell = (x0 + dx - margin, y0 + dy - margin, x1 + dx + margin, y1 + dy + margin)
                hit = _intersect_bounds(cell, canvas)
                if hit is not None:
                    rects.append(hit)
    rects = _merge_rects(rects)
    area = sum((r[2] - r[0]) * (r[3] - r[1]) for r in rects)
    if area > MAX_DIRTY_FRACTION * canvas[2] * canvas[3]:
        return _fallback(f"dirty area {area} px is over {MAX_DIRTY_FRACTION:.0%} of the canvas")
    return rects


def _same_database(a: Any, b: Any) -> bool:
    try:
        schema_a = a.execute("SELECT type, name, sql FROM sqlite_master ORDER BY type, name").fetchall()
        schema_b = b.execute("SELECT type, name, sql FROM sqlite_master ORDER BY type, name").fetchall()
        if schema_a != schema_b:
            return False
        for kind, name, _sql in schema_a:
            if kind != "table" or name in _SKIPPED_TABLES:
                continue
            if name == "ExternalChunk":
                # Offsets move with every rewritten payload; the set of ids
                # must not change.
                ids = "SELECT ExternalID FROM ExternalChunk ORDER BY ExternalID"
                if a.execute(ids).fetchall() != b.execute(ids).fetchall():
                    return False
                continue
            query = f'SELECT * FROM "{name}" ORDER BY rowid'
            if name == "LayerThumbnail":
                # The write-back marks the edited layer's thumbnail stale (its
                # *NeedRefresh* columns); no composite reads them.
                keep = [row[1] for row in a.execute('PRAGMA table_info("LayerThumbnail")') if "NeedRefresh" not in row[1]]
                query = "SELECT " + ", ".join('"%s"' % c.replace('"', '""') for c in keep) + ' FROM "LayerThumbnail" ORDER BY rowid'
            rows_a = a.execute(query).fetchall()
            rows_b = b.execute(query).fetchall()
            if name == "Offscreen":
                cols = [row[1] for row in a.execute('PRAGMA table_info("Offscreen")')]
                if "Attribute" in cols:
                    k = cols.index("Attribute")
                    rows_a = [_without_block_sizes(row, k) for row in rows_a]
                    rows_b = [_without_block_sizes(row, k) for row in rows_b]
            if rows_a != rows_b:
                return False
    except Exception:
        return False
    return True


def _without_block_sizes(row: tuple, k: int) -> tuple:
    """``row`` with its Offscreen.Attribute cut before the BlockSize table
    (the per-tile record sizes tmi rewrites with the tiles)."""
    attr = row[k]
    if not isinstance(attr, (bytes, bytearray, memoryview)):
        return row
    attr = bytes(attr)
    name = "BlockSize".encode("utf-16-be")
    at = attr.find(name)
    if at < 4:
        return row
    cut = at + len(name) + 12  # u32 record size, u32 count, u32 entry size
    return row[:k] + (attr[:cut],) + row[k + 1:]


def _same_payload(old: Any, new: Any, outer_id: str) -> bool:
    a, b = old.extas[outer_id], new.extas[outer_id]
    if a.payload_size != b.payload_size:
        return False
    # Compared in place (tmi_accel, without the GIL); slicing the bytes would
    # copy every payload. A payload cut short by the end of its file raises
    # ValueError (as numpy's frombuffer did): the caller renders in full.
    return accel.native().same_bytes(old.raw, a.payload_off, new.raw, b.payload_off, a.payload_size)


def _offscreen_uses(clip: Any) -> dict[str, list[tuple[str, Any, int]]]:
    """outer id -> [(kind, node, outline reach)] for every offscreen the
    layer tree points at. The reach sums the フチ margins of the node and its
    enclosing folders: how far a change of its pixels can spread."""
    from tmi.formats import clip_style

    uses: dict[str, list[tuple[str, Any, int]]] = {}

    def add(kind: str, mipmap: Any, node: Any, margin: int) -> None:
        info = C.layer_offscreen_info(clip, mipmap) if mipmap else None
        if info:
            uses.setdefault(info[2], []).append((kind, node, margin))

    def walk(nodes: list[Any], inherited: int) -> None:
        for node in nodes:
            margin = inherited + C._edge_margin(clip, node)
            add("render" if not node.folder else "folder-render", node.render_mipmap, node, margin)
            add("mask", node.mask_mipmap, node, margin)
            original = clip_style.layer_style(clip, node.main_id).get("ResizableOriginalMipmap")
            add("material", original, node, margin)
            walk(node.children, margin)

    walk(clip.root.children, 0)
    return uses


def _changed_cells(
    old: Any,
    new: Any,
    outer_id: str,
    mipmap: int | None,
) -> list[tuple[int, int, int, int]] | None:
    """Offscreen-space 256x256 cells whose tile records differ."""
    info = C.layer_offscreen_info(new, mipmap)
    if info is None or info[2] != outer_id:
        return None
    attr = C.parse_offscreen_attribute(info[1])
    cols = max(1, attr.tile_cols)
    a, b = _records(old, outer_id), _records(new, outer_id)
    cells = []
    for block in set(a) | set(b):
        if a.get(block) != b.get(block):
            x, y = (block % cols) * C.TILE_SIZE, (block // cols) * C.TILE_SIZE
            cells.append((x, y, x + C.TILE_SIZE, y + C.TILE_SIZE))
    return cells


def _records(clip: Any, outer_id: str) -> dict[int, list[bytes]]:
    _header, inner, records = C._offscreen_tile_index(clip, outer_id)
    out: dict[int, list[bytes]] = {}
    for block, off, size in records:
        out.setdefault(block, []).append(bytes(inner[off:off + size]))
    return out


def _decodes(clip: Any, node: Any) -> bool:
    buf = C._render_offscreen_buffer(clip, node.render_mipmap)
    return buf is not None


def _merge_rects(rects: list[tuple[int, int, int, int]]) -> list[tuple[int, int, int, int]]:
    """Union overlapping or touching rectangles; collapse to the bounding box
    when too many remain."""
    out = list(rects)
    merged = True
    while merged:
        merged = False
        result: list[tuple[int, int, int, int]] = []
        for r in out:
            for i, s in enumerate(result):
                if r[0] <= s[2] and s[0] <= r[2] and r[1] <= s[3] and s[1] <= r[3]:
                    result[i] = _union_bounds(r, s)
                    merged = True
                    break
            else:
                result.append(r)
        out = result
    if len(out) > MAX_RECTS:
        box = out[0]
        for r in out[1:]:
            box = _union_bounds(box, r)
        out = [box]
    return out
