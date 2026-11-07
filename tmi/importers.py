from __future__ import annotations

import hashlib
import json
import os
import re
import sys
import threading
from concurrent.futures import Future, ThreadPoolExecutor
from dataclasses import dataclass, field
from pathlib import Path
from typing import TYPE_CHECKING, Any, Callable, Collection, Mapping

from . import raster
from .image_ops import LayerCrop, decoded_twin, image_to_png_bytes, make_thumbnail
from .pixel_limit import CanvasTooLargeError, check_pixels

# A PNG / JPEG is imported without Pillow (tmi.raster, RUST_MIGRATION.md
# 2b); the CLIP / MDP2 readers bring it in themselves.
if TYPE_CHECKING:
    from PIL import Image

ProgressFn = Callable[[str], None]


def _layer_ticker(on_progress: ProgressFn | None, total: int) -> Callable[[], None]:
    """A thread-safe "decoding layers k/total" tick for the leaf pools."""
    if on_progress is None:
        return lambda: None
    lock = threading.Lock()
    done = [0]
    on_progress(f"decoding layers 0/{total}")

    def tick() -> None:
        with lock:
            done[0] += 1
            on_progress(f"decoding layers {done[0]}/{total}")

    return tick


def _pool_map_while(
    fn: Callable[[Any], tuple[Any, Any]],
    items: list[Any],
    workers: int,
    meanwhile: Callable[[], None] | None,
) -> dict[Any, Any]:
    """``dict(pool.map(fn, items))``, running ``meanwhile`` on the calling
    thread while the pool works (it may use thread-bound state such as the
    CLIP SQLite connection). Results do not depend on the overlap."""
    if not items:
        if meanwhile is not None:
            meanwhile()
        return {}
    with ThreadPoolExecutor(max_workers=workers) as pool:
        results = pool.map(fn, items)  # submits every item now
        if meanwhile is not None:
            meanwhile()
        return dict(results)


# Formats with a bespoke layer-aware reader. Everything else is still
# versionable: it falls back to a raster image (if it decodes) or an opaque
# binary blob, so tmi can track files of any extension like Git does.
LAYER_AWARE_EXTENSIONS = {".mdp", ".clip"}
RASTER_EXTENSIONS = {".png", ".jpg", ".jpeg"}
SUPPORTED_EXTENSIONS = RASTER_EXTENSIONS | LAYER_AWARE_EXTENSIONS


@dataclass
class ImportedLayer:
    layer_id: str
    stable_source_id: str
    name: str
    kind: str
    visible: bool = True
    opacity: float = 1.0
    blend_mode: str = "normal"
    clip_to_below: bool = False
    offset_x: int = 0
    offset_y: int = 0
    mask_ref: str | None = None
    blob_bytes: bytes | None = None
    pixel_bbox: list[int] | None = None
    pixel_sha256: str | None = None
    source_meta: dict[str, Any] = field(default_factory=dict)
    children: list["ImportedLayer"] = field(default_factory=list)


@dataclass
class ImportedFile:
    source_format: str
    source_bytes: bytes
    source_digest: str
    canvas_meta: dict[str, Any]
    layers: list[ImportedLayer]
    composite_png: bytes
    thumbnail_png: bytes
    source_meta: dict[str, Any] = field(default_factory=dict)
    # ``image_from_bytes(composite_png)``, when the caller asked for it
    # (``import_file(composite_image=True)``) and the composite was rendered
    # in this import rather than served from the render cache: a caller that
    # needs the pixels (diff) then does not decode the PNG they were just
    # encoded to.
    composite_image: Any = field(default=None, repr=False, compare=False)


def is_supported_path(path: Path) -> bool:
    # tmi versions any regular file. Known illustration formats get layer-aware
    # handling; anything else is tracked as a raster image or opaque blob.
    return True


def import_file(
    path: Path,
    *,
    render_composite: bool = True,
    cache: Any | None = None,
    reuse_pixels: Mapping[str, str] | None = None,
    reuse_decoded: Mapping[str, Mapping[str, Any]] | None = None,
    on_progress: ProgressFn | None = None,
    composite_base: Any | None = None,
    source: bytes | None = None,
    source_digest: str | None = None,
    composite_image: bool = False,
    layer_blobs: bool | Collection[str] = True,
) -> ImportedFile:
    """Import a file into tmi's layer model.

    ``render_composite=False`` skips the composite + thumbnail preview render,
    which callers that only need the layer set (e.g. ``status``) do not use.
    ``cache`` is an optional :class:`tmi.render_cache.RenderCache` consulted for
    the composite/thumbnail, keyed by the source digest. ``composite_image``
    keeps the rendered composite as an image (``ImportedFile.composite_image``)
    besides its PNG.

    ``layer_blobs=False`` leaves decoded layers unencoded (``blob_bytes``
    None, ``pixel_sha256`` set), for callers that only compare pixel digests
    and store nothing; a collection of stable ids still encodes those layers.

    ``reuse_pixels`` maps a layer's ``stable_source_id`` to a pixel digest whose
    encoded blob is already known to exist in the object store. When a decoded
    layer's pixel digest matches, its blob encoding is skipped (``blob_bytes`` is
    left ``None`` while ``pixel_sha256`` is still set), so callers can reuse the
    existing blob ref without re-encoding or re-storing unchanged pixels.

    ``reuse_decoded`` goes one step further for CLIP/MDP: it maps a stable id to
    a prior decode ``{"pixel_source", "pixel_sha256", "pixel_bbox"}``. When the
    layer's ``pixel_source`` key (a hash of everything its pixels are decoded
    from, see ``PIXEL_SOURCE_VERSION``) is unchanged, the layer is not decoded
    at all and the prior digest/bbox are returned, again with ``blob_bytes``
    None so the stored blob is reused.

    ``on_progress`` receives phase messages ("decoding layers 3/40",
    "rendering composite", …) for the CLI's progress line. Per-layer ticks
    come from the decode pool's worker threads, one call at a time.

    ``composite_base`` (a ``tmi.formats.clip_partial.CompositeBase``: an
    earlier version of this CLIP file) lets a layered composite re-render
    only the area that changed since it; the result is the same image.

    ``source`` passes the file's bytes when the caller has already read them
    (to compare digests, say), with ``source_digest`` their sha256: the file
    is then neither read nor hashed again. A digest without the bytes is
    ignored.
    """
    # .tmiattributes (PLUGINS.md 4.1): a plugin format is refused here (the
    # tmi executable runs plugins), never recorded another way.
    from . import attributes as _attributes

    if _attributes.check_import(path) == "binary":
        return import_opaque_binary(path, source=source, source_digest=source_digest)
    imported = _import_file(
        path, render_composite=render_composite, cache=cache, reuse_pixels=reuse_pixels,
        reuse_decoded=reuse_decoded, on_progress=on_progress, composite_base=composite_base,
        source=source, source_digest=source_digest, layer_blobs=layer_blobs,
    )
    if not composite_image:
        imported.composite_image = None
    return imported


