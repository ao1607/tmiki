"""The render-parity groundwork: TMI_RENDER_TRACE and the corpus / diff tools."""
from __future__ import annotations

import hashlib
import json
import shutil
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

PROJECT_ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(PROJECT_ROOT))

TAMA = PROJECT_ROOT / "example" / "tama.clip"


class RenderTraceTests(unittest.TestCase):
    def setUp(self) -> None:
        if not TAMA.exists():
            self.skipTest("example/tama.clip is missing")
        root = PROJECT_ROOT / ".test-tmp"
        root.mkdir(exist_ok=True)
        self.tmp = Path(tempfile.mkdtemp(prefix="tmi-trace-", dir=root))

    def tearDown(self) -> None:
        shutil.rmtree(self.tmp, ignore_errors=True)
        try:
            (PROJECT_ROOT / ".test-tmp").rmdir()
        except OSError:
            pass

    def _render(self, trace: Path | None, renderer: str = "rust", **kwargs) -> str:
        """tmi's render of tama.clip (``renderer`` "rust"), or the reference
        renderer's ("python"), traced to ``trace``."""
        from tmi.formats import clip as clip_mod
        from tmi.formats import render_trace

        if renderer == "python":
            from tmiref import clip as renderer_mod
        else:
            renderer_mod = clip_mod
        clip = clip_mod.open_clip(TAMA)
        try:
            with mock.patch.object(renderer_mod, "_TRACE", trace is not None), \
                    mock.patch.object(render_trace, "PATH", str(trace or "")):
                image = renderer_mod.render_layered_composite_rgba(clip, **kwargs)
        finally:
            clip.db.close()
        return hashlib.sha256(image.tobytes()).hexdigest()

    def test_trace_is_off_unless_set(self) -> None:
        import os

        from tmi.formats import clip as clip_mod
        from tmiref import clip as ref

        self.assertEqual(bool(os.environ.get("TMI_RENDER_TRACE")), clip_mod._TRACE)
        self.assertEqual(bool(os.environ.get("TMI_RENDER_TRACE")), ref._TRACE)

    def test_both_renderers_write_the_same_trace(self) -> None:
        """tmi's renderer and the reference emit the same records, banded and
        windowed (what tools/render_diff.py walks in step)."""
        for kwargs in ({}, {"region": (100, 300, 612, 700)}):
            traces = {}
            for renderer in ("rust", "python"):
                traces[renderer] = self.tmp / f"{renderer}-{len(kwargs)}.jsonl"
                self._render(traces[renderer], renderer, **kwargs)
            self.assertTrue(traces["rust"].read_bytes())
            self.assertEqual(traces["python"].read_bytes(), traces["rust"].read_bytes(), kwargs)

    def test_banded_trace_is_deterministic_and_leaves_the_render_alone(self) -> None:
        plain = self._render(None)
        first, second = self.tmp / "a.jsonl", self.tmp / "b.jsonl"
        self.assertEqual(plain, self._render(first))
        self.assertEqual(plain, self._render(second))
        self.assertEqual(first.read_bytes(), second.read_bytes())
        records = [json.loads(line) for line in first.read_text(encoding="utf-8").splitlines()]
        self.assertTrue(records)
        self.assertEqual({"phase", "band", "main_id", "window", "bounds", "sha256"}, set(records[0]))
        self.assertEqual({"prerender", "clip", "apply"}, {r["phase"] for r in records})
        bands = []
        for rec in records:
            if not bands or bands[-1] != rec["band"]:
                bands.append(rec["band"])
        self.assertGreater(len(bands), 1, "tama.clip renders in bands")
        self.assertEqual(sorted(bands, key=lambda b: (b[1], b[0])), bands)
        self.assertEqual(len(bands), len({tuple(b) for b in bands}), "each band's records stay together")

    def test_region_render_traces_its_window(self) -> None:
        region = (100, 300, 612, 700)
        trace = self.tmp / "r.jsonl"
        self.assertEqual(self._render(None, region=region), self._render(trace, region=region))
        records = [json.loads(line) for line in trace.read_text(encoding="utf-8").splitlines()]
        self.assertEqual({tuple(region)}, {tuple(r["band"]) for r in records})


