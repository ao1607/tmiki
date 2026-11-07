from __future__ import annotations

from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable

from . import raster
from .image_ops import diff_bbox, image_to_png_bytes, pixels_from_bytes
from .native import replace_layer_pixels
from .objects import canonical_json_bytes


PIXEL_PROPS = {"blob_ref", "pixel_bbox", "pixel_sha256"}
MERGEABLE_FORMATS = {"clip", "mdp2", "png", "jpeg"}
# Re-saving a lossy file moves pixels all over the image (99.6% of them in
# issue #22's 1200x900 JPEG), so each side's change spans nearly the whole
# canvas and the two always overlap, however far apart the strokes were. No
# tolerance is applied to tell that noise from an edit: a faint glaze or a
# slight colour correction changes pixels by as little, and treating it as
# "unchanged" would drop it from the merge without a word.
LOSSY_FORMATS = {"jpeg"}
LOSSY_CONFLICT_REASON = (
    "JPEG changes every pixel slightly on each save, so it cannot be merged "
    "automatically; take one side with --ours or --theirs"
)


_NO_WRITER_NAMES = {"gif": "GIF", "bmp": "BMP", "tiff": "TIFF", "webp": "WebP", "psd": "PSD"}


def no_merge_writer_reason(source_format: str) -> str:
    """Why a file of a format tmi cannot write back conflicts whenever both
    sides changed it (issue #43). The Rust ``merge.rs`` has the same texts."""
    name = _NO_WRITER_NAMES.get(source_format)
    if name:
        return f"{name} files cannot be merged automatically (tmi cannot write them back); take one side with --ours or --theirs"
    return "this file cannot be merged automatically (tmi does not read it as a drawing); take one side with --ours or --theirs"


@dataclass
class LayerConflict:
    stable_source_id: str
    name: str
    path_name: str | None
    reason: str


@dataclass
class LayerMergeResult:
    entry: dict[str, Any] | None = None
    conflicts: list[str] = field(default_factory=list)
    notes: list[str] = field(default_factory=list)
    # On conflict: the per-layer reasons, and the file with every change that
    # did merge applied while each conflicting layer keeps the local (OURS)
    # version — what the working tree should hold while the merge is pending.
    layer_conflicts: list[LayerConflict] = field(default_factory=list)
    partial_entry: dict[str, Any] | None = None