def _import_file(
    path: Path,
    *,
    render_composite: bool,
    cache: Any | None,
    reuse_pixels: Mapping[str, str] | None,
    reuse_decoded: Mapping[str, Mapping[str, Any]] | None,
    on_progress: ProgressFn | None,
    composite_base: Any | None,
    source: bytes | None,
    source_digest: str | None,
    layer_blobs: bool | Collection[str],
) -> ImportedFile:
    source, source_digest = _read_source(path, source, source_digest)
    suffix = path.suffix.lower()
    if suffix in LAYER_AWARE_EXTENSIONS:
        fmt = "mdp2" if suffix == ".mdp" else "clip"
        try:
            if suffix == ".mdp":
                return import_mdp2(
                    path, render_composite=render_composite, cache=cache,
                    reuse_pixels=reuse_pixels, reuse_decoded=reuse_decoded,
                    on_progress=on_progress, source=source, source_digest=source_digest,
                    layer_blobs=layer_blobs,
                )
            return import_clip_metadata(
                path, render_composite=render_composite, cache=cache,
                reuse_pixels=reuse_pixels, reuse_decoded=reuse_decoded,
                on_progress=on_progress, composite_base=composite_base,
                source=source, source_digest=source_digest, layer_blobs=layer_blobs,
            )
        except CanvasTooLargeError:
            # Not corrupt, just over core.maxImagePixels: say so instead of
            # silently tracking a drawing as an opaque blob.
            raise
        except Exception as exc:
            # A truncated / corrupt drawing file must not block `tmiki add`: fall
            # back to an opaque binary blob so it stays versionable, diffable and
            # byte-for-byte restorable, just without layer awareness.
            print(
                f"tmiki: warning: could not parse {path.name} as {fmt}; "
                f"tracking as opaque binary ({exc})",
                file=sys.stderr,
            )
            return import_opaque_binary(path, source=source, source_digest=source_digest)
    # Any other file, whatever its extension: an image when tmi's own decoders
    # read it (tmi_accel, the Rust rule: PLUGINS.md P0e), else an opaque blob.
    # Pillow takes no part in the verdict, so what is recorded does not depend
    # on the Pillow installed.
    decoded = raster.import_decode(source)
    if decoded is None:
        note = unreadable_image_note(path.name, source)
        if note is not None:
            print(f"tmiki: warning: could not read {path.name} as {note}; tracking as opaque binary", file=sys.stderr)
        return import_opaque_binary(path, source=source, source_digest=source_digest)
    return import_raster_image(
        path, render_composite=render_composite, reuse_pixels=reuse_pixels,
        source=source, source_digest=source_digest, layer_blobs=layer_blobs, decoded=decoded,
    )


_IMAGE_SUFFIXES = {
    ".png": "PNG", ".jpg": "JPEG", ".jpeg": "JPEG", ".jpe": "JPEG", ".jfif": "JPEG", ".gif": "GIF",
    ".bmp": "BMP", ".dib": "BMP", ".tif": "TIFF", ".tiff": "TIFF", ".webp": "WebP", ".psd": "PSD",
}


def _image_signature(suffix: str, data: bytes) -> str | None:
    if data.startswith(b"\x89PNG\r\n\x1a\n"):
        return "PNG"
    if data[:3] == b"\xff\xd8\xff":
        return "JPEG"
    if data[:6] in (b"GIF87a", b"GIF89a"):
        return "GIF"
    if data[:4] in (b"II*\x00", b"MM\x00*", b"II+\x00", b"MM\x00+"):
        return "TIFF"
    if data[:4] == b"RIFF" and data[8:12] == b"WEBP":
        return "WebP"
    if data[:4] == b"8BPS":
        return "PSD"
    if data[:2] == b"BM" and suffix in (".bmp", ".dib"):
        return "BMP"
    return None


def unreadable_image_note(name: str, data: bytes) -> str | None:
    """For a file tmi did not read as an image: what to tell the user if it
    looks like one (an image extension, or an image signature), else None.
    Such a file is kept as an opaque blob; saying so lets the user see that
    the visual diff is gone (issue #37). ``tmi_format::unreadable_image_note``
    in Rust is the same function."""
    suffix = Path(name).suffix.lower()
    fmt = _image_signature(suffix, data)
    if fmt is None:
        fmt = _IMAGE_SUFFIXES.get(suffix)
        if fmt is None:
            return None
        why = "the file is empty" if not data else f"it does not contain {fmt} data"
    else:
        why = "the data is damaged, incomplete or of a kind tmi cannot read; was the file still being saved or downloaded?"
    return f"a {fmt} image ({why})"


