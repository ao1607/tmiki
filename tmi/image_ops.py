from __future__ import annotations

import hashlib
import io
from dataclasses import dataclass
from pathlib import Path
from typing import TYPE_CHECKING, Any, Iterable

from . import layer_blob as _layer_blob
from . import pixel_limit as _pixel_limit
from . import raster
from .raster import Rgba, Sparse

# Pillow is imported where a PIL image is made or read (``_pil()``), not
# here: decoding, comparing and encoding PNG / JPEG pixels runs on
# tmi_accel (``tmi.raster``, RUST_MIGRATION.md 2b).
if TYPE_CHECKING:
    from PIL import Image

_pil_ready = False


class PillowUnavailableError(BaseException):
    """Pillow is needed (an image tmi_accel leaves to it) and not installed.

    A BaseException, like ``accel.AccelUnavailableError``: a fallback that
    catches Exception (e.g. "not an image: track it as an opaque file") must
    not quietly store something else than an installation with Pillow
    stores. ``tmi.cli`` reports it as an error."""


def _pil():
    """``PIL.Image``, with tmi's image-size limit installed in place of
    Pillow's decompression-bomb default (see tmi.pixel_limit). Raises
    PillowUnavailableError without Pillow.

    Pillow is the decoder of what tmi_accel leaves to it among data tmi has
    stored itself: raster formats other than PNG and JPEG, an animated PNG,
    a JPEG libjpeg warns about, headers ``tmi.raster`` does not read with
    certainty (an MPO, Photoshop resources, ...), a CanvasPreview in another
    format. It never decides whether a file is an image (PLUGINS.md P0e:
    that is ``raster.import_decode``, tmi_accel's). Nothing else in tmi
    imports it."""
    global _pil_ready
    try:
        from PIL import Image
    except ImportError as exc:
        raise PillowUnavailableError(
            "tmi needs Pillow for this file (an image tmi_accel does not read), and it cannot be "
            f"imported ({exc}); "
            "install it with `pip install Pillow`"
        ) from None

    if not _pil_ready:
        _pixel_limit.apply_to_pil()
        _pil_ready = True
    return Image


def _is_pil(image: Any) -> bool:
    return not isinstance(image, (Rgba, Sparse))


def image_to_png_bytes(
    image: Any,
    *,
    parallel: bool = True,
    text: tuple[str, str] | None = None,
    rgba: bytes | None = None,
) -> bytes:
    """Encode to PNG, optionally with one tEXt chunk.

    RGBA pixels (an ``Rgba`` / ``Sparse``, or an RGBA PIL image) are encoded
    by tmi_accel (mtpng), split across cores (~5x faster on a single
    multi-megapixel image) and, unlike Pillow, to the same bytes on every
    machine: Pillow 12's Windows wheels deflate level 1 far more loosely (a
    27MB vs 17MB layer on tama.clip), which then costs seconds in the object
    store. Other modes, and ``parallel=False``, go through Pillow. ``rgba``
    may pass ``image.tobytes()`` when the caller already has it.
    """
    if not _is_pil(image):
        image = image.dense()
        if parallel and image.width and image.height:
            return raster.png(image, text)
        image = image.to_pil()
    if parallel and image.mode == "RGBA" and image.width and image.height:
        from . import accel

        data = rgba if rgba is not None else image.tobytes()
        return accel.native().encode_png_rgba(data, image.width, image.height, text)
    buf = io.BytesIO()
    pnginfo = None
    if text is not None:
        from PIL.PngImagePlugin import PngInfo

        pnginfo = PngInfo()
        pnginfo.add_text(*text)
    # compress_level=1 trades a slightly larger file for a much faster encode
    # (~5x on a multi-megapixel canvas). Safe because change detection keys off
    # decoded-pixel digests (pixel_sha256), not PNG bytes — see P1-1.
    _pil()
    image.save(buf, format="PNG", compress_level=1, pnginfo=pnginfo)
    return buf.getvalue()


