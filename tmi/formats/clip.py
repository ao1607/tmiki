"""CLIP (Clip Studio Paint) files: the container, the embedded SQLite layer
tree, offscreen tiles, the layered composite and pixel write-back.

Rendering is tmi_accel's (``render_layered_composite_rgba`` and friends call
``tmi_accel.render_clip`` once per render; tile decodes are
``decode_clip_offscreen``). The Python compositor that renderer reproduces
byte for byte is the reference package ``tmiref`` (not part of tmi).

Renders, decoded layers and the CanvasPreview are ``tmi.raster.Rgba``
images (RUST_MIGRATION.md 2d): reading a CLIP file loads neither Pillow nor
numpy. Pillow is imported only to decode a CanvasPreview in a format
tmi_accel leaves to it (``_decode_preview``).
"""
from __future__ import annotations

import functools
import io
import math
import os
import sqlite3
import struct
import tempfile
import zlib
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Iterator

from tmi import accel
from tmi.raster import Rgba, Sparse
from tmi.formats.composite import (
    BLEND_NORMAL,
    BLEND_PASS_THROUGH,
    _canvas_size,
    _EMPTY_LAYER_BUFFER,
    _EmptyLayerBuffer,
    _intersect_bounds,
    _is_empty_buffer,
    _LayerBuffer,
)

# ``TMI_RENDER_TRACE=<path>``: tmi_accel's renders return a JSON record per
# pre-rendered, clipped and composited sibling, appended to that file (see
# render_trace.py). Read once at import.
_TRACE = bool(os.environ.get("TMI_RENDER_TRACE"))

TILE_SIZE = 256
BEGIN_NAME = "BlockDataBeginChunk"
END_NAME = "BlockDataEndChunk"
COLOR_WORDS = (33, 1, 4, 5, 65536, 4, 1024, 1, 256, 65536, 256, 256, 8, 8, 0, 0)
MAGIC = b"CSFCHUNK"


@dataclass
class Chunk:
    cid: bytes
    file_off: int
    payload_off: int
    payload_size: int


@dataclass
class Container:
    file_size: int
    first_chunk_off: int
    chunks: list[Chunk]

    def index_extas(self, data: bytes) -> dict[str, Chunk]:
        out: dict[str, Chunk] = {}
        for chunk in self.chunks:
            if chunk.cid == b"Exta":
                outer_id = data[chunk.payload_off + 8:chunk.payload_off + 48].decode("ascii")
                out[outer_id] = chunk
        return out

    def sqli_chunk(self) -> Chunk:
        return next(chunk for chunk in self.chunks if chunk.cid == b"SQLi")


@dataclass
class HeadFields:
    version: int
    sqli_offset: int
    constant_16: int
    uuid: bytes


@dataclass
class Tile:
    block_index: int
    bytes_per_tile: int
    tile_w: int
    tile_h: int
    data: bytes | None


@dataclass
class OffscreenAttribute:
    header_size: int
    constant_66: int
    constant_2a: int
    block_table_span: int
    parameter_name: str
    pixel_width: int
    pixel_height: int
    tile_cols: int
    tile_rows: int
    channel_profile: str
    channel_words: tuple[int, ...]
    init_color_record_size: int
    init_color_words: tuple[int, ...]
    init_color_extra_words: tuple[int, ...]
    block_record_size: int
    block_count: int
    block_entry_bytes: int
    block_record_sizes: list[int]

    @property
    def tile_count(self) -> int:
        return self.tile_cols * self.tile_rows


@dataclass
class CanvasInfo:
    width: int
    height: int
    resolution: float
    root_folder_id: int


@dataclass
class LayerNode:
    main_id: int
    name: str
    type: int
    folder: int
    visible: bool
    opacity: int
    composite: int
    clip: int
    masking: int
    offset_x: int
    offset_y: int
    uuid: str | None
    render_mipmap: int | None
    mask_mipmap: int | None
    next_sibling: int | None
    first_child: int | None
    children: list["LayerNode"] = field(default_factory=list)
    render_outer_id: str | None = None
    mask_outer_id: str | None = None
    # LayerVisibility bit 1. None for nodes built without it (tests, older
    # callers), which fall back to the LayerMasking flag — see _mask_is_enabled.
    mask_enabled: bool | None = None
    # The render offscreen sits at LayerOffset + LayerRenderOffscrOffset; the
    # two are often equal and opposite (e.g. (537,40) and (-537,-40)), so
    # ignoring the second misplaced whole layers by hundreds of pixels.
    render_offset_x: int = 0
    render_offset_y: int = 0
    # Likewise the mask offscreen sits at LayerOffset + LayerMaskOffset +
    # LayerMaskOffscrOffset.
    mask_offset_x: int = 0
    mask_offset_y: int = 0
    # Comic-frame folders (ComicFrameLineMipmap set): CSP derives their mask
    # from the frame's vector shape and saves an empty mask raster.
    comic_frame: bool = False


@dataclass
class ClipFile:
    raw: bytes
    container: Container
    head: HeadFields
    canvas: CanvasInfo
    root: LayerNode
    extas: dict[str, Chunk]
    db: sqlite3.Connection
    # Per-layer styling columns keyed by MainId (see clip_style.py).
    style_cache: dict = field(default_factory=dict)
    # MainIds whose render offscreen is known to decode in full. When every
    # leaf is listed, a windowed render (``region``) decodes only the tiles
    # it needs (clip_partial's windows); otherwise each layer is decoded
    # whole, so a damaged tile outside the window still drops the layer
    # exactly as a whole-canvas render does.
    decodable_layers: frozenset | None = None
    # When a set: MainIds whose render offscreen exists but failed to decode
    # during a render are added to it (see clip_partial.render_full).
    decode_failures: set | None = None
    # Tile-record index of Exta payloads, keyed by outer id.
    tile_index_cache: dict = field(default_factory=dict)
    # Render state of the reference renderer (``tmiref``), which draws these
    # same objects; tmi's own renders are one tmi_accel call and use none
    # of it. Decoded layer masks by outer id; comic-frame shapes and masks;
    # render offscreens decoded ahead of a composite; the running render's
    # memo; the windowed decode failures of a banded render.
    mask_cache: dict = field(default_factory=dict)
    frame_cache: dict = field(default_factory=dict)
    offscreen_prefetch: dict = field(default_factory=dict)
    render_memo: Any = None
    window_decode_failures: set | None = None