def _read_source(path: Path, source: bytes | None, source_digest: str | None) -> tuple[bytes, str]:
    """``(bytes, sha256)`` of the file: the caller's when it passed the bytes."""
    if source is None:
        source = path.read_bytes()
        source_digest = None
    return source, source_digest or _sha256(source)


def _render_composite_cached(
    cache: Any | None,
    digest: str,
    render_composite: bool,
    render_fn: Any,
    on_progress: ProgressFn | None = None,
) -> tuple[bytes, bytes, Any]:
    """Return ``(composite_png, thumbnail_png, composite_image)`` for a
    layered document.

    Skips rendering entirely when ``render_composite`` is False, serves a cached
    pair when ``cache`` has one for ``digest``, and otherwise renders via
    ``render_fn`` and populates the cache. ``render_fn(on_image)`` returns
    the composite ``Image``, or ``(Image, png_bytes)`` when it holds a PNG of
    exactly those pixels (CSP's CanvasPreview, or its own encode), which is
    then stored as-is instead of re-encoded; when it encodes the image
    itself it may call ``on_image(image)`` first, so the thumbnail is made
    meanwhile. The cache is best-effort: failures fall back to render.

    The thumbnail is resampled and encoded on a thread of its own while the
    composite is encoded; the bytes are those of doing one after the other.

    ``composite_image`` is what decoding ``composite_png`` gives (see
    ``ImportedFile.composite_image``), or None when nothing was rendered.
    """
    if not render_composite:
        return b"", b"", None
    if cache is not None:
        hit = cache.get(digest)
        if hit is not None:
            return (*hit, None)
    if on_progress:
        on_progress("rendering composite")
    thumbnail_job: list[Future] = []

    def start_thumbnail(image: Any) -> None:
        if not thumbnail_job:
            thumbnail_job.append(_in_background(lambda: _thumbnail_png(image)))

    rendered = render_fn(start_thumbnail)
    composite, composite_png = rendered if isinstance(rendered, tuple) else (rendered, None)
    if composite_png is None:
        start_thumbnail(composite)
        if on_progress:
            on_progress("encoding composite PNG")
        composite_png = image_to_png_bytes(composite)
    if on_progress:
        on_progress("rendering thumbnail")
    thumbnail_png = thumbnail_job[0].result() if thumbnail_job else _thumbnail_png(composite)
    if cache is not None:
        cache.put(digest, composite_png, thumbnail_png)
    return composite_png, thumbnail_png, decoded_twin(composite, composite_png)


def _thumbnail_png(image: Any) -> bytes:
    return image_to_png_bytes(make_thumbnail(image))


def _in_background(fn: Callable[[], Any]) -> Future:
    """Run ``fn`` on a thread of its own; ``.result()`` joins it and raises
    what it raised."""
    pool = ThreadPoolExecutor(max_workers=1)
    try:
        return pool.submit(fn)
    finally:
        pool.shutdown(wait=False)


def import_raster_image(
    path: Path,
    *,
    render_composite: bool = True,
    reuse_pixels: Mapping[str, str] | None = None,
    source: bytes | None = None,
    source_digest: str | None = None,
    layer_blobs: bool | Collection[str] = True,
    decoded: "raster.Decoded | None" = None,
) -> ImportedFile:
    """Track a file tmi reads as an image: one raster layer. ``decoded`` is
    ``raster.import_decode(source)`` when the caller has it already; a file
    tmi does not read raises ``ValueError`` (``import_file`` stores it as an
    opaque blob instead)."""
    source, source_digest = _read_source(path, source, source_digest)
    if decoded is None:
        decoded = raster.import_decode(source)
        if decoded is None:
            raise ValueError(f"{path.name} is not an image tmi reads")
    detected = decoded.format
    image = decoded.image
    composite_job = thumbnail_job = None
    if render_composite:
        # The composite PNG and the thumbnail are encoded beside the layer
        # blob (tmi_accel releases the GIL).
        composite_job = _in_background(lambda: raster.png(image))
        thumbnail_job = _in_background(lambda: _thumbnail_png(image))
    crop = LayerCrop.from_full(image)
    pixel_sha = crop.digest()
    skip_encode = (
        not _wants_blob(layer_blobs, "image:0")
        or reuse_pixels is not None and reuse_pixels.get("image:0") == pixel_sha
    )
    layer_png = None if skip_encode else crop.encode()
    suffix = path.suffix.lower()
    if suffix in {".jpg", ".jpeg"} or detected in {"jpeg", "jpg"}:
        source_format = "jpeg"
    elif suffix == ".png" or detected == "png":
        source_format = "png"
    else:
        source_format = detected or "image"
    layer = ImportedLayer(
        layer_id="image:0",
        stable_source_id="image:0",
        name=path.stem,
        kind="raster",
        blob_bytes=layer_png,
        pixel_bbox=crop.alpha_bbox(),
        pixel_sha256=pixel_sha,
        source_meta={"source_path_name": path.name},
    )
    composite_png = composite_job.result() if composite_job is not None else b""
    thumbnail_png = thumbnail_job.result() if thumbnail_job is not None else b""
    return ImportedFile(
        source_format=source_format,
        source_bytes=source,
        source_digest=source_digest,
        canvas_meta={
            "width": image.width,
            "height": image.height,
            "dpi": decoded.dpi,
            "color_profile": "embedded" if decoded.icc else None,
            "source_format": source_format,
        },
        layers=[layer],
        composite_png=composite_png,
        thumbnail_png=thumbnail_png,
        composite_image=decoded_twin(image, composite_png) if render_composite else None,
    )


