"""The Python CLIP CanvasPreview decode and layer write-back tmi used before
stage 2c (RUST_MIGRATION.md): Pillow for the preview and the tiles, numpy
for the tile planes, CPython's zlib and sqlite3 for the bytes. tmi's own
(``tmi.formats.clip``) runs in tmi_accel and is tested against these byte
for byte (tests/test_clip_io_parity.py).

The layer write-back, when the pixels change, rewrites the layer's Exta with
CSP's trailing tables (``_trailing_tables``: BlockStatus, the source's value
or 1, and BlockCheckSum, the adler32 of every stored tile blob), updates the
Offscreen.Attribute's BlockSize table, marks the layer's LayerThumbnail stale
(``_mark_thumbnail_stale``: every ``*NeedRefresh*`` column set to 1, so CSP
redraws the thumbnail instead of trusting the old one; research/
clip-writeback-consistency.md), rewrites ExternalChunk and clears the
CanvasPreview's ImageData."""
from __future__ import annotations

import io
import sqlite3
import struct
import tempfile
import zlib
from pathlib import Path
from typing import Any

import numpy as np
from PIL import Image

from tmi.formats.clip import (
    BEGIN_NAME,
    END_NAME,
    TILE_SIZE,
    Chunk,
    Container,
    find_layer_node,
    layer_offscreen_info,
    open_clip,
    parse_container,
    parse_offscreen_attribute,
    render_layer_rgba,
)


def render_canvas_preview_rgba(clip: Any, *, canvas_size: bool = False) -> Image.Image | None:
    """The saved CSP canvas preview, if the CLIP database has one (Pillow)."""
    try:
        row = clip.db.execute(
            "SELECT ImageWidth, ImageHeight, ImageData FROM CanvasPreview LIMIT 1"
        ).fetchone()
    except Exception:
        return None
    if not row or row[2] is None:
        return None
    try:
        image = Image.open(io.BytesIO(bytes(row[2]))).convert("RGBA")
    except Exception:
        return None
    expected_size = (int(row[0] or image.width), int(row[1] or image.height))
    if image.size != expected_size:
        image = image.resize(expected_size, Image.Resampling.LANCZOS)
    if canvas_size:
        target_size = (clip.canvas.width, clip.canvas.height)
        if image.size != target_size:
            image = image.resize(target_size, Image.Resampling.LANCZOS)
    return image


def emit_byte_exact(source: bytes) -> bytes:
    container = parse_container(source)
    chunks = [(c.cid, source[c.payload_off:c.payload_off + c.payload_size]) for c in container.chunks]
    return emit_container(container.first_chunk_off, chunks)


def _invalidate_canvas_preview(con: sqlite3.Connection) -> bool:
    try:
        columns = {row[1] for row in con.execute("PRAGMA table_info(CanvasPreview)")}
    except sqlite3.Error:
        return False
    if "ImageData" not in columns:
        return False
    cursor = con.execute("UPDATE CanvasPreview SET ImageData = NULL WHERE ImageData IS NOT NULL")
    return cursor.rowcount > 0


def _emit_container_with_sqli_payload(
    container: Container,
    payload_by_file_off: dict[int, bytes],
    sqli_payload: bytes,
) -> bytes:
    chunks: list[tuple[bytes, bytes]] = []
    sqli_offset = container.first_chunk_off
    for chunk in container.chunks:
        if chunk.cid == b"Head":
            # filled after the final SQLi offset is known
            payload = bytearray(payload_by_file_off[chunk.file_off])
            chunks.append((chunk.cid, bytes(payload)))
        elif chunk.cid == b"SQLi":
            chunks.append((chunk.cid, sqli_payload))
        else:
            chunks.append((chunk.cid, payload_by_file_off[chunk.file_off]))

    off = container.first_chunk_off
    for cid, payload in chunks:
        if cid == b"SQLi":
            sqli_offset = off
            break
        off += 16 + len(payload)

    patched: list[tuple[bytes, bytes]] = []
    for cid, payload in chunks:
        if cid == b"Head":
            head = bytearray(payload)
            head[8:16] = struct.pack(">Q", sqli_offset)
            patched.append((cid, bytes(head)))
        else:
            patched.append((cid, payload))
    return emit_container(container.first_chunk_off, patched)


