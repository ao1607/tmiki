from __future__ import annotations

import os
from concurrent.futures import Future, ThreadPoolExecutor
from dataclasses import dataclass
from datetime import datetime
from pathlib import Path
from typing import TYPE_CHECKING, Any, Callable

# tmi.raster, tmi.image_ops and tmi.importers are imported where pixels are
# handled: a diff that finds nothing changed (or --name-only) needs none.
# The pixels are `tmi.raster` images (tmi_accel's kernels, RUST_MIGRATION.md
# 2b-2d): a diff of PNG / JPEG / MDP2 / CLIP files or of stored versions
# loads neither Pillow nor numpy.
if TYPE_CHECKING:
    from .raster import Rgba, Sparse

    Image = Any  # an Rgba / Sparse (what decodes and renders give)

from .layer_props import RASTER_LAYER_ID, layer_props, property_changes, walk_imported, walk_tree
from .repo import Repo, _is_deleted_index_entry, _reuse_decoded, _StatCache


# HTML reports embed previews, not originals: a 60 MB illustration's full-size
# PNGs made a 144 MB index.html. Originals are written next to it and linked.
HTML_COMPOSITE_EDGE = 2048
HTML_LAYER_EDGE = 1024
HTML_PREVIEW_QUALITY = 82
# libwebp's effort, 0-6. From 3 up it adds rate-distortion passes: on
# 2048 px illustration previews 4 took ~3x as long as 2 for 2-4 % smaller
# files at the same PSNR.
HTML_PREVIEW_METHOD = 2


@dataclass
class DiffSide:
    """One side of a diff: a commit, the index (``rev=":index"``) or the
    working tree (``rev=None``)."""

    rev: str | None
    label: str


WORKTREE = DiffSide(None, "working tree")


def index_side() -> DiffSide:
    return DiffSide(":index", "index")


@dataclass
class _LayerView:
    name: str
    pixel: str | None
    props: dict[str, Any]
    load: Callable[[], Rgba | Sparse] | None
    metadata: Any = None


def diff_working(
    repo: Repo,
    path_filter: str | None = None,
    mode: str = "side-by-side",
    out_dir: Path | None = None,
) -> list[dict[str, Any]]:
    """Working tree vs HEAD (kept for callers of the old API)."""
    head = repo.current_commit()
    if not head:
        raise RuntimeError("no commit to diff against")
    return diff_sides(repo, DiffSide(head, head[:12]), WORKTREE, [path_filter] if path_filter else [], mode, out_dir)


