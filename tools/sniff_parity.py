"""tmi's "is it an image?" rule, Rust against Python, and against Pillow (PLUGINS.md P0e).

A file is recorded as a raster image exactly when tmi's own decoders read it
(a recognised signature and a full decode with no decline), else as an opaque
blob, whatever its extension says. Pillow takes no part in that verdict. This
checks, over a corpus, that

* the Rust import (``tmi-conform sniff``: ``tmi_import::import_file``) and the
  Python import (``tmi.importers.import_file``) give the same verdict for every
  file: 0 may differ;
* every file tmi calls an image is one Pillow also opens, loads and converts
  to RGBA (tmi never reads what Pillow refuses);

and reports how many files Pillow opens that tmi stores as opaque blobs (the
formats and variants tmi does not decode: TGA, PCX, ICO, TIFF in YCbCr or
LAB, damaged JPEGs, ...). The corpus:

* files Pillow writes in every format and mode it saves, and headers made
  for the formats it only reads (FLI, FITS, GBR, IMT, IPTC, PCD, ...);
* the images under example/ and .test-images/;
* files that are not images: the repository's own files, the build's
  binaries under rust/target, the venv's packages, random bytes and texts
  that begin like an image header.

    python tools/sniff_parity.py [--conform PATH] [--max-files N] [--json PATH]
"""
from __future__ import annotations

import argparse
import io
import json
import os
import random
import subprocess
import sys
import tempfile
import warnings
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))
EXE = ".exe" if os.name == "nt" else ""


def pillow_files(out: Path) -> list[Path]:
    """Every format Pillow saves, from a few images in several modes."""
    from PIL import Image

    Image.init()
    rng = random.Random(7)
    base = Image.new("RGBA", (37, 23))
    base.putdata([(rng.randrange(256), rng.randrange(256), rng.randrange(256), rng.randrange(256)) for _ in range(37 * 23)])
    images = {
        "RGBA": base, "RGB": base.convert("RGB"), "L": base.convert("L"), "P": base.convert("RGB").quantize(16),
        "1": base.convert("1"), "LA": base.convert("LA"), "I;16": base.convert("I;16"), "F": base.convert("F"),
        "CMYK": base.convert("CMYK"),
    }
    files = []
    for fmt in sorted(Image.SAVE):
        for mode, image in images.items():
            buf = io.BytesIO()
            try:
                image.save(buf, format=fmt)
            except Exception:
                continue
            path = out / f"pillow-{fmt}-{mode.replace(';', '_')}.bin"
            path.write_bytes(buf.getvalue())
            files.append(path)
    return files


