from __future__ import annotations

import functools
import io
import os
import shutil
import struct
import subprocess
import sys
import tempfile
import unittest
import zlib
from pathlib import Path

from PIL import Image


PROJECT_ROOT = Path(__file__).resolve().parents[1]


class TmiBugfixRegressionTests(unittest.TestCase):
    """One regression test per recently fixed bug. Conventions (temp repo,
    env vars, CLI invocation, tiny PNG assets) mirror tests/test_cli_regressions.py."""

    def setUp(self) -> None:
        root = PROJECT_ROOT / ".test-tmp"
        root.mkdir(exist_ok=True)
        self.tmp = Path(tempfile.mkdtemp(prefix="tmi-bugfix-", dir=root))
        self.env = os.environ.copy()
        self.env["PYTHONPATH"] = str(PROJECT_ROOT) + os.pathsep + self.env.get("PYTHONPATH", "")
        self.env["TMI_AUTHOR_NAME"] = "Tester"
        self.env["TMI_AUTHOR_EMAIL"] = "tester@example.com"

    def tearDown(self) -> None:
        shutil.rmtree(self.tmp, ignore_errors=True)
        try:
            (PROJECT_ROOT / ".test-tmp").rmdir()
        except OSError:
            pass

    def run_tmi(self, repo: Path, *args: str, check: bool = True) -> subprocess.CompletedProcess[str]:
        result = subprocess.run(
            [sys.executable, "-m", "tmi", *args],
            cwd=repo,
            env=self.env,
            text=True,
            capture_output=True,
            timeout=30,
        )
        if check and result.returncode:
            self.fail(
                f"tmi {' '.join(args)} failed with {result.returncode}\n"
                f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}"
            )
        return result

    def run_tmi_binary(self, repo: Path, *args: str, check: bool = True) -> subprocess.CompletedProcess[bytes]:
        """Same as run_tmi but keeps stdout as raw bytes (for `show` of a blob)."""
        result = subprocess.run(
            [sys.executable, "-m", "tmi", *args],
            cwd=repo,
            env=self.env,
            capture_output=True,
            timeout=30,
        )
        if check and result.returncode:
            self.fail(
                f"tmi {' '.join(args)} failed with {result.returncode}\n"
                f"stderr:\n{result.stderr.decode('utf-8', 'replace')}"
            )
        return result

    # 1. A failed switch (untracked file would be overwritten) must leave HEAD put.
    def test_failed_switch_does_not_move_head(self) -> None:
        repo = self.tmp / "repo"
        repo.mkdir()
        write_png(repo / "a.png", (255, 0, 0, 255))

        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "a.png")
        self.run_tmi(repo, "commit", "-m", "base")
        self.run_tmi(repo, "switch", "-c", "feature")
        write_png(repo / "b.png", (0, 255, 0, 255))
        self.run_tmi(repo, "add", "b.png")
        self.run_tmi(repo, "commit", "-m", "add b")
        self.run_tmi(repo, "switch", "main")
        self.assertFalse((repo / "b.png").exists())

        # An untracked b.png with different bytes now blocks switching to feature
        # (which carries a committed b.png).
        write_png(repo / "b.png", (0, 0, 255, 255))
        result = self.run_tmi(repo, "switch", "feature", check=False)

        self.assertNotEqual(0, result.returncode)
        head = (repo / ".tmi" / "HEAD").read_text(encoding="utf-8").strip()
        self.assertEqual("ref: refs/heads/main", head)

    # 2. MDP2 composite hides children of an invisible folder.
    def test_mdp2_composite_hides_children_of_invisible_folder(self) -> None:
        from tmi.formats.mdp2 import parse_mdp2, render_composite_rgba

        MAGIC = b"mdipack\x00"
        PAC_HEADER_SIZE = 132

        def pac_record(name: str, payload: bytes) -> bytes:
            header = bytearray(PAC_HEADER_SIZE)
            header[0:4] = b"PAC "
            struct.pack_into("<IIII", header, 4, PAC_HEADER_SIZE + len(payload), 0, len(payload), len(payload))
            nb = name.encode("ascii")
            header[68:68 + len(nb)] = nb
            return bytes(header) + payload

        def tile_payload_red() -> bytes:
            tile = bytes([0, 0, 255, 255]) * (128 * 128)  # BGRA red
            comp = zlib.compress(tile)
            buf = struct.pack("<IIIIII", 1, 128, 0, 0, 0, len(comp)) + comp
            return buf + b"\x00" * ((-len(buf)) % 4)

        def build(folder_visible: str) -> bytes:
            xml = (
                '<mdipack width="128" height="128" dpi="72"><Layers>'
                f'<Layer name="folder" id="1" parentId="-1" type="folder" visible="{folder_visible}" alpha="255"/>'
                '<Layer name="child" id="2" parentId="1" type="32bpp" visible="true" alpha="255" '
                'width="128" height="128" bin="bin00" mode="normal"/>'
                '</Layers></mdipack>'
            ).encode("utf-8")
            pack = pac_record("bin00", tile_payload_red())
            return MAGIC + struct.pack("<III", 0, len(xml), len(pack)) + xml + pack

        hidden = render_composite_rgba(parse_mdp2(build("false")))
        self.assertEqual((255, 255, 255, 255), hidden.getpixel((10, 10)))

        shown = render_composite_rgba(parse_mdp2(build("true")))
        self.assertEqual((255, 0, 0, 255), shown.getpixel((10, 10)))

    # 3. gc --prune=now must keep the commit a detached HEAD points at.
    def test_gc_prune_now_keeps_detached_head_commit(self) -> None:
        repo = self.tmp / "repo"
        repo.mkdir()
        write_png(repo / "a.png", (255, 0, 0, 255))

        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "a.png")
        self.run_tmi(repo, "commit", "-m", "c1")
        write_png(repo / "a.png", (0, 255, 0, 255))
        self.run_tmi(repo, "add", "a.png")
        self.run_tmi(repo, "commit", "-m", "c2")

        oids = [line.strip() for line in self.run_tmi(repo, "log", "--format=%H").stdout.splitlines() if line.strip()]
        first_commit = oids[-1]  # log lists newest-first, so the last line is c1
        self.assertEqual(64, len(first_commit))

        self.run_tmi(repo, "checkout", first_commit)
        self.run_tmi(repo, "gc", "--prune", "now")

        self.run_tmi(repo, "log", "-n", "1")  # must still succeed
        obj = repo / ".tmi" / "objects" / first_commit[:2] / first_commit[2:]
        self.assertTrue(obj.exists())

    # 4. rm refuses to delete a staged-but-never-committed file.
    def test_rm_refuses_staged_uncommitted_file(self) -> None:
        repo = self.tmp / "repo"
        repo.mkdir()
        write_png(repo / "a.png", (255, 0, 0, 255))

        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "a.png")
        self.run_tmi(repo, "commit", "-m", "base")

        write_png(repo / "c.png", (0, 255, 0, 255))
        self.run_tmi(repo, "add", "c.png")

        blocked = self.run_tmi(repo, "rm", "c.png", check=False)
        self.assertNotEqual(0, blocked.returncode)
        self.assertTrue((repo / "c.png").exists())

        # --cached only unstages, keeping the file on disk.
        self.run_tmi(repo, "rm", "--cached", "c.png")
        self.assertTrue((repo / "c.png").exists())

        # Re-add, then -f is allowed to delete it.
        self.run_tmi(repo, "add", "c.png")
        self.run_tmi(repo, "rm", "-f", "c.png")
        self.assertFalse((repo / "c.png").exists())

    # 5. reset --soft preserves already-staged entries.
    def test_reset_soft_preserves_staged_entries(self) -> None:
        repo = self.tmp / "repo"
        repo.mkdir()
        write_png(repo / "a.png", (255, 0, 0, 255))

        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "a.png")
        self.run_tmi(repo, "commit", "-m", "c1")

        write_png(repo / "a.png", (0, 255, 0, 255))
        v2_bytes = (repo / "a.png").read_bytes()
        self.run_tmi(repo, "add", "a.png")
        self.run_tmi(repo, "commit", "-m", "c2")

        write_png(repo / "d.png", (0, 0, 255, 255))
        self.run_tmi(repo, "add", "d.png")
        self.run_tmi(repo, "reset", "--soft", "HEAD~1")
        self.run_tmi(repo, "commit", "-m", "x")

        shown = self.run_tmi_binary(repo, "show", "HEAD:a.png")
        self.assertEqual(v2_bytes, shown.stdout)
        self.run_tmi_binary(repo, "show", "HEAD:d.png")  # must succeed (binary blob output)

    # 6. stash apply must not revert files it never touched.
    def test_stash_apply_does_not_revert_untouched_files(self) -> None:
        repo = self.tmp / "repo"
        repo.mkdir()
        write_png(repo / "a.png", (255, 0, 0, 255))
        write_png(repo / "b.png", (255, 0, 0, 255))

        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "a.png")
        self.run_tmi(repo, "add", "b.png")
        self.run_tmi(repo, "commit", "-m", "c1")

        write_png(repo / "a.png", (0, 255, 0, 255))
        a_v2 = (repo / "a.png").read_bytes()
        self.run_tmi(repo, "stash", "push")

        write_png(repo / "b.png", (0, 0, 255, 255))
        b_v2b = (repo / "b.png").read_bytes()
        self.run_tmi(repo, "add", "b.png")
        self.run_tmi(repo, "commit", "-m", "b2")

        self.run_tmi(repo, "stash", "apply")

        self.assertEqual(b_v2b, (repo / "b.png").read_bytes())
        self.assertEqual(a_v2, (repo / "a.png").read_bytes())

    # 7. ObjectStore.write_raw rejects corrupt/mismatched bytes.
    def test_object_store_write_raw_rejects_corrupt_bytes(self) -> None:
        from tmi.objects import ObjectStore

        src_dir = self.tmp / "src"
        dst_dir = self.tmp / "dst"
        store = ObjectStore(src_dir)
        other = ObjectStore(dst_dir)

        oid = store.write("blob", b"hello")
        other_oid = store.write("blob", b"world")
        self.assertNotEqual(oid, other_oid)

        with self.assertRaises(ValueError):
            other.write_raw(oid, b"not-zlib")
        with self.assertRaises(ValueError):
            other.write_raw(oid, store.path_for(other_oid).read_bytes())

        other.write_raw(oid, store.path_for(oid).read_bytes())
        self.assertEqual(b"hello", other.read_blob(oid))

    # 8. native.replace_layer_pixels returns valid JPEG bytes for jpeg targets.
    def test_replace_layer_pixels_reencodes_jpeg(self) -> None:
        from tmi.native import replace_layer_pixels

        jpg = self.tmp / "x.jpg"
        Image.new("RGB", (8, 8), (10, 20, 30)).save(jpg, format="JPEG")

        buf = io.BytesIO()
        Image.new("RGBA", (8, 8), (255, 0, 0, 255)).save(buf, format="PNG")
        png_data = buf.getvalue()

        result = replace_layer_pixels(jpg, "jpeg", "image:0", png_data)
        self.assertEqual(b"\xff\xd8", result[:2])  # JPEG SOI marker
        reopened = Image.open(io.BytesIO(result))
        reopened.load()
        self.assertEqual("JPEG", reopened.format)

    # 9. commit -a drops a stale staged entry when the worktree was reverted.
    def test_commit_all_drops_stale_staged_entry(self) -> None:
        repo = self.tmp / "repo"
        repo.mkdir()
        write_png(repo / "a.png", (255, 0, 0, 255))
        v1_bytes = (repo / "a.png").read_bytes()

        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "a.png")
        self.run_tmi(repo, "commit", "-m", "c1")

        write_png(repo / "a.png", (0, 255, 0, 255))
        self.run_tmi(repo, "add", "a.png")
        # Revert the worktree to exactly HEAD's bytes: the staged v2 entry is now stale.
        (repo / "a.png").write_bytes(v1_bytes)

        result = self.run_tmi(repo, "commit", "-a", "-m", "x", check=False)
        self.assertNotEqual(0, result.returncode)
        self.assertIn("nothing to commit", result.stderr)

        status = self.run_tmi(repo, "status", "--porcelain")
        self.assertEqual("", status.stdout.strip())

    # 10. branch -d with a different-case name must not delete the current branch.
    @unittest.skipUnless(os.name == "nt", "requires a case-insensitive filesystem")
    def test_branch_delete_case_insensitive_guards_current_branch(self) -> None:
        repo = self.tmp / "repo"
        repo.mkdir()
        write_png(repo / "a.png", (255, 0, 0, 255))

        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "a.png")
        self.run_tmi(repo, "commit", "-m", "base")

        result = self.run_tmi(repo, "branch", "-d", "MAIN", check=False)
        self.assertNotEqual(0, result.returncode)
        self.assertTrue((repo / ".tmi" / "refs" / "heads" / "main").exists())

    # 11. restore without --source restores the staged content, not HEAD.
    def test_restore_uses_staged_content(self) -> None:
        repo = self.tmp / "repo"
        repo.mkdir()
        write_png(repo / "a.png", (255, 0, 0, 255))

        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "a.png")
        self.run_tmi(repo, "commit", "-m", "c1")

        write_png(repo / "a.png", (0, 255, 0, 255))
        v2_bytes = (repo / "a.png").read_bytes()
        self.run_tmi(repo, "add", "a.png")
        write_png(repo / "a.png", (0, 0, 255, 255))  # v3, unstaged

        self.run_tmi(repo, "restore", "a.png")
        self.assertEqual(v2_bytes, (repo / "a.png").read_bytes())

    # 12. Progress (issue #9): phases go to stderr only on a TTY or with
    # TMI_PROGRESS=1; a captured (non-TTY) stderr stays silent by default.
    def _progress_flow(self, repo: Path) -> dict[str, subprocess.CompletedProcess[str]]:
        from tests.test_cli_regressions import write_mdp2_layers

        repo.mkdir()
        art = repo / "art.mdp"
        write_mdp2_layers(art, [(200, 0, 0, 255), (0, 0, 200, 255)])
        out: dict[str, subprocess.CompletedProcess[str]] = {}
        self.run_tmi(repo, "init")
        out["add"] = self.run_tmi(repo, "add", "art.mdp")
        self.run_tmi(repo, "commit", "-m", "base")
        self.run_tmi(repo, "branch", "other")
        write_mdp2_layers(art, [(0, 200, 0, 255), (0, 0, 200, 255)])
        out["commit -a"] = self.run_tmi(repo, "commit", "-a", "-m", "layer1")
        self.run_tmi(repo, "switch", "other")
        write_mdp2_layers(art, [(200, 0, 0, 255), (200, 200, 0, 255)])
        self.run_tmi(repo, "commit", "-a", "-m", "layer2")
        self.run_tmi(repo, "switch", "main")
        out["merge"] = self.run_tmi(repo, "merge", "other", "-m", "merge")
        out["diff --html"] = self.run_tmi(repo, "diff", "--html", "--no-open", "--out", "d", "HEAD~1", "HEAD")
        return out

    def test_progress_is_silent_when_stderr_is_not_a_tty(self) -> None:
        self.env.pop("TMI_PROGRESS", None)
        results = self._progress_flow(self.tmp / "repo")
        self.assertIn("Merged other", results["merge"].stdout)
        for name, result in results.items():
            self.assertEqual("", result.stderr, f"{name} wrote to a non-TTY stderr")

    def test_progress_env_prints_phases_to_stderr(self) -> None:
        self.env["TMI_PROGRESS"] = "1"
        results = self._progress_flow(self.tmp / "repo")
        expected = {
            "add": ["parsing art.mdp", "decoding layers", "rendering composite", "writing source blob"],
            "commit -a": ["decoding layers", "rendering composite"],
            "merge": ["merging layers of art.mdp", "writing merged layers 1/1", "updating working tree"],
            "diff --html": ["comparing composites of art.mdp", "writing HTML report"],
        }
        for name, phases in expected.items():
            err = results[name].stderr
            for phase in phases:
                self.assertIn(phase, err, f"{name}: missing phase {phase!r}")
            self.assertRegex(err, r"tmiki: \[\s*\d+\.\ds\] ", f"{name}: no elapsed time")
            self.assertRegex(err.splitlines()[-1], r"^tmiki: done in \d+\.\ds \(", f"{name}: no closing breakdown")
            self.assertNotIn("tmiki: [", results[name].stdout, f"{name}: progress leaked to stdout")
        self.assertIn("staged art.mdp", results["add"].stdout)

    def test_progress_tty_spinner_and_opt_out(self) -> None:
        from unittest import mock

        from tmi.cli import Progress

        class FakeTTY(io.StringIO):
            def isatty(self) -> bool:
                return True

        with mock.patch.dict(os.environ, {"TMI_PROGRESS": ""}), mock.patch("sys.stdout", FakeTTY()):
            stream = FakeTTY()
            progress = Progress(stream=stream, interval=60)
            progress("decoding layers 1/2")
            progress.done()
            text = stream.getvalue()
            self.assertIn("tmiki: decoding layers 1/2 (", text)
            self.assertRegex(text, r"\(\d+\.\ds\)")
            self.assertTrue(text.endswith("\r"), "spinner line not erased")
        # issue #26: no spinner when stdout is not a terminal either
        with mock.patch.dict(os.environ, {"TMI_PROGRESS": ""}), mock.patch("sys.stdout", io.StringIO()):
            stream = FakeTTY()
            progress = Progress(stream=stream, interval=60)
            progress("decoding layers 1/2")
            progress.done()
            self.assertEqual("", stream.getvalue())
        with mock.patch.dict(os.environ, {"TMI_PROGRESS": "0"}), mock.patch("sys.stdout", FakeTTY()):
            stream = FakeTTY()
            progress = Progress(stream=stream, interval=60)
            progress("decoding layers 1/2")
            progress.done()
            self.assertEqual("", stream.getvalue())
        with mock.patch.dict(os.environ, {"TMI_PROGRESS": ""}):
            stream = io.StringIO()
            progress = Progress(stream=stream)
            progress("decoding layers 1/2")
            progress.done()
            self.assertEqual("", stream.getvalue())

    def test_progress_resumes_a_run_the_front_door_hands_over(self) -> None:
        # Issue #26: a pull the Rust front door hands to Python after its
        # fetch passes its clock and phases on (TMI_PROGRESS_RESUME).
        from unittest import mock

        from tmi.cli import Progress

        state = ('{"elapsed": 5.0, "phase": "copying object", "phase_elapsed": 1.0, '
                 '"times": [["enumerating remote", 2.0]], "last_line_phase": "copying object", "last_line_ago": 0.5}')
        with mock.patch.dict(os.environ, {"TMI_PROGRESS": "1", "TMI_PROGRESS_RESUME": state}):
            stream = io.StringIO()
            progress = Progress(stream=stream)
            self.assertNotIn("TMI_PROGRESS_RESUME", os.environ)
            progress("copying object [9/9] abc")  # same phase, less than a second on: no line
            progress("merging layers of a.clip")
            progress.done()
        lines = stream.getvalue().splitlines()
        self.assertEqual(2, len(lines), lines)
        self.assertRegex(lines[0], r"^tmiki: \[\s*5\.\ds\] merging layers of a\.clip$")
        self.assertRegex(lines[1], r"^tmiki: done in 5\.\ds \(enumerating remote 2\.0s, copying object 1\.\ds, merging layers 0\.0s\)$")