@dataclass
class ClipLayerPixels:
    image: Any  # a tmi.raster.Rgba of the offscreen's size, or None
    pixel_width: int
    pixel_height: int
    tile_cols: int
    tile_rows: int
    channel_profile: str
    outer_id: str
    offscreen_id: int


def parse_container(data: bytes) -> Container:
    if data[:8] != MAGIC:
        raise ValueError("not a CLIP file (bad magic)")
    file_size = struct.unpack(">Q", data[8:16])[0]
    first_chunk_off = struct.unpack(">Q", data[16:24])[0]
    if file_size != len(data):
        raise ValueError(f"file_size header {file_size} != actual {len(data)}")
    chunks: list[Chunk] = []
    off = first_chunk_off
    while off < len(data):
        if data[off:off + 4] != b"CHNK":
            raise ValueError(f"expected CHNK at 0x{off:X}")
        cid = data[off + 4:off + 8]
        payload_size = struct.unpack(">Q", data[off + 8:off + 16])[0]
        chunks.append(Chunk(cid=cid, file_off=off, payload_off=off + 16, payload_size=payload_size))
        off += 16 + payload_size
    return Container(file_size=file_size, first_chunk_off=first_chunk_off, chunks=chunks)


def parse_head(payload: bytes) -> HeadFields:
    if len(payload) != 40:
        raise ValueError(f"Head payload must be 40 bytes, got {len(payload)}")
    return HeadFields(
        version=struct.unpack(">Q", payload[0:8])[0],
        sqli_offset=struct.unpack(">Q", payload[8:16])[0],
        constant_16=struct.unpack(">Q", payload[16:24])[0],
        uuid=bytes(payload[24:40]),
    )


def iter_tiles(data: bytes, exta: Chunk) -> Iterator[Tile]:
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
        bytes_per_tile = struct.unpack(">I", body[4:8])[0]
        tile_w = struct.unpack(">I", body[8:12])[0]
        tile_h = struct.unpack(">I", body[12:16])[0]
        has_data = struct.unpack(">I", body[16:20])[0]
        if has_data:
            data_size = struct.unpack(">I", body[20:24])[0]
            blob = body[24:24 + data_size]
            tile_data = zlib.decompress(blob[4:])
            if len(tile_data) != bytes_per_tile:
                raise ValueError(f"tile {block_index}: decompressed {len(tile_data)} != bpt {bytes_per_tile}")
            yield Tile(block_index, bytes_per_tile, tile_w, tile_h, tile_data)
        else:
            yield Tile(block_index, bytes_per_tile, tile_w, tile_h, None)
        p += rec_size


def _u32s(buf: bytes) -> tuple[int, ...]:
    if len(buf) % 4:
        raise ValueError(f"u32 buffer length is not aligned: {len(buf)}")
    return tuple(struct.unpack(">I", buf[i:i + 4])[0] for i in range(0, len(buf), 4))


def parse_offscreen_attribute(payload: bytes) -> OffscreenAttribute:
    if len(payload) < 194:
        raise ValueError(f"Offscreen.Attribute too short: {len(payload)}")
    header_size, constant_66, constant_2a, block_table_span = struct.unpack(">IIII", payload[:16])
    name_len = struct.unpack(">I", payload[16:20])[0]
    parameter_name_end = 20 + name_len * 2
    if parameter_name_end > len(payload):
        raise ValueError("Offscreen.Attribute Parameter name overruns payload")
    parameter_name = payload[20:parameter_name_end].decode("utf-16-be")
    if parameter_name != "Parameter":
        raise ValueError(f"unexpected Offscreen.Attribute parameter name {parameter_name!r}")

    param_off = parameter_name_end
    pixel_width, pixel_height, tile_cols, tile_rows = struct.unpack(">IIII", payload[param_off:param_off + 16])
    channel_words = _u32s(payload[param_off + 16:param_off + 80])
    if channel_words[0] == 33:
        channel_profile = "color-alpha-bgra"
    elif channel_words[0] == 17:
        channel_profile = "gray-alpha"
    elif channel_words[0] == 1:
        channel_profile = "single-channel"
    else:
        channel_profile = f"unknown-{channel_words[0]}"

    init_name_bytes = "InitColor".encode("utf-16-be")
    block_name_bytes = "BlockSize".encode("utf-16-be")
    init_name_off = payload.find(init_name_bytes)
    block_name_off = payload.find(block_name_bytes)
    if init_name_off < 4 or block_name_off < 4:
        raise ValueError("Offscreen.Attribute missing InitColor or BlockSize section")

    init_off = init_name_off - 4
    init_name_len = struct.unpack(">I", payload[init_off:init_off + 4])[0]
    if init_name_len != 9:
        raise ValueError(f"bad InitColor name length: {init_name_len}")
    init_size_off = init_off + 4 + init_name_len * 2
    init_color_record_size = struct.unpack(">I", payload[init_size_off:init_size_off + 4])[0]
    init_primary_start = init_size_off + 4
    init_primary_end = init_primary_start + init_color_record_size - 4

    block_off = block_name_off - 4
    init_color_words = _u32s(payload[init_primary_start:init_primary_end])
    init_color_extra_words = _u32s(payload[init_primary_end:block_off])

    block_name_len = struct.unpack(">I", payload[block_off:block_off + 4])[0]
    if block_name_len != 9:
        raise ValueError(f"bad BlockSize name length: {block_name_len}")
    block_size_off = block_off + 4 + block_name_len * 2
    block_record_size, block_count, block_entry_bytes = struct.unpack(">III", payload[block_size_off:block_size_off + 12])
    table_off = block_size_off + 12
    table_end = table_off + block_count * block_entry_bytes
    if block_entry_bytes != 4:
        raise ValueError(f"unsupported BlockSize entry width: {block_entry_bytes}")
    if table_end != len(payload):
        raise ValueError(f"BlockSize table ends at {table_end}, attribute length is {len(payload)}")
    if block_count != tile_cols * tile_rows:
        raise ValueError(f"BlockSize count {block_count} != grid {tile_cols}*{tile_rows}")
    if block_table_span != 34 + block_count * block_entry_bytes:
        raise ValueError(f"unexpected table span {block_table_span}; expected {34 + block_count * block_entry_bytes}")
    block_record_sizes = [
        struct.unpack(">I", payload[table_off + i * 4:table_off + i * 4 + 4])[0]
        for i in range(block_count)
    ]
    return OffscreenAttribute(
        header_size,
        constant_66,
        constant_2a,
        block_table_span,
        parameter_name,
        pixel_width,
        pixel_height,
        tile_cols,
        tile_rows,
        channel_profile,
        channel_words,
        init_color_record_size,
        init_color_words,
        init_color_extra_words,
        block_record_size,
        block_count,
        block_entry_bytes,
        block_record_sizes,
    )


