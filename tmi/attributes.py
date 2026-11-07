"""`.tmiattributes` (PLUGINS.md 4.1): which reader a file goes to.

The same rules as the Rust side (`tmi_repo::attributes`): one pattern and
its attributes per line, `#` starts a comment, `.tmiignore`'s matching but
case-insensitive in ASCII on every OS, the last matching line wins per
attribute. `format=plugin:<id>` files are read by format plugins, which run
in the tmi executable only: :func:`check_import` refuses them here rather
than record them another way. `format=binary` files are opaque blobs.
"""

from __future__ import annotations

import fnmatch
import re
from pathlib import Path, PurePosixPath

FILE = ".tmiattributes"
PLUGIN_DIR = ".tmiplugins"
_ID = re.compile(r"[a-z0-9-]{1,40}")
_UPPER = str.maketrans("ABCDEFGHIJKLMNOPQRSTUVWXYZ", "abcdefghijklmnopqrstuvwxyz")


class PluginFormatError(RuntimeError):
    """A file `.tmiattributes` maps to a format plugin, imported in Python."""


def fold(text: str) -> str:
    return text.replace("\\", "/").translate(_UPPER)


def _matches(rel: str, pattern: str) -> bool:
    try:
        if PurePosixPath(rel).match(pattern):
            return True
    except ValueError:
        pass
    if fnmatch.fnmatchcase(rel, pattern):
        return True
    return fnmatch.fnmatchcase(rel.rsplit("/", 1)[-1], pattern)


def parse(text: str) -> list[tuple[str, str | None]]:
    """[(folded pattern, format value or None)] in order."""
    rules: list[tuple[str, str | None]] = []
    for raw in text.splitlines():
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        words = []
        for word in line.split():
            if word.startswith("#"):
                break
            words.append(word)
        if not words:
            continue
        fmt = None
        for word in words[1:]:
            key, _, value = word.partition("=")
            if key == "format":
                fmt = value
        rules.append((fold(words[0]), fmt))
    return rules


def lookup(rules: list[tuple[str, str | None]], rel: str) -> str:
    """The format of ``rel``: "builtin", "binary", "plugin:<id>" or the text
    of a value tmi does not know."""
    path = fold(rel)
    if path == fold(FILE) or path.startswith(fold(PLUGIN_DIR) + "/"):
        return "builtin"
    fmt = "builtin"
    for pattern, value in rules:
        if value is not None and _matches(path, pattern):
            fmt = value
    return fmt


_CACHE: dict[str, tuple[float, int, list[tuple[str, str | None]]]] = {}


def _repo_root(path: Path) -> Path | None:
    for parent in path.parents:
        if (parent / ".tmi").is_dir():
            return parent
    return None


def format_for(path: Path) -> tuple[str, str]:
    """(format, rel) of the working file ``path`` in its repository."""
    try:
        full = path.resolve()
    except OSError:
        return "builtin", path.name
    root = _repo_root(full)
    if root is None:
        return "builtin", path.name
    attrs = root / FILE
    try:
        st = attrs.stat()
    except OSError:
        return "builtin", path.name
    key = str(attrs)
    cached = _CACHE.get(key)
    if cached is None or cached[0] != st.st_mtime or cached[1] != st.st_size:
        rules = parse(attrs.read_bytes().decode("utf-8", errors="replace"))
        _CACHE[key] = (st.st_mtime, st.st_size, rules)
    else:
        rules = cached[2]
    try:
        rel = full.relative_to(root).as_posix()
    except ValueError:
        return "builtin", path.name
    return lookup(rules, rel), rel


def component_id(rel: str) -> str | None:
    """The plugin id of a component path ``.tmiplugins/<id>.wasm``."""
    prefix = PLUGIN_DIR + "/"
    if not rel.startswith(prefix) or not rel.endswith(".wasm"):
        return None
    plugin = rel[len(prefix):-len(".wasm")]
    return plugin if _ID.fullmatch(plugin) else None


def removed_component_warnings(repo, paths) -> list[str]:
    """Issue #50: for each of ``paths`` (just removed, or staged by ``add`` /
    ``commit -a``) that is a component ``.tmiplugins/<id>.wasm`` no longer
    tracked while the working tree's ``.tmiattributes`` still maps files to
    plugin ``<id>``: the warning that those files cannot be added without it.
    The same words as ``tmi_repo::formats::removed_component_warnings``."""
    ids = [(rel, component_id(rel)) for rel in paths]
    if not any(plugin for _rel, plugin in ids):
        return []
    head = repo.entries_for_commit(repo.current_commit())
    index = repo.read_index().get("entries", {})
    root = repo.worktree
    try:
        text = (root / FILE).read_bytes().decode("utf-8", errors="replace") if root is not None else ""
    except OSError:
        text = ""
    rules = parse(text)
    out = []
    for rel, plugin in ids:
        if plugin is None:
            continue
        entry = index.get(rel)
        tracked = not entry.get("deleted") if entry is not None else rel in head
        if tracked or not any(value == f"plugin:{plugin}" for _pattern, value in rules):
            continue
        count = 0
        for path in sorted(set(head) | set(index)):
            e = index.get(path) or head.get(path)
            if e is not None and e.get("deleted"):
                continue
            if lookup(rules, path) == f"plugin:{plugin}":
                count += 1
        out.append(
            f"warning: {FILE} still maps files to plugin '{plugin}' ({count} tracked {'file' if count == 1 else 'files'}); "
            f"without {rel} they cannot be added or committed. To stop using the plugin, run 'tmiki plugin remove {plugin}'"
        )
    return out


def check_import(path: Path) -> str:
    """Before Python imports ``path``: "builtin" or "binary", or
    :class:`PluginFormatError` for a plugin format (never recorded here)."""
    fmt, rel = format_for(path)
    if fmt.startswith("plugin:"):
        plugin = fmt.removeprefix("plugin:")
        raise PluginFormatError(
            f"{rel}: {FILE} maps it to plugin '{plugin}'; format plugins run in the tmi executable only "
            "(python -m tmi does not run them)"
        )
    if fmt not in ("builtin", "binary"):
        raise RuntimeError(f"{rel}: {FILE}: format={fmt} (use plugin:<id>, binary or builtin)")
    return fmt

DEV_ENV = "TMI_PLUGIN_DEV"
DEV_COMPONENT = "dev"


def is_dev_record(entry: object) -> bool:
    """Whether a record was made by a development plugin (``tmiki plugin dev``):
    ``importer.component_sha256`` is ``"dev"``."""
    importer = entry.get("importer") if isinstance(entry, dict) else None
    return isinstance(importer, dict) and importer.get("component_sha256") == DEV_COMPONENT


def refuse_dev_records(items) -> None:
    """PLUGINS.md 5: no commit (merge commit, stash) holds a record a
    development plugin made. ``items``: (path, entry) pairs. The same words
    as ``tmi_repo::formats::refuse_dev_records``."""
    dev = sorted({path for path, entry in items if is_dev_record(entry)})
    if not dev:
        return
    listed = ", ".join(dev[:5])
    if len(dev) > 5:
        listed += f", +{len(dev) - 5} more"
    them = "it" if len(dev) == 1 else "them"
    raise RuntimeError(
        f"{listed}: recorded with a development plugin (tmiki plugin dev); a commit cannot hold such records. "
        f"Add {them} again without {DEV_ENV} first"
    )