def replace_layer_pixels(
    source_path: str | Path,
    selector: str,
    png_bytes: bytes,
    *,
    invalidate_preview_on_noop: bool = False,
) -> bytes:
    source_path = Path(source_path)
    clip = open_clip(source_path)
    node = find_layer_node(clip, selector)
    if node is None:
        raise ValueError(f"CLIP layer not found: {selector}")
    info = layer_offscreen_info(clip, node.render_mipmap)
    if not info:
        raise ValueError(f"CLIP layer has no writable raster offscreen: {selector}")
    offscreen_id, attr_blob, outer_id = info
    if outer_id not in clip.extas:
        raise ValueError(f"CLIP layer has no backed Exta payload: {selector}")
    attr = parse_offscreen_attribute(attr_blob)
    if attr.channel_profile != "color-alpha-bgra":
        raise ValueError(f"CLIP write-back supports color raster layers only, got {attr.channel_profile}")

    from tmi.image_ops import image_from_bytes

    image = image_from_bytes(png_bytes)
    if image.width > attr.pixel_width or image.height > attr.pixel_height:
        raise ValueError(
            f"replacement image {image.width}x{image.height} is larger than layer offscreen "
            f"{attr.pixel_width}x{attr.pixel_height}"
        )
    if image.size != (attr.pixel_width, attr.pixel_height):
        padded = Image.new("RGBA", (attr.pixel_width, attr.pixel_height), (0, 0, 0, 0))
        padded.alpha_composite(image, (0, 0))
        image = padded

    current_pixels = render_layer_rgba(clip, node)  # tmi's decode: a tmi.raster.Rgba
    current_image = current_pixels.image.to_pil() if current_pixels else None
    if current_pixels and current_image.tobytes() == image.tobytes():
        if invalidate_preview_on_noop:
            container = parse_container(clip.raw)
            sqli_index = next((i for i, c in enumerate(container.chunks) if c.cid == b"SQLi"), None)
            if sqli_index is not None and any(
                c.cid == b"Exta" and i > sqli_index for i, c in enumerate(container.chunks)
            ):
                raise ValueError("CLIP write-back: an Exta chunk after SQLi is unsupported")
            con = _clone_sqlite_connection(clip)
            if _invalidate_canvas_preview(con):
                con.commit()
                payload_by_file_off = {
                    chunk.file_off: clip.raw[chunk.payload_off:chunk.payload_off + chunk.payload_size]
                    for chunk in container.chunks
                }
                sqli_payload = _serialize_sqlite(con)
                return _emit_container_with_sqli_payload(container, payload_by_file_off, sqli_payload)
        return clip.raw

    reuse = None
    if current_pixels and current_image.size == image.size:
        reuse = (current_image, _exta_records_by_block(clip.raw, clip.extas[outer_id]))
    exta_payload, record_sizes = build_color_exta_payload(
        outer_id,
        image,
        attr.tile_cols,
        attr.tile_rows,
        reuse=reuse,
        status=_exta_block_status(clip.raw, clip.extas[outer_id]),
    )
    new_attr = replace_block_sizes(attr_blob, record_sizes)

    con = _clone_sqlite_connection(clip)
    con.execute("UPDATE Offscreen SET Attribute = ? WHERE MainId = ?", (new_attr, offscreen_id))
    _mark_thumbnail_stale(con, offscreen_id)
    con.commit()

    container = parse_container(clip.raw)
    payload_by_file_off: dict[int, bytes] = {
        chunk.file_off: clip.raw[chunk.payload_off:chunk.payload_off + chunk.payload_size]
        for chunk in container.chunks
    }
    payload_by_file_off[clip.extas[outer_id].file_off] = exta_payload

    # _compute_exta_offsets walks the chunk list using the *original* SQLi
    # payload size (payload_by_file_off still holds the pre-edit SQLi bytes),
    # then we re-serialise SQLi to a new, possibly different, size below. That is
    # only safe when every Exta precedes SQLi, which is the layout every real CSP
    # file uses ([Head, Exta..., SQLi, Foot]). If an Exta ever sat after SQLi its
    # ExternalChunk.Offset would be computed against the stale size and point into
    # garbage. Refuse rather than silently emit a corrupt file.
    sqli_index = next((i for i, c in enumerate(container.chunks) if c.cid == b"SQLi"), None)
    if sqli_index is not None and any(
        c.cid == b"Exta" and i > sqli_index for i, c in enumerate(container.chunks)
    ):
        raise ValueError("CLIP write-back: an Exta chunk after SQLi is unsupported")

    exta_offsets = _compute_exta_offsets(container.first_chunk_off, container.chunks, payload_by_file_off)
    con.execute("DELETE FROM ExternalChunk")
    con.executemany(
        "INSERT INTO ExternalChunk(ExternalID, Offset) VALUES (?, ?)",
        [(external_id, offset) for external_id, offset in sorted(exta_offsets.items())],
    )
    _invalidate_canvas_preview(con)
    con.commit()
    sqli_payload = _serialize_sqlite(con)
    return _emit_container_with_sqli_payload(container, payload_by_file_off, sqli_payload)


