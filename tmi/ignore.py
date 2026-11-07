"""`.tmiignore` parser and matcher.

Subset of `.gitignore` syntax extended with a layer selector. Each non-empty,
non-comment line is one of:

    pattern              file-level ignore (gitignore-ish)
    !pattern             un-ignore a previously ignored path
    pattern:layer        ignore a specific layer inside files matching `pattern`

File pattern grammar (intentionally small):
    *       any chars except path separators
    **      zero or more path components
    ?       single char
"""

from __future__ import annotations

import fnmatch
from dataclasses import dataclass
from pathlib import Path, PurePosixPath
from typing import Iterable


@dataclass(frozen=True)
class IgnorePattern:
    kind: str          # "file" | "layer"
    negate: bool       # leading "!"
    file_glob: str
    layer_glob: str | None = None

    def matches_file(self, rel: str) -> bool:
        if self.kind != "file":
            return False
        return _match_path(rel, self.file_glob)

    def matches_layer(self, rel: str, layer_name: str) -> bool:
        if self.kind != "layer":
            return False
        if not _match_path(rel, self.file_glob):
            return False
        if self.layer_glob is None:
            return False
        return fnmatch.fnmatch(layer_name, self.layer_glob)


def parse_lines(lines: Iterable[str]) -> list[IgnorePattern]:
    patterns: list[IgnorePattern] = []
    for raw in lines:
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        negate = False
        if line.startswith("!"):
            negate = True
            line = line[1:]
        file_part, layer_part = _split_layer(line)
        if layer_part is None:
            patterns.append(IgnorePattern(kind="file", negate=negate, file_glob=file_part))
        else:
            patterns.append(
                IgnorePattern(
                    kind="layer",
                    negate=negate,
                    file_glob=file_part,
                    layer_glob=layer_part,
                )
            )
    return patterns


def parse_file(path: Path) -> list[IgnorePattern]:
    if not path.exists():
        return []
    return parse_lines(path.read_text(encoding="utf-8").splitlines())


def is_file_ignored(rel: str, patterns: list[IgnorePattern]) -> bool:
    decided = False
    for pat in patterns:
        if pat.kind != "file":
            continue
        if pat.matches_file(rel):
            decided = not pat.negate
    return decided


def layer_patterns_for(rel: str, patterns: list[IgnorePattern]) -> list[IgnorePattern]:
    return [p for p in patterns if p.kind == "layer" and _match_path(rel, p.file_glob)]


def is_layer_ignored(rel: str, layer_name: str, patterns: list[IgnorePattern]) -> bool:
    decided = False
    for pat in patterns:
        if pat.matches_layer(rel, layer_name):
            decided = not pat.negate
    return decided


def file_globs(patterns: list[IgnorePattern]) -> list[str]:
    """Return just the file-level glob strings (for legacy code using fnmatch directly)."""
    return [p.file_glob for p in patterns if p.kind == "file" and not p.negate]


def _split_layer(line: str) -> tuple[str, str | None]:
    if ":" not in line:
        return line, None
    file_part, _, layer_part = line.rpartition(":")
    if not file_part or not layer_part:
        return line, None
    return file_part, layer_part


def _match_path(rel: str, pattern: str) -> bool:
    rel_posix = rel.replace("\\", "/")
    pat = pattern.replace("\\", "/")
    if PurePosixPath(rel_posix).match(pat):
        return True
    if fnmatch.fnmatch(rel_posix, pat):
        return True
    name = rel_posix.rsplit("/", 1)[-1]
    if fnmatch.fnmatch(name, pat):
        return True
    return False