def open_sqlite_from_chunk(data: bytes, sqli: Chunk) -> sqlite3.Connection:
    blob = data[sqli.payload_off:sqli.payload_off + sqli.payload_size]
    con = sqlite3.connect(":memory:")
    if hasattr(con, "deserialize"):
        try:
            con.deserialize(blob)
            return con
        except sqlite3.NotSupportedError:
            con.close()

    fd, tmp_path = tempfile.mkstemp(suffix=".sqlite")
    try:
        with os.fdopen(fd, "wb") as fp:
            fp.write(blob)
        disk_con = sqlite3.connect(tmp_path)
        try:
            mem_con = sqlite3.connect(":memory:")
            disk_con.backup(mem_con)
            return mem_con
        finally:
            disk_con.close()
    finally:
        try:
            os.unlink(tmp_path)
        except OSError:
            pass


def load_canvas(con: sqlite3.Connection) -> CanvasInfo:
    cur = con.cursor()
    cur.execute("SELECT CanvasUnit, CanvasWidth, CanvasHeight, CanvasResolution, CanvasRootFolder FROM Canvas")
    unit, width, height, resolution, root = cur.fetchone()
    px_w, px_h = _canvas_to_pixels(float(width), float(height), float(resolution), int(unit or 0))
    return CanvasInfo(px_w, px_h, float(resolution), int(root))


def _canvas_to_pixels(width: float, height: float, dpi: float, unit: int) -> tuple[int, int]:
    if unit == 0:
        factor = 1.0
    elif unit == 1:
        factor = dpi / 2.54
    elif unit == 2:
        factor = dpi / 25.4
    elif unit == 3:
        factor = dpi
    elif unit == 4:
        factor = dpi / 72.0
    else:
        factor = 1.0
    return max(1, round(width * factor)), max(1, round(height * factor))


def _outer_id_for_mipmap(con: sqlite3.Connection, mipmap_id: int | None) -> str | None:
    if not mipmap_id:
        return None
    cur = con.cursor()
    row = cur.execute(
        """
        SELECT CAST(o.BlockData AS TEXT) FROM Mipmap m
        JOIN MipmapInfo mi ON mi.MainId = m.BaseMipmapInfo
        JOIN Offscreen o   ON o.MainId  = mi.Offscreen
        WHERE m.MainId = ?
        """,
        (mipmap_id,),
    ).fetchone()
    return row[0] if row else None


def load_layer(con: sqlite3.Connection, main_id: int) -> LayerNode | None:
    cur = con.cursor()
    row = cur.execute(
        """
        SELECT MainId, LayerName, LayerType, LayerFolder, LayerVisibility,
               LayerOpacity, LayerComposite, LayerClip, LayerMasking,
               LayerOffsetX, LayerOffsetY, LayerUuid,
               LayerRenderMipmap, LayerLayerMaskMipmap,
               LayerNextIndex, LayerFirstChildIndex
        FROM Layer WHERE MainId = ?
        """,
        (main_id,),
    ).fetchone()
    if not row:
        return None
    (
        mid, name, typ, folder, vis, op, clp_mode, clip_to_below, masking,
        ox, oy, uuid, render, mask, next_sibling, first_child,
    ) = row
    node = LayerNode(
        main_id=mid,
        name=name,
        type=typ,
        folder=folder or 0,
        # LayerVisibility is a bit field, not a bool: bit 0 is the eye toggle,
        # bit 1 marks an active layer mask (CSP 5.1.5: a visible correction
        # layer stores 3, a hidden one 2; a raster layer with a mask 3).
        visible=bool((vis or 0) & 1),
        mask_enabled=bool((vis or 0) & 2),
        opacity=op or 0,
        composite=clp_mode or 0,
        clip=clip_to_below or 0,
        masking=masking or 0,
        offset_x=ox or 0,
        offset_y=oy or 0,
        uuid=uuid,
        render_mipmap=render,
        mask_mipmap=mask,
        next_sibling=next_sibling,
        first_child=first_child,
    )
    node.render_outer_id = _outer_id_for_mipmap(con, render)
    node.mask_outer_id = _outer_id_for_mipmap(con, mask)
    extras = _optional_layer_columns(con, main_id)
    node.render_offset_x = extras.get("LayerRenderOffscrOffsetX") or 0
    node.render_offset_y = extras.get("LayerRenderOffscrOffsetY") or 0
    node.mask_offset_x = (extras.get("LayerMaskOffsetX") or 0) + (extras.get("LayerMaskOffscrOffsetX") or 0)
    node.mask_offset_y = (extras.get("LayerMaskOffsetY") or 0) + (extras.get("LayerMaskOffscrOffsetY") or 0)
    node.comic_frame = bool(extras.get("ComicFrameLineMipmap"))
    return node


_OPTIONAL_LAYER_COLUMNS = (
    "LayerRenderOffscrOffsetX",
    "LayerRenderOffscrOffsetY",
    "LayerMaskOffsetX",
    "LayerMaskOffsetY",
    "LayerMaskOffscrOffsetX",
    "LayerMaskOffscrOffsetY",
    "ComicFrameLineMipmap",
)


def _optional_layer_columns(con: sqlite3.Connection, main_id: int) -> dict[str, Any]:
    """Layer columns that not every CLIP schema has; missing ones read as absent."""
    present = {row[1] for row in con.execute("PRAGMA table_info(Layer)")}
    wanted = [c for c in _OPTIONAL_LAYER_COLUMNS if c in present]
    if not wanted:
        return {}
    row = con.execute(f"SELECT {', '.join(wanted)} FROM Layer WHERE MainId = ?", (main_id,)).fetchone()
    return dict(zip(wanted, row)) if row else {}


