"""Human-readable layer property changes, shared by ``status`` and ``diff``.

A layer's properties live in two places: the plain fields every format has
(name, visibility, opacity, blend mode, clipping, offset) and the format's own
``source_meta``. Users should read what changed (``name: 色足し -> 色足し改``,
``opacity: 100% -> 50%``), never a digest of it; whatever has no readable form
is summed up as "other properties" (issue #15).
"""
from __future__ import annotations

from typing import Any, Iterator, Mapping

OTHER = "other properties"
# The one layer of a flat raster file (PNG / JPEG / other decodable images):
# the image itself, reported as "pixels" rather than as a layer named after
# the file (issue #24).
RASTER_LAYER_ID = "image:0"
FOLDER_TOGGLE = "folder open/closed"
TOP_LEVEL = "(top level)"

# source_meta keys with a readable form: key -> label.
_META_LABELS = {
    "protect_alpha": "lock transparency",
    "draft": "draft",
    "color": "layer color",
    "text": "text",
    "text_lines": "text",
}
# source_meta keys a readable item already covers (the MDP2 parent id is the
# folder; CLIP's composite id is the blend mode), or that only describe how
# the pixels are stored (a pixel edit changes them; it is reported as one).
_META_COVERED = {"parent_id", "composite_id", "pixel_width", "pixel_height", "channel_profile", "width", "height"}
# MDP2 keeps every XML attribute in ``xml_attrs``; these are shown readably
# or are pixel storage.
_XML_COVERED = {
    "name", "alpha", "mode", "visible", "clipping", "ofsx", "ofsy", "parentId",
    "protectAlpha", "draft", "color", "id", "bin", "width", "height", "folderOpen",
}


def format_opacity(value: Any) -> str:
    """Opacity as a whole percentage, as ``ls-layers`` shows it."""
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return "" if value is None else str(value)
    return f"{round(float(value) * 100)}%"


def opacity_texts(old: Any, new: Any) -> tuple[str, str]:
    """Both sides of an opacity change in percent; one decimal when they
    differ by less than a whole percent (e.g. CLIP's 255/256 vs 256/256)."""
    old_text, new_text = format_opacity(old), format_opacity(new)
    if old_text == new_text and isinstance(old, (int, float)) and isinstance(new, (int, float)):
        old_text, new_text = f"{float(old) * 100:.1f}%", f"{float(new) * 100:.1f}%"
    return old_text, new_text


def _on_off(value: Any) -> str:
    if value is None:
        return "-"
    return "on" if value else "off"


def _text(value: Any, limit: int = 40) -> str:
    if isinstance(value, (list, tuple)):
        value = "\n".join(str(v) for v in value)
    text = "" if value is None else str(value)
    text = text.replace("\r\n", "\n").replace("\n", " / ")
    if len(text) > limit:
        text = text[: limit - 1] + "…"
    return f'"{text}"'


def layer_props(layer: Any, folder: tuple[str | None, str] | None = None) -> dict[str, Any]:
    """The comparable properties of a stored layer (a tree dict) or an
    ``ImportedLayer``. ``folder`` is ``(parent stable id, parent path)``."""
    get = layer.get if isinstance(layer, Mapping) else (lambda key, default=None: getattr(layer, key, default))
    return {
        "name": get("name"),
        "kind": get("kind"),
        "visible": get("visible"),
        "opacity": get("opacity"),
        "blend_mode": get("blend_mode"),
        "clip_to_below": get("clip_to_below"),
        "offset_x": get("offset_x"),
        "offset_y": get("offset_y"),
        "folder": folder,
        "source_meta": get("source_meta") or {},
    }


def walk_tree(repo: Any, tree_ref: str) -> Iterator[tuple[dict[str, Any], tuple[str | None, str]]]:
    """Every stored layer with ``(parent stable id, parent path)``."""

    def walk(ref: str, parent: tuple[str | None, str]):
        for layer in repo.store.read_json(ref, "tree").get("layers", []):
            yield layer, parent
            if layer.get("subtree_ref"):
                path = f"{parent[1]}/{layer['name']}" if parent[1] else layer["name"]
                yield from walk(layer["subtree_ref"], (layer.get("stable_source_id"), path))

    yield from walk(tree_ref, (None, ""))


def walk_imported(layers: list[Any]) -> Iterator[tuple[Any, tuple[str | None, str]]]:
    """Every ``ImportedLayer`` with ``(parent stable id, parent path)``."""

    def walk(items: list[Any], parent: tuple[str | None, str]):
        for layer in items:
            yield layer, parent
            if layer.children:
                path = f"{parent[1]}/{layer.name}" if parent[1] else layer.name
                yield from walk(layer.children, (layer.stable_source_id, path))

    yield from walk(layers, (None, ""))