def pixels_from_bytes(data: bytes) -> Rgba | Sparse:
    """A stored PNG / JPEG / layer blob's pixels without Pillow: a layer blob
    (TMIL, or a legacy PNG with the "tmi-layer" header) as a ``Sparse``
    layer of its full size, any other image as an ``Rgba``. The pixels of
    ``image_from_bytes(data)``."""
    if _layer_blob.is_layer_blob(data):
        header, rgba = _layer_blob.decode(data)
        _layer_blob._check_pixels(*header.size)
        if header.rect is None:
            return Sparse(header.size)
        return Sparse(header.size, header.x0, header.y0, Rgba(header.width, header.height, rgba))
    try:
        legacy = raster.png_text_before_idat(data, LAYER_BLOB_TEXT_KEY)
    except Exception:  # raster._Unsure, or a header tmi.raster misreads: Pillow reads it
        return raster.as_rgba(_pil_image_from_bytes(data))
    decoded = raster.decode_rgba(data)
    if legacy is None:
        return decoded
    size, rect = _parse_layer_header(legacy)
    if rect is None:
        return Sparse(size)
    return Sparse(size, rect[0], rect[1], decoded)


def image_from_bytes(data: bytes) -> "Image.Image":
    """Decode a PNG/JPEG to an RGBA ``PIL.Image``. Layer blobs come back at
    full layer size. The ``info`` is empty: what tmi stores carries no
    metadata a reader uses (the pixels come from tmi_accel's decoders, not
    Pillow's). tmi itself uses ``pixels_from_bytes``, the same without
    Pillow; this is for the tests, tools/ and ``tmiref``."""
    if _layer_blob.is_layer_blob(data):
        return _layer_blob.decode_image(data, expand=True)
    pixels = pixels_from_bytes(data)
    image = pixels.to_pil()
    image.info = {}
    return image


def _pil_image_from_bytes(data: bytes) -> "Image.Image":
    """The Pillow decode (what tmi did before 2b), for a PNG whose text
    chunks ``tmi.raster`` does not read with certainty."""
    image = _pil().open(io.BytesIO(data))
    header = image.info.get(LAYER_BLOB_TEXT_KEY)
    if header is None:
        out = as_rgba(image)
        out.info = {}
        return out
    size, rect = _parse_layer_header(header)
    full = _pil().new("RGBA", size, (0, 0, 0, 0))
    if rect is not None:
        full.paste(as_rgba(image), rect[:2])
    return full


def decoded_twin(image: Any, png: bytes) -> Any | None:
    """What ``pixels_from_bytes(png)`` / ``image_from_bytes(png)`` give,
    without decoding ``png``, given that ``png`` is a PNG of exactly
    ``image``'s RGBA pixels (it was encoded from them, or ``image`` was
    decoded from it): an image over the same pixels with empty ``info``
    (a decode carries none).

    None when that cannot be told from the PNG's chunks before the image
    data: a layer blob, another size or colour type, an animation, or
    chunks after the image data. The caller then decodes."""
    if image.mode != "RGBA" or not png.startswith(raster.PNG_SIGNATURE) or not _png_tail_is_plain(png):
        return None
    try:
        info = raster.png_header(png, after_idat=False)
    except Exception:  # not read with certainty: the caller decodes
        return None
    if (
        info.get("mode") != "RGBA"
        or info["size"] != image.size
        or info.get("animated")
        or LAYER_BLOB_TEXT_KEY in info.get("text_before", {})
        or LAYER_BLOB_TEXT_KEY in info.get("compressed_text_keys", ())
        or png[24] != 8  # bit depth: 16-bit RGBA decodes to RGBA too, but not to these bytes
    ):
        return None
    if not _is_pil(image):
        return image.dense()
    twin = share_pixels(image)
    twin.info = {}
    return twin