def walk_layer_tree(con: sqlite3.Connection, root_layer_id: int) -> LayerNode:
    root = load_layer(con, root_layer_id)
    if not root:
        raise ValueError(f"no Layer row for root {root_layer_id}")
    root.children = _children_of(con, root.first_child) if root.first_child else []
    return root


def _children_of(con: sqlite3.Connection, first_child_id: int | None) -> list[LayerNode]:
    out: list[LayerNode] = []
    cur_id = first_child_id
    while cur_id:
        child = load_layer(con, cur_id)
        if not child:
            break
        if child.first_child:
            child.children = _children_of(con, child.first_child)
        out.append(child)
        cur_id = child.next_sibling
    return out


def open_clip(path: str | Path):
    return open_clip_bytes(Path(path).read_bytes())


def open_clip_bytes(raw: bytes):
    container = parse_container(raw)
    head_chunk = next(chunk for chunk in container.chunks if chunk.cid == b"Head")
    head = parse_head(raw[head_chunk.payload_off:head_chunk.payload_off + head_chunk.payload_size])
    db = open_sqlite_from_chunk(raw, container.sqli_chunk())
    canvas = load_canvas(db)
    root = walk_layer_tree(db, canvas.root_folder_id)
    extas = container.index_extas(raw)
    return ClipFile(raw=raw, container=container, head=head, canvas=canvas, root=root, extas=extas, db=db)


def layer_offscreen_info(clip: Any, render_mipmap: int | None) -> tuple[int, bytes, str] | None:
    if not render_mipmap:
        return None
    cur = clip.db.cursor()
    row = cur.execute(
        """
        SELECT o.MainId, o.Attribute, CAST(o.BlockData AS TEXT)
        FROM Mipmap m
        JOIN MipmapInfo mi ON mi.MainId = m.BaseMipmapInfo
        JOIN Offscreen o   ON o.MainId  = mi.Offscreen
        WHERE m.MainId = ?
        """,
        (render_mipmap,),
    ).fetchone()
    if not row:
        return None
    offscreen_id, attr, outer_id = row
    return int(offscreen_id), bytes(attr), str(outer_id)


def render_layer_rgba(clip: Any, node: Any) -> ClipLayerPixels | None:
    return _render_offscreen(clip, node.render_mipmap)


def _render_offscreen(clip: Any, mipmap_id: int | None) -> ClipLayerPixels | None:
    """Render an Offscreen (raster mip-map) into a ``tmi.raster.Rgba``."""
    return render_offscreen_from_info(clip, layer_offscreen_info(clip, mipmap_id))


def _decode_offscreen(clip: Any, outer_id: str, attr: OffscreenAttribute):
    """An offscreen's tiles decoded by tmi_accel into its alpha-bbox buffer:
    a ``_LayerBuffer``, or ``_EMPTY_LAYER_BUFFER`` when nothing is painted.
    Unknown tile encodings are warned about (drawn transparent); raises
    ValueError when the offscreen is unreadable. No database access."""
    exta = clip.extas[outer_id]
    rgba, x, y, w, h, unknown_sizes = accel.native().decode_clip_offscreen(
        clip.raw,
        exta.payload_off,
        exta.payload_size,
        attr.tile_cols,
        attr.tile_rows,
        attr.pixel_width,
        attr.pixel_height,
    )
    for bpt, pixels in unknown_sizes:
        _warn_unknown_tile_size(bpt, pixels)
    if w == 0 or h == 0:
        return _EMPTY_LAYER_BUFFER
    return _LayerBuffer(_rgba_image(rgba, (w, h)), x, y)  # a tmi.raster.Rgba


def offscreen_meta_from_info(clip: Any, info: tuple[int, bytes, str] | None) -> ClipLayerPixels | None:
    """The ``meta`` half of ``render_offscreen_crop_from_info`` without decoding
    any tile. None when the offscreen is missing or its attribute unreadable."""
    if not info:
        return None
    offscreen_id, attr_blob, outer_id = info
    if outer_id not in clip.extas:
        return None
    try:
        attr = parse_offscreen_attribute(attr_blob)
    except Exception:
        return None
    return ClipLayerPixels(
        image=None,
        pixel_width=attr.pixel_width,
        pixel_height=attr.pixel_height,
        tile_cols=attr.tile_cols,
        tile_rows=attr.tile_rows,
        channel_profile=attr.channel_profile,
        outer_id=outer_id,
        offscreen_id=offscreen_id,
    )


def render_offscreen_crop_from_info(clip: Any, info: tuple[int, bytes, str] | None):
    """Decode an offscreen into its painted rectangle only.

    Returns ``(meta, crop)``: ``meta`` is a ``ClipLayerPixels`` whose ``image``
    is None (the full-size image is never built), ``crop`` a ``LayerCrop`` of
    the offscreen. None when the offscreen is missing or unreadable, like
    ``render_offscreen_from_info``. Thread-safe the same way (no DB access).
    """
    from tmi.image_ops import LayerCrop

    if not info:
        return None
    offscreen_id, attr_blob, outer_id = info
    if outer_id not in clip.extas:
        return None
    try:
        attr = parse_offscreen_attribute(attr_blob)
        buf = _decode_offscreen(clip, outer_id, attr)
    except Exception as exc:
        _warn_offscreen_failure(outer_id, exc)
        return None
    size = (attr.pixel_width, attr.pixel_height)
    if _is_empty_buffer(buf):
        crop = LayerCrop.from_tight_crop(size, 0, 0, None)
    else:
        crop = LayerCrop.from_tight_crop(size, buf.x, buf.y, buf.image, buf.image.tobytes())
    meta = ClipLayerPixels(
        image=None,
        pixel_width=attr.pixel_width,
        pixel_height=attr.pixel_height,
        tile_cols=attr.tile_cols,
        tile_rows=attr.tile_rows,
        channel_profile=attr.channel_profile,
        outer_id=outer_id,
        offscreen_id=offscreen_id,
    )
    return meta, crop