def property_changes(
    old: Mapping[str, Any],
    new: Mapping[str, Any],
    *,
    metadata_changed: bool = False,
) -> list[tuple[str, str | None, str | None]]:
    """``(label, old, new)`` for every readable property that differs between
    two ``layer_props``, then ``(OTHER, None, None)`` when something without a
    readable form differs too. ``metadata_changed`` is the caller's verdict
    that the layer's metadata changed at all; if nothing readable explains it,
    it is reported as other properties."""
    changes: list[tuple[str, str | None, str | None]] = []

    def add(label: str, before: Any, after: Any) -> None:
        changes.append((label, before, after))

    if old.get("name") != new.get("name"):
        add("name", old.get("name"), new.get("name"))
    if old.get("kind") != new.get("kind"):
        add("kind", old.get("kind"), new.get("kind"))
    old_folder, new_folder = old.get("folder"), new.get("folder")
    if old_folder is not None and new_folder is not None and old_folder[0] != new_folder[0]:
        add("folder", old_folder[1] or TOP_LEVEL, new_folder[1] or TOP_LEVEL)
    if old.get("visible") != new.get("visible"):
        add("visible", _yes_no(old.get("visible")), _yes_no(new.get("visible")))
    if old.get("opacity") != new.get("opacity"):
        add("opacity", *opacity_texts(old.get("opacity"), new.get("opacity")))
    if old.get("blend_mode") != new.get("blend_mode"):
        add("blend mode", old.get("blend_mode"), new.get("blend_mode"))
    if bool(old.get("clip_to_below")) != bool(new.get("clip_to_below")):
        add("clipping", _on_off(old.get("clip_to_below")), _on_off(new.get("clip_to_below")))
    old_offset = (old.get("offset_x") or 0, old.get("offset_y") or 0)
    new_offset = (new.get("offset_x") or 0, new.get("offset_y") or 0)
    if old_offset != new_offset:
        add("offset", f"({old_offset[0]}, {old_offset[1]})", f"({new_offset[0]}, {new_offset[1]})")

    old_meta, new_meta = old.get("source_meta") or {}, new.get("source_meta") or {}
    shown: set[str] = set()
    for key, label in _META_LABELS.items():
        if key not in old_meta and key not in new_meta:
            continue
        before, after = old_meta.get(key), new_meta.get(key)
        if before == after or label in shown:
            continue
        shown.add(label)
        if key in {"text", "text_lines"}:
            add(label, _text(before), _text(after))
        elif isinstance(before, bool) or isinstance(after, bool):
            add(label, _on_off(before), _on_off(after))
        else:
            add(label, "-" if before is None else str(before), "-" if after is None else str(after))
    if _folder_toggled(old_meta, new_meta):
        add(FOLDER_TOGGLE, None, None)

    if _other_meta_changed(old_meta, new_meta) or (metadata_changed and not changes):
        add(OTHER, None, None)
    return changes


def _folder_toggled(old_meta: Mapping[str, Any], new_meta: Mapping[str, Any]) -> bool:
    """A folder was expanded or collapsed in the layer panel: MDP2's
    ``folderOpen``, or CLIP's ``LayerFolder`` changing in a bit other than
    bit 0 (the "is a folder" bit; tama.clip's folders switch between 17 and
    1, the other bits are not decoded, hence no direction is claimed)."""
    if "folder_open" in old_meta and "folder_open" in new_meta:
        return bool(old_meta["folder_open"]) != bool(new_meta["folder_open"])
    return _clip_folder_toggle(old_meta.get("folder"), new_meta.get("folder"))


def _clip_folder_toggle(before: Any, after: Any) -> bool:
    return (
        isinstance(before, int) and isinstance(after, int)
        and not isinstance(before, bool) and not isinstance(after, bool)
        and before & 1 == 1 and after & 1 == 1 and before != after
    )


def _yes_no(value: Any) -> str:
    if value is None:
        return "-"
    return "yes" if value else "no"


def _other_meta_changed(old_meta: Mapping[str, Any], new_meta: Mapping[str, Any]) -> bool:
    from .repo import PARTIAL_STAGE_SOURCE_META_KEYS

    for key in PARTIAL_STAGE_SOURCE_META_KEYS:
        if key in _META_LABELS or key in _META_COVERED:
            continue
        if key not in old_meta or key not in new_meta:
            # Older commits lack some keys (e.g. the CLIP payload digests);
            # a key only one side records is no evidence of a change.
            continue
        before, after = old_meta.get(key), new_meta.get(key)
        if key == "folder" and _clip_folder_toggle(before, after):
            continue  # shown as FOLDER_TOGGLE
        if key == "xml_attrs" and isinstance(before, Mapping) and isinstance(after, Mapping):
            before = {k: v for k, v in before.items() if k not in _XML_COVERED}
            after = {k: v for k, v in after.items() if k not in _XML_COVERED}
        if before != after:
            return True
    return False


def format_change(label: str, before: str | None, after: str | None) -> str:
    """One change as a line fragment: ``name: a -> b``, or ``<label>
    changed`` for a change without values (other properties, folder
    open/closed)."""
    if before is None and after is None:
        return f"{label} changed"
    return f"{label}: {before} -> {after}"