def _exta_records_by_block(data: bytes, exta: Chunk) -> dict[int, bytes]:
    """The BlockDataBeginChunk records of an Exta payload, by block index."""
    payload = data[exta.payload_off:exta.payload_off + exta.payload_size]
    if payload[8:16] != b"extrnlid":
        return {}
    inner = payload[56:56 + struct.unpack(">Q", payload[48:56])[0]]
    out: dict[int, bytes] = {}
    p = 0
    while p + 8 <= len(inner):
        rec_size, name_len = struct.unpack(">II", inner[p:p + 8])
        if name_len * 2 + 8 > rec_size or p + rec_size > len(inner):
            break
        if inner[p + 8:p + 8 + 2 * name_len].decode("utf-16-be", "replace") == BEGIN_NAME:
            out[struct.unpack(">I", inner[p + 46:p + 50])[0]] = inner[p:p + rec_size]
        p += rec_size
    return out


def _exta_block_status(data: bytes, exta: Chunk) -> int:
    """The first entry of the BlockStatus table an Exta payload ends with (CSP's
    files all have one; every entry of a table is the same), 1 when there is
    none."""
    payload = data[exta.payload_off:exta.payload_off + exta.payload_size]
    if payload[8:16] != b"extrnlid" or len(payload) < 56:
        return 1
    inner = payload[56:56 + struct.unpack(">Q", payload[48:56])[0]]

    def be(at: int) -> int | None:
        return struct.unpack(">I", inner[at:at + 4])[0] if at + 4 <= len(inner) else None

    # The tile records first (the walk of _exta_records_by_block) ...
    p = 0
    while p + 8 <= len(inner):
        rec_size, name_len = struct.unpack(">II", inner[p:p + 8])
        if name_len * 2 + 8 > rec_size or p + rec_size > len(inner):
            break
        p += rec_size
    # ... then the tables: u32 name length, UTF-16BE name, u32 12, u32 count,
    # u32 4, count x u32.
    status_name = "BlockStatus".encode("utf-16-be")
    while (name_len := be(p)) is not None:
        name_end = p + 4 + name_len * 2
        count, entry = be(name_end + 4), be(name_end + 12)
        if count is None or entry is None:
            return 1
        if inner[p + 4:name_end] == status_name:
            return entry if count > 0 else 1
        p = name_end + 12 + count * 4
    return 1


def _record_checksum(record: bytes) -> int:
    """The adler32 of the blob a tile record stores (the ``data_size`` bytes at
    body +24: the little-endian length and the zlib stream), 0 for an empty
    tile."""
    if len(record) < 8:
        return 0
    body = 8 + 2 * struct.unpack(">I", record[4:8])[0]
    if body + 24 > len(record) or struct.unpack(">I", record[body + 16:body + 20])[0] == 0:
        return 0
    size = struct.unpack(">I", record[body + 20:body + 24])[0]
    return zlib.adler32(record[body + 24:body + 24 + size])


def _trailing_tables(status: int, checksums: list[int]) -> bytes:
    """BlockStatus (every entry ``status``) and BlockCheckSum (one adler32 per
    record), after the tile records of an Exta."""
    out = bytearray()
    for name, entries in (("BlockStatus", [status] * len(checksums)), ("BlockCheckSum", checksums)):
        out += struct.pack(">I", len(name)) + name.encode("utf-16-be")
        out += struct.pack(">III", 12, len(entries), 4)
        out += b"".join(struct.pack(">I", entry) for entry in entries)
    return bytes(out)


