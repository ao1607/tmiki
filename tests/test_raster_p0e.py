"""PLUGINS.md P0e: a file is an image exactly when tmi's own decoders read it
(``raster.import_decode`` in Python, ``tmi_import::import_file`` in Rust:
both ask ``tmi_image::import::open_for_import``), else an opaque blob.

- Pillow-written images of the formats tmi decodes are images.
- What Pillow opens but tmi does not (TGA, PCX, ICO, PPM, a TIFF in LAB or
  with JPEG compression, a truncated JPEG, a PNG cut short) is opaque, in
  Python and in Rust (``tmi-conform sniff``: needs ``cargo build --release
  -p tmi-conform``), whatever the extension.
- The verdict does not consult Pillow: with ``import PIL`` failing, the
  same files give the same verdicts and Pillow is not loaded.
"""
from __future__ import annotations

import io
import json
import os
import subprocess
import sys
import tempfile
import unittest
import warnings
from pathlib import Path

from PIL import Image

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))

from tmi import raster  # noqa: E402

EXE = ROOT / "rust" / "target" / "release" / ("tmi-conform.exe" if os.name == "nt" else "tmi-conform")


def _save(image: Image.Image, fmt: str, **kw) -> bytes:
    buf = io.BytesIO()
    image.save(buf, fmt, **kw)
    return buf.getvalue()


def _picture() -> Image.Image:
    img = Image.new("RGB", (23, 17))
    img.putdata([((x * 11) % 256, (y * 15) % 256, (x * y) % 256) for y in range(17) for x in range(23)])
    return img


def cases() -> tuple[dict[str, bytes], dict[str, bytes], dict[str, bytes]]:
    """(images tmi reads, files Pillow opens that tmi stores as opaque, files
    that are not images at all)."""
    rgb = _picture()
    rgba = rgb.convert("RGBA")
    jpeg = _save(rgb, "JPEG", quality=90)
    images = {
        "rgba.png": _save(rgba, "PNG"), "pal.png": _save(rgb.quantize(16), "PNG"), "rgb.jpg": _save(rgb, "JPEG"),
        "grey.jpg": _save(rgb.convert("L"), "JPEG"), "pic.gif": _save(rgb.quantize(32), "GIF"), "pic.bmp": _save(rgb, "BMP"),
        "pic.tif": _save(rgba, "TIFF"), "lzw.tif": _save(rgb, "TIFF", compression="tiff_lzw"),
        "pic.webp": _save(rgba, "WEBP", lossless=True), "gif.png": _save(rgb.quantize(8), "GIF"), "noext": _save(rgb, "BMP"),
    }
    pillow_only = {
        "pic.tga": _save(rgb, "TGA"), "pic.pcx": _save(rgb, "PCX"), "pic.ico": _save(rgba, "ICO", sizes=[(16, 16)]),
        "pic.ppm": _save(rgb, "PPM"), "lab.tif": _save(Image.new("LAB", (8, 8)), "TIFF"),
        "jpeg-in-tiff.tif": _save(rgb, "TIFF", compression="jpeg"), "pic.sgi": _save(rgb, "SGI"),
        "cut.jpg": jpeg[: len(jpeg) * 2 // 3], "cut.png": _save(rgba, "PNG")[:-40],
    }
    others = {
        "empty": b"", "text.txt": b"hello\n", "fake.gif": b"GIF89a\x01\x00\x01\x00not really",
        "fake.png": b"\x89PNG\r\n\x1a\nnot really", "fake.jpg": b"\xff\xd8\xff", "zip.bin": b"PK\x03\x04" + bytes(64),
    }
    return images, pillow_only, others


def _pillow_opens(data: bytes) -> bool:
    try:
        with warnings.catch_warnings():
            warnings.simplefilter("ignore")
            Image.open(io.BytesIO(data)).convert("RGBA")
        return True
    except Exception:
        return False


class ImageOrOpaqueTests(unittest.TestCase):
    def test_python_verdicts(self) -> None:
        images, pillow_only, others = cases()
        for name, data in images.items():
            with self.subTest(image=name):
                decoded = raster.import_decode(data)
                self.assertIsNotNone(decoded)
                self.assertEqual(Image.open(io.BytesIO(data)).convert("RGBA").tobytes(), decoded.image.tobytes())
        for name, data in pillow_only.items():
            with self.subTest(opaque=name):
                if not name.startswith("cut."):  # (a cut file may or may not open in Pillow: it recovers what it can)
                    self.assertTrue(_pillow_opens(data), "the contrast: Pillow reads it")
                self.assertIsNone(raster.import_decode(data))
        for name, data in others.items():
            with self.subTest(other=name):
                self.assertIsNone(raster.import_decode(data))

    def test_importer_records_opaque_whatever_the_extension(self) -> None:
        from tmi.importers import import_file

        images, pillow_only, others = cases()
        with tempfile.TemporaryDirectory() as tmp:
            for group, want in ((images, None), (pillow_only, "binary"), (others, "binary")):
                for name, data in group.items():
                    path = Path(tmp) / name
                    path.write_bytes(data)
                    got = import_file(path, render_composite=False)
                    with self.subTest(file=name):
                        if want is None:
                            self.assertNotEqual("binary", got.source_format)
                            self.assertEqual("raster", got.layers[0].kind)
                        else:
                            self.assertEqual("binary", got.source_format)
                            self.assertEqual("binary", got.layers[0].kind)

    @unittest.skipUnless(EXE.exists(), "tmi-conform not built (cargo build --release -p tmi-conform)")
    def test_rust_gives_the_same_verdicts(self) -> None:
        images, pillow_only, others = cases()
        every = {**images, **pillow_only, **others}
        with tempfile.TemporaryDirectory() as tmp:
            paths = []
            for name, data in every.items():
                path = Path(tmp) / name
                path.write_bytes(data)
                paths.append(path)
            out = subprocess.run([str(EXE), "sniff"], input="\n".join(map(str, paths)), capture_output=True, text=True,
                                 encoding="utf-8", check=True).stdout.splitlines()
        rust = {Path(line.split("\t", 1)[1]).name: line.split("\t", 1)[0] for line in out}
        for name, data in every.items():
            with self.subTest(file=name):
                self.assertEqual("opaque" if raster.import_decode(data) is None else "image", rust[name])

    def test_the_verdict_does_not_consult_pillow(self) -> None:
        images, pillow_only, others = cases()
        every = {**images, **pillow_only, **others}
        script = (
            "import sys, json\n"
            "sys.modules['PIL'] = None\n"
            f"sys.path.insert(0, {str(ROOT)!r})\n"
            "from tmi import raster\n"
            "files = json.loads(sys.stdin.read())\n"
            "out = {n: raster.import_decode(bytes.fromhex(h)) is not None for n, h in files.items()}\n"
            "assert 'PIL.Image' not in sys.modules\n"
            "print(json.dumps(out))\n"
        )
        result = subprocess.run([sys.executable, "-c", script], input=json.dumps({n: d.hex() for n, d in every.items()}),
                                capture_output=True, text=True, timeout=120)
        self.assertEqual(0, result.returncode, result.stderr)
        blocked = json.loads(result.stdout)
        self.assertEqual({n: raster.import_decode(d) is not None for n, d in every.items()}, blocked)
        self.assertEqual(set(images), {n for n, v in blocked.items() if v})


if __name__ == "__main__":
    unittest.main()