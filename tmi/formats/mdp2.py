"""MDP2 (MediBang / FireAlpaca ``.mdp``) files: the container, the XML layer
table, layer pixels, the layered composite and layer write-back.

The reading, decoding, compositing and the write-back's tile encode run in
tmi_accel (RUST_MIGRATION.md stage 2c: ``parse_mdp2``, ``mdp2_layer_pixels``,
``render_mdp2``, ``mdp2_write_layer``). The
Python code they reproduce byte for byte (ElementTree, Pillow) is the
reference package ``tmiref.mdp2``. Format notes: research/mdp2-format.md.
"""
from __future__ import annotations

import re
import struct
from dataclasses import dataclass, field
from pathlib import Path

from tmi import accel

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
    # Position in the file's layer table (what tmi_accel calls the layer).
    index: int = -1
    # ``is_renderable_raster``: the layer's bin blob is a 128px tile stream.
    renderable: bool = False


@dataclass
class Mdp2File:
    raw: bytes
    reserved: int
    xml_size: int
    pack_size: int
    xml_text: str
    canvas_width: int
    canvas_height: int
    dpi: int
    blobs: list[PacBlob]
    blob_by_name: dict[str, PacBlob]
    layers: list[Mdp2Layer]


_LAYER_FIELDS = (
    "name", "width", "height", "mode", "alpha", "layer_id", "parent_id", "bin_name", "type", "visible",
    "offset_x", "offset_y", "clipping", "protect_alpha", "folder_open", "bin_type", "color", "masking",
    "masking_type", "draft", "lumi", "transform_offset_x", "transform_offset_y", "halftone_type",
    "halftone_line", "text_lines", "attributes", "renderable",
)


