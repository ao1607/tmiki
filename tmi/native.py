from __future__ import annotations

from pathlib import Path

# The CLIP / MDP2 modules (and Pillow and numpy with them) are imported by
# the branches that use them: a PNG / JPEG write-back needs neither.


def replace_layer_pixels(
    path: Path,
    source_format: str,
    layer_selector: str,
    png_bytes: bytes,
    *,
    invalidate_preview_on_noop: bool = False,
) -> bytes:
    suffix = path.suffix.lower()
    fmt = source_format.lower()
    if fmt == "clip" or suffix == ".clip":
        from .formats import clip

        return clip.replace_layer_pixels(
            path,
            layer_selector,
            png_bytes,
            invalidate_preview_on_noop=invalidate_preview_on_noop,
        )
    if fmt == "mdp2" or suffix == ".mdp":
        from .formats import mdp2

        return mdp2.replace_layer_pixels(path, layer_selector, png_bytes)
    if fmt in {"png", "jpeg"} or suffix in {".png", ".jpg", ".jpeg"}:
        # A raster file *is* its single layer: write the full-size image, not
        # the cropped layer blob, in the format of the file at ``path`` (a
        # .jpg must not end up holding PNG bytes) and with that file's
        # metadata and colour mode (issue #21).
        from .image_ops import encode_raster_like, pixels_from_bytes

        jpeg = fmt == "jpeg" or (fmt not in {"png", "jpeg"} and suffix in {".jpg", ".jpeg"})
        original = path.read_bytes() if path.is_file() else None
        return encode_raster_like(pixels_from_bytes(png_bytes), original, "jpeg" if jpeg else "png")
    raise ValueError(f"native layer write-back is not implemented for {source_format}")


def render_folder(path: Path, source_format: str, folder_selector: str):
    """Composite a folder's contents (blend modes, clipping, masks) on a
    transparent canvas, as the source app would show that folder alone
    (CLIP: tmi_accel's renderer, ``clip.render_folder``)."""
    suffix = path.suffix.lower()
    fmt = source_format.lower()
    if fmt == "clip" or suffix == ".clip":
        from .formats import clip

        data = clip.open_clip(path)
        node = clip.find_layer_node(data, folder_selector)
        if node is None or not getattr(node, "folder", 0):
            raise ValueError(f"not a folder: {folder_selector}")
        return clip.render_folder(data, node.main_id)
    if fmt == "mdp2" or suffix == ".mdp":
        import dataclasses

        from .formats import mdp2

        data = mdp2.open_mdp2(path)
        folder = mdp2._find_layer_by_selector(data, folder_selector)
        if folder is None or not folder.children:
            raise ValueError(f"not a folder: {folder_selector}")
        by_id = {layer.layer_id: layer for layer in data.layers}

        def inside(layer) -> bool:
            seen = set()
            cur = by_id.get(layer.parent_id)
            while cur is not None and cur.layer_id not in seen:
                if cur.layer_id == folder.layer_id:
                    return True
                seen.add(cur.layer_id)
                cur = by_id.get(cur.parent_id)
            return False

        subset = [layer for layer in data.layers if inside(layer)]
        return mdp2.render_composite_rgba(dataclasses.replace(data, layers=subset), background=(0, 0, 0, 0))
    raise ValueError(f"folders are not supported for {source_format}")