def import_opaque_binary(
    path: Path,
    *,
    source: bytes | None = None,
    source_digest: str | None = None,
) -> ImportedFile:
    """Track an arbitrary file as a single opaque blob (no layers, no preview).

    Used for extensions tmi has no visual reader for. The file still versions,
    diffs (as "binary changed"), and restores byte-for-byte via its source
    blob, just without layer awareness or thumbnails.
    """
    source, source_digest = _read_source(path, source, source_digest)
    layer = ImportedLayer(
        layer_id="blob:0",
        stable_source_id="blob:0",
        name=path.name,
        kind="binary",
        blob_bytes=None,
        pixel_bbox=None,
        source_meta={"source_path_name": path.name, "byte_size": len(source)},
    )
    return ImportedFile(
        source_format="binary",
        source_bytes=source,
        source_digest=source_digest,
        canvas_meta={
            "width": None,
            "height": None,
            "dpi": None,
            "color_profile": None,
            "source_format": "binary",
        },
        layers=[layer],
        composite_png=b"",
        thumbnail_png=b"",
        source_meta={"byte_size": len(source), "write_capability": "opaque-blob"},
    )


# Bump whenever layer decoding changes what pixels a given source yields (tile
# formats, CLIP/MDP decoders, LayerCrop/digest definition, tmi_accel decode).
# ``pixel_source`` keys from an older version then never match, so every layer
# is decoded afresh instead of reusing a result the old decoder produced.
PIXEL_SOURCE_VERSION = 1


def _pixel_source_key(kind: str, params: str, *payloads: Any) -> str:
    """Hash of everything a layer's pixels are decoded from. A payload is a
    bytes-like object or a tuple of them standing for their concatenation."""
    h = hashlib.sha256(f"tmi-pixel-source v{PIXEL_SOURCE_VERSION} {kind} {params}\0".encode("utf-8"))
    for payload in payloads:
        pieces = payload if isinstance(payload, tuple) else (payload,)
        h.update(sum(len(piece) for piece in pieces).to_bytes(8, "big"))
        for piece in pieces:
            h.update(piece)
    return h.hexdigest()


def _clip_exta_pieces(clip: Any, outer_id: str) -> tuple[memoryview, ...]:
    """An Exta payload without its embedded external id (bytes 8..48, see
    ``_clip_normalized_exta_payload_sha256``), as views into ``clip.raw``:
    hashing them copies nothing (a copy would hold the GIL against the
    other decode workers)."""
    chunk = clip.extas[outer_id]
    view = memoryview(clip.raw)[chunk.payload_off:chunk.payload_off + chunk.payload_size]
    if len(view) >= 48 and view[8:16] == b"extrnlid":
        return view[:8], view[48:]
    return (view,)


def _reusable_decode(
    reuse_decoded: Mapping[str, Mapping[str, Any]] | None,
    sid: str,
    source_key: str,
) -> Mapping[str, Any] | None:
    prior = (reuse_decoded or {}).get(sid)
    if prior and prior.get("pixel_source") == source_key and prior.get("pixel_sha256"):
        return prior
    return None


# Above this many RGBA bytes a layer's digest runs beside its blob encode
# instead of before it: one huge layer otherwise ends the leaf pool alone.
_OVERLAP_DIGEST_BYTES = 32 * 1024 * 1024


def _wants_blob(layer_blobs: bool | Collection[str], sid: str) -> bool:
    """Whether ``import_file(layer_blobs=...)`` asks for layer ``sid``'s blob."""
    return layer_blobs if isinstance(layer_blobs, bool) else sid in layer_blobs


def _digest_and_encode(
    crop: LayerCrop,
    sid: str,
    reuse_pixels: Mapping[str, str] | None,
    layer_blobs: bool | Collection[str] = True,
) -> tuple[str, bytes | None]:
    """``(crop.digest(), crop.encode())``, the encode skipped (None) when the
    digest matches ``reuse_pixels[sid]`` or ``layer_blobs`` does not ask for
    the layer's blob."""
    if not _wants_blob(layer_blobs, sid):
        return crop.digest(), None
    prior = reuse_pixels.get(sid) if reuse_pixels is not None else None
    if prior is None and len(crop.rgba) >= _OVERLAP_DIGEST_BYTES:
        # The encode is needed whatever the digest is; hashlib releases the GIL.
        result: list[str] = []
        hasher = threading.Thread(target=lambda: result.append(crop.digest()))
        hasher.start()
        try:
            blob = crop.encode()
        finally:
            hasher.join()
        return result[0], blob
    digest = crop.digest()
    return digest, None if prior == digest else crop.encode()