def _png_tail_is_plain(png: bytes) -> bool:
    """Whether nothing but image data follows the first IDAT chunk (Pillow
    adds chunks read after the image data to ``info`` when it decodes)."""
    import struct

    pos = 8
    seen_idat = False
    while pos + 8 <= len(png):
        length, kind = struct.unpack(">I4s", png[pos:pos + 8])
        if kind == b"IEND":
            return True
        if kind == b"IDAT":
            seen_idat = True
        elif seen_idat:
            return False
        pos += 12 + length
    return False


def as_rgba(image: "Image.Image") -> "Image.Image":
    """``image.convert("RGBA")`` (same pixels, same ``info``) without the
    full copy convert() makes of an image that is RGBA already."""
    image.load()
    if image.mode != "RGBA":
        return image.convert("RGBA")
    return share_pixels(image)


def share_pixels(image: "Image.Image") -> "Image.Image":
    """A new ``Image`` over ``image``'s pixel storage with a copy of its
    ``info``: what ``image.copy()`` gives, minus copying the pixels. For an
    image nothing writes to in place afterwards (Pillow operations return new
    images; ``thumbnail()`` rebinds its storage rather than writing into it)."""
    try:
        return image._new(image.im)
    except (AttributeError, TypeError):  # a Pillow without the private helper
        return image.copy()


# ─────────────────────────── layer blobs ──────────────────────────────────────
#
# A layer blob stores only the layer's painted rectangle, in the TMIL container
# (``tmi.layer_blob``: Paeth-filtered rows in independent zstd bands) with the
# full layer size and where the rectangle sits. ``image_from_bytes`` expands it
# back, so readers see full-size layer images (``pixels_from_bytes`` keeps it
# sparse); anything handed to the user or written into a raster file
# (exports, conflict files, PNG/JPEG write-back) goes through
# ``layer_blob_to_png``.
#
# Blobs written before TMIL are PNGs of the crop plus a tEXt chunk
# "tmi-layer" = "v1 W H [x0 y0 x1 y1]"; they are still read (by magic: "TMIL"
# vs the PNG signature), never written.

LAYER_BLOB_TEXT_KEY = "tmi-layer"


def _parse_layer_header(header: str) -> tuple[tuple[int, int], tuple[int, int, int, int] | None]:
    parts = header.split()
    if not parts or parts[0] != "v1" or len(parts) not in (3, 7):
        raise ValueError(f"unsupported {LAYER_BLOB_TEXT_KEY} header: {header!r}")
    nums = [int(p) for p in parts[1:]]
    size = (nums[0], nums[1])
    rect = tuple(nums[2:6]) if len(nums) == 6 else None
    return size, rect  # type: ignore[return-value]