class PixelLimitTests(unittest.TestCase):
    """Issue #17: tmi's own image-size limit (core.maxImagePixels) replaces
    Pillow's decompression-bomb guard."""

    GiB = 1 << 30

    def setUp(self) -> None:
        from unittest import mock

        root = PROJECT_ROOT / ".test-tmp"
        root.mkdir(exist_ok=True)
        self.tmp = Path(tempfile.mkdtemp(prefix="tmi-pixels-", dir=root))
        self.user_home = self.tmp / "user-config"
        self.user_home.mkdir()
        env = {
            "TMI_CONFIG_HOME": str(self.user_home),
            "TMI_AUTHOR_NAME": "Tester",
            "TMI_AUTHOR_EMAIL": "tester@example.com",
            "TMI_MAX_IMAGE_PIXELS": "",
            "TMI_PROGRESS": "0",
        }
        patcher = mock.patch.dict(os.environ, env)
        patcher.start()
        self.addCleanup(patcher.stop)
        self.env = os.environ.copy()
        self.env["PYTHONPATH"] = str(PROJECT_ROOT) + os.pathsep + self.env.get("PYTHONPATH", "")

    def tearDown(self) -> None:
        from tmi import pixel_limit

        pixel_limit.set_repo_dir(None)
        pixel_limit.apply_to_pil()
        shutil.rmtree(self.tmp, ignore_errors=True)
        try:
            (PROJECT_ROOT / ".test-tmp").rmdir()
        except OSError:
            pass

    def run_tmi(self, repo: Path, *args: str, check: bool = True) -> subprocess.CompletedProcess[str]:
        result = subprocess.run(
            [sys.executable, "-m", "tmi", *args],
            cwd=repo, env=self.env, text=True, capture_output=True, timeout=120,
        )
        if check and result.returncode:
            self.fail(f"tmi {' '.join(args)} failed\nstdout:\n{result.stdout}\nstderr:\n{result.stderr}")
        return result

    def main_in(self, repo: Path, *args: str) -> tuple[int, str, str]:
        """Run the CLI in-process (so total_ram_bytes can be mocked)."""
        import contextlib
        import warnings

        from tmi.cli import main

        out, err = io.StringIO(), io.StringIO()
        cwd = os.getcwd()
        os.chdir(repo)
        try:
            with warnings.catch_warnings(record=True) as caught, contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
                warnings.simplefilter("always")
                code = main(list(args))
        finally:
            os.chdir(cwd)
        shown = [w for w in caught if "DecompressionBomb" in type(w.message).__name__ or "decompression bomb" in str(w.message)]
        self.assertEqual([], shown, "a decompression-bomb warning reached the user")
        return code, out.getvalue(), err.getvalue()

    def test_default_comes_from_memory_with_a_floor(self) -> None:
        from tmi import pixel_limit

        self.assertEqual(178_956_970, pixel_limit.FLOOR)
        self.assertEqual(pixel_limit.FLOOR, pixel_limit.default_limit(None))
        self.assertEqual(pixel_limit.FLOOR, pixel_limit.default_limit(8 * self.GiB))
        expected = (64 * self.GiB // 2) // pixel_limit.LAYERED_BYTES_PER_PIXEL
        self.assertGreater(expected, pixel_limit.FLOOR)
        self.assertEqual(expected, pixel_limit.default_limit(64 * self.GiB))
        # A single raster image needs far less memory per pixel than a layered
        # drawing composited as a whole, so its default is higher.
        self.assertEqual(pixel_limit.FLOOR, pixel_limit.default_limit(None, pixel_limit.RASTER))
        raster = (8 * self.GiB // 2) // pixel_limit.RASTER_BYTES_PER_PIXEL
        self.assertGreater(raster, pixel_limit.FLOOR)
        self.assertEqual(raster, pixel_limit.default_limit(8 * self.GiB, pixel_limit.RASTER))
        self.assertIsInstance(pixel_limit.total_ram_bytes(), (int, type(None)))

    def test_env_beats_repo_config_beats_user_config_beats_default(self) -> None:
        from unittest import mock

        from tmi import config as tmi_config
        from tmi import pixel_limit

        repo_dir = self.tmp / "repo" / ".tmi"
        repo_dir.mkdir(parents=True)
        with mock.patch.object(pixel_limit, "total_ram_bytes", return_value=64 * self.GiB):
            limit = pixel_limit.resolve(repo_dir)
            self.assertEqual(("default", (32 * self.GiB) // pixel_limit.LAYERED_BYTES_PER_PIXEL), (limit.source, limit.value))
            self.assertEqual("default, from 64.0 GB of memory", limit.origin())

            tmi_config.write_user_config({"version": 1, "core": {"maxImagePixels": "250000000"}})
            self.assertEqual(("user config", 250_000_000), (pixel_limit.resolve(repo_dir).source, pixel_limit.resolve(repo_dir).value))
            (repo_dir / "config").write_text('{"version": 1, "core": {"maxImagePixels": "0"}}', encoding="utf-8")
            limit = pixel_limit.resolve(repo_dir)
            self.assertEqual(("repo config", 0), (limit.source, limit.value))
            self.assertEqual("no limit, from the repo config", limit.origin())
            with mock.patch.dict(os.environ, {"TMI_MAX_IMAGE_PIXELS": "1234"}):
                self.assertEqual(("env", 1234), (pixel_limit.resolve(repo_dir).source, pixel_limit.resolve(repo_dir).value))
            with mock.patch.dict(os.environ, {"TMI_MAX_IMAGE_PIXELS": "-5"}):
                with self.assertRaisesRegex(RuntimeError, "whole number of pixels"):
                    pixel_limit.resolve(repo_dir)

    def test_zero_means_no_limit_in_pillow(self) -> None:
        from unittest import mock

        from tmi import pixel_limit

        with mock.patch.dict(os.environ, {"TMI_MAX_IMAGE_PIXELS": "0"}):
            pixel_limit.apply_to_pil()
            self.assertIsNone(Image.MAX_IMAGE_PIXELS)
            pixel_limit.check_pixels(100_000, 100_000)  # no limit: no error
        with mock.patch.dict(os.environ, {"TMI_MAX_IMAGE_PIXELS": "300000000"}):
            pixel_limit.apply_to_pil()
            self.assertEqual(300_000_000, Image.MAX_IMAGE_PIXELS)

    def _poster(self) -> Path:
        repo = self.tmp / "repo"
        repo.mkdir()
        self.run_tmi(repo, "init")
        # The issue's A0-class canvas: 182 Mpx, ~47 KB on disk.
        Image.new("1", (14000, 13000), 1).save(repo / "poster.png")
        return repo

    def test_large_poster_adds_under_a_memory_based_default(self) -> None:
        from unittest import mock

        from tmi import pixel_limit

        repo = self._poster()
        with mock.patch.object(pixel_limit, "total_ram_bytes", return_value=4 * self.GiB):
            # 4 GB -> the floor (178,956,970), which the 182 Mpx poster exceeds.
            code, out, err = self.main_in(repo, "add", "poster.png")
            self.assertEqual(1, code)
            self.assertIn("canvas too large: 182000000 pixels (limit 178956970 for images, based on 4.0 GB of memory)", err)
            self.assertIn("may not have enough memory", err)
            self.assertIn("tmiki config core.maxImagePixels 182000000", err)
        with mock.patch.object(pixel_limit, "total_ram_bytes", return_value=8 * self.GiB):
            # 8 GB: a raster image of this size fits (20 B/px), while a layered
            # canvas of the same size still gets the floor (100 B/px).
            code, out, err = self.main_in(repo, "config", "core.maxImagePixels")
            raster = (4 * self.GiB) // pixel_limit.RASTER_BYTES_PER_PIXEL
            self.assertEqual(
                f"{pixel_limit.FLOOR} for CLIP/MDP canvases, {raster} for other images (default, from 8.0 GB of memory)\n",
                out,
            )
            with self.assertRaisesRegex(pixel_limit.CanvasTooLargeError, "limit 178956970 for CLIP/MDP canvases"):
                pixel_limit.check_pixels(14000, 13000, "poster.clip")
            code, out, err = self.main_in(repo, "add", "poster.png")
            self.assertEqual(0, code, err)
            self.assertIn("staged poster.png", out)
            self.assertNotIn("Decompression", err)

    def test_over_limit_image_fails_with_a_tmi_message_and_no_warning(self) -> None:
        repo = self.tmp / "repo"
        repo.mkdir()
        self.run_tmi(repo, "init")
        Image.new("1", (2000, 1000), 1).save(repo / "wide.png")
        Image.new("RGBA", (8, 8), (1, 2, 3, 255)).save(repo / "small.png")

        self.env["TMI_MAX_IMAGE_PIXELS"] = "1000000"
        failed = self.run_tmi(repo, "add", "wide.png", check=False)
        self.assertEqual(1, failed.returncode)
        self.assertEqual(
            "tmiki: error: wide.png: canvas too large: 2000000 pixels (limit 1000000, set by TMI_MAX_IMAGE_PIXELS). "
            "Raise it with TMI_MAX_IMAGE_PIXELS=2000000  (0 = no limit)\n",
            failed.stderr,
        )
        self.assertIn("1000000 (from TMI_MAX_IMAGE_PIXELS)", self.run_tmi(repo, "config", "core.maxImagePixels").stdout)

        self.env["TMI_MAX_IMAGE_PIXELS"] = ""
        self.run_tmi(repo, "config", "core.maxImagePixels", "1_500_000")
        failed = self.run_tmi(repo, "add", "wide.png", check=False)
        self.assertEqual(1, failed.returncode)
        self.assertIn("limit 1500000, set by core.maxImagePixels in the repo config", failed.stderr)
        self.assertIn("tmiki config core.maxImagePixels 2000000", failed.stderr)
        self.assertNotIn("Traceback", failed.stderr)
        self.assertNotIn("Decompression", failed.stderr)
        self.run_tmi(repo, "add", "small.png")

        bad = self.run_tmi(repo, "config", "core.maxImagePixels", "lots", check=False)
        self.assertEqual(1, bad.returncode)
        self.assertIn("whole number of pixels", bad.stderr)

        # A layered file over the limit is refused too, not tracked as an
        # opaque blob ("could not parse ... as mdp2").
        from tests.test_cli_regressions import write_mdp2_layers

        write_mdp2_layers(repo / "art.mdp", [(255, 0, 0, 255)])
        self.env["TMI_MAX_IMAGE_PIXELS"] = "10000"
        failed = self.run_tmi(repo, "add", "art.mdp", check=False)
        self.assertEqual(1, failed.returncode)
        self.assertIn("tmiki: error: art.mdp: canvas too large: 16384 pixels (limit 10000", failed.stderr)
        self.assertNotIn("opaque", failed.stderr)
        self.env["TMI_MAX_IMAGE_PIXELS"] = ""

        self.run_tmi(repo, "config", "core.maxImagePixels", "0")
        self.assertIn("0 (no limit, from the repo config)", self.run_tmi(repo, "config", "core.maxImagePixels").stdout)
        added = self.run_tmi(repo, "add", "wide.png")
        self.assertEqual("", added.stderr)


@functools.lru_cache(maxsize=None)
def _srgb_icc() -> bytes:
    # Cached: the profile carries its creation time.
    from PIL import ImageCms

    return ImageCms.ImageCmsProfile(ImageCms.createProfile("sRGB")).tobytes()


def _encoded(image: Image.Image, fmt: str, **params) -> bytes:
    buf = io.BytesIO()
    image.save(buf, format=fmt, **params)
    return buf.getvalue()


class RasterWriteBackTests(unittest.TestCase):
    """Issue #21: a PNG/JPEG that tmi writes back (merge, restore of its
    layer) keeps the replaced file's dpi, ICC profile, EXIF, colour mode and
    JPEG quantisation."""

    setUp = TmiBugfixRegressionTests.setUp
    tearDown = TmiBugfixRegressionTests.tearDown
    run_tmi = TmiBugfixRegressionTests.run_tmi

    DPI = (350, 350)

    def _save(self, image: Image.Image, path: Path) -> None:
        params = {"dpi": self.DPI}
        if image.mode != "L":
            params["icc_profile"] = _srgb_icc()
        if path.suffix == ".jpg":
            params["quality"] = 80
        image.save(path, **params)

    def _edit(self, path: Path, color, box) -> None:
        image = Image.open(path)
        image.load()
        image = image.convert(image.mode)  # a plain copy, not the JPEG/PNG object
        image.paste(color, box)
        self._save(image, path)

    def _metadata(self, path: Path) -> tuple:
        with Image.open(path) as image:
            image.load()
        dpi = tuple(round(v) for v in image.info["dpi"]) if "dpi" in image.info else None
        return image.format, image.mode, dpi, image.info.get("icc_profile")

    def _diverged_repo(self, files: dict[str, Image.Image], ours_box, theirs_box) -> Path:
        repo = self.tmp / "repo"
        repo.mkdir()
        for name, image in files.items():
            self._save(image, repo / name)
        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", *files)
        self.run_tmi(repo, "commit", "-m", "base")
        self.run_tmi(repo, "branch", "other")
        for name in files:
            self._edit(repo / name, (255, 0, 0) if files[name].mode != "L" else 0, ours_box)
        self.run_tmi(repo, "commit", "-a", "-m", "ours")
        self.run_tmi(repo, "switch", "other")
        for name in files:
            self._edit(repo / name, (0, 0, 255) if files[name].mode != "L" else 255, theirs_box)
        self.run_tmi(repo, "commit", "-a", "-m", "theirs")
        self.run_tmi(repo, "switch", "main")
        return repo

    def test_merge_keeps_dpi_icc_and_mode(self) -> None:
        files = {
            "a.png": Image.new("RGB", (64, 48), (200, 220, 240)),
            "g.png": Image.new("L", (64, 48), 128),
            "b.jpg": Image.new("RGB", (64, 48), (200, 220, 240)),
        }
        repo = self._diverged_repo(files, (0, 0, 16, 16), (48, 32, 64, 48))
        before = {name: self._metadata(repo / name) for name in files}
        from PIL import JpegImagePlugin

        jpeg_before = Image.open(repo / "b.jpg")
        tables, sampling = jpeg_before.quantization, JpegImagePlugin.get_sampling(jpeg_before)

        merged = self.run_tmi(repo, "merge", "other")
        self.assertIn("Merged other", merged.stdout)
        for name in files:
            self.assertEqual(before[name], self._metadata(repo / name), name)
        self.assertEqual(("PNG", "RGB", self.DPI, _srgb_icc()), before["a.png"])
        self.assertEqual("L", before["g.png"][1])

        a = Image.open(repo / "a.png")
        self.assertEqual((255, 0, 0), a.getpixel((0, 0)), "our edit")
        self.assertEqual((0, 0, 255), a.getpixel((63, 47)), "their edit")
        self.assertEqual((200, 220, 240), a.getpixel((32, 24)))
        g = Image.open(repo / "g.png")
        self.assertEqual((0, 255, 128), (g.getpixel((0, 0)), g.getpixel((63, 47)), g.getpixel((32, 24))))
        b = Image.open(repo / "b.jpg")
        self.assertEqual(tables, b.quantization, "JPEG keeps its quantisation tables")
        self.assertEqual(sampling, JpegImagePlugin.get_sampling(b), "and its subsampling")

    def test_restore_side_of_a_conflicted_raster_keeps_metadata(self) -> None:
        files = {"a.png": Image.new("RGB", (32, 32), (200, 220, 240))}
        repo = self._diverged_repo(files, (0, 0, 20, 20), (10, 10, 32, 32))
        before = self._metadata(repo / "a.png")
        merged = self.run_tmi(repo, "merge", "other", check=False)
        self.assertEqual(1, merged.returncode, merged.stdout + merged.stderr)

        # The layer route writes the pixels into the working (OURS) file.
        self.run_tmi(repo, "restore", "--theirs", "a.png:a")
        self.assertEqual(before, self._metadata(repo / "a.png"))
        restored = Image.open(repo / "a.png")
        self.assertEqual((0, 0, 255), restored.getpixel((15, 15)))
        self.assertEqual((200, 220, 240), restored.getpixel((5, 5)), "the whole image is THEIRS")

        self.run_tmi(repo, "restore", "--ours", "a.png:a")
        self.assertEqual(before, self._metadata(repo / "a.png"))
        self.assertEqual((255, 0, 0), Image.open(repo / "a.png").getpixel((15, 15)))

        # The whole-file route copies the side's committed file.
        self.run_tmi(repo, "restore", "--theirs", "a.png")
        self.assertEqual(before, self._metadata(repo / "a.png"))
        self.assertEqual((0, 0, 255), Image.open(repo / "a.png").getpixel((15, 15)))

    def test_in_place_layer_restore_keeps_metadata(self) -> None:
        repo = self.tmp / "repo"
        repo.mkdir()
        self._save(Image.new("RGB", (32, 32), (10, 20, 30)), repo / "a.png")
        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "a.png")
        self.run_tmi(repo, "commit", "-m", "base")
        before = self._metadata(repo / "a.png")
        self._edit(repo / "a.png", (255, 0, 0), (0, 0, 8, 8))
        self.run_tmi(repo, "restore", "--in-place", "--source", "HEAD", "a.png:a")
        self.assertEqual(before, self._metadata(repo / "a.png"))
        self.assertEqual((10, 20, 30), Image.open(repo / "a.png").getpixel((0, 0)))

    def test_jpeg_edited_on_both_sides_conflicts_with_a_jpeg_reason(self) -> None:
        # Issue #22: re-saving a detailed JPEG moves pixels all over it, so
        # edits far apart still overlap. The reason must say why; PNG merges.
        from PIL import ImageFilter

        noise = Image.effect_noise((96, 64), 60).filter(ImageFilter.GaussianBlur(1))
        detailed = Image.merge("RGB", (noise, noise.rotate(90, expand=False), noise.transpose(Image.Transpose.FLIP_LEFT_RIGHT)))
        repo = self._diverged_repo({"c.jpg": detailed, "d.png": detailed}, (0, 0, 16, 16), (80, 48, 96, 64))
        merged = self.run_tmi(repo, "merge", "other", check=False)
        self.assertEqual(1, merged.returncode, merged.stdout + merged.stderr)
        self.assertIn(
            "    c: JPEG changes every pixel slightly on each save, so it cannot be merged automatically; "
            "take one side with --ours or --theirs\n",
            merged.stdout,
        )
        self.assertNotIn("conflict d.png", merged.stdout)
        self.assertNotIn("overlapping pixel edits", merged.stdout)
        self.assertIn("c: JPEG changes every pixel slightly", self.run_tmi(repo, "status").stdout)

    def test_encode_raster_like_modes(self) -> None:
        from tmi.image_ops import encode_raster_like

        def rt(original: bytes, rgba: Image.Image, fmt: str = "png") -> Image.Image:
            out = Image.open(io.BytesIO(encode_raster_like(rgba, original, fmt)))
            out.load()
            return out

        gray = Image.linear_gradient("L").resize((32, 32))
        # L stays L; LA stays LA (opaque or not); a coloured pixel needs RGB.
        self.assertEqual("L", rt(_encoded(gray, "PNG"), gray.convert("RGBA")).mode)
        self.assertEqual("LA", rt(_encoded(gray.convert("LA"), "PNG"), gray.convert("RGBA")).mode)
        coloured = gray.convert("RGBA")
        coloured.putpixel((0, 0), (255, 0, 0, 255))
        self.assertEqual("RGB", rt(_encoded(gray, "PNG"), coloured).mode)
        # A source with alpha keeps it even when the result is opaque.
        rgb = Image.new("RGB", (16, 16), (1, 2, 3))
        self.assertEqual("RGBA", rt(_encoded(rgb.convert("RGBA"), "PNG"), rgb.convert("RGBA")).mode)
        # An RGB source and a translucent result: RGBA, nothing lost.
        translucent = rgb.convert("RGBA")
        translucent.putpixel((0, 0), (1, 2, 3, 100))
        self.assertEqual(translucent.tobytes(), rt(_encoded(rgb, "PNG"), translucent).convert("RGBA").tobytes())
        # P with transparency keeps its palette while the pixels fit it.
        pal = Image.new("P", (16, 16), 2)
        pal.putpalette([0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255])
        pal.putpixel((0, 0), 1)
        pal_png = _encoded(pal, "PNG", transparency=bytes([255, 255, 128, 0]))
        decoded = Image.open(io.BytesIO(pal_png)).convert("RGBA")
        out = rt(pal_png, decoded)
        self.assertEqual("P", out.mode)
        self.assertEqual(decoded.tobytes(), out.convert("RGBA").tobytes())
        foreign = decoded.copy()
        foreign.putpixel((1, 1), (9, 9, 9, 255))
        out = rt(pal_png, foreign)
        self.assertEqual("RGBA", out.mode, "a colour outside the palette, and the source had alpha")
        self.assertEqual(foreign.tobytes(), out.tobytes())
        # An RGB colour key (tRNS) is kept when the transparent pixels are the key.
        keyed = _encoded(rgb, "PNG", transparency=(1, 2, 3))
        keyed_rgba = Image.open(io.BytesIO(keyed)).convert("RGBA")
        keyed_rgba.putpixel((0, 0), (200, 0, 0, 255))
        out = rt(keyed, keyed_rgba)
        self.assertEqual(("RGB", (1, 2, 3)), (out.mode, out.info.get("transparency")))
        self.assertEqual(keyed_rgba.tobytes(), out.convert("RGBA").tobytes())
        # Colour-space and text chunks, dpi, ICC and EXIF are carried over.
        from PIL.PngImagePlugin import PngInfo

        info = PngInfo()
        info.add_text("Comment", "hello")
        info.add(b"gAMA", struct.pack(">I", 45455))
        exif = Image.Exif()
        exif[0x0131] = "painter"
        src = _encoded(rgb, "PNG", pnginfo=info, dpi=(300, 300), icc_profile=_srgb_icc(), exif=exif.tobytes())
        out = rt(src, rgb.convert("RGBA"))
        self.assertEqual(("RGB", "hello", _srgb_icc()), (out.mode, out.info.get("Comment"), out.info.get("icc_profile")))
        self.assertAlmostEqual(45455 / 100000, out.info.get("gamma"), places=5)
        self.assertEqual((300, 300), tuple(round(v) for v in out.info["dpi"]))
        self.assertEqual("painter", out.getexif().get(0x0131))
        # JPEG: grey stays grey; a CMYK file is written as RGB without its
        # (now wrong) CMYK profile; quantisation tables are kept.
        self.assertEqual("L", rt(_encoded(gray, "JPEG"), gray.convert("RGBA"), "jpeg").mode)
        cmyk_icc = bytes(16) + b"CMYK" + bytes(108)
        cmyk = _encoded(Image.new("CMYK", (16, 16), (0, 50, 100, 0)), "JPEG", icc_profile=cmyk_icc)
        out = rt(cmyk, Image.open(io.BytesIO(cmyk)).convert("RGBA"), "jpeg")
        self.assertEqual("RGB", out.mode)
        self.assertNotIn("icc_profile", out.info)
        q30 = _encoded(rgb, "JPEG", quality=30, dpi=(144, 144), icc_profile=_srgb_icc())
        out = rt(q30, rgb.convert("RGBA"), "jpeg")
        self.assertEqual(Image.open(io.BytesIO(q30)).quantization, out.quantization)
        self.assertEqual(((144, 144), _srgb_icc()), (out.info.get("dpi"), out.info.get("icc_profile")))


def write_png(path: Path, color: tuple[int, int, int, int]) -> None:
    Image.new("RGBA", (8, 8), color).save(path)


if __name__ == "__main__":
    unittest.main()