def merge_entries(
    repo: Any,
    path: str,
    base: dict[str, Any] | None,
    local: dict[str, Any] | None,
    remote: dict[str, Any] | None,
    *,
    on_progress: Callable[[str], None] | None = None,
) -> LayerMergeResult:
    if _same_entry(local, remote):
        return LayerMergeResult(entry=local)
    if _same_entry(base, local):
        return LayerMergeResult(entry=remote)
    if _same_entry(base, remote):
        return LayerMergeResult(entry=local)
    if not base or not local or not remote:
        return LayerMergeResult(conflicts=[f"{path}: add/delete conflict"])
    if local.get("source_format") != remote.get("source_format") or base.get("source_format") != local.get("source_format"):
        return LayerMergeResult(conflicts=[f"{path}: source format changed on both sides"])
    source_format = str(local.get("source_format") or "").lower()
    if source_format not in MERGEABLE_FORMATS:
        return LayerMergeResult(conflicts=[f"{path}: {no_merge_writer_reason(source_format)}"])

    base_layers = _layers(repo, base)
    local_layers = _layers(repo, local)
    remote_layers = _layers(repo, remote)
    differing = [
        sid
        for sid in sorted(set(base_layers) | set(local_layers) | set(remote_layers))
        if not _same_layer(local_layers.get(sid), remote_layers.get(sid))
    ]

    # The writers can only replace a layer's pixels. So the merge starts from
    # the side whose layer *structure or properties* changed (added / deleted /
    # renamed / reordered layers, opacity, blend mode, visibility…) and writes
    # the other side's pixel edits into it. Only when both sides changed
    # structure is there no source to start from.
    def structural(side: dict[str, dict[str, Any]]) -> list[str]:
        return [sid for sid in differing if not _same_layer(base_layers.get(sid), side.get(sid)) and _props_changed(base_layers.get(sid), side.get(sid))]

    local_structural = structural(local_layers)
    remote_structural = structural(remote_layers)
    start_is_remote = bool(remote_structural) and not local_structural
    start, other = (remote, local) if start_is_remote else (local, remote)
    start_layers, other_layers = (remote_layers, local_layers) if start_is_remote else (local_layers, remote_layers)

    replacements: dict[str, bytes] = {}
    layer_conflicts: list[LayerConflict] = []
    notes: list[str] = []

    def conflict(sid: str, reason: str) -> None:
        layer = local_layers.get(sid) or remote_layers.get(sid) or base_layers.get(sid)
        layer_conflicts.append(LayerConflict(sid, _layer_name(layer, sid), (layer or {}).get("path_name"), reason))

    if local_structural and remote_structural:
        both = sorted(set(local_structural) | set(remote_structural))
        for sid in both:
            conflict(sid, "layer structure/properties changed on both sides")

    for sid in differing:
        b = base_layers.get(sid)
        s = start_layers.get(sid)
        o = other_layers.get(sid)
        if any(c.stable_source_id == sid for c in layer_conflicts):
            continue
        if _same_layer(b, o):
            continue  # only the start side changed it: already in its source
        if _props_changed(b, o):
            # A structural change on the non-start side: only possible when
            # both sides changed structure, already reported above.
            continue
        # The other side changed this layer's pixels only.
        if s is None:
            conflict(sid, "deleted on one side, edited on the other")
        elif not _pixel_changed(b, s):
            replacements[sid] = repo.store.read_blob(o["blob_ref"])
        else:
            merged = _merge_non_overlapping_pixels(repo, b, s, o)
            if merged is None:
                conflict(sid, LOSSY_CONFLICT_REASON if source_format in LOSSY_FORMATS else "overlapping pixel edits")
            else:
                replacements[sid] = merged
                notes.append(f"{path}:{_layer_name(b, sid)} merged non-overlapping pixel edits")

    conflicts = [f"{path}:{c.name} {c.reason}" for c in layer_conflicts]
    if layer_conflicts:
        # The working tree gets everything that did merge, with each conflicting
        # layer left at OURS. That needs a local start: a remote start may
        # lack a layer (deleted there) that OURS still has.
        if start_is_remote:
            for c in layer_conflicts:
                ours = local_layers.get(c.stable_source_id)
                if ours is None or c.stable_source_id not in start_layers or _props_changed(start_layers[c.stable_source_id], ours):
                    return LayerMergeResult(conflicts=conflicts, notes=notes, layer_conflicts=layer_conflicts)
                replacements[c.stable_source_id] = repo.store.read_blob(ours["blob_ref"])
        if not replacements:
            return LayerMergeResult(conflicts=conflicts, notes=notes, layer_conflicts=layer_conflicts)
        try:
            partial = _write_merged(
                repo, path, start, source_format, replacements,
                _merged_blob_reuse(start_layers, other_layers, local_layers, replacements), on_progress,
            )
        except Exception:
            partial = None
        return LayerMergeResult(conflicts=conflicts, notes=notes, layer_conflicts=layer_conflicts, partial_entry=partial)
    if not replacements:
        # Every difference lives in the start side already (e.g. both sides
        # made the same edits and it made more).
        return LayerMergeResult(entry=start, notes=notes)
    try:
        return LayerMergeResult(
            entry=_write_merged(
                repo, path, start, source_format, replacements,
                _merged_blob_reuse(start_layers, other_layers, local_layers, replacements), on_progress,
            ),
            notes=notes,
        )
    except Exception as exc:
        return LayerMergeResult(conflicts=[f"{path}: native merge failed: {exc}"], notes=notes)


def _blob_reuse_info(layer: dict[str, Any] | None) -> dict[str, Any] | None:
    """One layer's entry in a ``Repo._layer_blob_reuse``-shaped map."""
    if not layer or layer.get("pixel_sha256") is None or layer.get("blob_ref") is None:
        return None
    return {
        "pixel_sha256": layer["pixel_sha256"],
        "blob_ref": layer["blob_ref"],
        "pixel_bbox": layer.get("pixel_bbox"),
        "pixel_source": (layer.get("source_meta") or {}).get("pixel_source"),
    }


