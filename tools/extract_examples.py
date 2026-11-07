"""Unpack the comic-page zips in example/additional_examples for the render corpus.

    python tools/extract_examples.py                    # every *.zip not yet unpacked
    python tools/extract_examples.py --examples D:/x/example --list

Each ``additional_examples/<name>.zip`` goes into its own new directory
``additional_examples/extracted/<name>/`` (``example/`` is gitignored). The
archives are data only: nothing in them is run or imported. A member is
written only if its path stays inside that directory (no absolute paths, no
``..``, no drive letters); links and other special entries are refused. A
zip is unpacked into a temporary directory first and renamed into place, so
a directory that exists is complete; it is left alone (``--force`` replaces
it).
"""
from __future__ import annotations

import argparse
import shutil
import stat
import sys
import zipfile
from pathlib import Path, PurePosixPath

ROOT = Path(__file__).resolve().parent.parent
CHUNK = 1 << 20


def _safe_parts(name: str) -> tuple[str, ...] | None:
    """The member's path components, or None if it could leave the target."""
    if not name or "\\" in name or ":" in name or name.startswith("/"):
        return None
    parts = PurePosixPath(name).parts
    if any(part in ("", ".", "..") for part in parts):
        return None
    return parts


def extract_zip(archive: Path, dest: Path) -> list[Path]:
    """Unpack ``archive`` into the new directory ``dest``; the files written."""
    tmp = dest.with_name(dest.name + ".partial")
    if tmp.exists():
        shutil.rmtree(tmp)
    tmp.mkdir(parents=True)
    written: list[Path] = []
    try:
        with zipfile.ZipFile(archive) as zf:
            for info in zf.infolist():
                parts = _safe_parts(info.filename)
                if parts is None:
                    raise ValueError(f"{archive.name}: refusing member {info.filename!r}")
                mode = info.external_attr >> 16
                if mode and stat.S_ISLNK(mode):
                    raise ValueError(f"{archive.name}: refusing link {info.filename!r}")
                target = tmp.joinpath(*parts)
                if info.is_dir():
                    target.mkdir(parents=True, exist_ok=True)
                    continue
                target.parent.mkdir(parents=True, exist_ok=True)
                with zf.open(info) as src, open(target, "xb") as out:
                    shutil.copyfileobj(src, out, CHUNK)
                written.append(dest.joinpath(*parts))
        tmp.rename(dest)
    except BaseException:
        shutil.rmtree(tmp, ignore_errors=True)
        raise
    return written


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--examples", type=Path, default=ROOT / "example", help="the example directory")
    parser.add_argument("--force", action="store_true", help="replace directories already unpacked")
    parser.add_argument("--list", action="store_true", help="only list the archives and their state")
    args = parser.parse_args(argv)

    source = args.examples / "additional_examples"
    out_root = source / "extracted"
    archives = sorted(source.glob("*.zip"))
    if not archives:
        print(f"no zips in {source}")
        return 0
    status = 0
    for archive in archives:
        dest = out_root / archive.stem
        if args.list:
            print(f"{archive.name:24} {'unpacked' if dest.is_dir() else 'not unpacked'}")
            continue
        if dest.exists():
            if not args.force:
                print(f"{archive.name}: already in {dest.relative_to(args.examples)}")
                continue
            shutil.rmtree(dest)
        try:
            files = extract_zip(archive, dest)
        except (OSError, ValueError, zipfile.BadZipFile) as exc:
            print(f"{archive.name}: {exc}", file=sys.stderr)
            status = 1
            continue
        clips = [f for f in files if f.suffix.lower() == ".clip"]
        print(f"{archive.name}: {len(files)} files ({len(clips)} .clip) -> {dest.relative_to(args.examples)}")
    return status


if __name__ == "__main__":
    raise SystemExit(main())
