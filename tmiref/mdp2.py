"""The Python MDP2 reader, compositor and write-back tmi used before stage 2c
(RUST_MIGRATION.md): ElementTree for the XML, Pillow for the tiles and the
composite, zlib for the tile encode. tmi's own (``tmi.formats.mdp2``) runs
in tmi_accel and is tested against this one byte for byte
(tests/test_mdp2_parity.py, tools/render_corpus.py --render python)."""
from __future__ import annotations

import io
import re
import struct
import xml.etree.ElementTree as ET
import zlib
from dataclasses import dataclass, field
from pathlib import Path
from typing import Iterator


MAGIC = b"mdipack\x00"
PAC_MAGIC = b"PAC "
PAC_HEADER_SIZE = 132
PAC_NAME_SIZE = 64
LAYER_TILE_HEADER_SIZE = 24
NEXT_TILE_HEADER_SIZE = 16


@dataclass
class PacBlob:
    name: str
    file_off: int
    record_size: int
    flag: int
    payload_size: int
    aux_size: int
    payload_off: int


@dataclass
class Mdp2Tile:
    index: int
    x: int
    y: int
    compressed_size: int
    data: bytes


@dataclass
class Mdp2Layer:
    name: str
    width: int
    height: int
    mode: str
    alpha: int
    layer_id: int
    parent_id: int
    bin_name: str | None
    type: str
    visible: bool
    offset_x: int = 0
    offset_y: int = 0
    clipping: bool = False
    protect_alpha: bool = False
    folder_open: bool = False
    bin_type: str | None = None
    color: str | None = None
    masking: bool = False
    masking_type: int | None = None
    draft: bool = False
    lumi: bool | None = None
    transform_offset_x: int | None = None
    transform_offset_y: int | None = None
    halftone_type: str | None = None
    halftone_line: int | None = None
    text_lines: list[str] = field(default_factory=list)
    attributes: dict[str, str] = field(default_factory=dict)
    children: list["Mdp2Layer"] = field(default_factory=list)


@dataclass
class Mdp2File:
    raw: bytes
    reserved: int
    xml_size: int
    pack_size: int
    xml_text: str
    xml_root: ET.Element
    canvas_width: int
    canvas_height: int
    dpi: int
    blobs: list[PacBlob]
    blob_by_name: dict[str, PacBlob]
    layers: list[Mdp2Layer]