def _render_mdp2_leaves_parallel(
    mdp: Any,
    reuse_pixels: Mapping[str, str] | None,
    reuse_decoded: Mapping[str, Mapping[str, Any]] | None = None,
    on_progress: ProgressFn | None = None,
    meanwhile: Callable[[], None] | None = None,
    layer_blobs: bool | Collection[str] = True,
) -> dict[int, tuple[bytes | None, list[int] | None, str | None, str | None]]:
    """Render+digest+encode every renderable MDP2 leaf, keyed by ``layer_id``.

    Mirrors the CLIP path (:func:`_render_clip_leaves_parallel`): the heavy
    per-layer work — tile zlib decode, image build, blob encode — fans out to a
    thread pool, where Pillow and zlib release the GIL. MDP2 has no SQLite
    handle, so there is no main-thread lookup constraint; ``mdp.raw`` is
    read-only shared state. The result is byte-for-byte identical to rendering
    the layers serially; only the execution is concurrent.

    Each worker computes the pixel digest first and only encodes the blob when the
    digest does not match ``reuse_pixels`` for that layer's stable id.

    ``meanwhile`` runs on the calling thread while the pool works.
    """
    from .formats.mdp2 import blob_payload, is_renderable_raster, render_layer_crop

    leaves = [
        layer
        for layer in mdp.layers
        if not layer.children and layer.type != "folder" and is_renderable_raster(mdp, layer)
    ]
    if not leaves:
        return _pool_map_while(None, [], 0, meanwhile)
    tick = _layer_ticker(on_progress, len(leaves))

    def render_one(layer: Any) -> tuple[int, tuple[bytes | None, list[int] | None, str | None, str | None]]:
        try:
            return _render_one(layer)
        finally:
            tick()

    def _render_one(layer: Any) -> tuple[int, tuple[bytes | None, list[int] | None, str | None, str | None]]:
        sid = f"mdp2:{layer.layer_id}"
        # render_layer_rgba reads the bin payload plus type/size, and 8bpp/1bpp
        # layers are tinted with the layer colour — all of it goes in the key.
        source_key = _pixel_source_key(
            "mdp2",
            f"{layer.type} {layer.width}x{layer.height} {layer.color}",
            blob_payload(mdp, mdp.blob_by_name[layer.bin_name]),
        )
        prior = _reusable_decode(reuse_decoded, sid, source_key)
        if prior is not None:
            return layer.layer_id, (None, prior.get("pixel_bbox"), prior["pixel_sha256"], source_key)
        # Pixel, text and vector layers all ship a tile raster cache, so they
        # all round-trip as raster pixels here. Store the layer's intrinsic
        # pixels (opacity kept separate, like CLIP) so a stage → restore
        # round-trip rewrites the tiles unchanged.
        crop = render_layer_crop(mdp, layer)
        digest, blob = _digest_and_encode(crop, sid, reuse_pixels, layer_blobs)
        return layer.layer_id, (blob, crop.alpha_bbox(), digest, source_key)

    # Same heuristic as the CLIP path: a little oversubscription (≈2× cores)
    # beats one-thread-per-core for the GIL-bound/GIL-free mix. Capped to bound
    # peak memory — every in-flight worker holds a full layer-sized RGBA image.
    workers = min(len(leaves), 16, max(4, (os.cpu_count() or 4) * 2))
    return _pool_map_while(render_one, leaves, workers, meanwhile)


def import_mdp2(
    path: Path,
    *,
    render_composite: bool = True,
    cache: Any | None = None,
    reuse_pixels: Mapping[str, str] | None = None,
    reuse_decoded: Mapping[str, Mapping[str, Any]] | None = None,
    on_progress: ProgressFn | None = None,
    source: bytes | None = None,
    source_digest: str | None = None,
    layer_blobs: bool | Collection[str] = True,
) -> ImportedFile:
    from .formats.mdp2 import parse_mdp2, render_composite_rgba

    source, source_digest = _read_source(path, source, source_digest)
    mdp = parse_mdp2(source)
    check_pixels(mdp.canvas_width, mdp.canvas_height, path.name)
    layers_by_id = {layer.layer_id: layer for layer in mdp.layers}
    previews: list[tuple[bytes, bytes, Any]] = []
    rendered = _render_mdp2_leaves_parallel(
        mdp, reuse_pixels, reuse_decoded, on_progress,
        # the composite renders on this thread while the pool decodes layers
        meanwhile=lambda: previews.append(_render_composite_cached(
            cache, source_digest, render_composite, lambda _on_image: render_composite_rgba(mdp), on_progress
        )),
        layer_blobs=layer_blobs,
    )

    def convert(layer: Any) -> ImportedLayer:
        children = [convert(child) for child in layer.children]
        blob_bytes, bbox, pixel_sha, pixel_source = rendered.get(layer.layer_id, (None, None, None, None))
        if children or layer.type == "folder":
            kind = "folder"
        else:
            kind = _mdp_layer_kind(layer)
        source_meta = _mdp_layer_source_meta(layer)
        if pixel_source is not None:
            source_meta["pixel_source"] = pixel_source
        return ImportedLayer(
            layer_id=f"mdp2:{layer.layer_id}",
            stable_source_id=f"mdp2:{layer.layer_id}",
            name=layer.name or f"layer-{layer.layer_id}",
            kind=kind,
            visible=layer.visible,
            opacity=round(layer.alpha / 255, 6),
            blend_mode=layer.mode or "normal",
            clip_to_below=layer.clipping,
            offset_x=layer.offset_x,
            offset_y=layer.offset_y,
            blob_bytes=blob_bytes,
            pixel_bbox=bbox,
            pixel_sha256=pixel_sha,
            source_meta=source_meta,
            children=children,
        )

    roots = [layer for layer in mdp.layers if layer.parent_id not in layers_by_id]
    layers = [convert(layer) for layer in roots]
    composite_png, thumbnail_png, composite_image = previews[0]
    return ImportedFile(
        source_format="mdp2",
        source_bytes=source,
        source_digest=source_digest,
        canvas_meta={
            "width": mdp.canvas_width,
            "height": mdp.canvas_height,
            "dpi": mdp.dpi,
            "color_profile": None,
            "source_format": "mdp2",
        },
        layers=layers,
        composite_png=composite_png,
        thumbnail_png=thumbnail_png,
        source_meta={"xml_size": mdp.xml_size, "pack_size": mdp.pack_size},
        composite_image=composite_image,
    )


def _walk_clip_nodes(node: Any):
    for child in node.children:
        yield child
        yield from _walk_clip_nodes(child)