def crafted_files(out: Path) -> list[Path]:
    """Headers of formats Pillow only reads (and near misses), followed by
    zeros or random bytes."""
    rng = random.Random(11)
    heads = [
        b"\0\0\0\0\xaf\x11\0\0\0\0\0\0\0\0\0\0", b"SIMPLE  =                    T", b"FTEX", b"\0\0\0\x1c\0\0\0\x01",
        b"\0\0\0\x1c\0\0\0\x02", b"width 4\nheight 4\npixel n8\n\x0c", b"\x1c\x03\x3c\0\x02", b"\0\0\0\0\0\0\0\x04",
        b"\0\0\x01\xb3", b"\x80\xe8\0\0", b"\x59\xa6\x6a\x95", b"/* XPM */\nstatic char", b"P7 332\n", b"\0\0\x02\0",
        b"\xb1\x68\xde\x3a", b"BLP1", b"BLP2", b"BUFR", b"ZCZC", b"DDS ", b"%!PS-Adobe", b"\xc5\xd0\xd3\xc6",
        b"GIF87a", b"GRIB\0\0\0\x01", b"\x89HDF\r\n\x1a\n", b"icns", b"\0\0\x01\0", b"MM\0*", b"II+\0", b"DanM",
        b"LinS", b"8BPS", b"qoif", b"\x01\xda", b"RIFF\0\0\0\0WEBPVP8L", b"RIFF\0\0\0\0WEBPVP8 ", b"\xd7\xcd\xc6\x9a\0\0",
        b"\x01\0\0\0", b"  #define x_width 4\n", b"P4\n4 4\n", b"Pf\n", b"\n\x05\x01", b"(\0\0\0", b"\x0c\0\0\0",
        b"\0\0\0\x0cjP  \r\n\x87\n", b"\xff\x4f\xff\x51", b"\0\0\0\x18ftypavif", b"\0\0\0\x18ftypmif1",
        b"Image type: RGB image\r\nName: x\r\n\x1a", b"File size: 1\nImage size (x*y): 4*4\n\x1a",
        b"Note: just a text file\n", b"\0\x01\x02\0\0\0\0\0\0\0\0\0\x04\0\x04\0\x20\0",
    ]
    files = []
    for i, head in enumerate(heads):
        for j, tail in enumerate([b"", bytes(64), bytes(rng.randrange(256) for _ in range(3000))]):
            path = out / f"crafted-{i:02d}-{j}.bin"
            path.write_bytes(head + tail)
            files.append(path)
    pcd = bytearray(rng.randrange(256) for _ in range(4096))
    pcd[2048:2052] = b"PCD_"
    (out / "crafted-pcd.bin").write_bytes(bytes(pcd))
    files.append(out / "crafted-pcd.bin")
    import struct

    for be in (True, False):  # SPIDER: 27 floats, iform 1, labrec * lenbyt == labbyt
        h = [0.0] * 27
        h[0], h[1], h[4], h[11], h[12], h[21], h[22] = 4, 4, 1, 4, 1, 1024, 1024
        data = struct.pack((">" if be else "<") + "27f", *h) + bytes(2000)
        path = out / f"crafted-spider-{'be' if be else 'le'}.bin"
        path.write_bytes(data)
        files.append(path)
    return files


def other_files(out: Path) -> list[Path]:
    """Random bytes and texts that are not images (some begin like one)."""
    rng = random.Random(13)
    files = []
    texts = [
        "", "\n", "hello world\n", "# notes\n\nsome text\n", "Plan: draw more\n", "P.S. later\n",
        "{\"a\": 1}\n", "<svg xmlns='http://www.w3.org/2000/svg'/>\n", "MZ", "PK\x03\x04", "%PDF-1.7\n", "\x1a\x1a",
        "BN", "GIF88a", "RIFF\0\0\0\0WAVEfmt ", "\0" * 200, " " * 50 + "\n", "Name:" + "x" * 200 + "\n",
    ]
    for i, t in enumerate(texts):
        path = out / f"text-{i:02d}.txt"
        path.write_bytes(t.encode("latin-1"))
        files.append(path)
    for i in range(3000):
        n = rng.choice([1, 2, 3, 5, 8, 16, 17, 18, 100, 120, 200, 1000, 2100, 5000])
        path = out / f"random-{i:04d}.bin"
        path.write_bytes(bytes(rng.randrange(256) for _ in range(n)))
        files.append(path)
    return files