def parse_mdp2(data: bytes) -> Mdp2File:
    if data[:8] != MAGIC:
        raise ValueError("not an mdipack/MDP2 file")
    reserved, xml_size, pack_size = struct.unpack("<III", data[8:20])
    xml_start = 20
    pack_start = xml_start + xml_size
    if pack_start + pack_size != len(data):
        raise ValueError(f"header sizes do not match file length: {pack_start}+{pack_size}!={len(data)}")

    xml_text = data[xml_start:pack_start].decode("utf-8")
    xml_root = ET.fromstring(xml_text)
    canvas_width = int(xml_root.attrib["width"])
    canvas_height = int(xml_root.attrib["height"])
    dpi = int(xml_root.attrib.get("dpi", "0"))

    blobs: list[PacBlob] = []
    off = pack_start
    while off < len(data):
        if data[off:off + 4] != PAC_MAGIC:
            raise ValueError(f"expected PAC at 0x{off:X}")
        record_size, flag, payload_size, aux_size = struct.unpack("<IIII", data[off + 4:off + 20])
        if record_size < PAC_HEADER_SIZE:
            raise ValueError(f"bad PAC record size at 0x{off:X}: {record_size}")
        payload_off = off + PAC_HEADER_SIZE
        name_off = off + 68
        name = data[name_off:name_off + PAC_NAME_SIZE].split(b"\0", 1)[0].decode("ascii")
        if record_size != PAC_HEADER_SIZE + payload_size:
            raise ValueError(f"PAC size mismatch for {name!r}")
        blobs.append(PacBlob(
            name=name,
            file_off=off,
            record_size=record_size,
            flag=flag,
            payload_size=payload_size,
            aux_size=aux_size,
            payload_off=payload_off,
        ))
        off += record_size

    layers: list[Mdp2Layer] = []
    layers_el = xml_root.find("Layers")
    if layers_el is not None:
        for el in layers_el.findall("Layer"):
            a = el.attrib
            offset_x = _int_attr(a, "ofsx", _int_attr(a, "tofsx", 0))
            offset_y = _int_attr(a, "ofsy", _int_attr(a, "tofsy", 0))
            layers.append(Mdp2Layer(
                name=a.get("name", ""),
                width=int(a.get("width", "0")),
                height=int(a.get("height", "0")),
                mode=a.get("mode", "normal"),
                alpha=int(a.get("alpha", "255")),
                layer_id=int(a.get("id", "0")),
                parent_id=int(a.get("parentId", "-1")),
                bin_name=a.get("bin"),
                type=a.get("type", ""),
                visible=a.get("visible", "true") != "false",
                offset_x=offset_x,
                offset_y=offset_y,
                clipping=a.get("clipping", "false") == "true",
                protect_alpha=a.get("protectAlpha", "false") == "true",
                folder_open=a.get("folderOpen", "false") == "true",
                bin_type=a.get("binType"),
                color=a.get("color"),
                masking=_bool_attr(a, "masking"),
                masking_type=_optional_int_attr(a, "maskingType"),
                draft=_bool_attr(a, "draft"),
                lumi=_optional_bool_attr(a, "lumi"),
                transform_offset_x=_optional_int_attr(a, "tofsx"),
                transform_offset_y=_optional_int_attr(a, "tofsy"),
                halftone_type=a.get("halftoneType"),
                halftone_line=_optional_int_attr(a, "halftoneLine"),
                text_lines=[line.attrib.get("text", "") for line in el.findall("Line")],
                attributes=dict(a),
            ))

    by_id = {layer.layer_id: layer for layer in layers}
    for layer in layers:
        parent = by_id.get(layer.parent_id)
        if parent:
            parent.children.append(layer)

    return Mdp2File(
        raw=data,
        reserved=reserved,
        xml_size=xml_size,
        pack_size=pack_size,
        xml_text=xml_text,
        xml_root=xml_root,
        canvas_width=canvas_width,
        canvas_height=canvas_height,
        dpi=dpi,
        blobs=blobs,
        blob_by_name={blob.name: blob for blob in blobs},
        layers=layers,
    )


def open_mdp2(path: str | Path) -> Mdp2File:
    return parse_mdp2(Path(path).read_bytes())


def emit_mdp2(mdp: Mdp2File) -> bytes:
    return (
        MAGIC
        + struct.pack("<III", mdp.reserved, mdp.xml_size, mdp.pack_size)
        + mdp.xml_text.encode("utf-8")
        + b"".join(mdp.raw[b.file_off:b.file_off + b.record_size] for b in mdp.blobs)
    )


def blob_payload(mdp: Mdp2File, blob: PacBlob) -> bytes:
    return mdp.raw[blob.payload_off:blob.payload_off + blob.payload_size]


def iter_layer_tiles(mdp: Mdp2File, blob: PacBlob) -> Iterator[Mdp2Tile]:
    payload = blob_payload(mdp, blob)
    if blob.payload_size <= 4:
        return
    count, tile_size, x, y, zero, compressed_size = struct.unpack("<IIIIII", payload[:LAYER_TILE_HEADER_SIZE])
    if tile_size != 128:
        raise ValueError(f"{blob.name}: unsupported tile size {tile_size}")
    pos = LAYER_TILE_HEADER_SIZE
    for index in range(count):
        compressed = payload[pos:pos + compressed_size]
        data = zlib.decompress(compressed)
        yield Mdp2Tile(index=index, x=x, y=y, compressed_size=compressed_size, data=data)
        pos += compressed_size
        if index != count - 1:
            pad = (-pos) % 4
            if any(payload[pos:pos + pad]):
                raise ValueError(f"{blob.name}: nonzero tile padding at payload offset {pos}")
            pos += pad
            x, y, zero, compressed_size = struct.unpack("<IIII", payload[pos:pos + NEXT_TILE_HEADER_SIZE])
            if zero != 0:
                raise ValueError(f"{blob.name}: unexpected tile header word {zero}")
            pos += NEXT_TILE_HEADER_SIZE
    pad = (-pos) % 4
    if pad and pos + pad == len(payload) and not any(payload[pos:pos + pad]):
        pos += pad
    if pos != len(payload):
        raise ValueError(f"{blob.name}: trailing payload bytes at {pos}/{len(payload)}")