def _render_clip_leaves_parallel(
    clip: Any,
    reuse_pixels: Mapping[str, str] | None = None,
    reuse_decoded: Mapping[str, Mapping[str, Any]] | None = None,
    on_progress: ProgressFn | None = None,
    meanwhile: Callable[[], None] | None = None,
    layer_blobs: bool | Collection[str] = True,
) -> dict[int, tuple[Any, bytes | None, list[int] | None, str | None, str | None]]:
    """Render every leaf layer's pixels + layer blob + bbox + digest, keyed by ``main_id``.

    The single per-layer SQLite lookup (``layer_offscreen_info``) runs here on
    the calling thread, since the embedded in-memory DB connection is
    single-thread only. The heavy work — tile decode, compositing, blob encode —
    then fans out to a thread pool; Pillow and zlib release the GIL during
    encode/decompress, so this is the bulk of an import's wall time. The result
    is byte-for-byte identical to rendering the layers sequentially; only the
    execution is concurrent.

    Each worker computes the pixel digest first and encodes the blob only when the
    digest does not match ``reuse_pixels`` for that layer's stable id (the same
    ``node.uuid or f"clip:{node.main_id}"`` expression ``convert()`` uses).

    Folders and pixel-less layers (text/vector/etc.) map to ``(None, None,
    None, None)``. ``meanwhile`` runs on the calling thread (which owns the
    DB connection) while the pool works.
    """
    from .formats.clip import layer_offscreen_info, offscreen_meta_from_info, render_offscreen_crop_from_info

    leaves = [node for node in _walk_clip_nodes(clip.root) if not node.folder]
    if not leaves:
        return _pool_map_while(None, [], 0, meanwhile)
    # DB access is confined to this main-thread pass.
    infos = {node.main_id: layer_offscreen_info(clip, node.render_mipmap) for node in leaves}

    def source_key_for(info: Any) -> str | None:
        # An offscreen's pixels are decoded from its Offscreen.Attribute (grid
        # and pixel size) and its Exta payload. The payload's embedded external
        # id is excluded, as in _clip_normalized_exta_payload_sha256: CSP may
        # rewrite it without touching the tiles, and the decoder never reads it.
        if not info or info[2] not in clip.extas:
            return None
        return _pixel_source_key("clip", "", info[1], _clip_exta_pieces(clip, info[2]))

    tick = _layer_ticker(on_progress, len(leaves))

    def render_one(node: Any) -> tuple[int, tuple[Any, bytes | None, list[int] | None, str | None, str | None]]:
        try:
            return _render_one(node)
        finally:
            tick()

    def _render_one(node: Any) -> tuple[int, tuple[Any, bytes | None, list[int] | None, str | None, str | None]]:
        info = infos[node.main_id]
        sid = node.uuid or f"clip:{node.main_id}"
        source_key = source_key_for(info)
        prior = _reusable_decode(reuse_decoded, sid, source_key) if source_key else None
        if prior is not None:
            meta = offscreen_meta_from_info(clip, info)
            if meta is not None:
                return node.main_id, (meta, None, prior.get("pixel_bbox"), prior["pixel_sha256"], source_key)
        rendered = render_offscreen_crop_from_info(clip, info)
        if not rendered:
            return node.main_id, (None, None, None, None, None)
        pixels, crop = rendered
        digest, blob = _digest_and_encode(crop, sid, reuse_pixels, layer_blobs)
        return node.main_id, (pixels, blob, crop.alpha_bbox(), digest, source_key)

    # The work is a mix of GIL-bound tile compositing and GIL-free zlib/zstd, so
    # a little oversubscription (≈2× cores) beats one-thread-per-core here. Cap
    # it to bound peak memory: every in-flight worker holds a full canvas-sized
    # RGBA image.
    workers = min(len(leaves), 16, max(4, (os.cpu_count() or 4) * 2))
    return _pool_map_while(render_one, leaves, workers, meanwhile)