def render_offscreen_from_info(clip: Any, info: tuple[int, bytes, str] | None) -> ClipLayerPixels | None:
    """Render an Offscreen from a prefetched ``layer_offscreen_info`` result:
    an image of the offscreen's full pixel size.

    Split from the SQLite lookup so callers can fetch ``info`` once on the main
    thread and run this pure ``clip.raw`` → image work on a worker thread (the
    embedded SQLite connection is single-thread only).

    Returns None for missing/unparseable offscreens or when the tile payload
    is unreadable (warned about); the caller treats None as "no pixels",
    which is the right thing for text/vector/3D/gradient layers that don't
    have a rasterised cache, and for masks whose data is malformed."""
    if not info:
        return None
    offscreen_id, attr_blob, outer_id = info
    if outer_id not in clip.extas:
        return None
    try:
        attr = parse_offscreen_attribute(attr_blob)
        buf = _decode_offscreen(clip, outer_id, attr)
    except Exception as exc:
        _warn_offscreen_failure(outer_id, exc)
        return None
    size = (attr.pixel_width, attr.pixel_height)
    if _is_empty_buffer(buf):
        image = Sparse(size).dense()
    else:
        image = Sparse(size, buf.x, buf.y, buf.image).dense()  # paste onto transparent black
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


def _render_offscreen_buffer(clip: Any, mipmap_id: int | None) -> _LayerBuffer | _EmptyLayerBuffer | None:
    """An offscreen's painted rectangle (``_decode_offscreen``), or None when
    it is missing or unreadable (warned about)."""
    info = layer_offscreen_info(clip, mipmap_id)
    if not info or info[2] not in clip.extas:
        return None
    try:
        return _decode_offscreen(clip, info[2], _parsed_attribute(info[1]))
    except Exception as exc:
        _warn_offscreen_failure(info[2], exc)
        return None


@functools.lru_cache(maxsize=1024)
def _parsed_attribute(attr_blob: bytes) -> OffscreenAttribute:
    """``parse_offscreen_attribute``, remembered. (Read-only; nothing mutates
    the result.)"""
    return parse_offscreen_attribute(attr_blob)


def _offscreen_tile_index(clip: Any, outer_id: str) -> tuple[bytes, memoryview, list[tuple[int, int, int]]]:
    """``(header, inner, [(block_index, offset, size)])`` of an Exta payload's
    BlockDataBeginChunk records, in file order (the loop mirrors ``iter_tiles``)."""
    cache = getattr(clip, "tile_index_cache", None)
    if cache is not None and outer_id in cache:
        return cache[outer_id]
    exta = clip.extas[outer_id]
    payload = memoryview(clip.raw)[exta.payload_off:exta.payload_off + exta.payload_size]
    if bytes(payload[8:16]) != b"extrnlid":
        raise ValueError("Exta payload missing extrnlid tag")
    inner_size = struct.unpack(">Q", payload[48:56])[0]
    inner = payload[56:56 + inner_size]
    begin = BEGIN_NAME.encode("utf-16-be")
    records: list[tuple[int, int, int]] = []
    p = 0
    while p + 8 <= len(inner):
        rec_size, name_len = struct.unpack_from(">II", inner, p)
        if name_len * 2 + 8 > rec_size or p + rec_size > len(inner):
            break
        if rec_size >= 50 and inner[p + 8:p + 8 + 2 * name_len] == begin:
            records.append((struct.unpack_from(">I", inner, p + 46)[0], p, rec_size))
        p += rec_size
    index = (bytes(payload[:48]), inner, records)
    if cache is not None:
        cache[outer_id] = index
    return index


_warned_offscreens: set[str] = set()


def _warn_offscreen_failure(outer_id: str, exc: Exception | str) -> None:
    """``exc``: what was raised, or (from tmi_accel) why, as text."""
    if outer_id in _warned_offscreens:
        return
    _warned_offscreens.add(outer_id)
    import sys

    why = exc if isinstance(exc, str) else f"{type(exc).__name__}: {exc}"
    print(f"tmiki: skipping unreadable CLIP offscreen {outer_id}: {why}", file=sys.stderr)


_warned_tile_sizes: set[int] = set()


def _warn_unknown_tile_size(bpt: int, pixels: int) -> None:
    if bpt in _warned_tile_sizes:
        return
    _warned_tile_sizes.add(bpt)
    import sys

    print(
        f"tmiki: unknown CLIP tile size {bpt} for {pixels}-pixel block; rendering as transparent",
        file=sys.stderr,
    )


# ─────────────────────────── compositing ──────────────────────────────────────
#
# CSP / CLIP LayerComposite ids, read back from a file where CLIP STUDIO PAINT
# 5.1.5 itself set every blend mode of its layer palette, one per layer
# (2026-10-10, example/csp515/blendids.clip; example/blend_modes.clip carries
# the same ids). An earlier table taken from a PSD-interop listing was wrong for
# almost every id (e.g. multiply was read as darken, overlay as hard light) and
# went unnoticed because blend_modes.clip's raster layers are all empty.
#
# 10 覆い焼き(発光) and 12 加算(発光) are CSP-only "glow" variants (see
# tmiref's _GLOW_MODES). 30 is folder pass-through; 36 is divide (not a second
# pass-through id). BLEND_NORMAL / BLEND_PASS_THROUGH come from composite.py.

_COMPOSITE_MAP: dict[int, str] = {
    0: "normal",
    1: "darken",          # 比較(暗)
    2: "multiply",        # 乗算
    3: "color_burn",      # 焼き込みカラー
    4: "linear_burn",     # 焼き込み(リニア)
    5: "subtract",        # 減算
    6: "darker_color",    # カラー比較(暗)
    7: "lighten",         # 比較(明)
    8: "screen",          # スクリーン
    9: "color_dodge",     # 覆い焼きカラー
    10: "glow_dodge",     # 覆い焼き(発光)
    11: "linear_dodge",   # 加算
    12: "glow_add",       # 加算(発光)
    13: "lighter_color",  # カラー比較(明)
    14: "overlay",        # オーバーレイ
    15: "soft_light",     # ソフトライト
    16: "hard_light",     # ハードライト
    17: "vivid_light",    # ビビッドライト
    18: "linear_light",   # リニアライト
    19: "pin_light",      # ピンライト
    20: "hard_mix",       # ハードミックス
    21: "difference",     # 差の絶対値
    22: "exclusion",      # 除外
    23: "hue",            # 色相
    24: "saturation",     # 彩度
    25: "color",          # カラー
    26: "luminosity",     # 輝度
    30: "pass_through",   # 通過 (folders)
    36: "divide",         # 除算
}