@dataclass(frozen=True)
class LayerCrop:
    """A layer image reduced to its tight painted rectangle.

    ``rect`` is the bbox of pixels with any non-zero channel (so RGB under
    alpha 0 survives the round trip), or None for an all-zero layer.
    ``image`` is the crop (an ``Rgba``, or the PIL image a caller gave),
    ``rgba`` its bytes.
    """

    size: tuple[int, int]
    rect: tuple[int, int, int, int] | None
    image: Any | None
    rgba: bytes = b""

    @classmethod
    def from_full(cls, image: Any) -> "LayerCrop":
        rgba = raster.dense(image)
        rect = rgba.bbox()
        if rect is None:
            return cls(rgba.size, None, None)
        crop = rgba.crop(rect)
        return cls(rgba.size, tuple(rect), crop, crop.tobytes())

    @classmethod
    def from_tight_crop(
        cls,
        size: tuple[int, int],
        x: int,
        y: int,
        image: Any | None,
        rgba_bytes: bytes | None = None,
    ) -> "LayerCrop":
        """Wrap a crop the caller already trimmed to its non-zero bbox.
        ``rgba_bytes`` may pass the RGBA buffer an RGBA ``image`` was built
        from, sparing a copy."""
        if image is None:
            return cls(size, None, None)
        rgba = image if image.mode == "RGBA" else image.convert("RGBA")
        if rgba_bytes is None or rgba is not image:
            rgba_bytes = rgba.tobytes()
        return cls(size, (x, y, x + rgba.width, y + rgba.height), rgba, rgba_bytes)

    def digest(self) -> str:
        """sha256 over the layer size, the crop rectangle and the crop's RGBA."""
        h = hashlib.sha256()
        h.update(f"{LAYER_BLOB_TEXT_KEY} {self.header()}:".encode("ascii"))
        h.update(self.rgba)
        return h.hexdigest()

    def header(self) -> str:
        w, h = self.size
        if self.rect is None:
            return f"v1 {w} {h}"
        return "v1 {} {} {} {} {} {}".format(w, h, *self.rect)

    def alpha_bbox(self) -> list[int] | None:
        if self.image is None:
            return None
        if self.rgba:
            from . import accel

            box = accel.native().rgba_alpha_bbox(self.rgba, self.rect[2] - self.rect[0])
        else:
            box = self.image.getbbox()
        if box is None:
            return None
        x, y = self.rect[0], self.rect[1]
        return [box[0] + x, box[1] + y, box[2] + x, box[3] + y]

    def encode(self) -> bytes:
        """The layer blob (TMIL) of this crop."""
        if self.rect is None:
            return _layer_blob.encode(b"", 0, 0, self.size)
        x0, y0, x1, y1 = self.rect
        rgba = self.rgba if self.rgba else self.image.tobytes()
        return _layer_blob.encode(rgba, x1 - x0, y1 - y0, self.size, x0, y0)


def layer_blob_to_png(data: bytes) -> bytes:
    """A plain full-size PNG for a layer blob (other PNG/JPEG bytes pass through)."""
    if _layer_blob.is_layer_blob(data):
        return raster.png(pixels_from_bytes(data))
    try:
        legacy = raster.png_text_before_idat(data, LAYER_BLOB_TEXT_KEY)
    except Exception:  # raster._Unsure, or a header tmi.raster misreads: Pillow reads it
        image = _pil().open(io.BytesIO(data))
        if LAYER_BLOB_TEXT_KEY not in image.info:
            return data
        return image_to_png_bytes(_pil_image_from_bytes(data))
    if legacy is None:
        return data
    return raster.png(pixels_from_bytes(data))


def layer_blob_digest(data: bytes) -> str:
    """The ``LayerCrop.digest`` of a stored layer blob (or of any plain image)."""
    if _layer_blob.is_layer_blob(data):
        header, rgba = _layer_blob.decode(data)
        return LayerCrop(header.size, header.rect, None, rgba).digest()
    try:
        legacy = raster.png_text_before_idat(data, LAYER_BLOB_TEXT_KEY)
    except Exception:  # raster._Unsure, or a header tmi.raster misreads: Pillow reads it
        return _pil_layer_blob_digest(data)
    if legacy is None:
        return LayerCrop.from_full(raster.decode_rgba(data)).digest()
    size, rect = _parse_layer_header(legacy)
    if rect is None:
        return LayerCrop(size, None, None).digest()
    return LayerCrop.from_tight_crop(size, rect[0], rect[1], raster.decode_rgba(data)).digest()


def _pil_layer_blob_digest(data: bytes) -> str:
    image = _pil().open(io.BytesIO(data))
    header = image.info.get(LAYER_BLOB_TEXT_KEY)
    if header is None:
        return LayerCrop.from_full(image).digest()
    size, rect = _parse_layer_header(header)
    if rect is None:
        return LayerCrop(size, None, None).digest()
    return LayerCrop.from_tight_crop(size, rect[0], rect[1], image).digest()


