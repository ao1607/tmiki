"""Append-only safety-net log for HEAD and branch ref movements.

Records every time HEAD or a branch ref moves, so users can recover from
mistakes (amend over the wrong commit, branch -D, switch + reset, ...) for at
least 90 days even when refs themselves no longer point at the lost commit.

The format is line-based, similar to git's reflog but with ISO timestamps:

    <old_oid> <new_oid> <name> <email> <iso8601_utc>\\t<reason>\\n

`<old_oid>` is 64 zeros when the ref is being created.
"""

from __future__ import annotations

from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Iterator


ZERO_OID = "0" * 64


@dataclass(frozen=True)
class ReflogEntry:
    old_oid: str
    new_oid: str
    name: str
    email: str
    timestamp: str
    reason: str

    @property
    def is_creation(self) -> bool:
        return self.old_oid == ZERO_OID


def _log_path(repo_dir: Path, ref_name: str) -> Path:
    if ref_name == "HEAD":
        return repo_dir / "logs" / "HEAD"
    return repo_dir / "logs" / ref_name


def append(
    repo_dir: Path,
    ref_name: str,
    old_oid: str | None,
    new_oid: str,
    identity: dict[str, str] | None,
    reason: str,
) -> None:
    """Append one entry to the reflog for `ref_name`.

    `ref_name` is either "HEAD" or a ref like "refs/heads/main".
    `old_oid=None` is treated as creation (zeros).
    `identity` may be None (e.g. before user has configured identity).
    """
    path = _log_path(repo_dir, ref_name)
    path.parent.mkdir(parents=True, exist_ok=True)
    name = (identity or {}).get("name") or "tmi"
    email = (identity or {}).get("email") or ""
    old = old_oid or ZERO_OID
    timestamp = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    safe_reason = reason.replace("\n", " ").replace("\t", " ")
    line = f"{old} {new_oid} {name} <{email}> {timestamp}\t{safe_reason}\n"
    with path.open("a", encoding="utf-8") as handle:
        handle.write(line)


def read(repo_dir: Path, ref_name: str) -> list[ReflogEntry]:
    path = _log_path(repo_dir, ref_name)
    if not path.exists():
        return []
    out: list[ReflogEntry] = []
    for raw in path.read_text(encoding="utf-8").splitlines():
        entry = _parse_line(raw)
        if entry is not None:
            out.append(entry)
    return out


def prune(repo_dir: Path, max_age_days: int) -> int:
    """Drop reflog entries older than `max_age_days`.

    `max_age_days <= 0` truncates everything. Returns the number of removed entries.
    """
    logs_dir = repo_dir / "logs"
    if not logs_dir.is_dir():
        return 0
    cutoff = datetime.now(timezone.utc).timestamp() - max_age_days * 86400
    removed = 0
    # Snapshot the file list — we unlink emptied logs as we go, and on Windows
    # mutating the directory mid-walk can skip or duplicate entries.
    for entry_path in list(logs_dir.rglob("*")):
        if not entry_path.is_file():
            continue
        kept: list[str] = []
        for raw in entry_path.read_text(encoding="utf-8").splitlines():
            entry = _parse_line(raw)
            if entry is None:
                continue
            try:
                ts = datetime.strptime(entry.timestamp, "%Y-%m-%dT%H:%M:%SZ").replace(tzinfo=timezone.utc)
            except ValueError:
                kept.append(raw)
                continue
            if ts.timestamp() >= cutoff:
                kept.append(raw)
            else:
                removed += 1
        if kept:
            entry_path.write_text("\n".join(kept) + "\n", encoding="utf-8")
        else:
            entry_path.unlink()
    return removed


def iter_all_reachable_oids(repo_dir: Path) -> Iterator[str]:
    """Yield every old/new oid recorded across all reflogs.

    Used by gc to keep reflog-referenced commits alive.
    """
    logs_dir = repo_dir / "logs"
    if not logs_dir.is_dir():
        return
    for entry_path in logs_dir.rglob("*"):
        if not entry_path.is_file():
            continue
        for raw in entry_path.read_text(encoding="utf-8").splitlines():
            entry = _parse_line(raw)
            if entry is None:
                continue
            if entry.old_oid != ZERO_OID:
                yield entry.old_oid
            yield entry.new_oid


def resolve_at_spec(repo_dir: Path, spec: str) -> str | None:
    """Resolve a HEAD@{N} style spec to an oid.

    Returns None if the spec is malformed or N is out of range.
    """
    if not spec.startswith("HEAD@{") or not spec.endswith("}"):
        return None
    try:
        index = int(spec[len("HEAD@{") : -1])
    except ValueError:
        return None
    entries = read(repo_dir, "HEAD")
    if index < 0 or index >= len(entries):
        return None
    # HEAD@{0} = most recent entry's new_oid (i.e. current HEAD)
    return entries[-(index + 1)].new_oid


def _parse_line(raw: str) -> ReflogEntry | None:
    if not raw.strip():
        return None
    if "\t" not in raw:
        return None
    head, _, reason = raw.partition("\t")
    parts = head.split(" ")
    if len(parts) < 5:
        return None
    old, new, *middle = parts
    timestamp = middle[-1]
    email_tok = middle[-2]
    name_tokens = middle[:-2]
    name = " ".join(name_tokens)
    email = email_tok.strip("<>")
    return ReflogEntry(
        old_oid=old,
        new_oid=new,
        name=name,
        email=email,
        timestamp=timestamp,
        reason=reason,
    )