def _tool(name: str):
    import importlib.util

    spec = importlib.util.spec_from_file_location(f"_tool_{name}", PROJECT_ROOT / "tools" / f"{name}.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class RenderCorpusToolTests(unittest.TestCase):
    def test_regions_stay_inside_the_canvas(self) -> None:
        corpus = _tool("render_corpus")
        for size in ((1, 1), (3, 2), (100, 256), (1600, 1200), (2894, 4093), (257, 513)):
            width, height = size
            got = dict(corpus.regions(width, height))
            self.assertEqual({"quarter", "tile_row_band", "column"}, set(got), size)
            for name, (x0, y0, x1, y1) in got.items():
                self.assertTrue(0 <= x0 < x1 <= width and 0 <= y0 < y1 <= height, (size, name))
            self.assertEqual(1, got["column"][2] - got["column"][0])
            if height > 256:
                y0, y1 = got["tile_row_band"][1], got["tile_row_band"][3]
                self.assertTrue(any(y0 < row < y1 for row in range(256, height, 256)), size)

    def test_compare_reports_each_changed_record(self) -> None:
        corpus = _tool("render_corpus")
        want = {
            "canvas": [10, 10], "composite": "a" * 64,
            "regions": [{"name": "quarter", "rect": [0, 0, 5, 5], "sha256": "b" * 64}],
            "folders": [{"main_id": 3, "name": "f", "sha256": "c" * 64}],
            "warnings": ["tmiki: old"],
        }
        self.assertEqual([], corpus.compare("k", want, json.loads(json.dumps(want))))
        got = json.loads(json.dumps(want))
        got["composite"] = "d" * 64
        got["regions"][0]["sha256"] = "e" * 64
        got["folders"][0] = {"main_id": 3, "name": "f", "error": "ValueError: x"}
        got["warnings"] = ["tmiki: new"]
        lines = corpus.compare("k", want, got)
        self.assertEqual(5, len(lines), lines)
        self.assertTrue(lines[0].startswith("composite"))

    def test_discover_groups_files(self) -> None:
        corpus = _tool("render_corpus")
        root = PROJECT_ROOT / ".test-tmp"
        root.mkdir(exist_ok=True)
        tmp = Path(tempfile.mkdtemp(prefix="tmi-corpus-", dir=root))
        try:
            for rel in ("a.clip", "csp515/b.clip", "additional_examples/probe_c.clip",
                        "additional_examples/extracted/z/z/page.clip", "notes.txt"):
                (tmp / rel).parent.mkdir(parents=True, exist_ok=True)
                (tmp / rel).write_bytes(b"")
            found = {key: group for key, (_path, group) in corpus.discover(tmp).items()}
            self.assertEqual({
                "example/a.clip": "example",
                "example/csp515/b.clip": "csp515",
                "example/additional_examples/probe_c.clip": "probe",
                "example/additional_examples/extracted/z/z/page.clip": "comic",
            }, found)
        finally:
            shutil.rmtree(tmp, ignore_errors=True)
            try:
                root.rmdir()
            except OSError:
                pass

    def test_committed_corpus_is_well_formed(self) -> None:
        corpus = _tool("render_corpus")
        data = json.loads(corpus.CORPUS_PATH.read_text(encoding="utf-8"))
        self.assertEqual(1, data["version"])
        self.assertIn("example/tama.clip", data["files"])
        self.assertIn("example/kakenhi-kaguya.mdp", data["files"])
        for key, record in data["files"].items():
            self.assertTrue(key.startswith("example/") and key.endswith((".clip", ".mdp")), key)
            if key.endswith(".mdp"):
                self.assertEqual("mdp", record["group"], key)
                self.assertEqual(["transparent"], [r["name"] for r in record["regions"]], key)
                self.assertTrue(record["layers"], key)
                continue
            self.assertIn(record["group"], ("example", "csp515", "probe", "comic"), key)
            self.assertEqual(3, len(record["regions"]), key)


class RenderDiffToolTests(unittest.TestCase):
    def test_configs(self) -> None:
        diff = _tool("render_diff")
        self.assertEqual(("rust", {}), diff.parse_config("rust"))
        self.assertEqual(("python", {}), diff.parse_config("python"))
        self.assertEqual(("python", {"A": "1", "B": "x=y"}), diff.parse_config("python,A=1,B=x=y"))
        for bad in ("nonsense", "A=1", "rust,B"):
            with self.assertRaises(Exception):
                diff.parse_config(bad)

    def test_first_divergence(self) -> None:
        import contextlib
        import io

        diff = _tool("render_diff")
        rec = {"phase": "apply", "band": [0, 0, 4, 4], "main_id": 1, "window": [0, 0, 4, 4],
               "bounds": [0, 0, 4, 4], "sha256": "0" * 64}
        a = [dict(rec, main_id=i) for i in range(5)]
        b = [dict(r) for r in a]
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            self.assertIsNone(diff.compare_traces(a, b))
            b[3]["sha256"] = "1" * 64
            self.assertEqual(3, diff.compare_traces(a, b))
            self.assertEqual(5, diff.compare_traces(a, a + [rec]))
        self.assertIn("(pixels)", out.getvalue())
        self.assertIn("(structure)", out.getvalue())


class ExtractExamplesToolTests(unittest.TestCase):
    def setUp(self) -> None:
        root = PROJECT_ROOT / ".test-tmp"
        root.mkdir(exist_ok=True)
        self.tmp = Path(tempfile.mkdtemp(prefix="tmi-extract-", dir=root))
        self.source = self.tmp / "example" / "additional_examples"
        self.source.mkdir(parents=True)

    def tearDown(self) -> None:
        shutil.rmtree(self.tmp, ignore_errors=True)
        try:
            (PROJECT_ROOT / ".test-tmp").rmdir()
        except OSError:
            pass

    def _zip(self, name: str, members: dict[str, bytes]) -> None:
        import zipfile

        with zipfile.ZipFile(self.source / name, "w") as zf:
            for member, data in members.items():
                zf.writestr(member, data)

    def test_each_zip_goes_into_its_own_new_directory(self) -> None:
        import contextlib
        import io

        tool = _tool("extract_examples")
        self._zip("good.zip", {"good/page.clip": b"CSF", "good/note.cmc": b"x"})
        self._zip("bad.zip", {"bad/ok.clip": b"1", "../escape.clip": b"2"})
        with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            status = tool.main(["--examples", str(self.tmp / "example")])
        self.assertEqual(1, status)
        out = self.source / "extracted"
        self.assertEqual(b"CSF", (out / "good" / "good" / "page.clip").read_bytes())
        self.assertFalse((out / "bad").exists())
        self.assertFalse((out / "bad.partial").exists())
        self.assertFalse((self.source / "escape.clip").exists())
        self.assertFalse((out / "escape.clip").exists())
        (out / "good" / "good" / "page.clip").write_bytes(b"edited")
        with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            tool.main(["--examples", str(self.tmp / "example")])
        self.assertEqual(b"edited", (out / "good" / "good" / "page.clip").read_bytes(), "unpacked once")


if __name__ == "__main__":
    unittest.main()