def import_clip_metadata(
    path: Path,
    *,
    render_composite: bool = True,
    cache: Any | None = None,
    reuse_pixels: Mapping[str, str] | None = None,
    reuse_decoded: Mapping[str, Mapping[str, Any]] | None = None,
    on_progress: ProgressFn | None = None,
    composite_base: Any | None = None,
    source: bytes | None = None,
    source_digest: str | None = None,
    layer_blobs: bool | Collection[str] = True,
) -> ImportedFile:
    from .formats.clip import _composite_name, open_clip_bytes, render_composite_with_png

    source, source_digest = _read_source(path, source, source_digest)
    clip = open_clip_bytes(source)
    check_pixels(clip.canvas.width, clip.canvas.height, path.name)
    previews: list[tuple[bytes, bytes, Any]] = []
    rendered = _render_clip_leaves_parallel(
        clip, reuse_pixels, reuse_decoded, on_progress,
        # the composite renders on this thread (it reads clip.db) while the
        # pool decodes layers
        meanwhile=lambda: previews.append(_render_composite_cached(
            cache, source_digest, render_composite,
            lambda on_image: render_composite_with_png(clip, composite_base, on_image=on_image),
            on_progress,
        )),
        layer_blobs=layer_blobs,
    )
    if on_progress:
        on_progress("reading layer metadata")

    def convert(node: Any) -> ImportedLayer:
        children = [convert(child) for child in node.children]
        pixels, blob_png, bbox, pixel_sha, pixel_source = rendered.get(node.main_id, (None, None, None, None, None))
        extra_meta = _clip_extra_layer_meta(clip, node)
        kind = _clip_layer_kind(node, pixels, extra_meta)
        render_payload_sha256 = None if pixels else _clip_exta_payload_sha256(clip, node.render_outer_id)
        mask_payload_sha256 = _clip_exta_payload_sha256(clip, node.mask_outer_id)
        semantic_payload = {
            "name": node.name,
            "type": node.type,
            "folder": node.folder,
            "visible": node.visible,
            "opacity": node.opacity,
            "composite": node.composite,
            "clip": node.clip,
            "masking": node.masking,
            "offset_x": node.offset_x,
            "offset_y": node.offset_y,
            "uuid": node.uuid,
            "render_payload_sha256": render_payload_sha256,
            "mask_payload_sha256": mask_payload_sha256,
            **extra_meta,
        }
        return ImportedLayer(
            layer_id=f"clip:{node.main_id}",
            stable_source_id=node.uuid or f"clip:{node.main_id}",
            name=node.name or f"layer-{node.main_id}",
            kind=kind,
            visible=node.visible,
            opacity=round((node.opacity or 0) / 256, 6),
            blend_mode=_composite_name(node.composite or 0),
            clip_to_below=bool(node.clip),
            offset_x=node.offset_x,
            offset_y=node.offset_y,
            blob_bytes=blob_png,
            pixel_bbox=bbox,
            pixel_sha256=pixel_sha,
            source_meta={
                "main_id": node.main_id,
                "type": node.type,
                "folder": node.folder,
                "composite_id": node.composite,
                "render_outer_id": node.render_outer_id,
                "mask_outer_id": node.mask_outer_id,
                "render_payload_sha256": render_payload_sha256,
                "mask_payload_sha256": mask_payload_sha256,
                "masking": node.masking,
                "pixel_width": pixels.pixel_width if pixels else None,
                "pixel_height": pixels.pixel_height if pixels else None,
                "tile_cols": pixels.tile_cols if pixels else None,
                "tile_rows": pixels.tile_rows if pixels else None,
                "channel_profile": pixels.channel_profile if pixels else None,
                "offscreen_id": pixels.offscreen_id if pixels else None,
                "write_capability": "raster-pixels" if pixels and pixels.channel_profile == "color-alpha-bgra" else "metadata-only",
                "pixel_source": pixel_source,
                **extra_meta,
                "semantic_digest_version": 2,
                "semantic_digest": _semantic_digest(semantic_payload),
            },
            children=children,
        )

    layers = [convert(child) for child in clip.root.children]
    composite_png, thumbnail_png, composite_image = previews[0]
    return ImportedFile(
        source_format="clip",
        source_bytes=source,
        source_digest=source_digest,
        canvas_meta={
            "width": clip.canvas.width,
            "height": clip.canvas.height,
            "dpi": clip.canvas.resolution,
            "color_profile": None,
            "source_format": "clip",
        },
        layers=layers,
        composite_png=composite_png,
        thumbnail_png=thumbnail_png,
        source_meta={
            "chunks": len(clip.container.chunks),
            "exta_count": len(clip.extas),
            "head_uuid": clip.head.uuid.hex(),
            "write_capability": "raster-pixels",
        },
        composite_image=composite_image,
    )


def _mdp_layer_kind(layer: Any) -> str:
    if layer.type == "folder":
        return "folder"
    if layer.masking and layer.masking_type == 0:
        return "mask"
    if layer.masking and layer.masking_type == 1:
        return "stencil"
    if layer.type in {"text", "vector"}:
        return layer.type
    if layer.type in {"32bpp", "8bpp", "1bpp", "64bpp"}:
        return "raster"
    return "unknown"


def _mdp_layer_source_meta(layer: Any) -> dict[str, Any]:
    attrs = dict(layer.attributes)
    semantic_attrs = {k: v for k, v in attrs.items() if k != "bin"}
    meta = {
        "type": layer.type,
        "width": layer.width,
        "height": layer.height,
        "bin_name": layer.bin_name,
        "bin_type": layer.bin_type,
        "parent_id": layer.parent_id,
        "protect_alpha": layer.protect_alpha,
        "folder_open": layer.folder_open,
        "masking": layer.masking,
        "masking_type": layer.masking_type,
        "draft": layer.draft,
        "lumi": layer.lumi,
        "color": layer.color,
        "halftone_type": layer.halftone_type,
        "halftone_line": layer.halftone_line,
        "transform_offset_x": layer.transform_offset_x,
        "transform_offset_y": layer.transform_offset_y,
        "text_lines": layer.text_lines,
        "xml_attrs": semantic_attrs,
    }
    meta["semantic_digest"] = _semantic_digest({
        "attrs": semantic_attrs,
        "text_lines": layer.text_lines,
    })
    if layer.type in {"32bpp", "8bpp", "1bpp", "text", "vector"} and not layer.masking:
        meta["write_capability"] = "raster-pixels"
    else:
        meta["write_capability"] = "metadata-only"
    return meta


def _clip_layer_kind(node: Any, pixels: Any | None, extra_meta: dict[str, Any]) -> str:
    if node.folder:
        return "folder"
    if extra_meta.get("text"):
        return "text"
    if extra_meta.get("vector_outer_ids"):
        return "vector"
    if pixels:
        return "raster"
    if node.type == 1584:
        return "paper"
    if node.type == 4098:
        return "adjustment"  # correction layer: re-grades the backdrop, no pixels
    return "unknown"