def _composite_name(composite_id: int) -> str:
    name = _COMPOSITE_MAP.get(composite_id)
    if name is None:
        _warn_unknown_composite(composite_id)
        return BLEND_NORMAL
    return name


_warned_composites: set[int] = set()


def _warn_unknown_composite(composite_id: int) -> None:
    if composite_id in _warned_composites:
        return
    _warned_composites.add(composite_id)
    import sys

    print(
        f"tmiki: unknown CLIP composite id {composite_id}; rendering as normal",
        file=sys.stderr,
    )


def render_canvas_preview_rgba(
    clip: Any,
    *,
    canvas_size: bool = False,
):
    """Return the saved CSP canvas preview, if the CLIP database has one, as
    a ``tmi.raster.Rgba`` (``canvas_preview``).

    ``CanvasPreview`` is generated by Clip Studio Paint itself, so it preserves
    CSP-only rendering that our layer compositor cannot fully reconstruct yet
    (adjustment layers, 3D/vector/text rasterization, and exact clipping-group
    semantics). Some files store it at reduced resolution; callers that need a
    canvas-sized image can request a resize.
    """
    return canvas_preview(clip, canvas_size=canvas_size)


def canvas_preview(clip: Any, *, canvas_size: bool = False):
    """The CanvasPreview as a ``tmi.raster.Rgba`` (None without one):
    decoded and resized (LANCZOS) by tmi_accel, byte for byte Pillow's."""
    try:
        row = clip.db.execute(
            "SELECT ImageWidth, ImageHeight, ImageData FROM CanvasPreview LIMIT 1"
        ).fetchone()
    except Exception:
        return None
    if not row or row[2] is None:
        return None
    data = bytes(row[2])
    if _preview_exceeds_canvas(data, row[0], row[1], clip.canvas.width, clip.canvas.height):
        return None
    decoded = _decode_preview(data)
    if decoded is None:
        return None
    width, height, rgba = decoded
    expected_size = (int(row[0] or width), int(row[1] or height))
    target_size = (clip.canvas.width, clip.canvas.height) if canvas_size else None
    if (width, height) != expected_size or (target_size is not None and expected_size != target_size):
        # Image.resize(LANCZOS) to the declared size, then to the canvas.
        width, height, rgba = accel.native().resize_canvas_preview(rgba, width, height, expected_size, target_size)
    return Rgba(width, height, rgba)


def _preview_exceeds_canvas(data: bytes, declared_w: Any, declared_h: Any, canvas_w: int, canvas_h: int) -> bool:
    """CSP writes a CanvasPreview no larger than the canvas; a bigger one (by
    its declared size, or a PNG's IHDR) is a decompression bomb, refused
    before anything is decoded (same rule as tmi-import's ``preview``)."""
    limit = max(int(canvas_w), 1) * max(int(canvas_h), 1)
    try:
        if max(int(declared_w or 0), 0) * max(int(declared_h or 0), 0) > limit:
            return True
    except (TypeError, ValueError):
        return False
    if data.startswith(b"\x89PNG\r\n\x1a\n") and len(data) >= 24 and data[12:16] == b"IHDR":
        w, h = struct.unpack(">II", data[16:24])
        return w * h > limit
    return False


def _decode_preview(data: bytes) -> tuple[int, int, bytes] | None:
    """``Image.open(data).convert("RGBA")`` as ``(width, height, rgba)``, or
    None where that raises. tmi_accel decodes PNG and JPEG (Pillow's pixels,
    RUST_MIGRATION.md 2a); Pillow takes whatever it leaves (another format,
    an APNG, damaged data)."""
    decoded = accel.native().decode_canvas_preview(data)
    if decoded is not None:
        return decoded
    from tmi.image_ops import _pil  # Pillow, with tmi's image-size limit installed

    try:
        image = _pil().open(io.BytesIO(data)).convert("RGBA")
    except Exception:
        return None
    return image.width, image.height, image.tobytes()


def render_composite_with_png(clip: Any, base: Any = None, *, on_image: Any = None):
    """``render_composite_rgba`` plus, when CSP's CanvasPreview is already a
    canvas-sized PNG, those PNG bytes — so the importer can store them instead
    of re-encoding the same pixels (~0.2s on a 12-megapixel canvas).

    Without a usable preview the layered render is returned with its PNG,
    tagged as such (``clip_partial.encode_layered_composite``). ``base`` (a
    ``clip_partial.CompositeBase``: an earlier version of this file) lets that
    render re-composite only what changed. ``on_image(image)``, if given, is
    called with the layered render before it is encoded, so the caller can
    work on it (the thumbnail) during the encode.

    Returns ``(image, png_bytes)``; ``png_bytes`` is None whenever the preview
    is not a PNG or needs resizing (then the caller encodes). A preview comes
    back as a ``tmi.raster.Rgba``, and so does a layered render."""
    try:
        row = clip.db.execute(
            "SELECT ImageWidth, ImageHeight, ImageData FROM CanvasPreview LIMIT 1"
        ).fetchone()
    except Exception:
        row = None
    if row and row[2] is not None:
        data = bytes(row[2])
        canvas = (clip.canvas.width, clip.canvas.height)
        declared = (int(row[0] or 0), int(row[1] or 0))
        # A PNG names its size in IHDR, the first chunk (Pillow reads no
        # further to open one): decode only a canvas-sized one.
        if (
            data.startswith(b"\x89PNG\r\n\x1a\n")
            and len(data) >= 24
            and data[12:16] == b"IHDR"
            and struct.unpack(">II", data[16:24]) == canvas
            and declared in {canvas, (0, 0)}
        ):
            decoded = _decode_preview(data)
            if decoded is not None and decoded[:2] == canvas:
                return Rgba(canvas[0], canvas[1], decoded[2]), data
    preview = canvas_preview(clip, canvas_size=True)
    if preview is not None:
        return preview, None
    from tmi.formats import clip_partial

    image = None
    if base is not None:
        image = clip_partial.render_incremental(clip, base)
        if image is not None and clip_partial.verify_enabled():
            clip_partial.check_against_full_render(clip, image)
    is_base = True
    if image is None:
        image, is_base = clip_partial.render_full(clip)
    if on_image is not None:
        on_image(image)
    return image, clip_partial.encode_layered_composite(image, is_base)


