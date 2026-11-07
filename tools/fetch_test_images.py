"""Fetch the external image corpora the image tests can use
(tests/test_rust_image_kernels.py, tests/test_raster.py,
tests/test_raster_formats.py).

    python tools/fetch_test_images.py              # into .test-images/ (gitignored)
    python tools/fetch_test_images.py --dest DIR

- ``pngsuite/``: Willem van Schaik's PngSuite (every PNG bit depth, colour
  type, interlacing, tRNS / gAMA / sBIT variants and corrupt files).
- ``pillow/``: the PNG, JPEG, GIF, BMP, TIFF, WebP and PSD files of Pillow
  12.3.0's own test suite (Tests/images in the sdist: CMYK and Adobe JPEGs,
  EXIF, progressive, truncated and corrupt files, APNGs, 16-bit PNGs, the
  bmpsuite subsets, TIFF compressions and layouts, animated GIF / WebP,
  PSD modes).
- ``bmpsuite/``: Jason Summers' BMP Suite 2.8 (good, questionable and bad
  BMPs of every header version, depth and compression).
- ``libtiff/``: libtiff 4.7.1's test images and the libtiff ``pics-3.8.0``
  collection (strips, tiles, compressions, photometrics, depths).

The tests read ``TMI_TEST_IMAGES`` (default ``.test-images`` at the repo
root) and skip the external corpora when it is missing. Archives are
extracted member by member; only image files are written, and a file that
is already there with the same bytes is left alone.
"""
from __future__ import annotations

import argparse
import io
import tarfile
import urllib.request
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
PNGSUITE_URL = "http://www.schaik.com/pngsuite/PngSuite-2017jul19.tgz"
PILLOW_URL = "https://files.pythonhosted.org/packages/source/p/pillow/pillow-12.3.0.tar.gz"
BMPSUITE_URL = "https://entropymine.com/jason/bmpsuite/releases/bmpsuite-2.8.zip"
LIBTIFF_URL = "https://download.osgeo.org/libtiff/tiff-4.7.1.tar.gz"
LIBTIFF_PICS_URL = "https://download.osgeo.org/libtiff/pics-3.8.0.tar.gz"
IMAGE_SUFFIXES = {
    ".png", ".jpg", ".jpeg", ".mpo", ".gif", ".bmp", ".dib", ".tif", ".tiff", ".webp", ".psd",
}


def _fetch(url: str) -> bytes:
    print(f"downloading {url}")
    with urllib.request.urlopen(url, timeout=120) as response:
        return response.read()


def _write(target: Path, data: bytes) -> bool:
    """Write ``data`` unless ``target`` already holds it (the corpus may be
    hard-linked from another checkout)."""
    if target.is_file() and target.read_bytes() == data:
        return False
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_bytes(data)
    return True


def _members(archive: bytes):
    """(name, bytes) of every regular file of a .tar.gz or .zip."""
    if archive[:2] == b"PK":
        with zipfile.ZipFile(io.BytesIO(archive)) as z:
            for info in z.infolist():
                if not info.is_dir():
                    yield info.filename, z.read(info)
        return
    with tarfile.open(fileobj=io.BytesIO(archive), mode="r:gz") as tar:
        for member in tar.getmembers():
            if not member.isfile():
                continue
            source = tar.extractfile(member)
            if source is not None:
                yield member.name, source.read()


def _extract_images(archive: bytes, dest: Path, prefix: str = "") -> int:
    """Write the image members under ``prefix`` of a .tar.gz / .zip into
    ``dest`` (flattened to their path below ``prefix``); never outside
    ``dest``."""
    count = 0
    dest.mkdir(parents=True, exist_ok=True)
    for name, data in _members(archive):
        name = name.replace("\\", "/")
        if prefix:
            if f"/{prefix}/" not in f"/{name}":
                continue
            name = name.split(f"{prefix}/", 1)[1]
        rel = Path(name)
        if rel.suffix.lower() not in IMAGE_SUFFIXES or rel.is_absolute() or ".." in rel.parts:
            continue
        _write(dest / rel, data)
        count += 1
    return count


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--dest", type=Path, default=ROOT / ".test-images")
    args = parser.parse_args(argv)
    n = _extract_images(_fetch(PNGSUITE_URL), args.dest / "pngsuite")
    print(f"pngsuite: {n} files")
    n = _extract_images(_fetch(PILLOW_URL), args.dest / "pillow", prefix="Tests/images")
    print(f"pillow: {n} files")
    n = _extract_images(_fetch(BMPSUITE_URL), args.dest / "bmpsuite", prefix="bmpsuite-2.8")
    print(f"bmpsuite: {n} files")
    n = _extract_images(_fetch(LIBTIFF_URL), args.dest / "libtiff" / "test", prefix="test/images")
    n += _extract_images(_fetch(LIBTIFF_PICS_URL), args.dest / "libtiff" / "pics", prefix="libtiffpic")
    print(f"libtiff: {n} files")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
