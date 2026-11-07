"""The TMIL layer blob container (tmi/layer_blob.py, rust/tmi-accel/src/layer_blob.rs).

Every codec check runs on both codecs: tmi's (tmi_accel) and the Python
reference ``tmiref.layer_blob`` (numpy + zstandard + Pillow)."""
from __future__ import annotations

import io
import os
import shutil
import struct
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

import numpy as np
from PIL import Image

PROJECT_ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(PROJECT_ROOT))

from tmi import layer_blob as L  # noqa: E402
from tmiref import layer_blob as R  # noqa: E402


def _codecs() -> list:
    """tmi's codec (tmi_accel) and the Python reference."""
    return [L, R]


def _png_filter(kind: int, cur: bytes, prev: bytes) -> bytes:
    """Reference PNG filter (bpp 4), one byte at a time."""
    out = bytearray(len(cur))
    for i, x in enumerate(cur):
        a = cur[i - 4] if i >= 4 else 0
        b = prev[i]
        c = prev[i - 4] if i >= 4 else 0
        if kind == 0:
            pred = 0
        elif kind == 1:
            pred = a
        elif kind == 2:
            pred = b
        elif kind == 3:
            pred = (a + b) >> 1
        else:
            p = a + b - c
            pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
            pred = a if pa <= pb and pa <= pc else b if pb <= pc else c
        out[i] = (x - pred) & 0xFF
    return bytes(out)


def _container(size, x0, y0, w, h, rows, frames: list[bytes]) -> bytes:
    head = struct.pack("<4sB3xIIIIIIII", b"TMIL", 1, size[0], size[1], x0, y0, w, h, rows, len(frames))
    return head + struct.pack(f"<{len(frames)}I", *map(len, frames)) + b"".join(frames)