def existing_files(max_files: int) -> dict[str, list[Path]]:
    groups: dict[str, list[Path]] = {}
    tracked = subprocess.run(["git", "ls-files"], cwd=ROOT, capture_output=True, text=True, check=True).stdout.splitlines()
    groups["repository"] = [ROOT / p for p in tracked if (ROOT / p).is_file()]
    groups["images"] = sorted(p for d in ("example", ".test-images") for p in (ROOT / d).rglob("*") if p.is_file())
    target = ROOT / "rust" / "target" / "release"
    build = sorted(p for p in target.rglob("*") if p.is_file() and p.stat().st_size < 20 << 20) if target.is_dir() else []
    groups["build"] = build[:: max(1, len(build) // max_files)][:max_files]
    site = Path(sys.prefix) / ("Lib" if os.name == "nt" else "lib")
    packages = sorted(p for p in site.rglob("*") if p.is_file() and p.stat().st_size < 20 << 20)
    groups["python"] = packages[:: max(1, len(packages) // max_files)][:max_files]
    return groups


def python_verdict(data: bytes) -> str:
    """What ``tmi.importers.import_file`` records for these bytes under the
    name ``sniffed.bin``: "image", "opaque", or "declined" (an image over
    core.maxImagePixels raises an error: it is not opaque, and the Rust import
    declines it)."""
    from tmi.importers import import_file
    from tmi.pixel_limit import CanvasTooLargeError

    try:
        imported = import_file(Path("sniffed.bin"), render_composite=False, source=data, layer_blobs=frozenset())
    except CanvasTooLargeError:
        return "declined"
    return "opaque" if imported.source_format == "binary" else "image"


def pillow_opens(data: bytes) -> str | None:
    """Pillow's format name when it opens, loads and converts to RGBA these
    bytes, else None."""
    from PIL import Image

    try:
        with warnings.catch_warnings():
            warnings.simplefilter("ignore")
            image = Image.open(io.BytesIO(data))
            name = image.format or "?"
            image.convert("RGBA")
        return name
    except Exception:
        return None


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--conform", type=Path, default=ROOT / "rust" / "target" / "release" / f"tmi-conform{EXE}")
    parser.add_argument("--max-files", type=int, default=4000, help="files taken from the build and the venv, each")
    parser.add_argument("--json", type=Path)
    args = parser.parse_args()
    import PIL

    print(f"Pillow {PIL.__version__}")
    with tempfile.TemporaryDirectory(prefix="tmi-sniff-") as tmp:
        tmp = Path(tmp)
        groups = {"pillow-written": pillow_files(tmp), "crafted": crafted_files(tmp), "generated": other_files(tmp)}
        groups.update(existing_files(args.max_files))
        report: dict = {"pillow": PIL.__version__, "groups": {}}
        differ, unsafe = [], []
        for name, files in groups.items():
            out = subprocess.run([str(args.conform), "sniff"], input="\n".join(map(str, files)), capture_output=True,
                                 text=True, encoding="utf-8", check=True).stdout.splitlines()
            rust = {line.split("\t", 1)[1]: line.split("\t", 1)[0] for line in out}
            counts = {"files": len(files), "image": 0, "opaque": 0, "too_large": 0, "pillow_opens": 0, "pillow_opens_but_opaque": 0,
                      "rust_python_differ": 0}
            for path in files:
                data = path.read_bytes()
                verdict, py = rust[str(path)], python_verdict(data)
                opens = pillow_opens(data)
                tally = report.setdefault("opaque_to_tmi_by_pillow_format", {})
                counts[verdict if verdict in counts else "too_large"] += 1
                counts["pillow_opens"] += opens is not None
                if verdict != py:
                    counts["rust_python_differ"] += 1
                    differ.append(f"{path} rust={verdict} python={py}")
                if verdict == "opaque" and opens:
                    counts["pillow_opens_but_opaque"] += 1
                    tally[opens] = tally.get(opens, 0) + 1
                if verdict == "image" and not opens:
                    unsafe.append(str(path))
            report["groups"][name] = counts
            print(f"{name:15} {counts}")
        report["differ"], report["unsafe"] = differ, unsafe
        print("Pillow opens, tmi stores as opaque, by Pillow format:",
              ", ".join(f"{k} {v}" for k, v in sorted(report.get("opaque_to_tmi_by_pillow_format", {}).items())))
    if args.json:
        args.json.write_text(json.dumps(report, indent=2), encoding="utf-8")
    if differ:
        print("DIFFER:", *differ[:10], sep="\n  ")
    if unsafe:
        print("tmi reads, Pillow does not:", *unsafe[:10], sep="\n  ")
    ok = not differ and not unsafe
    print("sniff parity:", "ok (Rust and Python agree on every file; tmi reads nothing Pillow refuses)" if ok else "FAIL")
    return 0 if ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
