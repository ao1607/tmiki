from __future__ import annotations

import io
import os
import random
import shutil
import struct
import subprocess
import sys
import tempfile
from tmiref import clip_io  # the Python write-back helpers that build test files
import unittest
import zlib
from pathlib import Path

import numpy as np

PROJECT_ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(PROJECT_ROOT))

from tmi import chunking  # noqa: E402
from tmi.image_ops import image_from_bytes  # noqa: E402
from tmi.objects import CHUNKED_MIN_SIZE, ObjectStore  # noqa: E402
from tmiref import chunking as ref_chunking  # noqa: E402


def _fake_clip(bodies: list[bytes], ids: list[str], sqli: bytes) -> bytes:
    """A CSFCHUNK container shaped like a CSP save: Head, Exta per body, SQLi, Foot."""
    chunks = [b"CHNK" + b"Head" + struct.pack(">Q", 40) + bytes(40)]
    for body, eid in zip(bodies, ids):
        payload = struct.pack(">Q", 0x28) + b"extrnlid" + eid.encode("ascii") + struct.pack(">Q", len(body)) + body
        chunks.append(b"CHNK" + b"Exta" + struct.pack(">Q", len(payload)) + payload)
    chunks.append(b"CHNK" + b"SQLi" + struct.pack(">Q", len(sqli)) + sqli)
    chunks.append(b"CHNK" + b"Foot" + struct.pack(">Q", 0))
    body = b"".join(chunks)
    return b"CSFCHUNK" + struct.pack(">QQ", 24 + len(body), 24) + body


def _new_ids(rng: random.Random, n: int) -> list[str]:
    return ["".join(rng.choice("0123456789ABCDEF") for _ in range(32)) for _ in range(n)]


def _stored_bytes(store: ObjectStore) -> int:
    return sum(store.path_for(oid).stat().st_size for oid in store.iter_object_ids())


class ChunkingTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = Path(tempfile.mkdtemp(prefix="tmi-dedup-"))
        self.rng = random.Random(7)

    def tearDown(self) -> None:
        shutil.rmtree(self.tmp, ignore_errors=True)

    def test_segments_cover_payload_and_keep_extrnlid_volatile(self) -> None:
        bodies = [self.rng.randbytes(n) for n in (900_000, 2_000, 1_500_000)]
        ids = _new_ids(self.rng, 3)
        data = _fake_clip(bodies, ids, self.rng.randbytes(300_000))
        segments = chunking.storage_segments(data)
        self.assertEqual(segments[0].start, 0)
        self.assertEqual(segments[-1].end, len(data))
        for prev, seg in zip(segments, segments[1:]):
            self.assertEqual(prev.end, seg.start)
        for eid in ids:
            at = data.index(eid.encode("ascii"))
            owner = next(seg for seg in segments if seg.start <= at < seg.end)
            self.assertFalse(owner.shareable, "the per-save external id must not land in a shared piece")
        for seg in segments:
            if seg.shareable:
                self.assertLessEqual(seg.end - seg.start, chunking.PIECE_MAX)

    def test_cdc_cuts_resync_after_an_insertion(self) -> None:
        data = self.rng.randbytes(4_000_000)
        edited = data[:1_000_000] + b"inserted bytes" + data[1_000_000:]

        def pieces(buf: bytes) -> set[bytes]:
            last, out = 0, set()
            for cut in chunking.cdc_cuts(buf):
                out.add(buf[last:cut])
                last = cut
            return out

        shared = pieces(data) & pieces(edited)
        self.assertGreater(sum(map(len, shared)), 0.8 * len(data))

    def test_large_blob_round_trips_and_a_resave_shares_untouched_layers(self) -> None:
        store = ObjectStore(self.tmp / "objects")
        bodies = [self.rng.randbytes(n) for n in (8_000_000, 3_000_000, 40_000)]
        v1 = _fake_clip(bodies, _new_ids(self.rng, 3), self.rng.randbytes(200_000))
        oid1 = store.write_large_blob(v1)
        self.assertEqual(store.read(oid1)[0], "chunked")
        self.assertEqual(store.read_blob(oid1), v1)
        before = _stored_bytes(store)

        # CSP re-save: every external id is new, the SQLite chunk is new, and
        # one layer changed in the middle.
        edited = bytearray(bodies[0])
        edited[4_000_000:4_000_100] = self.rng.randbytes(100)
        v2 = _fake_clip([bytes(edited), bodies[1], bodies[2]], _new_ids(self.rng, 3), self.rng.randbytes(210_000))
        oid2 = store.write_large_blob(v2)
        self.assertEqual(store.read_blob(oid2), v2)
        growth = _stored_bytes(store) - before
        self.assertLess(growth, 0.3 * len(v2), f"re-save stored {growth} of {len(v2)} bytes")

    def test_small_or_unsplittable_payloads_stay_plain_blobs(self) -> None:
        store = ObjectStore(self.tmp / "objects")
        small = self.rng.randbytes(CHUNKED_MIN_SIZE - 1)
        self.assertEqual(store.read(store.write_large_blob(small))[0], "blob")