def _clip_extra_layer_meta(clip: Any, node: Any) -> dict[str, Any]:
    columns = _table_columns(clip.db, "Layer")
    wanted = [
        "TextLayerString",
        "TextLayerAttributes",
        "TextLayerAddAttributesV01",
        "TextLayerAttributesVersion",
        "TextLayerNameAutoChangeEnabled",
        "VectorNormalType",
        "VectorNormalStrokeIndex",
        "VectorNormalFillIndex",
        "VectorNormalBalloonIndex",
        "VectorNormalStroke",
        "VectorNormalFill",
        "VectorNormalBalloon",
    ]
    selected = [name for name in wanted if name in columns]
    if not selected:
        return {}
    row = clip.db.execute(
        f"SELECT {', '.join(selected)} FROM Layer WHERE MainId = ?",
        (node.main_id,),
    ).fetchone()
    if not row:
        return {}
    values = dict(zip(selected, row))
    meta: dict[str, Any] = {}
    text_raw = values.get("TextLayerString")
    if text_raw is not None:
        meta["text"] = _decode_clip_text(text_raw)
    for key in ["TextLayerAttributes", "TextLayerAddAttributesV01"]:
        raw = values.get(key)
        if raw is not None:
            meta[_snake(key) + "_sha256"] = hashlib.sha256(bytes(raw)).hexdigest()
            meta[_snake(key) + "_bytes"] = len(raw)
    for key in ["TextLayerAttributesVersion", "TextLayerNameAutoChangeEnabled", "VectorNormalType"]:
        if values.get(key) is not None:
            meta[_snake(key)] = values[key]

    vector_indices: dict[str, int] = {}
    for key in ["VectorNormalStrokeIndex", "VectorNormalFillIndex", "VectorNormalBalloonIndex"]:
        value = values.get(key)
        if value:
            vector_indices[_snake(key)] = int(value)
    if vector_indices:
        meta["vector_indices"] = vector_indices
        meta["vector_outer_ids"] = _clip_vector_outer_ids(clip, vector_indices.values())
    for key in ["VectorNormalStroke", "VectorNormalFill", "VectorNormalBalloon"]:
        raw = values.get(key)
        if raw is not None:
            meta[_snake(key) + "_sha256"] = hashlib.sha256(bytes(raw)).hexdigest()
    return meta


def _clip_vector_outer_ids(clip: Any, indices: Any) -> list[str]:
    columns = _table_columns(clip.db, "VectorObjectList")
    if "MainId" not in columns or "VectorData" not in columns:
        return []
    out: list[str] = []
    for index in indices:
        row = clip.db.execute("SELECT VectorData FROM VectorObjectList WHERE MainId = ?", (index,)).fetchone()
        if not row or row[0] is None:
            continue
        raw = bytes(row[0])
        try:
            text = raw.decode("ascii")
        except UnicodeDecodeError:
            text = hashlib.sha256(raw).hexdigest()
        out.append(text)
    return out


def _clip_exta_payload_sha256(clip: Any, outer_id: str | None) -> str | None:
    if not outer_id or outer_id not in clip.extas:
        return None
    h = hashlib.sha256()
    for piece in _clip_exta_pieces(clip, outer_id):
        h.update(piece)
    return h.hexdigest()


def _clip_normalized_exta_payload_sha256(payload: bytes) -> str:
    # Exta payloads embed their own external id at bytes 8..48. Clip Studio
    # can rewrite that id while leaving the payload content identical, so omit
    # the id bytes from semantic comparisons.
    if len(payload) >= 48 and payload[8:16] == b"extrnlid":
        payload = payload[:8] + payload[48:]
    return hashlib.sha256(payload).hexdigest()


def _table_columns(con: Any, table: str) -> set[str]:
    try:
        return {row[1] for row in con.execute(f"PRAGMA table_info({table})")}
    except Exception:
        return set()


def _decode_clip_text(raw: Any) -> str:
    if isinstance(raw, str):
        return raw
    try:
        return bytes(raw).decode("utf-8")
    except UnicodeDecodeError:
        return bytes(raw).decode("utf-8", errors="replace")


def _snake(name: str) -> str:
    return re.sub(r"(?<!^)(?=[A-Z])", "_", name).lower()


def _semantic_digest(value: Any) -> str:
    data = json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":")).encode("utf-8")
    return hashlib.sha256(data).hexdigest()


def find_layer(layers: list[ImportedLayer], selector: str) -> ImportedLayer | None:
    """Select a layer by id / stable id, by folder path (``人物/塗り/ドレス``) or
    by name. A name shared by several layers is refused with the candidates'
    paths and ids (``tmiki ls-layers`` lists them all)."""
    normalized = _norm(selector)
    id_matches: list[ImportedLayer] = []
    path_matches: list[ImportedLayer] = []
    name_matches: list[tuple[str, ImportedLayer]] = []
    for path, layer in walk_imported_layers_with_paths(layers):
        if _norm(layer.layer_id) == normalized or _norm(layer.stable_source_id) == normalized:
            id_matches.append(layer)
        elif _norm(path) == normalized:
            path_matches.append(layer)
        elif _norm(layer.name) == normalized:
            name_matches.append((path, layer))
    if id_matches:
        return id_matches[0]
    if path_matches:
        if len(path_matches) > 1:
            raise ValueError(_ambiguous_message(selector, [(selector, layer.layer_id) for layer in path_matches]))
        return path_matches[0]
    if len(name_matches) > 1:
        raise ValueError(_ambiguous_message(selector, [(path, layer.layer_id) for path, layer in name_matches]))
    if name_matches:
        return name_matches[0][1]
    return None


def _ambiguous_message(selector: str, candidates: list[tuple[str, str]]) -> str:
    shown = "; ".join(f"{path} [{layer_id}]" for path, layer_id in candidates[:8])
    more = f"; +{len(candidates) - 8} more" if len(candidates) > 8 else ""
    return (
        f"layer selector {selector!r} is ambiguous: {len(candidates)} layers share that name ({shown}{more}). "
        "Rename the layers so each name is unique, or select one by its folder path or id."
    )


def walk_imported_layers_with_paths(layers: list[ImportedLayer], prefix: str = "") -> list[tuple[str, ImportedLayer]]:
    out: list[tuple[str, ImportedLayer]] = []
    for layer in layers:
        path = f"{prefix}/{layer.name}" if prefix else layer.name
        out.append((path, layer))
        out.extend(walk_imported_layers_with_paths(layer.children, path))
    return out


def walk_imported_layers(layers: list[ImportedLayer]) -> list[ImportedLayer]:
    out: list[ImportedLayer] = []
    for layer in layers:
        out.append(layer)
        out.extend(walk_imported_layers(layer.children))
    return out


def _sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def _norm(value: str) -> str:
    return re.sub(r"\s+", " ", value.strip()).casefold()