# ─────────────────────────── raster write-back ────────────────────────────────
#
# A PNG/JPEG *is* its single layer, so writing a layer back (merge,
# ``restore --ours/--theirs`` of a layer, in-place ``restore`` of a layer)
# re-encodes the whole file. The pixels arrive as RGBA; the file keeps what the
# file it replaces had (issue #21): resolution, colour profile, EXIF, the
# colour mode where the pixels still fit it, and for JPEG the quantisation
# tables and chroma subsampling.

# PNG ancillary chunks copied verbatim from the original (Pillow writes them
# from ``pnginfo``). Colour-space chunks, and text (comments, XMP as iTXt).
# iCCP / pHYs / eXIf go through Pillow's own ``icc_profile`` / ``dpi`` /
# ``exif``; tRNS / PLTE / sBIT / bKGD describe the old pixel format and are
# rebuilt or dropped; tIME would be stale.
_PNG_COPY_CHUNKS = {b"gAMA", b"cHRM", b"sRGB", b"cICP", b"tEXt", b"zTXt", b"iTXt"}


def encode_raster_like(image: Any, original: bytes | None, source_format: str) -> bytes:
    """Encode ``image`` as the PNG or JPEG (``source_format``) that replaces
    ``original``, keeping ``original``'s metadata and colour mode.

    Mode: the original's mode when the pixels fit it exactly (RGB when the
    original had no alpha and every pixel is opaque; L / LA when every pixel
    is grey; P with the original palette when every pixel is a palette
    entry), else RGBA (PNG) / RGB (JPEG). The ICC profile is dropped only
    when the written mode left the original's colour space and the profile
    does not describe the new one: a CMYK JPEG is written as RGB, and its
    CMYK profile would then misdescribe the file.

    tmi_accel picks the mode and encodes (``writeback_png`` /
    ``writeback_jpeg``, Pillow's chunk and marker layout); the original's
    metadata is read by ``tmi.raster`` (Pillow only for headers it does not
    read with certainty). The JPEG bytes are what Pillow wrote; the PNG
    bytes are not (mtpng's deflate), which nothing needs: the file goes to
    the working tree, not the store.
    """
    from . import accel

    rgba = raster.dense(image)
    is_jpeg = source_format.lower() in {"jpeg", "jpg"}
    src = _source_info(original, jpeg=is_jpeg) if original else None
    native = accel.native()
    if src is None:
        if is_jpeg:
            return native.writeback_jpeg(rgba.data, rgba.width, rgba.height, quality=95)[0]
        return native.writeback_png(rgba.data, rgba.width, rgba.height)[0]
    info = src["info"]
    icc = info.get("icc_profile") or None
    if is_jpeg:
        params: dict = {}
        if src["format"] == "JPEG" and src.get("quantization"):
            # What quality="keep" does, which Pillow allows only when saving the
            # opened JPEG itself: its quantisation tables and subsampling.
            q = src["quantization"]
            params["qtables"] = [list(q[k]) for k in range(len(q)) if k in q]
            if not 0 < len(params["qtables"]) < 5:
                raise ValueError("None or too many quantization tables")
            if src.get("sampling", -1) != -1:
                params["subsampling"] = src["sampling"]
        else:
            params["quality"] = 95
        dpi = info.get("dpi")
        comment = info.get("comment")
        return native.writeback_jpeg(
            rgba.data, rgba.width, rgba.height, src_mode=src["mode"],
            progressive=bool(info.get("progressive") or info.get("progression")),
            # An Adobe RGB-stored JPEG, not YCbCr: kept so when written as RGB.
            adobe_rgb=src["mode"] == "RGB" and info.get("adobe_transform") == 0,
            dpi=tuple(round(x) for x in dpi) if dpi else (0, 0),
            exif=_bytes_or_none(info.get("exif")), xmp=_bytes_or_none(info.get("xmp")), icc=icc,
            comment=comment.encode() if isinstance(comment, str) else _bytes_or_none(comment),
            **params,
        )[0]
    before, after = _png_chunks_for(info, src)
    return native.writeback_png(
        rgba.data, rgba.width, rgba.height,
        src_mode=src["mode"], src_palette=src.get("palette"), src_transparency=info.get("transparency"),
        icc=icc, before=before, after=after,
    )[0]