def is_renderable_raster(mdp: Mdp2File, layer: Mdp2Layer) -> bool:
    """True when a layer carries a 128px tile raster we can decode.

    MediBang stores a rasterised cache for non-pixel layers too — text and
    vector layers ship the same ``128x128`` BGRA tile stream as ordinary
    ``32bpp`` layers, just under a different ``type``. So rather than gate on
    ``type == "32bpp"`` we check whether the layer's bin blob actually looks
    like a tile stream. Folders (which carry no pixel bin) and any future
    non-tile payload fall through as non-renderable.
    """
    if not layer.bin_name:
        return False
    blob = mdp.blob_by_name.get(layer.bin_name)
    if not blob or blob.payload_size < 4:
        return False
    payload = blob_payload(mdp, blob)
    if _supported_layer_type(layer.type) and blob.payload_size == 4:
        try:
            return struct.unpack("<I", payload[:4])[0] == 0
        except struct.error:
            return False
    if blob.payload_size <= LAYER_TILE_HEADER_SIZE:
        return False
    try:
        count, tile_size, *_ = struct.unpack("<IIIIII", payload[:LAYER_TILE_HEADER_SIZE])
    except struct.error:
        return False
    return tile_size == 128 and count > 0 and _supported_layer_type(layer.type)


def render_layer_rgba(mdp: Mdp2File, layer: Mdp2Layer, apply_opacity: bool = True):
    """Decode a layer's tile raster into an RGBA image.

    ``apply_opacity`` (the default, used by compositing) bakes ``layer.alpha``
    into the result. Callers that need the layer's *intrinsic* pixels — the
    per-layer blob the importer stores and that write-back round-trips — pass
    ``apply_opacity=False`` so opacity stays a separate property and the tile
    bytes survive a stage → restore cycle unchanged.
    """
    from PIL import Image

    if not is_renderable_raster(mdp, layer):
        return Image.new("RGBA", (max(layer.width, 1), max(layer.height, 1)), (0, 0, 0, 0))
    blob = mdp.blob_by_name[layer.bin_name]  # type: ignore[index]

    image = Image.new("RGBA", (layer.width, layer.height), (0, 0, 0, 0))
    for tile in iter_layer_tiles(mdp, blob):
        tile_image = _tile_to_rgba(layer, tile)
        image.alpha_composite(tile_image, (tile.x * 128, tile.y * 128))
    if apply_opacity and layer.alpha < 255:
        r, g, b, a = image.split()
        image.putalpha(a.point(lambda v, alpha=layer.alpha: v * alpha // 255))
    return image


def render_layer_crop(mdp: Mdp2File, layer: Mdp2Layer):
    """``LayerCrop.from_full(render_layer_rgba(mdp, layer, apply_opacity=False))``:
    what the importer stores for the layer."""
    from tmi.image_ops import LayerCrop

    return LayerCrop.from_full(render_layer_rgba(mdp, layer, apply_opacity=False))


def _render_layer_buffer(mdp: Mdp2File, layer: Mdp2Layer, apply_opacity: bool = True):
    """Decode a layer into a tight RGBA rectangle plus its layer-local offset.

    The public ``render_layer_rgba`` keeps returning the full layer-sized image
    because tmi stores that PNG for layer restore/write-back. Composite preview
    only needs painted pixels, so this internal path avoids allocating one
    canvas-sized buffer per layer.
    """
    from PIL import Image

    from tmiref.composite import _EMPTY_LAYER_BUFFER, _LayerBuffer, _clip_buffer_to_bounds, _trim_buffer, _union_bounds

    if not is_renderable_raster(mdp, layer):
        return None
    blob = mdp.blob_by_name[layer.bin_name]  # type: ignore[index]
    layer_bounds = (0, 0, layer.width, layer.height)
    tiles = []
    union = None

    for tile in iter_layer_tiles(mdp, blob):
        tile_image = _tile_to_rgba(layer, tile)
        tile_buf = _LayerBuffer(tile_image, tile.x * 128, tile.y * 128)
        tile_buf = _clip_buffer_to_bounds(tile_buf, layer_bounds)
        if tile_buf is None:
            continue
        tile_buf = _trim_buffer(tile_buf)
        if tile_buf is None:
            continue
        tiles.append(tile_buf)
        union = tile_buf.bounds if union is None else _union_bounds(union, tile_buf.bounds)

    if union is None:
        return _EMPTY_LAYER_BUFFER
    image = Image.new("RGBA", (union[2] - union[0], union[3] - union[1]), (0, 0, 0, 0))
    for tile_buf in tiles:
        image.alpha_composite(tile_buf.image, (tile_buf.x - union[0], tile_buf.y - union[1]))
    buffer = _LayerBuffer(image, union[0], union[1])
    if apply_opacity and layer.alpha < 255:
        r, g, b, a = buffer.image.split()
        buffer.image.putalpha(a.point(lambda v, alpha=layer.alpha: v * alpha // 255))
    return _trim_buffer(buffer) or _EMPTY_LAYER_BUFFER


def render_composite_rgba(mdp: Mdp2File, background=(255, 255, 255, 255)):
    """Composite all visible 32bpp layers in file order (bottom → top).

    Clipping (``clipping=true``) masks the layer by the alpha of the previous
    non-clipping sibling layer in file order. Folders are not recursed into;
    their children appear flat in ``mdp.layers`` and are composited at the
    canvas level.
    """
    from PIL import Image
    from tmiref.composite import (
        _EMPTY_LAYER_BUFFER,
        _CompositeSurface,
        _LayerBuffer,
        _blend_buffer_onto_surface,
        _clip_buffer_to_bounds,
        _is_empty_buffer,
        _mask_buffer_by_base_alpha,
    )

    canvas = Image.new("RGBA", (mdp.canvas_width, mdp.canvas_height), background)
    surface = _CompositeSurface.fixed_canvas(canvas)
    canvas_bounds = (0, 0, mdp.canvas_width, mdp.canvas_height)

    rendered: list[object | None] = [None] * len(mdp.layers)
    for i, layer in enumerate(mdp.layers):
        # Folders appear flat in mdp.layers; their children render individually,
        # so skip the folder node itself (it carries no own pixels).
        if not _effectively_visible(mdp, layer) or layer.children or layer.masking or not is_renderable_raster(mdp, layer):
            continue
        buf = _render_layer_buffer(mdp, layer)
        if buf is None:
            continue
        if _is_empty_buffer(buf):
            rendered[i] = _EMPTY_LAYER_BUFFER
            continue
        buf = _LayerBuffer(buf.image, buf.x + layer.offset_x, buf.y + layer.offset_y)
        buf = _clip_buffer_to_bounds(buf, canvas_bounds)
        if buf is None:
            rendered[i] = _EMPTY_LAYER_BUFFER
            continue
        rendered[i] = buf

    for i, layer in enumerate(mdp.layers):
        if rendered[i] is None or not layer.clipping:
            continue
        base_idx = _clipping_base_index(mdp.layers, i)
        if base_idx is None or rendered[base_idx] is None:
            # No visible clipping base: the clipped layer clips to nothing and
            # must disappear, matching the CLIP compositor (and the source app).
            rendered[i] = _EMPTY_LAYER_BUFFER
            continue
        rendered[i] = _mask_buffer_by_base_alpha(rendered[i], rendered[base_idx])  # type: ignore[arg-type]

    for i, buf in enumerate(rendered):
        if buf is not None:
            _blend_buffer_onto_surface(surface, buf, _MDP_BLEND_TO_TMI.get(mdp.layers[i].mode, "normal"), 1.0)
    return canvas


def _effectively_visible(mdp: Mdp2File, layer: Mdp2Layer) -> bool:
    """A layer renders only when it and every ancestor folder are visible.

    ``mdp.layers`` is flat, so ``layer.visible`` alone misses hidden parent
    folders; walk the ``parent_id`` chain (with a guard against malformed
    cyclic parent links).
    """
    by_id = {l.layer_id: l for l in mdp.layers}
    seen: set[int] = set()
    cur: Mdp2Layer | None = layer
    while cur is not None:
        if not cur.visible:
            return False
        if cur.layer_id in seen:
            return False
        seen.add(cur.layer_id)
        cur = by_id.get(cur.parent_id)
    return True


def _clipping_base_index(layers: list[Mdp2Layer], start: int) -> int | None:
    """Return the index of the clipping base for layer ``start``.

    The base is the nearest earlier sibling (same ``parent_id``) without
    ``clipping`` enabled. Sticking to the same parent prevents a clipped layer
    inside a folder from accidentally clipping to a layer outside the folder.
    """
    parent_id = layers[start].parent_id
    for j in range(start - 1, -1, -1):
        if layers[j].parent_id != parent_id:
            continue
        if not layers[j].clipping:
            return j
    return None


def _mask_by_alpha(image, mask_alpha) -> None:
    from PIL import ImageChops

    a = image.getchannel("A")
    image.putalpha(ImageChops.multiply(a, mask_alpha))


def _tile_to_rgba(layer: Mdp2Layer, tile: Mdp2Tile):
    from PIL import Image

    if layer.type in {"32bpp", "text", "vector"}:
        if len(tile.data) != 128 * 128 * 4:
            raise ValueError(f"{layer.bin_name} tile {tile.index}: expected 32bpp data, got {len(tile.data)} bytes")
        return Image.frombytes("RGBA", (128, 128), tile.data, "raw", "BGRA")
    if layer.type == "8bpp":
        if len(tile.data) != 128 * 128:
            raise ValueError(f"{layer.bin_name} tile {tile.index}: expected 8bpp data, got {len(tile.data)} bytes")
        alpha = Image.frombytes("L", (128, 128), tile.data)
        return _tinted_alpha_image(layer.color, alpha)
    if layer.type == "1bpp":
        if len(tile.data) != 128 * 128 // 8:
            raise ValueError(f"{layer.bin_name} tile {tile.index}: expected 1bpp data, got {len(tile.data)} bytes")
        alpha = Image.frombytes("1", (128, 128), tile.data).convert("L")
        return _tinted_alpha_image(layer.color, alpha)
    raise ValueError(f"{layer.bin_name}: unsupported layer type {layer.type!r}")


def _tinted_alpha_image(color: str | None, alpha):
    from PIL import Image

    r, g, b, a = _parse_argb(color)
    image = Image.new("RGBA", alpha.size, (r, g, b, 0))
    if a != 255:
        alpha = alpha.point(lambda v, ca=a: v * ca // 255)
    image.putalpha(alpha)
    return image


def _parse_argb(value: str | None) -> tuple[int, int, int, int]:
    if not value:
        return 0, 0, 0, 255
    text = value.strip().removeprefix("#")
    try:
        raw = int(text, 16)
    except ValueError:
        return 0, 0, 0, 255
    if len(text) <= 6:
        return (raw >> 16) & 0xFF, (raw >> 8) & 0xFF, raw & 0xFF, 255
    return (raw >> 16) & 0xFF, (raw >> 8) & 0xFF, raw & 0xFF, (raw >> 24) & 0xFF


def _supported_layer_type(layer_type: str) -> bool:
    return layer_type in {"32bpp", "8bpp", "1bpp", "text", "vector"}


# ───────────────────────────── layer write-back ─────────────────────────────
#
# Inverse of the read path: take a layer's RGBA pixels (the intrinsic per-layer
# PNG tmi stores, i.e. ``render_layer_rgba(..., apply_opacity=False)``) and
# rebuild that layer's PAC tile blob, leaving every other blob byte-for-byte
# unchanged. This powers per-layer ``add`` / ``commit`` / ``restore`` / merge
# for ``.mdp`` files, mirroring ``clip.replace_layer_pixels``.

# 32bpp / text / vector store full BGRA tiles; 8bpp / 1bpp store an alpha-only
# tile tinted by the layer's ``color`` attribute at render time.
_BGRA_TILE_TYPES = {"32bpp", "text", "vector"}
WRITABLE_LAYER_TYPES = _BGRA_TILE_TYPES | {"8bpp", "1bpp"}


def replace_layer_pixels(source_path: str | Path, selector: str, png_bytes: bytes) -> bytes:
    """Return new ``.mdp`` bytes with ``selector``'s layer raster replaced by
    ``png_bytes`` (a PNG). When the replacement is pixel-identical to the
    layer's current raster the original file bytes are returned unchanged, so an
    unmodified layer never gets needlessly re-encoded."""
    from PIL import Image

    mdp = open_mdp2(source_path)
    layer = _find_layer_by_selector(mdp, selector)
    if layer is None:
        raise ValueError(f"MDP2 layer not found: {selector}")
    if not layer.bin_name or layer.bin_name not in mdp.blob_by_name:
        raise ValueError(f"MDP2 layer has no writable pixel blob: {selector}")
    if layer.type not in WRITABLE_LAYER_TYPES:
        raise ValueError(f"MDP2 write-back not supported for layer type {layer.type!r}: {selector}")

    from tmi.image_ops import image_from_bytes

    image = image_from_bytes(png_bytes)
    if image.width > layer.width or image.height > layer.height:
        raise ValueError(
            f"replacement image {image.width}x{image.height} is larger than layer "
            f"{layer.width}x{layer.height}; stage the whole file instead"
        )
    if image.size != (layer.width, layer.height):
        padded = Image.new("RGBA", (layer.width, layer.height), (0, 0, 0, 0))
        padded.paste(image, (0, 0))
        image = padded

    current = render_layer_rgba(mdp, layer, apply_opacity=False)
    if current.size == image.size and current.tobytes() == image.tobytes():
        return mdp.raw

    payload = _build_layer_payload(layer, image)
    return _emit_with_replaced_blobs(mdp, {layer.bin_name: payload})


def _find_layer_by_selector(mdp: Mdp2File, selector: str) -> Mdp2Layer | None:
    """Resolve a stage/restore selector to a layer. Accepts ``mdp2:<id>``, a
    bare numeric id, or a (whitespace-normalised, case-folded) layer name — the
    same candidate forms ``repo.materialize_layer_stage`` tries in turn."""
    sel = (selector or "").strip()
    if not sel:
        return None
    body = sel[5:] if sel[:5].casefold() == "mdp2:" else sel
    if body.isdigit():
        candidate_id = int(body)
        for layer in mdp.layers:
            if layer.layer_id == candidate_id:
                return layer
        # No id match: fall through in case a layer is literally *named* the digits.
    normalized = re.sub(r"\s+", " ", sel).casefold()
    name_matches = [
        layer for layer in mdp.layers if re.sub(r"\s+", " ", layer.name or "").casefold() == normalized
    ]
    if len(name_matches) > 1:
        # MediBang layer names are not unique; refuse rather than silently
        # rewriting the pixels of whichever layer happened to be listed first.
        raise ValueError(
            f"MDP2 layer selector {selector!r} is ambiguous: {len(name_matches)} layers share that "
            "name. Rename the layers so each name is unique, or select by mdp2:<id>."
        )
    return name_matches[0] if name_matches else None


def _build_layer_payload(layer: Mdp2Layer, image) -> bytes:
    """Encode an RGBA image into a sparse 128px tile stream (§5 of
    research/mdp2-format.md). Fully transparent/empty tiles are dropped, matching
    how the source app stores only painted tiles; a layer with no painted tiles
    collapses to the 4-byte ``tile_count=0`` sentinel."""
    cols = max(1, (image.width + 127) // 128)
    rows = max(1, (image.height + 127) // 128)
    tiles: list[tuple[int, int, bytes]] = []
    for row in range(rows):
        for col in range(cols):
            tile_data = _encode_tile(layer, image, col, row)
            if tile_data is None:
                continue
            tiles.append((col, row, zlib.compress(tile_data)))

    if not tiles:
        return struct.pack("<I", 0)

    buf = bytearray()
    col0, row0, comp0 = tiles[0]
    buf += struct.pack("<IIIIII", len(tiles), 128, col0, row0, 0, len(comp0))
    buf += comp0
    for col, row, comp in tiles[1:]:
        buf += b"\x00" * ((-len(buf)) % 4)
        buf += struct.pack("<IIII", col, row, 0, len(comp))
        buf += comp
    buf += b"\x00" * ((-len(buf)) % 4)
    return bytes(buf)


def _encode_tile(layer: Mdp2Layer, image, col: int, row: int) -> bytes | None:
    """Build one 128x128 tile's decompressed bytes for ``layer``'s bit depth,
    or ``None`` when the tile is entirely empty (so it can be skipped)."""
    from PIL import Image

    x0, y0 = col * 128, row * 128
    region = image.crop((x0, y0, min(x0 + 128, image.width), min(y0 + 128, image.height)))
    tile = Image.new("RGBA", (128, 128), (0, 0, 0, 0))
    tile.paste(region, (0, 0))

    if layer.type in _BGRA_TILE_TYPES:
        data = tile.tobytes("raw", "BGRA")
        return data if any(data) else None

    # 8bpp / 1bpp: the painted value lives in alpha; the layer's `color`
    # attribute is only a display tint applied at render, so we recover the
    # stored value by undoing that tint's alpha scaling.
    alpha = tile.getchannel("A")
    _, _, _, color_alpha = _parse_argb(layer.color)
    if color_alpha and color_alpha != 255:
        alpha = alpha.point(lambda v, ca=color_alpha: min(255, (v * 255 + ca // 2) // ca))
    if layer.type == "8bpp":
        data = alpha.tobytes()
        return data if any(data) else None
    # 1bpp: pack to MSB-first monochrome (128/8 == 16 bytes per row, no padding).
    bits = alpha.point(lambda v: 255 if v >= 128 else 0).convert("1")
    data = bits.tobytes()
    return data if any(data) else None


def _emit_with_replaced_blobs(mdp: Mdp2File, replacements: dict[str, bytes]) -> bytes:
    """Re-emit the file with the named blobs' payloads swapped out. Untouched
    blobs are copied verbatim; the header, XML, and every other record stay
    byte-for-byte identical, and ``pack_size`` is recomputed from the result."""
    records: list[bytes] = []
    for blob in mdp.blobs:
        if blob.name in replacements:
            new_payload = replacements[blob.name]
            header = bytearray(mdp.raw[blob.file_off:blob.file_off + PAC_HEADER_SIZE])
            struct.pack_into("<I", header, 4, PAC_HEADER_SIZE + len(new_payload))  # record_size
            struct.pack_into("<I", header, 12, len(new_payload))                   # payload_size
            struct.pack_into("<I", header, 16, len(new_payload))                   # aux_size == payload_size for layer blobs
            records.append(bytes(header) + new_payload)
        else:
            records.append(mdp.raw[blob.file_off:blob.file_off + blob.record_size])

    pack = b"".join(records)
    return (
        MAGIC
        + struct.pack("<III", mdp.reserved, mdp.xml_size, len(pack))
        + mdp.xml_text.encode("utf-8")
        + pack
    )


_MDP_BLEND_TO_TMI = {
    "normal": "normal",
    "through": "pass_through",
    "mul": "multiply",
    "add": "linear_dodge",
    "add2": "linear_dodge",
    "ldodge": "linear_dodge",
    "sub": "subtract",
    "div": "divide",
    "overlay": "overlay",
    "screen": "screen",
    "light": "lighten",
    "dark": "darken",
    "diff": "difference",
    "exclusion": "exclusion",
    "dodge": "color_dodge",
    "burn": "color_burn",
    "lburn": "linear_burn",
    "softlight": "soft_light",
    "hardlight": "hard_light",
    "hue": "hue",
    "saturation": "saturation",
    "color": "color",
    "luminosity": "luminosity",
    "hardmix": "hard_mix",
}


def _bool_attr(attrs: dict[str, str], key: str, default: bool = False) -> bool:
    value = attrs.get(key)
    if value is None:
        return default
    return value.lower() == "true"


def _optional_bool_attr(attrs: dict[str, str], key: str) -> bool | None:
    value = attrs.get(key)
    if value is None:
        return None
    return value.lower() == "true"


def _int_attr(attrs: dict[str, str], key: str, default: int = 0) -> int:
    value = attrs.get(key)
    if value is None:
        return default
    try:
        return int(value)
    except ValueError:
        return default


def _optional_int_attr(attrs: dict[str, str], key: str) -> int | None:
    value = attrs.get(key)
    if value is None:
        return None
    try:
        return int(value)
    except ValueError:
        return None