def diff_sides(
    repo: Repo,
    left: DiffSide,
    right: DiffSide,
    path_filters: list[str] | None = None,
    mode: str = "side-by-side",
    out_dir: Path | None = None,
    on_progress: Callable[[str], None] | None = None,
) -> list[dict[str, Any]]:
    progress = on_progress or (lambda _msg: None)
    left_entries = _side_entries(repo, left)
    right_entries = _side_entries(repo, right)
    paths = sorted(set(left_entries) | set(right_entries))
    if path_filters:
        known = set(paths)
        wanted = {repo.pathspec_relpath(spec, known) for spec in path_filters}
        missing = sorted(wanted - known)
        if missing:
            raise RuntimeError(f"path is not tracked in {left.label} or {right.label}: {missing[0]}")
        paths = [p for p in paths if p in wanted]
    # "names" (--name-only / --name-status) needs only which paths differ;
    # "summary" (--stat) also counts changed layers but renders nothing.
    names_only = mode == "names"
    summary = mode in {"summary", "names"}
    html_mode = mode == "html"
    out_dir = out_dir or (repo.repo_dir / "diff" / datetime.now().strftime("%Y%m%d-%H%M%S"))
    results: list[dict[str, Any]] = []
    # status's stat cache, read only: a diff of some paths must not prune
    # the others' entries.
    stat_cache = _StatCache(repo) if right.rev is None else None

    for rel in paths:
        old = left_entries.get(rel)
        new = right_entries.get(rel)
        full = repo.abspath(rel) if right.rev is None else None
        source: tuple[bytes, str] | None = None
        if right.rev is None:
            if full is None or not full.is_file():
                if old is not None:
                    results.append({"path": rel, "state": "deleted", "layers": [], "outputs": []})
                continue
            # Compare file digests first: an unchanged file is never decoded.
            # A changed file's bytes, once read and hashed, go to the import.
            if old is not None:
                modified, source = stat_cache.changed(rel, full, old.get("source_digest"), want_bytes=not names_only)
                if not modified:
                    continue
        elif old is not None and new is not None and old.get("source_digest") == new.get("source_digest"):
            continue
        if new is None and right.rev is not None:
            results.append({"path": rel, "state": "deleted", "layers": [], "outputs": []})
            continue

        if names_only:
            # --name-only / --name-status print the path and A/M/D only: the
            # digest comparison above already decided that, so no file is
            # parsed at all.
            results.append({"path": rel, "state": "added" if old is None else "modified", "layers": [], "outputs": []})
            continue

        from . import raster
        from .image_ops import pixels_from_bytes
        from .importers import import_file

        # The left composite is read and inflated beside the right side's
        # import/render (Pillow releases the GIL while it decodes).
        old_composite_job = None if summary else _start(lambda e=old: _entry_composite_decoded(repo, e))
        blob_reuse: dict[str, dict] = {}
        if full is not None:
            progress(f"parsing {rel}")
            # Layers whose pixels did not change since the left side, the
            # index or HEAD are neither decoded nor encoded again: their stored
            # pixel digest (and blob, for previews) stands in, exactly as
            # `tmiki add` / `tmiki status` do. A changed layer is always decoded.
            blob_reuse = _working_file_reuse(repo, rel, old)
            # The render cache is content-addressed (source digest + renderer
            # version), so a file already added/committed is not re-rendered;
            # --stat needs no composite at all.
            imported = import_file(
                full,
                render_composite=not summary,
                cache=repo.render_cache,
                reuse_pixels={sid: d["pixel_sha256"] for sid, d in blob_reuse.items()} or None,
                reuse_decoded=_reuse_decoded(blob_reuse),
                on_progress=on_progress,
                composite_base=None if summary else _working_composite_base(repo, rel, old),
                source=source[0] if source else None,
                source_digest=source[1] if source else None,
                composite_image=not summary,
            )
            source = None
        else:
            imported = None
        new_format = imported.source_format if imported is not None else new.get("source_format")
        if new_format == "binary" or (old or {}).get("source_format") == "binary":
            results.append({"path": rel, "state": "added" if old is None else "modified", "binary": True, "layers": [], "outputs": []})
            continue

        file_result: dict[str, Any] = {"path": rel, "state": "added" if old is None else "modified", "layers": [], "outputs": []}
        old_layers = _entry_layers(repo, old) if old else {}
        new_layers = _imported_layers(imported, repo, blob_reuse) if imported is not None else _entry_layers(repo, new)

        composite_job = None
        if not summary:
            progress(f"comparing composites of {rel}")
            new_png = imported.composite_png if imported is not None else _entry_composite_png(repo, new)
            new_composite = None
            if imported is not None:
                # The image the import encoded new_png from, when it rendered
                # one: decoding new_png would give the same pixels and info.
                new_composite = imported.composite_image
                imported.composite_image = None
            if new_composite is None and (imported is not None or new_png is not None):
                new_composite = pixels_from_bytes(new_png)
            elif new_composite is not None:
                new_composite = raster.as_rgba(new_composite)  # (a tmi.raster image already: no copy)
            old_png, old_composite = old_composite_job.result()
            if new_composite is not None:
                if old_composite is None:
                    old_composite = raster.Rgba(*new_composite.size)
                if html_mode:
                    # The composites' bbox and previews only fill
                    # file_result["bbox"] / ["html"]: the layers are compared
                    # meanwhile.
                    def compare_composites(o=old_composite, n=new_composite, op=old_png, np_=new_png) -> None:
                        bbox = _diff_bbox(o, n)
                        file_result["bbox"] = bbox
                        if bbox:
                            _emit_composite(file_result, out_dir, rel, mode, o, n, old_png=op, new_png=np_, bbox=bbox)

                    composite_job = _start(compare_composites)
                else:
                    bbox = _diff_bbox(old_composite, new_composite)
                    file_result["bbox"] = bbox
                    if bbox:
                        progress(f"encoding composite previews of {rel}")
                        _emit_composite(
                            file_result, out_dir, rel, mode, old_composite, new_composite,
                            old_png=old_png, new_png=new_png, bbox=bbox,
                        )
            del old_composite, new_composite

        candidates = list(new_layers) + [sid for sid in old_layers if sid not in new_layers]
        if candidates and all(sid == RASTER_LAYER_ID for sid in candidates):
            # A PNG/JPEG: its one "layer" is the image itself.
            file_result["raster"] = True
        for idx, sid in enumerate(candidates, 1):
            progress(f"comparing layers {idx}/{len(candidates)}")
            before = old_layers.get(sid)
            after = new_layers.get(sid)
            if before is None:
                layer_result: dict[str, Any] = {"name": after.name, "state": "added", "prop_changes": []}
            elif after is None:
                layer_result = {"name": before.name, "state": "deleted", "prop_changes": []}
            else:
                # Readable property changes (name, opacity in percent, folder
                # ...), never a metadata digest (issue #15); a metadata change
                # nothing readable explains is "other properties".
                metadata_changed = (
                    before.metadata is not None
                    and after.metadata is not None
                    and before.metadata[0] == after.metadata[0]
                    and before.metadata[1] != after.metadata[1]
                    and before.pixel == after.pixel
                )
                prop_changes = [
                    {"key": label, "old": old_text, "new": new_text}
                    for label, old_text, new_text in property_changes(
                        before.props, after.props, metadata_changed=metadata_changed
                    )
                ]
                if before.pixel == after.pixel and not prop_changes:
                    continue
                layer_result = {"name": after.name, "state": "changed", "prop_changes": prop_changes}
            if not summary:
                # The two sides inflate side by side.
                left_job = _start(before.load) if before and before.load else None
                try:
                    right_image = after.load() if after and after.load else None
                except BaseException:
                    if left_job is not None:
                        left_job.result()  # the left side's error, if any, comes first
                    raise
                left_image = left_job.result() if left_job is not None else None
                if left_image is not None or right_image is not None:
                    left_image = left_image or _transparent_like(right_image)
                    right_image = right_image or _transparent_like(left_image)
                    bbox = _diff_bbox(left_image, right_image)
                    layer_result["bbox"] = bbox
                    # The HTML report shows a PNG/JPEG's image once, as the
                    # composite; no layer preview to encode.
                    if bbox and not (html_mode and file_result.get("raster")):
                        _emit_layer_preview(file_result, out_dir, rel, layer_result["name"], mode, left_image, right_image, layer_result, bbox)
            file_result["layers"].append(layer_result)

        if composite_job is not None:
            if not composite_job.done():
                progress(f"encoding composite previews of {rel}")
            composite_job.result()
        if summary or file_result.get("bbox") or file_result["layers"] or old is None:
            results.append(file_result)
    return results