def _bytes_or_none(value: Any) -> bytes | None:
    return bytes(value) if value else None


def _png_chunks_for(info: dict, src: dict) -> tuple[list, list]:
    """The chunks Pillow's PNG ``_save`` wrote around PLTE / tRNS: before it
    the original's colour-space chunks (each once) and text chunks, copied
    (tmi_accel puts iCCP first and then drops sRGB); after it pHYs and
    eXIf."""
    import struct

    before: list[tuple[bytes, bytes]] = []
    once = [b"cHRM", b"cICP", b"gAMA", b"sBIT", b"sRGB", b"tIME"]
    if src["format"] == "PNG":
        for kind, data in src.get("chunks", []):
            if kind not in _PNG_COPY_CHUNKS:
                continue
            if kind in once:
                once.remove(kind)
                before.append((kind, data))
            elif kind in (b"tEXt", b"zTXt", b"iTXt"):
                before.append((kind, data))
    after: list[tuple[bytes, bytes]] = []
    dpi = info.get("dpi")
    if dpi:
        after.append((b"pHYs", struct.pack(">IIB", int(dpi[0] / 0.0254 + 0.5), int(dpi[1] / 0.0254 + 0.5), 1)))
    exif = info.get("exif")
    if exif:
        if exif.startswith(b"Exif\x00\x00"):
            exif = exif[6:]
        after.append((b"eXIf", exif))
    return before, after


def _source_info(original: bytes, *, jpeg: bool) -> dict | None:
    """The original file's format, mode, palette and the ``info`` entries
    the write-back keeps, as ``Image.open(original)`` (no ``load()``) reads
    them; None for a file that is not an image. A PNG that becomes a JPEG
    (a .jpg holding PNG data) is read by Pillow: its text chunks may hold a
    comment or XMP for the JPEG."""
    try:
        if original.startswith(raster.PNG_SIGNATURE) and not jpeg:
            h = raster.png_header(original, after_idat=False)
            mode = h.get("mode")
            if mode is None:
                raise raster._Unsure("mode")
            info = {k: h[k] for k in ("dpi", "icc_profile", "exif", "transparency") if k in h}
            return {
                "format": "PNG", "mode": mode, "palette": h.get("palette"), "info": info,
                "chunks": _png_ancillary_chunks(original),
            }
        if original[:3] == b"\xff\xd8\xff":
            h = raster.jpeg_header(original)
            info = {k: h[k] for k in ("dpi", "icc_profile", "exif", "comment", "xmp", "progressive", "adobe_transform") if k in h}
            from . import accel

            frame, tables = accel.native().jpeg_tables(original)
            src = {"format": "JPEG", "mode": h["mode"], "info": info, "quantization": dict(tables)}
            layers = h.get("layers", [])
            if h["layer_count"] in (1, 4) or len(layers) < 3:
                src["sampling"] = -1
            else:
                key = tuple(v for layer in layers[:3] for v in layer[1:3])
                src["sampling"] = {(1, 1, 1, 1, 1, 1): 0, (2, 1, 1, 1, 1, 1): 1, (2, 2, 1, 1, 1, 1): 2}.get(key, -1)
            return src
    except Exception:  # raster._Unsure, or a header tmi.raster misreads: Pillow reads it
        pass
    return _pil_source_info(original)