def _merged_blob_reuse(
    start_layers: dict[str, dict[str, Any]],
    other_layers: dict[str, dict[str, Any]],
    local_layers: dict[str, dict[str, Any]],
    replacements: dict[str, bytes],
) -> dict[str, dict[str, Any]]:
    """Which stored decode/blob each layer of the merged file may reuse.

    Every layer not in ``replacements`` keeps ``start``'s tiles byte for byte,
    so ``start``'s decode applies (``pixel_source`` matches, nothing is
    decoded or re-encoded). A replaced layer gets new tiles and is decoded;
    if its pixels then equal the side they were taken from (other's edit, or
    OURS for a conflict left at ours), that side's stored blob is reused
    instead of encoding an identical PNG. The keys are content digests, so a
    wrong guess only costs a decode/encode — it can never store wrong pixels.
    """
    reuse: dict[str, dict[str, Any]] = {}
    for sid, layer in start_layers.items():
        info = _blob_reuse_info(layer)
        if info is not None:
            reuse[sid] = info
    for sid in replacements:
        info = _blob_reuse_info(other_layers.get(sid)) or _blob_reuse_info(local_layers.get(sid))
        # pixel_source is the *start* file's tiles for this sid; the rewritten
        # tiles differ, so only the pixel digest may match.
        if info is not None:
            reuse[sid] = dict(info, pixel_source=None)
        else:
            reuse.pop(sid, None)
    return reuse


def _write_merged(
    repo: Any,
    path: str,
    start: dict[str, Any],
    source_format: str,
    replacements: dict[str, bytes],
    blob_reuse: dict[str, dict[str, Any]] | None = None,
    on_progress: Callable[[str], None] | None = None,
) -> dict[str, Any]:
    """Write ``replacements`` (layer pixels) into ``start``'s source file.

    Layers that only the start side edited — and layers both sides edited
    identically — already live in that source; starting from base would
    silently revert them."""
    source_path = _write_temp_source(repo, path, repo.store.read_blob(start["source_blob_ref"]))
    try:
        for idx, (sid, png_bytes) in enumerate(replacements.items(), 1):
            if on_progress:
                on_progress(f"writing merged layers {idx}/{len(replacements)}")
            source_path.write_bytes(replace_layer_pixels(source_path, source_format, sid, png_bytes))
        if on_progress:
            on_progress(f"parsing merged {path}")
        # Only the replaced layers' pixels differ from ``start``, so its
        # composite is where an incremental composite of the result begins.
        return repo.materialize_file_from_path(
            path, source_path, blob_reuse=blob_reuse, on_progress=on_progress, base_entry=start,
        )
    finally:
        source_path.unlink(missing_ok=True)


def _merge_non_overlapping_pixels(repo: Any, base: dict[str, Any], local: dict[str, Any], remote: dict[str, Any]) -> bytes | None:
    if not (base.get("blob_ref") and local.get("blob_ref") and remote.get("blob_ref")):
        return None
    # The layers stay sparse (their painted rectangles): tmi_accel compares
    # and pastes them without Pillow and without full-canvas copies.
    base_image = pixels_from_bytes(repo.store.read_blob(base["blob_ref"]))
    local_image = pixels_from_bytes(repo.store.read_blob(local["blob_ref"]))
    remote_image = pixels_from_bytes(repo.store.read_blob(remote["blob_ref"]))
    local_bbox = diff_bbox(base_image, local_image)
    remote_bbox = diff_bbox(base_image, remote_image)
    if not local_bbox:
        return repo.store.read_blob(remote["blob_ref"])
    if not remote_bbox:
        return repo.store.read_blob(local["blob_ref"])
    if _overlaps(local_bbox, remote_bbox):
        return None
    # Each bbox is exactly the region where that side differs from base, and the
    # two regions do not overlap, so the merge is "replace base's pixels with the
    # editing side's pixels" in each region. Use paste (a straight band copy,
    # alpha included) rather than alpha_composite: source-over would drop edits
    # that *erase* or *reduce* alpha — a transparent source pixel composited onto
    # an opaque base leaves the base showing, silently undoing the erasure.
    base_full = base_image.dense()
    merged = raster.compose(
        base_full.width, base_full.height,
        [
            ("paste", local_image.region(local_bbox), local_bbox[0], local_bbox[1]),
            ("paste", remote_image.region(remote_bbox), remote_bbox[0], remote_bbox[1]),
        ],
        base=base_full,
    )
    return image_to_png_bytes(merged)