def _side_entries(repo: Repo, side: DiffSide) -> dict[str, dict[str, Any]]:
    if side.rev is None or side.rev == ":index":
        entries = repo.entries_for_commit(repo.current_commit())
        for path, entry in repo.read_index().get("entries", {}).items():
            if _is_deleted_index_entry(entry):
                entries.pop(path, None)
            else:
                entries[path] = entry
        return entries
    return repo.entries_for_commit(side.rev)


def _entry_composite_png(repo: Repo, entry: dict[str, Any] | None) -> bytes | None:
    if not entry or not entry.get("composite_ref"):
        return None
    return repo.store.read_blob(entry["composite_ref"])


def _entry_composite_decoded(repo: Repo, entry: dict[str, Any] | None) -> tuple[bytes | None, Image | None]:
    from .image_ops import pixels_from_bytes

    data = _entry_composite_png(repo, entry)
    return data, (pixels_from_bytes(data) if data is not None else None)


def _start(fn: Callable[[], Any]) -> Future:
    """Run ``fn`` on a thread of its own; ``.result()`` joins it."""
    pool = ThreadPoolExecutor(max_workers=1)
    try:
        return pool.submit(fn)
    finally:
        pool.shutdown(wait=False)


def _entry_composite(repo: Repo, entry: dict[str, Any] | None) -> Image | None:
    from .image_ops import pixels_from_bytes

    data = _entry_composite_png(repo, entry)
    return pixels_from_bytes(data) if data is not None else None