def _pil_source_info(original: bytes) -> dict | None:
    """``_source_info`` read by Pillow (headers ``tmi.raster`` leaves to it).

    Only a PNG or a JPEG has metadata to keep. What tmi would not read as an
    image (it is an opaque blob to tmiki: PLUGINS.md P0e) has none: another
    format, a malformed MPO, a palette PNG whose pixels do not load. The
    tmi_accel write-back (Rust ``source_info``) decides the same."""
    import warnings

    if not (original.startswith(raster.PNG_SIGNATURE) or original[:3] == b"\xff\xd8\xff"):
        return None
    try:
        with warnings.catch_warnings(record=True) as caught:
            warnings.simplefilter("always")
            src = _pil().open(io.BytesIO(original))
    except Exception:
        return None
    if any("malformed MPO" in str(w.message) for w in caught):
        return None
    out: dict = {"format": src.format, "mode": src.mode, "info": dict(src.info)}
    if src.format == "PNG":
        out["chunks"] = _png_ancillary_chunks(original)
    if src.mode == "P":
        try:
            palette = src.getpalette() or []
        except Exception:
            return None
        out["palette"] = bytes(palette)
    if src.format == "JPEG" and getattr(src, "quantization", None):
        from PIL import JpegImagePlugin

        out["quantization"] = dict(src.quantization)
        out["sampling"] = JpegImagePlugin.get_sampling(src)
    return out


def _png_ancillary_chunks(png: bytes) -> list[tuple[bytes, bytes]]:
    import struct

    out: list[tuple[bytes, bytes]] = []
    if not png.startswith(b"\x89PNG\r\n\x1a\n"):
        return out
    pos = 8
    while pos + 8 <= len(png):
        length, kind = struct.unpack(">I4s", png[pos:pos + 8])
        if kind == b"IEND":
            break
        if kind in _PNG_COPY_CHUNKS:
            out.append((kind, png[pos + 8:pos + 8 + length]))
        pos += 12 + length
    return out


def open_image(path: Path) -> "Image.Image":
    return _pil().open(path).convert("RGBA")


def make_thumbnail(image: Any, size: int = 384) -> Rgba:
    """``image`` shrunk with ``thumbnail((size, size), LANCZOS)`` and centred
    on a transparent ``size`` x ``size`` canvas (tmi_accel; the pixels
    Pillow gave)."""
    return raster.thumbnail(image, size)


def image_bbox(image: Any) -> list[int] | None:
    bbox = raster.dense(image).alpha_bbox()
    return list(bbox) if bbox else None


def diff_bbox(left: Any, right: Any) -> list[int] | None:
    """The box where ``right`` visibly differs from ``left``: the alpha
    differs, or a colour differs by enough to show at the larger alpha
    (``ImageChops`` difference / lighter / multiply and ``getbbox``, in one
    pass in tmi_accel). Sides of different sizes are compared on the larger
    canvas."""
    return raster.diff_bbox(left, right)


def side_by_side(left: Any, right: Any) -> Rgba:
    left, right = raster.same_size(left, right)
    gutter = 16
    # Source-over of an opaque image is the image itself, byte for byte, so
    # one alpha_composite covers what used to be paste-when-opaque.
    return raster.compose(
        left.width * 2 + gutter, left.height,
        [("over", left, 0, 0), ("over", right, left.width + gutter, 0)],
        fill=(32, 32, 32, 255),
    )


def overlay(left: Any, right: Any) -> Rgba:
    left, right = raster.same_size(left, right)
    return raster.blend(left, right, 0.5)


def diff_preview(
    left: Any,
    right: Any,
    *,
    bbox: Iterable[int] | None = None,
) -> Rgba:
    """``right`` with the changed area outlined. ``bbox`` may pass a known
    ``diff_bbox(left, right)`` to skip computing it again."""
    left, right = raster.same_size(left, right)
    if bbox is None:
        bbox = raster.diff_bbox(left, right)
    else:
        bbox = tuple(bbox)
    if not bbox:
        return right
    return raster.outline(right, bbox, (255, 0, 0, 255), max(2, right.width // 300))


def composite_images(images: Iterable[tuple[Any, tuple[int, int]]], size: tuple[int, int]) -> Rgba:
    ops = [("over", raster.dense(image), offset[0], offset[1]) for image, offset in images]
    return raster.compose(size[0], size[1], ops, fill=(255, 255, 255, 255))