def parse_mdp2(data: bytes) -> Mdp2File:
    """Read an MDP2 file (ValueError when it is not one or is malformed)."""
    data = bytes(data)
    parsed = accel.native().parse_mdp2(data)
    blobs = [PacBlob(*row) for row in parsed["blobs"]]
    layers = [
        Mdp2Layer(index=i, **{name: row[name] for name in _LAYER_FIELDS})
        for i, row in enumerate(parsed["layers"])
    ]
    for layer, row in zip(layers, parsed["layers"]):
        layer.children = [layers[k] for k in row["children"]]
    xml_size = parsed["xml_size"]
    return Mdp2File(
        raw=data,
        reserved=parsed["reserved"],
        xml_size=xml_size,
        pack_size=parsed["pack_size"],
        xml_text=data[20:20 + xml_size].decode("utf-8"),
        canvas_width=parsed["canvas_width"],
        canvas_height=parsed["canvas_height"],
        dpi=parsed["dpi"],
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


def is_renderable_raster(mdp: Mdp2File, layer: Mdp2Layer) -> bool:
    """True when a layer carries a 128px tile raster we can decode.

    MediBang stores a rasterised cache for non-pixel layers too — text and
    vector layers ship the same ``128x128`` BGRA tile stream as ordinary
    ``32bpp`` layers, just under a different ``type``. So rather than gate on
    ``type == "32bpp"`` the reader checks whether the layer's bin blob
    actually looks like a tile stream. Folders (which carry no pixel bin) and
    any future non-tile payload fall through as non-renderable.
    """
    return layer.renderable


def render_layer_crop(mdp: Mdp2File, layer: Mdp2Layer):
    """The layer's own pixels (opacity kept separate) as a ``LayerCrop``:
    what the importer stores and write-back compares against. Decoded in
    tmi_accel into the painted rectangle (no layer-sized image); raises
    ValueError for a layer whose tiles do not decode. The crop is a
    ``tmi.raster.Rgba``."""
    from tmi import pixel_limit
    from tmi.image_ops import LayerCrop
    from tmi.raster import Rgba

    # the layer's own size is held to the layered limit before a tile is
    # decoded: its tiles can paint a rectangle of any size up to it
    if layer.renderable:
        pixel_limit.check_pixels(layer.width, layer.height, f"layer {layer.name!r}")
    w, h, rect, rgba = accel.native().mdp2_layer_pixels(mdp.raw, layer.index)
    if rect is None:
        return LayerCrop.from_tight_crop((w, h), 0, 0, None)
    x0, y0, x1, y1 = rect
    return LayerCrop.from_tight_crop((w, h), x0, y0, Rgba(x1 - x0, y1 - y0, rgba), rgba)


def render_layer_rgba(mdp: Mdp2File, layer: Mdp2Layer, apply_opacity: bool = True):
    """The layer as a layer-sized ``tmi.raster.Rgba`` (``render_layer_crop``
    expanded).

    ``apply_opacity`` (the default) bakes ``layer.alpha`` into the alpha as
    Pillow's ``point(lambda v: v * alpha // 255)`` does (clipped to a byte);
    ``apply_opacity=False`` gives the intrinsic pixels the importer stores."""
    from tmi.raster import Rgba, Sparse

    crop = render_layer_crop(mdp, layer)
    image = Sparse(crop.size, *(crop.rect[:2] if crop.rect else (0, 0)), crop.image).dense()
    if apply_opacity and layer.alpha < 255:
        if layer.alpha < -(2**31):
            raise OverflowError("Python int too large to convert to C long")
        data = bytearray(image.tobytes())
        lut = bytes(max(0, min(255, v * layer.alpha // 255)) for v in range(256))
        data[3::4] = data[3::4].translate(lut)
        image = Rgba(image.width, image.height, bytes(data))
    return image


def render_composite_rgba(mdp: Mdp2File, background=(255, 255, 255, 255)):
    """Composite all visible raster layers in file order (bottom → top).

    Clipping (``clipping=true``) masks the layer by the alpha of the previous
    non-clipping sibling layer in file order. Folders are not recursed into;
    their children appear flat in ``mdp.layers`` and are composited at the
    canvas level. ``mdp.layers`` may be a subset of the file's layers (a
    folder's contents, ``native.render_folder``): visibility and clipping
    bases are then looked up within it. Returns a ``tmi.raster.Rgba``."""
    from tmi.raster import Rgba

    w, h, rgba = accel.native().render_mdp2(mdp.raw, tuple(background), [layer.index for layer in mdp.layers])
    return Rgba(w, h, rgba)


# ───────────────────────────── layer write-back ─────────────────────────────
#
# Inverse of the read path: take a layer's RGBA pixels (the intrinsic per-layer
# pixels tmi stores, i.e. ``render_layer_rgba(..., apply_opacity=False)``) and
# rebuild that layer's PAC tile blob, leaving every other blob byte-for-byte
# unchanged. This powers per-layer ``add`` / ``commit`` / ``restore`` / merge
# for ``.mdp`` files, mirroring ``clip.replace_layer_pixels``. The tile encode
# (stock zlib 1.3.1 at level 6, as CPython's ``zlib.compress``) and the
# re-emit run in tmi_accel (``mdp2_write_layer``), byte for byte the bytes
# ``tmiref.mdp2.replace_layer_pixels`` writes.

# 32bpp / text / vector store full BGRA tiles; 8bpp / 1bpp store an alpha-only
# tile tinted by the layer's ``color`` attribute at render time.
WRITABLE_LAYER_TYPES = {"32bpp", "text", "vector", "8bpp", "1bpp"}


def replace_layer_pixels(source_path: str | Path, selector: str, png_bytes: bytes) -> bytes:
    """Return new ``.mdp`` bytes with ``selector``'s layer raster replaced by
    ``png_bytes`` (a PNG or layer blob). When the replacement is pixel-identical
    to the layer's current raster the original file bytes are returned unchanged,
    so an unmodified layer never gets needlessly re-encoded."""
    mdp = open_mdp2(source_path)
    layer = _find_layer_by_selector(mdp, selector)
    if layer is None:
        raise ValueError(f"MDP2 layer not found: {selector}")
    if not layer.bin_name or layer.bin_name not in mdp.blob_by_name:
        raise ValueError(f"MDP2 layer has no writable pixel blob: {selector}")
    if layer.type not in WRITABLE_LAYER_TYPES:
        raise ValueError(f"MDP2 write-back not supported for layer type {layer.type!r}: {selector}")

    from tmi.image_ops import pixels_from_bytes

    image = pixels_from_bytes(png_bytes)  # tmi.raster: the pixels without Pillow
    if image.width > layer.width or image.height > layer.height:
        raise ValueError(
            f"replacement image {image.width}x{image.height} is larger than layer "
            f"{layer.width}x{layer.height}; stage the whole file instead"
        )
    rgba = image.tobytes()
    out = accel.native().mdp2_write_layer(mdp.raw, layer.index, rgba, image.width, image.height)
    return mdp.raw if out is None else out


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