def _entry_layers(repo: Repo, entry: dict[str, Any]) -> dict[str, _LayerView]:
    from .image_ops import pixels_from_bytes

    views: dict[str, _LayerView] = {}
    for layer, parent in walk_tree(repo, entry["tree_ref"]):
        blob = layer.get("blob_ref")
        pixel = layer.get("pixel_sha256")
        if pixel is None and blob:
            pixel = repo._blob_pixel_digest(blob)
        views[layer["stable_source_id"]] = _LayerView(
            name=layer.get("name") or layer["stable_source_id"],
            pixel=pixel,
            props=layer_props(layer, parent),
            load=(lambda b=blob: pixels_from_bytes(repo.store.read_blob(b))) if blob else None,
            metadata=_semantic_key(layer.get("source_meta")),
        )
    return views


def _semantic_key(meta: dict[str, Any] | None) -> tuple[Any, Any] | None:
    """The layer's semantic digest with its version (digests of different
    versions are not comparable)."""
    meta = meta or {}
    digest = meta.get("semantic_digest")
    return None if digest is None else (meta.get("semantic_digest_version"), digest)


def _imported_layers(
    imported: Any,
    repo: Repo | None = None,
    blob_reuse: dict[str, dict] | None = None,
) -> dict[str, _LayerView]:
    from .image_ops import pixels_from_bytes

    views: dict[str, _LayerView] = {}
    for layer, parent in walk_imported(imported.layers):
        blob = layer.blob_bytes
        load = (lambda b=blob: pixels_from_bytes(b)) if blob else None
        reused = (blob_reuse or {}).get(layer.stable_source_id)
        if (
            load is None
            and repo is not None
            and reused is not None
            and layer.pixel_sha256 is not None
            and reused.get("pixel_sha256") == layer.pixel_sha256
        ):
            # Not re-encoded because a stored blob holds the same pixels.
            store = repo.store
            load = lambda ref=reused["blob_ref"]: pixels_from_bytes(store.read_blob(ref))
        views[layer.stable_source_id] = _LayerView(
            name=layer.name,
            pixel=layer.pixel_sha256,
            props=layer_props(layer, parent),
            load=load,
            metadata=_semantic_key(layer.source_meta),
        )
    return views


def _working_file_entries(repo: Repo, rel: str, left: dict[str, Any] | None) -> list[dict[str, Any]]:
    """Stored versions of ``rel`` a working file most likely shares layers
    with, later ones preferred: the diff's left side, HEAD, then the index."""
    head = repo.entries_for_commit(repo.current_commit()).get(rel)
    staged = repo.read_index().get("entries", {}).get(rel)
    out: list[dict[str, Any]] = []
    for entry in (left, head, staged):
        if entry and not _is_deleted_index_entry(entry) and entry not in out:
            out.append(entry)
    return out


def _working_file_reuse(repo: Repo, rel: str, left: dict[str, Any] | None) -> dict[str, dict]:
    reuse: dict[str, dict] = {}
    for entry in _working_file_entries(repo, rel, left):
        reuse.update(repo._layer_blob_reuse(entry))
    return reuse


def _working_composite_base(repo: Repo, rel: str, left: dict[str, Any] | None) -> Any | None:
    for entry in reversed(_working_file_entries(repo, rel, left)):
        base = repo._composite_base(entry)
        if base is not None:
            return base
    return None


def _safe(value: str) -> str:
    from .repo import safe_filename

    return safe_filename(value)


def _safe_rel_stem(rel: str) -> str:
    # The whole path, extension included: art.clip and its export art.png
    # must not share preview files (issue #38).
    return _safe(Path(rel).as_posix())