def render_composite_rgba(
    clip: Any,
    background=(255, 255, 255, 255),
    *,
    prefer_canvas_preview: bool = True,
) -> Rgba:
    """Composite the entire CLIP into a single RGBA canvas.

    When available, uses CSP's saved ``CanvasPreview`` first because it already
    contains the application's exact final render. If a file has no preview,
    falls back to tmi's layer compositor, which handles raster layers, masks,
    clipping, blend modes, pass-through folders, and offsets.
    """
    if prefer_canvas_preview:
        preview = render_canvas_preview_rgba(clip, canvas_size=True)
        if preview is not None:
            return preview
    return render_layered_composite_rgba(clip, background)


def render_layered_composite_rgba(
    clip: Any,
    background=(255, 255, 255, 255),
    *,
    region: tuple[int, int, int, int] | None = None,
) -> Rgba:
    """Composite the CLIP's layers without using CanvasPreview.

    ``region`` (canvas pixels, x0 y0 x1 y1) renders just that window and
    returns an image of its size holding exactly the pixels the whole-canvas
    render has there; the whole canvas is the special case ``region=None``.

    tmi_accel's ``render_clip`` draws it (raster, vector and material layers,
    folders and pass-through, clipping, layer masks, every blend mode,
    correction layers, fills and gradients, トーン and フチ, comic frames),
    byte for byte as the reference renderer ``tmiref`` does, which documents
    why a window equals the crop of the whole render. Layers whose data is
    unreadable are warned about and left out (``clip.decode_failures``
    collects those whose stored pixels failed); a file the renderer cannot
    draw raises ``ClipRenderError``.
    """
    full = (0, 0, clip.canvas.width, clip.canvas.height)
    window = full if region is None else _intersect_bounds(tuple(region), full)
    if window is None:
        raise ValueError(f"render region {region} lies outside the {full[2]}x{full[3]} canvas")
    return _render(clip, background, regions=None if window == full else [window])[0]


def render_layered_regions(
    clip: Any,
    regions: list[tuple[int, int, int, int]],
    background=(255, 255, 255, 255),
) -> list[Rgba]:
    """``render_layered_composite_rgba(clip, background, region=r)`` for each
    of ``regions`` (inside the canvas), drawn in one tmi_accel call (shared
    work: vector strokes, comic-frame shapes, masks and material originals
    are prepared once)."""
    full = (0, 0, clip.canvas.width, clip.canvas.height)
    windows = []
    for region in regions:
        window = _intersect_bounds(tuple(region), full)
        if window is None:
            raise ValueError(f"render region {region} lies outside the {full[2]}x{full[3]} canvas")
        windows.append(window)
    if not windows:
        return []
    return _render(clip, background, regions=windows)


def render_folder(clip: Any, main_id: int) -> Rgba:
    """A folder's contents (blend modes, clipping, masks) composited on a
    transparent canvas, as the source app would show that folder alone."""
    return _render(clip, (0, 0, 0, 0), folder=main_id)[0]


class ClipRenderError(RuntimeError):
    """The CLIP renderer cannot draw this file (``tmi_accel.render_clip``
    declined it: something it does not reproduce)."""


def _render(clip: Any, background, *, regions: list | None = None, folder: int | None = None) -> list[Rgba]:
    """``tmi_accel.render_clip`` of ``clip`` (which reads the file from
    ``clip.raw``; the tree ``open_clip_bytes`` built is not consulted): the
    whole canvas, each of ``regions``, or one folder. Its warnings are
    replayed through the ``_warn_*`` functions (same text, same
    de-duplication), its decode failures added to ``clip.decode_failures``
    when that is a set, and its trace records written when
    ``TMI_RENDER_TRACE`` is set."""
    bg = tuple(background) if isinstance(background, (tuple, list)) else None
    if bg is None or len(bg) != 4 or not all(isinstance(v, int) and 0 <= v <= 255 for v in bg):
        raise ValueError(f"background {background!r} is not an RGBA tuple")
    decodable = getattr(clip, "decodable_layers", None)
    decodable_all = bool(decodable) and regions is not None and all(
        n.main_id in decodable for n in walk_layer_nodes(clip.root.children) if not n.folder
    )
    raw = clip.raw if isinstance(clip.raw, bytes) else bytes(clip.raw)
    result = accel.native().render_clip(
        raw, bg, regions=regions, decodable_all=decodable_all, folder=folder, trace=_TRACE
    )
    if "declined" in result:
        raise ClipRenderError(f"the CLIP renderer cannot draw this file ({result['declined']})")
    failures = getattr(clip, "decode_failures", None)
    if failures is not None:
        failures.update(result["decode_failures"])
    for kind, key in result["warnings"]:
        if kind == "composite":
            _warn_unknown_composite(key)
        elif kind == "filter":
            _warn_unsupported_filter(key)
        elif kind == "offscreen":
            _warn_offscreen_failure(key[0], key[1])
        elif kind == "tile-size":
            _warn_unknown_tile_size(*key)
    if _TRACE and result["trace"]:
        from tmi.formats import render_trace

        render_trace.write(result["trace"])
    if regions is not None:
        sizes = [_canvas_size(w) for w in regions]
    else:
        sizes = [(clip.canvas.width, clip.canvas.height)]
    return [_rgba_image(rgba, size) for rgba, size in zip(result["images"], sizes)]


def _rgba_image(rgba: bytes, size: tuple[int, int]) -> Rgba:
    """A ``tmi.raster.Rgba`` over ``rgba`` (no copy; nothing writes to it)."""
    return Rgba(size[0], size[1], rgba)