def _mark_thumbnail_stale(con: sqlite3.Connection, offscreen_id: int) -> None:
    """Every ``*NeedRefresh*`` column of the LayerThumbnail rows of the layers
    whose render offscreen is ``offscreen_id`` set to 1. Nothing happens when
    the table, the columns or the layer are missing."""
    try:
        columns = [row[1] for row in con.execute("PRAGMA table_info(LayerThumbnail)") if "NeedRefresh" in row[1]]
    except sqlite3.Error:
        return
    if not columns:
        return
    sets = ", ".join('"%s" = 1' % column.replace('"', '""') for column in columns)
    try:
        con.execute(
            f"UPDATE LayerThumbnail SET {sets} WHERE MainId IN (SELECT l.LayerRenderThumbnail FROM Layer l "
            "JOIN Mipmap m ON m.MainId = l.LayerRenderMipmap "
            "JOIN MipmapInfo mi ON mi.MainId = m.BaseMipmapInfo WHERE mi.Offscreen = ?)",
            (offscreen_id,),
        )
    except sqlite3.Error:
        pass


def build_color_exta_payload(
    outer_id: str,
    image: Image.Image,
    tile_cols: int,
    tile_rows: int,
    *,
    reuse: tuple[Image.Image, dict[int, bytes]] | None = None,
    status: int = 1,
) -> tuple[bytes, list[int]]:
    """``reuse = (current image, its records by block)`` keeps the original
    record of every tile whose pixels did not change, byte for byte: CSP's
    own encoding stays, and the object store shares those bytes with the
    version the file came from. The records are followed by the BlockStatus
    (``status``) and BlockCheckSum tables CSP writes."""
    records: list[bytes] = []
    record_sizes: list[int] = []
    for block_index in range(tile_cols * tile_rows):
        col = block_index % tile_cols
        row = block_index // tile_cols
        box = (col * TILE_SIZE, row * TILE_SIZE, (col + 1) * TILE_SIZE, (row + 1) * TILE_SIZE)
        crop = image.crop(box)
        if reuse is not None and block_index in reuse[1] and reuse[0].crop(box).tobytes() == crop.tobytes():
            record = reuse[1][block_index]
            records.append(record)
            record_sizes.append(len(record))
            continue
        tile = Image.new("RGBA", (TILE_SIZE, TILE_SIZE), (0, 0, 0, 0))
        tile.alpha_composite(crop, (0, 0))
        record = build_tile_record(block_index, tile)
        records.append(record)
        record_sizes.append(len(record))
    inner = b"".join(records) + _trailing_tables(status, [_record_checksum(record) for record in records])
    outer_hex = outer_id.removeprefix("extrnlid")
    if len(outer_hex) != 32:
        raise ValueError(f"bad CLIP external id: {outer_id}")
    payload = struct.pack(">Q", 0x28) + b"extrnlid" + outer_hex.encode("ascii") + struct.pack(">Q", len(inner)) + inner
    return payload, record_sizes