def _is_png(data: bytes | None) -> bool:
    return data is not None and data.startswith(b"\x89PNG\r\n\x1a\n")


def _diff_bbox(left: Image, right: Image) -> list[int] | None:
    """``diff_bbox(left, right)``: one pass of tmi_accel over the union of
    the two sides' painted rectangles (a layer blob's pixels stay sparse;
    outside both rectangles nothing differs)."""
    from . import raster

    return raster.diff_bbox(left, right)


def _transparent_like(image: Image) -> Sparse:
    from .raster import Sparse

    return Sparse(image.size)


def _fit(size: tuple[int, int], edge: int) -> tuple[int, int]:
    """``size`` scaled down to at most ``edge`` on its longer side."""
    scale = edge / max(size[0], size[1], 1)
    if scale >= 1:
        return size
    return max(1, round(size[0] * scale)), max(1, round(size[1] * scale))


def _shrink(image: Image, edge: int) -> Rgba:
    from . import raster

    image = raster.dense(image)
    return image.resize(_fit(image.size, edge))


if TYPE_CHECKING:
    _ResizeJob = tuple[Rgba, tuple[int, int], tuple[float, float, float, float] | None]


def _resize_all(jobs: list[_ResizeJob]) -> list[Rgba]:
    """Each ``image.resize(size, LANCZOS, box=box)`` (the image itself when
    there is nothing to do), resampled side by side: tmi_accel releases the
    GIL while it resamples."""

    def one(job: _ResizeJob) -> Rgba:
        image, size, box = job
        return image.resize(size, box)

    if sum(box is not None or image.size != size for image, size, box in jobs) < 2:
        return [one(job) for job in jobs]
    with ThreadPoolExecutor(max_workers=min(len(jobs), max(2, os.cpu_count() or 2))) as pool:
        return list(pool.map(one, jobs))


def _preview_bytes(image: Image) -> bytes:
    """An HTML report preview, encoded small: lossy WebP, which keeps alpha
    (tmi_accel's libwebp 1.6.0, the version Pillow 12.3 carries). Lossless
    PNG previews made a 60 MB illustration's report ~30 MB; the originals
    are linked from the report at full quality anyway."""
    from . import raster

    return raster.webp(image, HTML_PREVIEW_QUALITY, HTML_PREVIEW_METHOD)


def _encode_previews(images: dict[str, Image]) -> dict[str, bytes]:
    """``{key: _preview_bytes(image)}`` in ``images``' order, the encodes run
    side by side (libwebp releases the GIL); each encode's bytes are those
    of encoding it alone."""
    if len(images) < 2:
        return {key: _preview_bytes(image) for key, image in images.items()}
    with ThreadPoolExecutor(max_workers=min(len(images), max(2, os.cpu_count() or 2))) as pool:
        futures = {key: pool.submit(_preview_bytes, image) for key, image in images.items()}
        return {key: future.result() for key, future in futures.items()}


def _shrink_both(a: Image, b: Image, edge: int) -> tuple[Rgba, Rgba]:
    """``(_shrink(a, edge), _shrink(b, edge))``, the two resampled side by side."""
    from . import raster

    a, b = raster.dense(a), raster.dense(b)
    small_a, small_b = _resize_all([(a, _fit(a.size, edge), None), (b, _fit(b.size, edge), None)])
    return small_a, small_b