class GearTableTests(unittest.TestCase):
    """The CDC gear table is a literal so cut points (and dedup across
    versions) cannot move with the installed numpy."""

    def test_table_is_256_distinct_63_bit_values(self) -> None:
        table = chunking.GEAR_TABLE
        self.assertEqual(256, len(table))
        self.assertEqual(256, len(set(table)))
        self.assertTrue(all(isinstance(v, int) and 0 <= v < 2**63 for v in table))
        self.assertEqual(list(table), chunking._GEAR_LIST)
        self.assertEqual(list(table), [int(v) for v in ref_chunking.GEAR])

    def test_table_is_what_numpy_generated(self) -> None:
        drawn = [int(v) for v in np.random.default_rng(0x746D69).integers(0, 2**63, 256, dtype=np.uint64)]
        if drawn != list(chunking.GEAR_TABLE):
            self.skipTest(
                f"numpy {np.__version__} draws a different sequence from default_rng(0x746D69) than the "
                "numpy the table was frozen from; the literal table is the reference, so cut points are unchanged"
            )
        self.assertEqual(drawn, list(chunking.GEAR_TABLE))

    def test_cut_points_are_pinned(self) -> None:
        data = random.Random(20261012).randbytes(4_000_000)
        want = [611989, 1431203, 1974140, 2084476, 2744957, 3388936, 3863139, 4000000]
        self.assertEqual(want, ref_chunking.cdc_cuts_numpy(data, 0, len(data)))
        self.assertEqual(want, chunking.cdc_cuts(data))
        self.assertEqual(want[:6] + [3_500_000], chunking.cdc_cuts(data, 1234, 3_500_000))


class NativeCdcParityTests(unittest.TestCase):
    """tmi_accel's cdc_cuts must cut exactly where the numpy reference
    (``tmiref.chunking``) does."""

    def assert_parity(self, data: bytes, start: int = 0, end: int | None = None) -> None:
        end = len(data) if end is None else end
        expected = ref_chunking.cdc_cuts_numpy(data, start, end)
        self.assertEqual(chunking.cdc_cuts(data, start, end), expected, f"len={len(data)} start={start} end={end}")

    def test_random_sizes_and_edges(self) -> None:
        rng = random.Random(11)
        big = rng.randbytes(9 * 1024 * 1024 + 4321)
        block = 4 * 1024 * 1024
        sizes = [
            0, 1, 64, 65, chunking.PIECE_MIN - 1, chunking.PIECE_MIN, chunking.PIECE_MIN + 1,
            chunking.PIECE_MIN + 65, chunking.PIECE_MIN + 66, 300_000, 1 << 20,
            chunking.PIECE_MAX, chunking.PIECE_MAX + 1, block - 1, block, block + 1, block + 64, block + 65,
            2 * block, 2 * block + 1, len(big),
        ]
        for n in sizes:
            self.assert_parity(big[:n])
        # sub-ranges, including starts that put block edges off alignment
        for start, end in [(1, len(big)), (65, block + 70), (block - 30, len(big)), (12345, 12345 + chunking.PIECE_MIN + 1)]:
            self.assert_parity(big, start, end)
        for _ in range(20):
            start = rng.randrange(0, len(big))
            self.assert_parity(big, start, rng.randrange(start, len(big) + 1))

    def test_no_candidates(self) -> None:
        # Constant bytes hash the same everywhere and (for these values) never
        # qualify, so every piece is a forced PIECE_MAX cut.
        for value in (0, 1, 0x80, 0xFF):
            self.assert_parity(bytes([value]) * (5 * chunking.PIECE_MAX + 17))
            self.assert_parity(bytes([value]) * (2 * chunking.PIECE_MAX))

    def test_example_files(self) -> None:
        paths = sorted((PROJECT_ROOT / "example").glob("*.clip")) + sorted((PROJECT_ROOT / "example").glob("*.mdp"))
        if not paths:
            self.skipTest("no example files")
        for path in paths:
            data = path.read_bytes()
            self.assert_parity(data)
            for start, end, kind in (chunking._clip_spans(data) if data[:8] == b"CSFCHUNK" else chunking._mdp2_spans(data)):
                if kind != "volatile":
                    self.assert_parity(data, start, end)