def _write_temp_source(repo: Any, relpath: str, source: bytes) -> Path:
    tmp_dir = repo.repo_dir / "tmp" / "merge"
    tmp_dir.mkdir(parents=True, exist_ok=True)
    target = tmp_dir / Path(relpath).name
    target.write_bytes(source)
    return target


def _layers(repo: Any, entry: dict[str, Any]) -> dict[str, dict[str, Any]]:
    layers = {layer["stable_source_id"]: layer for layer in repo.flatten_layers(entry["tree_ref"])}
    for layer in layers.values():
        # Children are flattened and compared one by one, so a folder only
        # contributes its child order. Its subtree_ref hashes the children's
        # blobs, which would make every edit inside a folder (or a mere PNG
        # re-encode) look like a structural change of the folder itself.
        subtree_ref = layer.pop("subtree_ref", None)
        if subtree_ref:
            tree = repo.store.read_json(subtree_ref, "tree")
            layer["child_ids"] = [child["stable_source_id"] for child in tree.get("layers", [])]
    return layers


def _pixel_changed(base: dict[str, Any] | None, side: dict[str, Any] | None) -> bool:
    if not base or not side:
        return False
    if base.get("pixel_sha256") and side.get("pixel_sha256"):
        return base["pixel_sha256"] != side["pixel_sha256"]
    return base.get("blob_ref") != side.get("blob_ref")


def _props_changed(base: dict[str, Any] | None, side: dict[str, Any] | None) -> bool:
    if not base or not side:
        return True
    return _props(base) != _props(side)


def _props(layer: dict[str, Any]) -> dict[str, Any]:
    props = {key: value for key, value in layer.items() if key not in PIXEL_PROPS and key not in {"path_name"}}
    return _without_pixel_source(props)


def _without_pixel_source(layer: dict[str, Any]) -> dict[str, Any]:
    # source_meta.pixel_source is a hash of the encoded tiles — it moves with
    # the pixels, so it is pixel state, not a layer property.
    meta = layer.get("source_meta")
    if isinstance(meta, dict) and "pixel_source" in meta:
        layer = dict(layer)
        layer["source_meta"] = {k: v for k, v in meta.items() if k != "pixel_source"}
    return layer


def _same(left: dict[str, Any] | None, right: dict[str, Any] | None) -> bool:
    if left is None or right is None:
        return left is right
    return canonical_json_bytes(left) == canonical_json_bytes(right)


def _same_entry(left: dict[str, Any] | None, right: dict[str, Any] | None) -> bool:
    # Identical source bytes are the same file even when the derived blobs
    # (layer blobs, composite, thumbnail) were encoded differently — e.g. one
    # side committed with tmi_accel and the other with the pure Python path.
    if left is not None and right is not None:
        digest = left.get("source_digest")
        if digest and digest == right.get("source_digest"):
            return True
    return _same(left, right)


def _same_layer(left: dict[str, Any] | None, right: dict[str, Any] | None) -> bool:
    # A layer blob's bytes (and so blob_ref) depend on the encoder: tmi_accel
    # or Python, the libzstd version, older PNG blobs. Compare decoded-pixel
    # digests instead whenever both sides carry one.
    if left is not None and right is not None and left.get("pixel_sha256") and right.get("pixel_sha256"):
        return _same(
            _without_pixel_source(_without_blob_ref(left)),
            _without_pixel_source(_without_blob_ref(right)),
        )
    return _same(left, right)


def _without_blob_ref(layer: dict[str, Any]) -> dict[str, Any]:
    return {key: value for key, value in layer.items() if key != "blob_ref"}


def _overlaps(a: list[int], b: list[int]) -> bool:
    return not (a[2] <= b[0] or b[2] <= a[0] or a[3] <= b[1] or b[3] <= a[1])


def _layer_name(layer: dict[str, Any] | None, fallback: str) -> str:
    return str(layer.get("name") if layer else fallback)