def _composite_preview_images(
    old: Image,
    new: Image,
    bbox: list[int] | tuple[int, ...] | None,
) -> dict[str, Rgba]:
    """The report's five composite previews, at most ``HTML_COMPOSITE_EDGE``
    on a side: old, new, side by side, overlaid, and new with the changed
    area outlined.

    Building each preview at full size and downscaling it cost five
    full-canvas resamples (one of a canvas twice as wide) plus full-canvas
    blends and copies. Now only the two canvases are resampled, side by
    side: to the preview size, and onto the side-by-side's pixel grid; the
    overlay and outline previews are made from the small canvases. The
    old, new and (opaque) side-by-side previews are the same images as
    before; the overlay differs by rounding, and the outline is drawn at
    preview scale. A canvas whose side-by-side fits in the edge gives the
    same images throughout."""
    import math

    from . import raster
    from .image_ops import diff_bbox, diff_preview, overlay, side_by_side

    edge = HTML_COMPOSITE_EDGE
    old, new = raster.dense(old), raster.dense(new)
    if bbox is None:
        bbox = diff_bbox(old, new)
    if old.size != new.size:
        # side_by_side / overlay pad the smaller canvas: build them at full size.
        return {
            "old_png": _shrink(old, edge),
            "new_png": _shrink(new, edge),
            "side_by_side_png": _shrink(side_by_side(old, new), edge),
            "overlay_png": _shrink(overlay(old, new), edge),
            "diff_preview_png": _shrink(diff_preview(old, new, bbox=bbox), edge),
        }

    width, height = new.size
    small = _fit(new.size, edge)
    jobs: list[_ResizeJob] = [(old, small, None), (new, small, None)]
    # side_by_side() sets the canvases 16 px apart and the pair shrinks as a
    # whole: each canvas is resampled on the pair's own pixel grid (``box``),
    # the columns that straddle the gutter left out.
    gutter = 16
    pair = (2 * width + gutter, height)
    pair_size = _fit(pair, edge)
    if pair_size != pair:
        step = pair[0] / pair_size[0]
        left_w = math.floor(width / step)
        right_x = math.ceil((width + gutter) / step)
        right_w = pair_size[0] - right_x
        if left_w < 1 or right_w < 1:
            pair_size = None
        else:
            # min / max: no float rounding past the canvas.
            jobs.append((old, (left_w, pair_size[1]), (0, 0, min(width, left_w * step), height)))
            jobs.append((new, (right_w, pair_size[1]), (max(0.0, right_x * step - width - gutter), 0, width, height)))
    small_old, small_new, *halves = _resize_all(jobs)
    if pair_size is None:
        pair_image = _shrink(side_by_side(old, new), edge)
    elif pair_size == pair:
        pair_image = side_by_side(small_old, small_new)
    else:
        # Columns whose Lanczos window (3 columns either way) reaches the
        # gutter: resample that narrow strip of the full-size pair.
        c0, c1 = max(0, left_w - 3), min(pair_size[0], right_x + 3)
        x0, x1 = max(0, math.floor((c0 - 4) * step)), min(pair[0], math.ceil((c1 + 4) * step))
        strip_ops = []
        if x0 < width:
            strip_ops.append(("over", old.crop((x0, 0, width, height)), 0, 0))
        if x1 > width + gutter:
            at = max(0, width + gutter - x0)
            strip_ops.append(("over", new.crop((x0 + at - width - gutter, 0, x1 - width - gutter, height)), at, 0))
        strip = raster.compose(x1 - x0, height, strip_ops, fill=(32, 32, 32, 255))
        box = (c0 * step - x0, 0, min(x1 - x0, c1 * step - x0), height)
        # An opaque half is pasted, a translucent one alpha-composited (the
        # same bytes either way); the strip is pasted as it is.
        pair_image = raster.compose(
            pair_size[0], pair_size[1],
            [("over", halves[0], 0, 0), ("over", halves[1], right_x, 0),
             ("paste", strip.resize((c1 - c0, pair_size[1]), box), c0, 0)],
            fill=(32, 32, 32, 255),
        )
    outline = None
    if bbox:
        sx, sy = small_new.width / width, small_new.height / height
        outline = (
            math.floor(bbox[0] * sx), math.floor(bbox[1] * sy),
            math.ceil(bbox[2] * sx), math.ceil(bbox[3] * sy),
        )
    return {
        "old_png": small_old,
        "new_png": small_new,
        "side_by_side_png": pair_image,
        "overlay_png": overlay(small_old, small_new),
        # diff_preview's outline is width // 300 wide: drawn on the small
        # image it is the full-size outline, downscaled.
        "diff_preview_png": diff_preview(small_old, small_new, bbox=outline or ()),
    }