def _mdp2_with_layer(tiles: list[tuple[int, int, bytes]], size: tuple[int, int], layer_type: str = "32bpp") -> bytes:
    """A one-layer MDP2 file; ``tiles`` are (col, row, raw tile bytes)."""
    payload = struct.pack("<I", len(tiles))
    payload += struct.pack("<I", 128)
    for i, (col, row, raw) in enumerate(tiles):
        comp = zlib.compress(raw)
        if i:
            payload += b"\x00" * ((-len(payload)) % 4)
            payload += struct.pack("<IIII", col, row, 0, len(comp))
        else:
            payload += struct.pack("<IIII", col, row, 0, len(comp))
        payload += comp
    if not tiles:
        payload += bytes(16)
    payload += b"\x00" * ((-len(payload)) % 4)
    name = "layer0img"
    pac = b"PAC " + struct.pack("<IIII", 132 + len(payload), 0, len(payload), len(payload))
    pac += b"\x00" * (68 - len(pac)) + name.encode("ascii").ljust(64, b"\x00") + payload
    xml = (
        f'<MDP width="{size[0]}" height="{size[1]}" dpi="72"><Layers>'
        f'<Layer name="l" width="{size[0]}" height="{size[1]}" mode="normal" alpha="200" '
        f'id="0" parentId="-1" bin="{name}" type="{layer_type}" visible="true" ofsx="0" ofsy="0"/>'
        "</Layers></MDP>"
    ).encode("utf-8")
    return b"mdipack\x00" + struct.pack("<III", 0, len(xml), len(pac)) + xml + pac


class NativeMdp2DecodeParityTests(unittest.TestCase):
    """mdp2.render_layer_crop (tmi_accel) must equal the Python reference
    decode (``tmiref.mdp2.render_layer_rgba``)."""

    def assert_same_crop(self, mdp, layer) -> None:
        from tmi.formats import mdp2
        from tmi.image_ops import LayerCrop
        from tmiref import mdp2 as ref

        native = mdp2.render_layer_crop(mdp, layer)
        ref_mdp = ref.parse_mdp2(mdp.raw)
        python = LayerCrop.from_full(ref.render_layer_rgba(ref_mdp, ref_mdp.layers[layer.index], apply_opacity=False))
        self.assertEqual(native.header(), python.header())
        self.assertEqual(native.rgba, python.rgba)
        self.assertEqual(native.digest(), python.digest())
        self.assertEqual(native.alpha_bbox(), python.alpha_bbox())

    def check(self, tiles: list[tuple[int, int, bytes]], size: tuple[int, int]) -> None:
        from tmi.formats import mdp2

        mdp = mdp2.parse_mdp2(_mdp2_with_layer(tiles, size))
        self.assert_same_crop(mdp, mdp.layers[0])

    def test_synthetic_layers(self) -> None:
        rng = np.random.default_rng(5)

        def tile(kind: str) -> bytes:
            px = rng.integers(0, 256, (128, 128, 4), dtype=np.uint8)
            if kind == "sparse":
                px[:, :, 3] = 0
                px[rng.integers(0, 128), rng.integers(0, 128), 3] = 7
            elif kind == "clear":
                px[:, :, 3] = 0  # colour under zero alpha must vanish
            else:
                px[:, :, 3] = np.where(px[:, :, 3] < 60, 0, px[:, :, 3])
            return px.tobytes()

        self.check([], (300, 200))
        self.check([(0, 0, tile("clear"))], (128, 128))
        self.check([(0, 0, tile("dense"))], (128, 128))
        # partial edge tiles on a layer that is not a multiple of 128
        self.check([(c, r, tile("dense")) for r in range(2) for c in range(3)], (300, 200))
        self.check([(2, 1, tile("sparse")), (0, 0, tile("sparse")), (1, 1, tile("clear"))], (300, 200))
        # duplicate (composited in order) and off-layer tiles
        self.check([(0, 0, tile("dense")), (0, 0, tile("dense"))], (200, 200))
        self.check([(0, 0, tile("dense")), (5, 5, tile("dense"))], (200, 200))

    def test_example_files(self) -> None:
        from tmi.formats import mdp2

        paths = sorted((PROJECT_ROOT / "example").glob("*.mdp"))
        if not paths:
            self.skipTest("no example MDP2 files")
        for path in paths:
            mdp = mdp2.open_mdp2(path)
            for layer in mdp.layers:
                if not layer.children and layer.type != "folder" and mdp2.is_renderable_raster(mdp, layer):
                    self.assert_same_crop(mdp, layer)


