"""Forged records for the tests of what tmi accepts from a remote.

A hostile remote writes tree objects tmi's own commands would never write:
``forge_branch`` puts a commit whose tree records the given paths (whatever
they are) on a branch of an existing repository, working or bare, by writing
the objects directly, as a server would. Nothing here goes through tmi.
"""
from __future__ import annotations

import hashlib
import json
import zlib
from pathlib import Path


def _dumps(value) -> bytes:
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode("utf-8")


def forge_branch(repo: Path, files: list[tuple[str, bytes]], branch: str = "evil", base: str = "main") -> str:
    """Add a commit on ``branch`` (made from ``base``'s head) whose tree is
    ``base``'s plus an opaque-blob entry for every ``(path, content)`` in
    ``files``. ``repo`` is a working tree or a bare repository directory.
    Returns the commit id."""
    repo_dir = repo / ".tmi" if (repo / ".tmi").is_dir() else repo
    objects = repo_dir / "objects"

    def put(kind: str, body: bytes) -> str:
        raw = kind.encode() + b"\x00" + body
        oid = hashlib.sha256(raw).hexdigest()
        path = objects / oid[:2] / oid[2:]
        if not path.exists():
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(zlib.compress(raw))
        return oid

    def get(oid: str) -> dict:
        raw = zlib.decompress((objects / oid[:2] / oid[2:]).read_bytes())
        return json.loads(raw.split(b"\x00", 1)[1])

    head = (repo_dir / "refs" / "heads" / base).read_text(encoding="utf-8").strip()
    parent = get(head)
    entries = list(get(parent["tree_ref"])["entries"])
    for path, data in files:
        name = path.replace("\\", "/").rsplit("/", 1)[-1]
        blob = put("blob", data)
        layers = put("tree", _dumps({
            "kind": "layer-tree", "version": 1,
            "layers": [{
                "blend_mode": "normal", "blob_ref": None, "clip_to_below": False, "kind": "binary",
                "layer_id": "blob:0", "mask_ref": None, "name": name, "offset_x": 0, "offset_y": 0,
                "opacity": 1.0, "pixel_bbox": None, "pixel_sha256": None,
                "source_meta": {"byte_size": len(data), "source_path_name": name},
                "stable_source_id": "blob:0", "subtree_ref": None, "visible": True,
            }],
        }))
        entries.append({
            "canvas_meta": {"color_profile": None, "dpi": None, "height": None, "source_format": "binary", "width": None},
            "composite_ref": None, "path": path, "source_blob_ref": blob,
            "source_digest": hashlib.sha256(data).hexdigest(), "source_format": "binary",
            "source_meta": {"byte_size": len(data), "write_capability": "opaque-blob"},
            "thumbnail_ref": None, "tree_ref": layers,
        })
    tree = put("tree", _dumps({"entries": entries, "kind": "repository", "version": 1}))
    commit = dict(parent)
    commit.update(parents=[head], tree_ref=tree, message="forged")
    oid = put("commit", _dumps(commit))
    ref = repo_dir / "refs" / "heads" / branch
    ref.parent.mkdir(parents=True, exist_ok=True)
    ref.write_text(oid + "\n", encoding="utf-8", newline="\n")
    return oid


def store_has(repo: Path, oid: str) -> bool:
    repo_dir = repo / ".tmi" if (repo / ".tmi").is_dir() else repo
    return (repo_dir / "objects" / oid[:2] / oid[2:]).exists()