def _emit_composite(
    file_result: dict[str, Any],
    out_dir: Path,
    rel: str,
    mode: str,
    old: Image,
    new: Image,
    *,
    old_png: bytes | None = None,
    new_png: bytes | None = None,
    bbox: list[int] | None = None,
) -> None:
    """``bbox``, when given, is ``diff_bbox(old, new)``."""
    if mode == "summary":
        return
    from .image_ops import diff_preview, image_to_png_bytes, overlay, side_by_side

    out_dir.mkdir(parents=True, exist_ok=True)
    stem = _safe_rel_stem(rel)
    if mode == "html":
        # Originals sit next to index.html; the report embeds previews. When
        # the composite PNG the image was decoded from is at hand, write it
        # as-is instead of re-encoding the same pixels.
        old_name, new_name = f"{stem}-old.png", f"{stem}-new.png"
        (out_dir / old_name).write_bytes(old_png if _is_png(old_png) else image_to_png_bytes(old))
        (out_dir / new_name).write_bytes(new_png if _is_png(new_png) else image_to_png_bytes(new))
        previews = _encode_previews(_composite_preview_images(old, new, bbox))
        file_result["html"] = {
            **previews,
            "size": [max(old.width, new.width), max(old.height, new.height)],
            "full_old": old_name,
            "full_new": new_name,
        }
        return
    if mode == "overlay":
        image, name = overlay(old, new), f"{stem}-overlay.png"
    elif mode == "per-layer":
        image, name = diff_preview(old, new, bbox=bbox), f"{stem}-composite-diff.png"
    else:
        image, name = side_by_side(old, new), f"{stem}-side-by-side.png"
    target = out_dir / name
    target.write_bytes(image_to_png_bytes(image))
    file_result["outputs"].append(str(target))


def _emit_layer_preview(
    file_result: dict[str, Any],
    out_dir: Path,
    rel: str,
    layer_name: str,
    mode: str,
    left: Image,
    right: Image,
    layer_result: dict[str, Any],
    bbox: list[int] | tuple[int, ...] | None = None,
) -> None:
    if mode == "summary":
        return
    from . import raster
    from .image_ops import diff_preview, image_to_png_bytes, overlay, side_by_side

    if mode == "html":
        # Show the changed region (padded), not the whole canvas.
        if bbox:
            pad = 16
            box = (max(0, bbox[0] - pad), max(0, bbox[1] - pad), min(left.width, bbox[2] + pad), min(left.height, bbox[3] + pad))
            # _shrink_both of the two regions, resampling only their painted
            # pixels: the regions themselves are never built.
            size = _fit((box[2] - box[0], box[3] - box[1]), HTML_LAYER_EDGE)
            with ThreadPoolExecutor(max_workers=2) as pool:
                left, right = pool.map(lambda image: raster.resize_region(image, box, size), (left, right))
        else:
            left, right = _shrink_both(left, right, HTML_LAYER_EDGE)
        # The report shows a changed layer side by side and overlaid only.
        layer_result["html"] = _encode_previews({
            "side_by_side_png": side_by_side(left, right),
            "overlay_png": overlay(left, right),
        })
        return
    out_dir.mkdir(parents=True, exist_ok=True)
    # The full-size images draw only the sides' painted pixels.
    if mode == "overlay":
        image = raster.overlay_painted(left, right)
        suffix = "overlay"
    elif mode == "per-layer":
        image = diff_preview(left, right) if bbox is None else raster.diff_preview_painted(left, right, bbox)
        suffix = "layer-diff"
    else:
        image = raster.side_by_side_painted(left, right)
        suffix = "side-by-side"
    if layer_name == Path(rel).stem:
        # An image's one layer is named after the file: "art.png-art-overlay"
        # says "art" twice, and "art.png-overlay" is the composite's (#38).
        name = f"{_safe_rel_stem(rel)}-layer-{suffix.removeprefix('layer-')}.png"
    else:
        name = f"{_safe_rel_stem(rel)}-{_safe(layer_name)}-{suffix}.png"
    target = out_dir / name
    target.write_bytes(image_to_png_bytes(image))
    file_result["outputs"].append(str(target))