class RepoDedupTests(unittest.TestCase):
    def setUp(self) -> None:
        root = PROJECT_ROOT / ".test-tmp"
        root.mkdir(exist_ok=True)
        self.tmp = Path(tempfile.mkdtemp(prefix="tmi-dedup-", dir=root))
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

    def test_clip_layer_edit_commit_stores_a_fraction_and_survives_gc(self) -> None:
        src = PROJECT_ROOT / "example" / "tama.clip"
        if not src.exists():
            self.skipTest("example/tama.clip is not available")
        from tmi import importers
        from tmi.formats import clip as clip_mod

        repo = self.tmp / "repo"
        repo.mkdir()
        art = repo / "tama.clip"
        shutil.copy(src, art)
        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "tama.clip")
        self.run_tmi(repo, "commit", "-m", "base")
        store = ObjectStore(repo / ".tmi" / "objects")
        base_size = _stored_bytes(store)

        imported = importers.import_file(art, render_composite=False)
        target = next(layer for layer in importers.walk_imported_layers(imported.layers)
                      if layer.name == "背景" and layer.blob_bytes)
        image = image_from_bytes(target.blob_bytes)
        image.paste((220, 40, 40, 255), (1700, 2291, 1820, 2371))
        buf = io.BytesIO()
        image.save(buf, "PNG")
        original = art.read_bytes()
        edited = clip_mod.replace_layer_pixels(art, target.stable_source_id, buf.getvalue())
        # Tiles the edit did not touch keep CSP's original records.
        container = clip_mod.parse_container(original)
        old_records = clip_io._exta_records_by_block(original, container.index_extas(original)[target.source_meta["render_outer_id"]])
        kept = sum(1 for record in old_records.values() if record in edited)
        # The 120x80 rectangle straddles at most four 256px tiles.
        self.assertGreaterEqual(kept, len(old_records) - 4)

        art.write_bytes(edited)
        self.run_tmi(repo, "commit", "-a", "-m", "edit")
        growth = _stored_bytes(store) - base_size
        # The composite is re-rendered (the writer invalidates CSP's preview);
        # the source itself must cost the volatile SQLite chunk plus the edit.
        self.assertLess(growth, len(edited) * 0.6, f"commit stored {growth / 1e6:.1f} MB")

        self.run_tmi(repo, "gc", "--prune=now")
        self.run_tmi(repo, "restore", "--source", "HEAD~1", "tama.clip")
        self.assertEqual(art.read_bytes(), original)
        self.run_tmi(repo, "restore", "--source", "HEAD", "tama.clip")
        self.assertEqual(art.read_bytes(), edited)


if __name__ == "__main__":
    unittest.main()