def build_tile_record(block_index: int, tile: Image.Image) -> bytes:
    rgba = tile.convert("RGBA").tobytes()
    if np is not None:
        # Vectorised equivalent of the per-pixel loop below: alpha plane (one
        # byte per pixel) followed by a BGRA plane whose 4th byte is always 0.
        # The bytes fed to zlib are byte-identical to the loop, so the emitted
        # record is identical too.
        arr = np.frombuffer(rgba, np.uint8).reshape(-1, 4)
        alpha = arr[:, 3]
        bgra = np.zeros((arr.shape[0], 4), dtype=np.uint8)
        bgra[:, :3] = arr[:, [2, 1, 0]]
        any_alpha = bool(alpha.any())
        raw_bytes = alpha.tobytes() + bgra.tobytes()
    else:
        alpha = bytearray(TILE_SIZE * TILE_SIZE)
        bgra = bytearray(TILE_SIZE * TILE_SIZE * 4)
        any_alpha = False
        for i in range(TILE_SIZE * TILE_SIZE):
            r, g, b, a = rgba[i * 4:i * 4 + 4]
            alpha[i] = a
            if a:
                any_alpha = True
            bgra[i * 4 + 0] = b
            bgra[i * 4 + 1] = g
            bgra[i * 4 + 2] = r
            bgra[i * 4 + 3] = 0
        raw_bytes = bytes(alpha + bgra)

    body = bytearray()
    body += struct.pack(">IIIII", block_index, TILE_SIZE * TILE_SIZE * 5, TILE_SIZE, TILE_SIZE, 1 if any_alpha else 0)
    if any_alpha:
        raw = raw_bytes
        compressed = zlib.compress(raw)
        blob = struct.pack("<I", len(compressed)) + compressed
        body += struct.pack(">I", len(blob)) + blob
    body += struct.pack(">I", len(END_NAME)) + END_NAME.encode("utf-16-be")

    name = BEGIN_NAME.encode("utf-16-be")
    record_size = 8 + len(name) + len(body)
    return struct.pack(">II", record_size, len(BEGIN_NAME)) + name + bytes(body)


def replace_block_sizes(attr_blob: bytes, record_sizes: list[int]) -> bytes:
    block_name = "BlockSize".encode("utf-16-be")
    block_name_off = attr_blob.find(block_name)
    if block_name_off < 4:
        raise ValueError("CLIP Offscreen.Attribute missing BlockSize")
    block_off = block_name_off - 4
    block_name_len = struct.unpack(">I", attr_blob[block_off:block_off + 4])[0]
    if block_name_len != len("BlockSize"):
        raise ValueError("bad CLIP BlockSize name length")
    block_size_off = block_off + 4 + block_name_len * 2
    record_size, block_count, entry_size = struct.unpack(">III", attr_blob[block_size_off:block_size_off + 12])
    if record_size != 12 or entry_size != 4:
        raise ValueError("unsupported CLIP BlockSize table")
    if block_count != len(record_sizes):
        raise ValueError(f"BlockSize count {block_count} != emitted records {len(record_sizes)}")
    table_off = block_size_off + 12
    out = bytearray(attr_blob)
    for i, size in enumerate(record_sizes):
        out[table_off + i * 4:table_off + i * 4 + 4] = struct.pack(">I", size)
    return bytes(out)


def emit_container(first_chunk_off: int, chunks: list[tuple[bytes, bytes]]) -> bytes:
    body = bytearray()
    for cid, payload in chunks:
        body += b"CHNK" + cid + struct.pack(">Q", len(payload)) + payload
    pad = first_chunk_off - 24
    out = bytearray()
    out += b"CSFCHUNK"
    out += struct.pack(">Q", 24 + pad + len(body))
    out += struct.pack(">Q", first_chunk_off)
    out += b"\x00" * pad
    out += body
    return bytes(out)


def _compute_exta_offsets(first_chunk_off: int, chunks: list[Any], payload_by_file_off: dict[int, bytes]) -> dict[str, int]:
    offsets: dict[str, int] = {}
    off = first_chunk_off
    for chunk in chunks:
        payload = payload_by_file_off[chunk.file_off]
        if chunk.cid == b"Exta":
            outer_id = payload[8:48].decode("ascii")
            offsets[outer_id] = off
        off += 16 + len(payload)
    return offsets


def _clone_sqlite_connection(clip: Any) -> sqlite3.Connection:
    src = clip.db
    dst = sqlite3.connect(":memory:")
    src.backup(dst)
    return dst


def _serialize_sqlite(con: sqlite3.Connection) -> bytes:
    if hasattr(con, "serialize"):
        try:
            return con.serialize()
        except sqlite3.NotSupportedError:
            pass
    # mkstemp returns an open fd; close it before connecting (Windows refuses to
    # unlink files that still have open handles, and sqlite3 needs to open the
    # path itself).
    import os as _os

    fd, tmp_path = tempfile.mkstemp(suffix=".sqlite")
    _os.close(fd)
    disk = sqlite3.connect(tmp_path)
    try:
        con.backup(disk)
        disk.close()
        return Path(tmp_path).read_bytes()
    finally:
        try:
            disk.close()
        except Exception:
            pass
        Path(tmp_path).unlink(missing_ok=True)