def _edge_margin(clip: Any, node: Any) -> int:
    """How far (px) a node's フチ outline reads and draws beyond its own pixels:
    ``apply_edge`` takes disk maxima of radius up to floor(width) + 1, so a
    window grown by ceil(width) + 1 holds every pixel its outline depends on.
    0 without an enabled outline."""
    if clip is None:
        return 0
    from tmi.formats import clip_style

    edge = clip_style.parse_edge_effect(clip_style.layer_style(clip, node.main_id))
    return 0 if edge is None else int(math.ceil(edge[0])) + 1


# ─────────────────────────── adjustment (filter) layers ───────────────────────
#
# A ``LayerType == 4098`` "correction layer" re-grades the pixels beneath it
# instead of contributing its own pixels. Its parameters live in the
# ``Layer.FilterLayerInfo`` BLOB: ``>u32 filter_type``, ``>u32 data_size``, then
# type-specific data. The filter ids were recovered from layer_types.clip (one
# correction layer of each kind, identified by its Japanese ``LayerName``):
#
#   1 brightness/contrast  2 levels      3 tone curve   4 hue/saturation/luminosity
#   5 colour balance       6 invert      7 posterize    8 binarise   9 gradient map
#
# tmi_accel renders (and tmiref fits, in Python) the filters whose maths is exact
# (invert, posterize, binarise) and transfers fitted to CSP-rendered probe clips
# (the rest; see research/clip-format.md §15.5). A zero-parameter layer decodes to None and
# is skipped, so an "identity" correction layer never changes the output; the
# gradient map (9) still passes the backdrop through unchanged.

_FILTER_NAMES = {
    1: "brightness_contrast", 2: "levels", 3: "tone_curve", 4: "hue_saturation",
    5: "color_balance", 6: "invert", 7: "posterize", 8: "binarize", 9: "gradient_map",
}


_warned_filters: set[int] = set()


def _warn_unsupported_filter(filter_type: int) -> None:
    if filter_type in _warned_filters:
        return
    _warned_filters.add(filter_type)
    import sys

    name = _FILTER_NAMES.get(filter_type, "?")
    print(
        f"tmiki: CLIP correction-layer filter {filter_type} ({name}) not yet rendered; "
        "passing the backdrop through unchanged",
        file=sys.stderr,
    )


def find_layer_node(clip: Any, selector: str) -> Any | None:
    """Resolve a layer selector to a node for write-back.

    Stable identities (``clip:<main_id>`` and the layer uuid) are unique and win.
    Names are *not* unique in CSP — duplicating or copying a layer keeps the same
    name — so when a name matches several layers we refuse rather than silently
    overwriting the pixels of whichever happened to come first in tree order.
    """
    normalized = _norm(selector)
    id_matches: list[Any] = []
    name_matches: list[Any] = []
    for node in walk_layer_nodes(clip.root.children):
        if _norm(f"clip:{node.main_id}") == normalized or (node.uuid and _norm(node.uuid) == normalized):
            id_matches.append(node)
        elif node.name and _norm(node.name) == normalized:
            name_matches.append(node)
    if id_matches:
        return id_matches[0]
    if len(name_matches) > 1:
        raise ValueError(
            f"CLIP layer selector {selector!r} is ambiguous: {len(name_matches)} layers share that "
            "name. Rename the layers so each name is unique, or select by clip:<id> or layer uuid."
        )
    return name_matches[0] if name_matches else None


def walk_layer_nodes(nodes: list[Any]) -> list[Any]:
    out: list[Any] = []
    for node in nodes:
        out.append(node)
        out.extend(walk_layer_nodes(node.children))
    return out


# ───────────────────────────── layer write-back ─────────────────────────────
#
# A layer's pixels written back into the file (per-layer stage, restore,
# merge): its offscreen's blocks are encoded again (CSP's own record kept for
# every block whose pixels did not change), the Offscreen.Attribute's block
# sizes, ExternalChunk's offsets and the CanvasPreview (cleared) updated, and
# the container emitted again. tmi_accel does all of it (``clip_write_layer``:
# stock zlib 1.3.1 and SQLite 3.50.4, the versions CPython 3.13 carries), byte
# for byte what the Python write-back (``tmiref.clip_io``) writes.


def replace_layer_pixels(
    source_path: str | Path,
    selector: str,
    png_bytes: bytes,
    *,
    invalidate_preview_on_noop: bool = False,
) -> bytes:
    """The file at ``source_path`` with ``selector``'s raster pixels replaced
    by ``png_bytes`` (a PNG or layer blob, at most the offscreen's size). When
    the pixels equal the layer's the file comes back unchanged, or, with
    ``invalidate_preview_on_noop``, with only its CanvasPreview cleared."""
    source_path = Path(source_path)
    clip = open_clip(source_path)
    try:
        node = find_layer_node(clip, selector)
        if node is None:
            raise ValueError(f"CLIP layer not found: {selector}")
        info = layer_offscreen_info(clip, node.render_mipmap)
    finally:
        clip.db.close()
    if not info:
        raise ValueError(f"CLIP layer has no writable raster offscreen: {selector}")
    offscreen_id, attr_blob, outer_id = info
    if outer_id not in clip.extas:
        raise ValueError(f"CLIP layer has no backed Exta payload: {selector}")
    attr = parse_offscreen_attribute(attr_blob)
    if attr.channel_profile != "color-alpha-bgra":
        raise ValueError(f"CLIP write-back supports color raster layers only, got {attr.channel_profile}")

    from tmi.image_ops import pixels_from_bytes

    image = pixels_from_bytes(png_bytes)  # tmi.raster: the pixels without Pillow
    if image.width > attr.pixel_width or image.height > attr.pixel_height:
        raise ValueError(
            f"replacement image {image.width}x{image.height} is larger than layer offscreen "
            f"{attr.pixel_width}x{attr.pixel_height}"
        )
    rgba = image.tobytes()
    out, decode_failure, unknown_sizes = accel.native().clip_write_layer(
        clip.raw, offscreen_id, attr_blob, outer_id, rgba, image.width, image.height, invalidate_preview_on_noop
    )
    # What decoding the layer's current pixels met (as render_layer_rgba
    # warns about it).
    if decode_failure is not None:
        _warn_offscreen_failure(outer_id, decode_failure)
    for bpt, pixels in unknown_sizes:
        _warn_unknown_tile_size(bpt, pixels)
    return clip.raw if out is None else out


def _norm(value: str) -> str:
    return " ".join(value.strip().casefold().split())