def _images(rng: np.random.Generator) -> dict[str, np.ndarray]:
    """(h, w, 4) arrays at awkward sizes and with awkward content."""
    smooth = np.zeros((300, 437, 4), dtype=np.uint8)
    yy, xx = np.mgrid[0:300, 0:437]
    smooth[..., 0] = (xx * 255 // 436).astype(np.uint8)
    smooth[..., 1] = (yy * 255 // 299).astype(np.uint8)
    smooth[..., 2] = ((xx + yy) % 256).astype(np.uint8)
    smooth[..., 3] = np.where((xx // 40 + yy // 30) % 3 == 0, 0, 200)
    return {
        "1x1": rng.integers(0, 256, (1, 1, 4), dtype=np.uint8),
        # 1 x N: 65536 rows per band, so two bands, the second of 4464 rows
        "1xN": rng.integers(0, 256, (70000, 1, 4), dtype=np.uint8),
        # N x 1: one row wider than a band
        "Nx1": rng.integers(0, 256, (1, 70001, 4), dtype=np.uint8),
        # 437 px: 149 rows per band, 300 rows -> 149 + 149 + 2
        "uneven bands": smooth,
        "transparent": np.zeros((513, 129, 4), dtype=np.uint8),
        "opaque": np.concatenate(
            [rng.integers(0, 256, (257, 1000, 3), dtype=np.uint8), np.full((257, 1000, 1), 255, np.uint8)], axis=2
        ),
        "noise": rng.integers(0, 256, (640, 480, 4), dtype=np.uint8),
    }


class LayerBlobCodecTests(unittest.TestCase):
    def test_round_trips_at_odd_sizes_on_every_path(self) -> None:
        rng = np.random.default_rng(1)
        for label, array in _images(rng).items():
            h, w = array.shape[:2]
            raw = array.tobytes()
            size, x0, y0 = (w + 3, h + 2), 2, 1
            for codec in _codecs():
                with self.subTest(label=label, codec=codec.__name__):
                    blob = codec.encode(raw, w, h, size, x0, y0)
                    self.assertTrue(codec.is_layer_blob(blob))
                    header, rgba = codec.decode(blob)
                    self.assertEqual(raw, rgba)
                    self.assertEqual((size, (x0, y0, x0 + w, y0 + h)), (header.size, header.rect))
                    self.assertEqual(codec.rows_per_band(w), header.rows_per_band)
                    self.assertEqual(-(-h // header.rows_per_band), len(header.band_sizes))
                    crop = codec.decode_image(blob, expand=False)
                    self.assertEqual((w, h), crop.size)
                    self.assertEqual(raw, crop.tobytes())
                    full = codec.decode_image(blob, expand=True)
                    self.assertEqual(size, full.size)
                    self.assertEqual(raw, full.crop((x0, y0, x0 + w, y0 + h)).tobytes())
                    outside = np.asarray(full).copy()
                    outside[y0:y0 + h, x0:x0 + w] = 0
                    self.assertFalse(outside.any(), "pixels outside the rectangle are zero")

    def test_empty_layer(self) -> None:
        for codec in _codecs():
            with self.subTest(codec=codec.__name__):
                blob = codec.encode(b"", 0, 0, (31, 17))
                self.assertEqual(codec.HEADER_SIZE, len(blob))
                header, rgba = codec.decode(blob)
                self.assertEqual(((31, 17), None, b""), (header.size, header.rect, rgba))
                self.assertIsNone(codec.decode_image(blob, expand=False))
                full = codec.decode_image(blob, expand=True)
                self.assertEqual(((31, 17), None), (full.size, full.getbbox(alpha_only=False)))

    def test_encoder_writes_paeth_rows_predicting_from_zeros_per_band(self) -> None:
        import zstandard

        rng = np.random.default_rng(2)
        array = rng.integers(0, 256, (40, 2000, 4), dtype=np.uint8)  # 32 rows per band
        for codec in _codecs():
            with self.subTest(codec=codec.__name__):
                blob = codec.encode(array.tobytes(), 2000, 40, (2000, 40))
                header = codec.parse_header(blob)
                self.assertEqual(32, header.rows_per_band)
                for y0, y1, offset, size in header.bands():
                    payload = zstandard.ZstdDecompressor().decompress(blob[offset:offset + size])
                    stride = 4 * 2000
                    expected = b"".join(
                        b"\x04" + _png_filter(4, array[y].tobytes(), array[y - 1].tobytes() if y > y0 else bytes(stride))
                        for y in range(y0, y1)
                    )
                    self.assertEqual(expected, payload)

    def test_decoder_accepts_all_five_filter_types(self) -> None:
        import zstandard

        rng = np.random.default_rng(3)
        w, h, rows = 13, 23, 5  # five bands, the last of 3 rows
        array = rng.integers(0, 256, (h, w, 4), dtype=np.uint8)
        array[7:12, :, :] //= 3  # some smooth-ish rows too
        stride = 4 * w
        frames = []
        for start in range(0, h, rows):
            payload = bytearray()
            for y in range(start, min(h, start + rows)):
                # Every type on every row position, a band's first row included.
                kind = (y + start // rows) % 5
                prev = array[y - 1].tobytes() if y > start else bytes(stride)
                payload += bytes([kind]) + _png_filter(kind, array[y].tobytes(), prev)
            frames.append(zstandard.ZstdCompressor(level=3).compress(bytes(payload)))
        blob = _container((w, h), 0, 0, w, h, rows, frames)
        for codec in _codecs():
            with self.subTest(codec=codec.__name__):
                self.assertEqual(array.tobytes(), codec.decode(blob)[1])
                self.assertEqual(array.tobytes(), codec.decode_image(blob, expand=True).tobytes())

    def test_bands_are_independent(self) -> None:
        rng = np.random.default_rng(4)
        w, h = 300, 900  # 218 rows per band: 5 bands
        array = rng.integers(0, 256, (h, w, 4), dtype=np.uint8)
        edited = array.copy()
        edited[500:510, 40:60] = 7  # inside band 2 (rows 436..653)
        for codec in _codecs():
            with self.subTest(codec=codec.__name__):
                a = codec.encode(array.tobytes(), w, h, (w, h))
                b = codec.encode(edited.tobytes(), w, h, (w, h))
                ha, hb = codec.parse_header(a), codec.parse_header(b)
                frames_a = [a[o:o + s] for _, _, o, s in ha.bands()]
                frames_b = [b[o:o + s] for _, _, o, s in hb.bands()]
                self.assertEqual([i for i in range(5) if frames_a[i] != frames_b[i]], [2])
                # Each band on its own is a complete blob of its rows.
                for (y0, y1, offset, size), frame in zip(ha.bands(), frames_a):
                    alone = _container((w, y1 - y0), 0, 0, w, y1 - y0, ha.rows_per_band, [frame])
                    self.assertEqual(array[y0:y1].tobytes(), codec.decode(alone)[1])

    def test_corrupt_or_truncated_blobs_raise_a_clean_error(self) -> None:
        rng = np.random.default_rng(5)
        w, h = 200, 700
        array = rng.integers(0, 256, (h, w, 4), dtype=np.uint8)
        array[:327] //= 16  # band 0 compresses better, so the band sizes differ
        blob = R.encode(array.tobytes(), w, h, (w, h))
        header = L.parse_header(blob)
        first = header.bands()[0][2]
        cases = {
            "empty": b"",
            "header only": blob[:L.HEADER_SIZE],
            "in the table": blob[:L.HEADER_SIZE + 2],
            "one byte short": blob[:-1],
            "one byte long": blob + b"\0",
            "half": blob[: len(blob) // 2],
            "bad magic": b"TMIX" + blob[4:],
            "bad version": blob[:4] + b"\x02" + blob[5:],
            "reserved": blob[:5] + b"\x01" + blob[6:],
            "rect outside": blob[:16] + struct.pack("<I", 1) + blob[20:],
            "band count": blob[:36] + struct.pack("<I", len(header.band_sizes) + 1) + blob[40:],
            "frame magic": blob[:first] + b"\0" + blob[first + 1:],
            # sizes swapped between two bands: total unchanged, frames misframed
            "table order": blob[:40] + struct.pack("<II", header.band_sizes[1], header.band_sizes[0]) + blob[48:],
        }
        # A band whose row has an unknown filter type (7).
        import zstandard

        bad = bytes([7]) + bytes(4 * w)
        cases["filter type"] = _container((w, 1), 0, 0, w, 1, 1, [zstandard.ZstdCompressor(level=3).compress(bad)])
        # A band holding fewer rows than the header says.
        short = bytes([0]) + bytes(4 * w)
        cases["short band"] = _container((w, 2), 0, 0, w, 2, 2, [zstandard.ZstdCompressor(level=3).compress(short)])
        from tmi.image_ops import image_from_bytes, layer_blob_digest, layer_blob_to_png

        for codec in _codecs():
            for label, data in cases.items():
                with self.subTest(codec=codec.__name__, case=label):
                    fns = [codec.decode, lambda d: codec.decode_image(d, expand=True)]
                    if codec is L:
                        fns += [image_from_bytes, layer_blob_digest, layer_blob_to_png]
                    for fn in fns:
                        if label in ("empty", "bad magic") and fn is not codec.decode:
                            continue  # not a layer blob at all: Pillow says so in its own way
                        with self.assertRaises(ValueError):
                            fn(data)
                    with self.assertRaises(codec.LayerBlobError):
                        codec.decode(data)

    def test_encode_validates_its_arguments(self) -> None:
        for codec in _codecs():
            with self.subTest(codec=codec.__name__):
                with self.assertRaises(ValueError):
                    codec.encode(bytes(12), 2, 2, (2, 2))  # buffer too short
                with self.assertRaises(ValueError):
                    codec.encode(bytes(16), 2, 2, (3, 3), 2, 0)  # rectangle outside the layer

    def test_thread_count_does_not_change_the_bytes(self) -> None:
        rng = np.random.default_rng(6)
        array = rng.integers(0, 64, (1200, 800, 4), dtype=np.uint8)
        raw = array.tobytes()
        for codec in _codecs():
            with self.subTest(codec=codec.__name__):
                once = codec.encode(raw, 800, 1200, (800, 1200))
                if codec is R:
                    with mock.patch.object(R, "_POOL_MIN_BANDS", 10**9):  # bands one by one
                        self.assertEqual(once, R.encode(raw, 800, 1200, (800, 1200)))
                from concurrent.futures import ThreadPoolExecutor

                with ThreadPoolExecutor(8) as pool:
                    self.assertEqual({once}, set(pool.map(lambda _: codec.encode(raw, 800, 1200, (800, 1200)), range(8))))


class LayerBlobParityTests(unittest.TestCase):
    """tmi's codec (tmi_accel) and the Python reference read each other's blobs."""

    def test_rust_and_python_read_each_other(self) -> None:
        rng = np.random.default_rng(7)
        cases = dict(_images(rng))
        tama = PROJECT_ROOT / "example" / "tama.clip"
        if tama.exists():
            from tmi.importers import import_file, walk_imported_layers

            layers = [l for l in walk_imported_layers(import_file(tama, render_composite=False).layers) if l.blob_bytes]
            for layer in sorted(layers, key=lambda l: -len(l.blob_bytes))[:3]:
                header, rgba = L.decode(layer.blob_bytes)
                cases[f"tama {layer.name}"] = np.frombuffer(rgba, np.uint8).reshape(header.height, header.width, 4)
        for label, array in cases.items():
            h, w = array.shape[:2]
            raw = array.tobytes()
            with self.subTest(label=label):
                rust_blob = L.encode(raw, w, h, (w + 1, h), 1, 0)
                python_blob = R.encode(raw, w, h, (w + 1, h), 1, 0)
                self.assertEqual(raw, R.decode(rust_blob)[1], "Rust encode -> Python decode")
                py_from_rust = R.decode_image(rust_blob, expand=True).tobytes()
                self.assertEqual(raw, L.decode(python_blob)[1], "Python encode -> Rust decode")
                self.assertEqual(py_from_rust, L.decode_image(python_blob, expand=True).tobytes())
                # Same container; the zstd frames may differ between libzstd
                # versions, which is harmless (identity is the pixel digest).
                self.assertEqual(rust_blob[:L.HEADER_SIZE], python_blob[:L.HEADER_SIZE])


class MixedFormatRepoTests(unittest.TestCase):
    """A repository whose older commits hold PNG layer blobs (the format before
    TMIL) keeps reading them: digests, diff, export and write-back."""

    def setUp(self) -> None:
        root = PROJECT_ROOT / ".test-tmp"
        root.mkdir(exist_ok=True)
        self.tmp = Path(tempfile.mkdtemp(prefix="tmi-tmil-", dir=root))
        pythonpath = os.pathsep.join(filter(None, [str(PROJECT_ROOT), os.environ.get("PYTHONPATH")]))
        self.env = dict(os.environ, PYTHONPATH=pythonpath, TMI_AUTHOR_NAME="Tester", TMI_AUTHOR_EMAIL="t@example.com")

    def tearDown(self) -> None:
        shutil.rmtree(self.tmp, ignore_errors=True)
        try:
            (PROJECT_ROOT / ".test-tmp").rmdir()
        except OSError:
            pass

    def run_tmi(self, repo: Path, *args: str) -> subprocess.CompletedProcess[str]:
        result = subprocess.run([sys.executable, "-m", "tmi", *args], cwd=repo, env=self.env, text=True,
                                capture_output=True, timeout=300)
        if result.returncode:
            self.fail(f"tmi {' '.join(args)} failed\n{result.stdout}\n{result.stderr}")
        return result

    def test_png_and_tmil_layer_blobs_side_by_side(self) -> None:
        import tmi.image_ops as image_ops
        from tests.test_cli_regressions import write_mdp2_layers
        from tmi.image_ops import image_from_bytes, layer_blob_digest
        from tmi.importers import import_file, walk_imported_layers
        from tmi.repo import Repo

        def png_layer_blob(crop) -> bytes:
            # How layer blobs were written before TMIL.
            image = crop.image if crop.image is not None else Image.new("RGBA", (1, 1), (0, 0, 0, 0))
            return image_ops.image_to_png_bytes(image, text=(image_ops.LAYER_BLOB_TEXT_KEY, crop.header()))

        root = self.tmp / "repo"
        root.mkdir()
        art = root / "art.mdp"
        write_mdp2_layers(art, [(255, 0, 0, 255), (0, 0, 255, 128)])
        repo = Repo.create(root)
        with mock.patch.dict(os.environ, {"TMI_AUTHOR_NAME": "T", "TMI_AUTHOR_EMAIL": "t@example.com"}):
            with mock.patch.object(image_ops.LayerCrop, "encode", png_layer_blob):
                repo.stage_file(str(art))
                repo.commit("png blobs")
            v1 = art.read_bytes()
            write_mdp2_layers(art, [(255, 0, 0, 255), (0, 200, 0, 255)])  # only layer2 changes
            repo.stage_file(str(art))
            repo.commit("tmil blob")

        def layers(rev: str) -> dict[str, dict]:
            entry = repo.entries_for_commit(repo.resolve_rev(rev))["art.mdp"]
            return {l["name"]: l for l in repo.flatten_layers(entry["tree_ref"]) if l.get("blob_ref")}

        old, new = layers("HEAD~1"), layers("HEAD")
        blob = lambda layer: repo.store.read_blob(layer["blob_ref"])  # noqa: E731
        self.assertTrue(blob(old["layer1"]).startswith(b"\x89PNG"))
        self.assertTrue(blob(old["layer2"]).startswith(b"\x89PNG"))
        # The unchanged layer keeps its PNG blob (same pixels), the edited one is TMIL.
        self.assertEqual(old["layer1"]["blob_ref"], new["layer1"]["blob_ref"])
        self.assertTrue(L.is_layer_blob(blob(new["layer2"])))
        for layer in [*old.values(), *new.values()]:
            self.assertEqual(layer["pixel_sha256"], layer_blob_digest(blob(layer)))
        self.assertEqual((0, 0, 255, 128), image_from_bytes(blob(old["layer2"])).getpixel((0, 0)))
        self.assertEqual((0, 200, 0, 255), image_from_bytes(blob(new["layer2"])).getpixel((0, 0)))

        # Exports are PNG whichever format the blob is in.
        for rev, colour in (("HEAD~1", (0, 0, 255, 128)), ("HEAD", (0, 200, 0, 255))):
            out = self.tmp / f"layer2-{rev.replace('~', '_')}.png"
            self.run_tmi(root, "restore", "--source", rev, "--output", str(out), "art.mdp:layer2")
            data = out.read_bytes()
            self.assertTrue(data.startswith(b"\x89PNG"))
            self.assertEqual(colour, Image.open(io.BytesIO(data)).convert("RGBA").getpixel((0, 0)))
        diff = self.run_tmi(root, "diff", "HEAD~1", "HEAD", "--html", "--no-open", "--out", str(self.tmp / "diff"))
        self.assertIn("layer2", diff.stdout + diff.stderr + "".join(
            p.read_text(encoding="utf-8") for p in (self.tmp / "diff").rglob("*.html")))
        # Write-back from the PNG-blob commit into the native file.
        self.run_tmi(root, "restore", "--source", "HEAD~1", "--in-place", "art.mdp:layer2")
        restored = {l.name: l for l in walk_imported_layers(import_file(art).layers)}
        self.assertEqual((0, 0, 255, 128), image_from_bytes(restored["layer2"].blob_bytes).getpixel((0, 0)))
        self.assertEqual(old["layer2"]["pixel_sha256"], restored["layer2"].pixel_sha256)
        del v1


if __name__ == "__main__":
    unittest.main()
