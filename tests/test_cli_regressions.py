from __future__ import annotations

import io
import os
import re
import shutil
import struct
import subprocess
import sys
import tempfile
from tmiref import clip_io  # the Python write-back helpers that build test files
import unittest
import zlib
from pathlib import Path

from PIL import Image


PROJECT_ROOT = Path(__file__).resolve().parents[1]


def tmi_command() -> list[str]:
    """How ``run_tmi`` starts tmiki: ``python -m tmi``, or the executable
    ``TMI_BIN`` names (the Rust front door, rust/crates/tmi-cli; what it
    does not serve itself it hands to ``TMI_PYTHON``, set in setUp).
    ``python tools/run_tests.py --variants rustcli`` runs the suite that way."""
    binary = os.environ.get("TMI_BIN")
    return [binary] if binary else [sys.executable, "-m", "tmi"]


class TmiCliRegressionTests(unittest.TestCase):
    def setUp(self) -> None:
        root = PROJECT_ROOT / ".test-tmp"
        root.mkdir(exist_ok=True)
        self.tmp = Path(tempfile.mkdtemp(prefix="tmi-cli-", dir=root))
        self.env = os.environ.copy()
        self.env["PYTHONPATH"] = str(PROJECT_ROOT) + os.pathsep + self.env.get("PYTHONPATH", "")
        self.env["TMI_AUTHOR_NAME"] = "Tester"
        self.env["TMI_AUTHOR_EMAIL"] = "tester@example.com"
        self.env.setdefault("TMI_PYTHON", sys.executable)

    def tearDown(self) -> None:
        shutil.rmtree(self.tmp, ignore_errors=True)
        try:
            (PROJECT_ROOT / ".test-tmp").rmdir()
        except OSError:
            pass

    def run_tmi(self, repo: Path, *args: str, check: bool = True) -> subprocess.CompletedProcess[str]:
        result = subprocess.run(
            [*tmi_command(), *args],
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

    def test_switch_removes_files_absent_from_target_commit_and_clears_discarded_index(self) -> None:
        repo = self.tmp / "repo"
        repo.mkdir()
        write_png(repo / "a.png", (255, 0, 0, 255))

        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "a.png")
        self.run_tmi(repo, "commit", "-m", "add a")
        self.run_tmi(repo, "branch", "feature")
        self.run_tmi(repo, "switch", "feature")
        write_png(repo / "b.png", (0, 255, 0, 255))
        self.run_tmi(repo, "add", "b.png")
        self.run_tmi(repo, "switch", "main", "--discard")

        self.assertTrue((repo / "a.png").exists())
        self.assertFalse((repo / "b.png").exists())
        status = self.run_tmi(repo, "status")
        self.assertIn("working tree clean", status.stdout)

    def test_restore_staged_unstages_new_and_modified_paths(self) -> None:
        repo = self.tmp / "repo"
        repo.mkdir()
        write_png(repo / "a.png", (255, 0, 0, 255))

        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "a.png")
        # Unstaging a brand-new file returns it to untracked.
        out = self.run_tmi(repo, "restore", "--staged", "a.png")
        self.assertIn("unstaged a.png", out.stdout)
        status = self.run_tmi(repo, "status")
        self.assertIn("untracked  a.png", status.stdout)

        self.run_tmi(repo, "add", "a.png")
        self.run_tmi(repo, "commit", "-m", "add a")
        write_png(repo / "a.png", (0, 255, 0, 255))
        self.run_tmi(repo, "add", "a.png")
        # Unstaging a staged modification preserves the working-tree change.
        self.run_tmi(repo, "restore", "-S", "a.png")
        status = self.run_tmi(repo, "status")
        self.assertIn("modified   a.png", status.stdout)
        self.assertNotIn("staged", status.stdout)

        # Unstaging an already-unstaged tracked file is a clean no-op.
        out = self.run_tmi(repo, "restore", "--staged", "a.png")
        self.assertIn("a.png was not staged", out.stdout)

        # Unstaging an unknown pathspec fails like git.
        out = self.run_tmi(repo, "restore", "--staged", "nope.png", check=False)
        self.assertNotEqual(out.returncode, 0)
        self.assertIn("did not match", out.stderr)

    def test_gc_keeps_staged_entry_objects_reachable(self) -> None:
        repo = self.tmp / "repo"
        repo.mkdir()
        write_png(repo / "x.png", (1, 2, 3, 255))

        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "x.png")
        self.run_tmi(repo, "gc", "--prune=now")
        self.run_tmi(repo, "commit", "-m", "x")
        diff = self.run_tmi(repo, "diff")

        self.assertIn("no differences", diff.stdout)

    def test_invalid_branch_name_cannot_escape_refs_directory(self) -> None:
        repo = self.tmp / "repo"
        repo.mkdir()
        write_png(repo / "a.png", (255, 0, 0, 255))

        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "a.png")
        self.run_tmi(repo, "commit", "-m", "base")
        result = self.run_tmi(repo, "branch", "../tags/bad", check=False)

        self.assertNotEqual(0, result.returncode)
        self.assertIn("invalid ref name", result.stderr)
        self.assertFalse((repo / ".tmi" / "refs" / "tags" / "bad").exists())

    def test_ref_names_with_empty_components_are_rejected(self) -> None:
        repo = self.tmp / "repo"
        repo.mkdir()
        write_png(repo / "a.png", (255, 0, 0, 255))

        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "a.png")
        self.run_tmi(repo, "commit", "-m", "base")

        created = self.run_tmi(repo, "branch", "feature/", check=False)
        self.assertNotEqual(0, created.returncode)
        self.assertIn("invalid ref name", created.stderr)

        deleted = self.run_tmi(repo, "branch", "-D", "main/", check=False)
        self.assertNotEqual(0, deleted.returncode)
        self.assertIn("invalid ref name", deleted.stderr)
        status = self.run_tmi(repo, "status")
        self.assertIn("On branch main", status.stdout)
        self.assertNotIn("no commits", status.stdout)

    def test_revision_must_name_an_existing_commitish_object(self) -> None:
        repo = self.tmp / "repo"
        repo.mkdir()
        write_png(repo / "a.png", (255, 0, 0, 255))

        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "a.png")
        self.run_tmi(repo, "commit", "-m", "base")

        bogus = "0" * 64
        missing = self.run_tmi(repo, "branch", "bad", bogus, check=False)
        self.assertNotEqual(0, missing.returncode)
        self.assertIn("unknown revision", missing.stderr)

        from tmi.repo import Repo

        opened = Repo.open(repo)
        blob_oid = opened.store.write("blob", b"not a commit")
        blob = self.run_tmi(repo, "branch", "blob", blob_oid, check=False)
        self.assertNotEqual(0, blob.returncode)
        self.assertIn("does not name a commit", blob.stderr)

    def test_diff_html_emits_self_contained_report(self) -> None:
        repo = self.tmp / "repo"
        repo.mkdir()
        write_png(repo / "a.png", (255, 0, 0, 255))

        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "a.png")
        self.run_tmi(repo, "commit", "-m", "v1")
        write_png(repo / "a.png", (0, 255, 0, 255))
        result = self.run_tmi(repo, "diff", "--html", "--no-open")

        htmls = list((repo / ".tmi" / "diff").rglob("index.html"))
        self.assertEqual(1, len(htmls))
        text = htmls[0].read_text(encoding="utf-8")
        self.assertIn("<html", text)
        self.assertRegex(text, r"data:image/(webp|jpeg|png);base64,")
        self.assertIn("swipe", text)
        self.assertIn("a.png", text)
        self.assertIn("wrote ", result.stdout)

    def test_diff_counts_files_and_layers_apart_and_rasters_say_pixels(self) -> None:
        # Issue #24: one changed layer read "changed: 2" / "2 changes" (the
        # file's composite counted too), and a PNG showed "changed layer a".
        repo = self.tmp / "repo"
        repo.mkdir()
        write_png(repo / "a.png", (255, 0, 0, 255))
        write_mdp2_layers(repo / "b.mdp", [(255, 0, 0, 255), (0, 0, 255, 255)])
        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "a.png", "b.mdp")
        self.run_tmi(repo, "commit", "-m", "v1")
        write_png(repo / "a.png", (0, 255, 0, 255))
        write_mdp2_layers(repo / "b.mdp", [(255, 0, 0, 255), (0, 255, 0, 255)])

        status = self.run_tmi(repo, "status").stdout
        self.assertIn("a.png  changed pixels\n", status)
        self.assertNotIn("changed layer a", status)
        self.assertIn("b.mdp  changed layer layer2", status)

        diff = self.run_tmi(repo, "diff", "--out", str(self.tmp / "sbs")).stdout
        self.assertIn("  changed pixels bbox=[0, 0, 8, 8]\n", diff)
        self.assertIn("  changed layer layer2 bbox=", diff)

        stat = self.run_tmi(repo, "diff", "--stat").stdout
        self.assertIn(" a.png | pixels changed\n", stat)
        self.assertIn(" b.mdp | 1 layer changed\n", stat)
        self.assertIn(" 2 files changed", stat)

        self.run_tmi(repo, "diff", "--html", "--no-open", "--out", str(self.tmp / "report"))
        text = (self.tmp / "report" / "index.html").read_text(encoding="utf-8")
        summary = re.sub(r"\s+", " ", re.sub(r"<[^>]+>", " ", text.split('<div class="summary">')[1].split("</div>")[0])).strip()
        self.assertEqual("files: 0 added 2 changed 0 deleted layers: 0 added 1 changed 0 deleted", summary)
        self.assertIn("changed pixels bbox: [0, 0, 8, 8]", text)
        self.assertEqual(1, text.count('<table class="layers">'), "no layer table for the PNG")
        self.assertIn('<div class="table-wrap">', text)
        self.assertIn('<td class="bbox-col">', text)
        self.assertIn('name="viewport"', text)

    def test_status_and_diff_name_property_changes_instead_of_digests(self) -> None:
        # Issue #15: a rename showed as `metadata: 78343a2221f9 -> b6b65389992d`
        # in diff and `changed layer 影 metadata` in status.
        repo = self.tmp / "repo"
        repo.mkdir()
        art = repo / "art.mdp"
        red, blue = (255, 0, 0, 255), (0, 0, 255, 255)
        folder = b'<Layer name="F" width="128" height="128" mode="normal" alpha="255" id="9" parentId="-1" type="folder" visible="true" ofsx="0" ofsy="0"/>'

        def write(edit=lambda xml: xml) -> None:
            write_mdp2_layers(art, [red, blue, red])
            patch_mdp2_xml(art, lambda xml: edit(xml.replace(b"</Layers>", folder + b"</Layers>")))

        write()
        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "art.mdp")
        self.run_tmi(repo, "commit", "-m", "base")

        def edit(xml: bytes) -> bytes:
            xml = xml.replace(b'name="layer1" width="128" height="128" mode="normal" alpha="255"',
                              'name="線画" width="128" height="128" mode="normal" alpha="128"'.encode("utf-8"))
            xml = xml.replace(b'alpha="255" id="1" parentId="-1" bin="layer1img" type="32bpp" visible="true"',
                              b'alpha="255" id="1" parentId="9" bin="layer1img" type="32bpp" visible="false" protectAlpha="true"')
            return xml.replace(b'name="layer3" width="128" height="128" mode="normal"',
                               b'name="layer3" width="128" height="128" mode="multiply"')

        write(edit)
        status = self.run_tmi(repo, "status").stdout
        self.assertIn("changed layer 線画 (name: layer1 -> 線画, opacity: 100% -> 50%)", status)
        self.assertIn("changed layer layer2 (folder: (top level) -> F, visible: yes -> no, lock transparency: off -> on)", status)
        self.assertIn("changed layer layer3 (blend mode: normal -> multiply)", status)
        self.assertNotIn("metadata", status)

        diff = self.run_tmi(repo, "diff", "--out", str(self.tmp / "out")).stdout
        for line in ("name: layer1 -> 線画", "opacity: 100% -> 50%", "folder: (top level) -> F",
                     "visible: yes -> no", "lock transparency: off -> on", "blend mode: normal -> multiply"):
            self.assertIn(f"    {line}\n", diff)
        self.assertNotIn("metadata", diff)
        self.assertNotRegex(diff, r"[0-9a-f]{12} ->")

        # Something with no readable form still shows, as one line.
        from tmi.layer_props import layer_props, property_changes

        before = layer_props({"name": "a", "source_meta": {"xml_attrs": {"lumi": "false"}}})
        after = layer_props({"name": "a", "source_meta": {"xml_attrs": {"lumi": "true"}}})
        self.assertEqual([("other properties", None, None)], property_changes(before, after))
        self.assertEqual([], property_changes(before, before))
        # Expanding / collapsing a folder in the layer panel.
        for old_meta, new_meta in (({"folder": 17}, {"folder": 1}), ({"folder_open": False}, {"folder_open": True})):
            self.assertEqual(
                [("folder open/closed", None, None)],
                property_changes(layer_props({"source_meta": old_meta}), layer_props({"source_meta": new_meta})),
            )

    def test_restore_output_can_export_from_the_working_file(self) -> None:
        # Issue #15: restore --output reads the index, so a layer's unstaged
        # edit could not be exported; --source WORKTREE reads the file.
        repo = self.tmp / "repo"
        repo.mkdir()
        art = repo / "art.mdp"
        red, blue, green = (255, 0, 0, 255), (0, 0, 255, 255), (0, 255, 0, 255)
        write_mdp2_layers(art, [red, blue])
        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "art.mdp")
        self.run_tmi(repo, "commit", "-m", "base")
        write_mdp2_layers(art, [red, green])
        edited = art.read_bytes()

        def pixel(name: str) -> tuple:
            return Image.open(repo / name).convert("RGBA").getpixel((5, 5))

        self.run_tmi(repo, "restore", "--output", "index.png", "art.mdp:layer2")
        self.assertEqual(blue, pixel("index.png"))
        out = self.run_tmi(repo, "restore", "--source", "WORKTREE", "--output", "work.png", "art.mdp:layer2")
        self.assertIn("restored work.png", out.stdout)
        self.assertEqual(green, pixel("work.png"))
        self.run_tmi(repo, "restore", "--source", "WORKTREE", "--output", "work1.png", "art.mdp:layer1")
        self.assertEqual(red, pixel("work1.png"))
        self.assertEqual(edited, art.read_bytes())

        for args in (["--source", "WORKTREE", "art.mdp:layer2"], ["--source", "WORKTREE", "--in-place", "art.mdp:layer2"]):
            refused = self.run_tmi(repo, "restore", *args, check=False)
            self.assertNotEqual(0, refused.returncode, args)
            self.assertIn("--output", refused.stderr, args)
        self.assertEqual(edited, art.read_bytes())
        self.assertIn("WORKTREE", self.run_tmi(repo, "restore", "-h").stdout)

    def test_show_thumbnail_writes_under_tmi_by_default(self) -> None:
        # Issue #15: the default <commit>-thumbnail.png landed in the working
        # tree, showed as untracked and was staged by `add -A`.
        repo = self.tmp / "repo"
        repo.mkdir()
        write_png(repo / "a.png", (255, 0, 0, 255))
        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "a.png")
        self.run_tmi(repo, "commit", "-m", "v1")

        shown = self.run_tmi(repo, "show", "--thumbnail").stdout.strip()
        self.assertTrue(shown.startswith("wrote "), shown)
        written = Path(shown[len("wrote "):])
        self.assertTrue(written.is_absolute())
        self.assertEqual((repo / ".tmi" / "thumbnails").resolve(), written.parent.resolve())
        thumb = Image.open(written).convert("RGBA")
        box = thumb.getbbox()
        self.assertEqual((255, 0, 0, 255), thumb.getpixel(((box[0] + box[2]) // 2, (box[1] + box[3]) // 2)))
        self.assertEqual(["a.png"], sorted(p.name for p in repo.iterdir() if p.name != ".tmi"))
        self.assertIn("nothing to commit, working tree clean", self.run_tmi(repo, "status").stdout)
        self.run_tmi(repo, "add", "-A")
        self.assertIn("nothing to commit, working tree clean", self.run_tmi(repo, "status").stdout)

        self.run_tmi(repo, "show", "--thumbnail", "-o", "thumb.png")
        self.assertTrue((repo / "thumb.png").is_file())

    def test_opacity_is_shown_in_percent_everywhere(self) -> None:
        # Issue #15: diff printed `opacity: 1.0 -> 0.5`, ls-layers `50%`.
        from tmi.layer_props import format_opacity, opacity_texts

        self.assertEqual("50%", format_opacity(0.5))
        self.assertEqual(("100%", "50%"), opacity_texts(1.0, 0.5))
        self.assertEqual(("99.6%", "100.0%"), opacity_texts(255 / 256, 1.0))

        repo = self.tmp / "repo"
        repo.mkdir()
        write_mdp2_layers(repo / "art.mdp", [(255, 0, 0, 255), (0, 0, 255, 255)])
        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "art.mdp")
        self.run_tmi(repo, "commit", "-m", "base")
        data = (repo / "art.mdp").read_bytes()
        (repo / "art.mdp").write_bytes(data.replace(b'alpha="255"', b'alpha="128"', 1))

        diff = self.run_tmi(repo, "diff", "--out", str(self.tmp / "out")).stdout
        self.assertIn("opacity: 100% -> 50%", diff)
        self.assertNotIn("0.5", diff)
        listing = self.run_tmi(repo, "ls-layers", "art.mdp").stdout
        self.assertRegex(listing, r"\b50%.*layer1")

    def test_plain_diff_hints_at_staged_when_only_staged_changes_exist(self) -> None:
        # Issue #15: `tmiki diff` is working tree vs index, so after `add` it
        # printed only "no differences".
        repo = self.tmp / "repo"
        repo.mkdir()
        write_png(repo / "a.png", (255, 0, 0, 255))
        write_png(repo / "b.png", (255, 0, 0, 255))
        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "a.png", "b.png")
        self.run_tmi(repo, "commit", "-m", "v1")

        clean = self.run_tmi(repo, "diff")
        self.assertIn("no differences", clean.stdout)
        self.assertNotIn("hint", clean.stderr)

        write_png(repo / "a.png", (0, 255, 0, 255))
        self.run_tmi(repo, "add", "a.png")
        for args in (["diff"], ["diff", "--name-only"], ["diff", "--stat"], ["diff", "a.png"]):
            result = self.run_tmi(repo, *args)
            self.assertIn("tmiki diff --staged", result.stderr, args)
            self.assertNotIn("hint", result.stdout, args)
        self.assertNotIn("hint", self.run_tmi(repo, "diff", "b.png").stderr)
        self.assertEqual("a.png", self.run_tmi(repo, "diff", "--staged", "--name-only").stdout.strip())
        self.assertIn("--staged", self.run_tmi(repo, "diff", "-h").stdout)
        for readme in ("README.md", "README_pub.md"):
            text = (PROJECT_ROOT / readme).read_text(encoding="utf-8")
            self.assertIn("tmiki diff --staged", text, readme)
            self.assertIn("tmiki diff HEAD~1", text, readme)

    def test_diff_html_previews_are_compressed_and_keep_alpha(self) -> None:
        # Issue #15: seven lossless 2048 px PNG previews made a ~30 MB report.
        # Previews are lossy now (WebP, or JPEG/PNG without WebP); originals
        # stay linked as PNG.
        from unittest import mock

        from tmi import diff as diff_mod
        from tmi.diff_html import _data_uri
        from tmi.image_ops import image_to_png_bytes

        # Grain, not white noise: libwebp's method 2 (the previews' effort)
        # keeps sigma-64 noise at ~65 % of its PNG, where 4 made ~38 %.
        noise = Image.effect_noise((512, 512), 16).convert("RGBA")
        lossless = len(image_to_png_bytes(noise))
        clear = Image.new("RGBA", (64, 64), (0, 0, 0, 0))

        # WebP always (tmi_accel's libwebp): no JPEG / PNG fallback any more.
        preview = diff_mod._preview_bytes(noise)
        self.assertTrue(_data_uri(preview).startswith("data:image/webp;base64,"))
        decoded = Image.open(io.BytesIO(diff_mod._preview_bytes(clear)))
        self.assertEqual(0, decoded.convert("RGBA").getpixel((5, 5))[3])
        self.assertLess(len(preview), lossless // 2)
        # The bytes Pillow's own WebP encoder writes (same libwebp, same settings).
        buf = io.BytesIO()
        noise.save(buf, format="WEBP", quality=diff_mod.HTML_PREVIEW_QUALITY, method=diff_mod.HTML_PREVIEW_METHOD)
        self.assertEqual(buf.getvalue(), preview)

        repo = self.tmp / "repo"
        repo.mkdir()
        write_png(repo / "a.png", (255, 0, 0, 255))
        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "a.png")
        self.run_tmi(repo, "commit", "-m", "v1")
        write_png(repo / "a.png", (0, 255, 0, 255))
        self.run_tmi(repo, "diff", "--html", "--no-open", "--out", str(self.tmp / "report"))
        text = (self.tmp / "report" / "index.html").read_text(encoding="utf-8")
        self.assertNotIn("data:image/png;base64,", text)
        self.assertIn('href="a.png-old.png"', text)
        self.assertEqual((255, 0, 0, 255), Image.open(self.tmp / "report" / "a.png-old.png").convert("RGBA").getpixel((0, 0)))

    def test_diff_html_with_no_changes_says_no_differences(self) -> None:
        repo = self.tmp / "repo"
        repo.mkdir()
        write_png(repo / "a.png", (255, 0, 0, 255))

        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "a.png")
        self.run_tmi(repo, "commit", "-m", "v1")
        result = self.run_tmi(repo, "diff", "--html", "--no-open")

        self.assertIn("no differences", result.stdout)

    def test_diff_html_is_mutually_exclusive_with_overlay(self) -> None:
        repo = self.tmp / "repo"
        repo.mkdir()
        write_png(repo / "a.png", (255, 0, 0, 255))
        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "a.png")
        self.run_tmi(repo, "commit", "-m", "v1")

        result = self.run_tmi(repo, "diff", "--html", "--overlay", "--no-open", check=False)
        self.assertNotEqual(0, result.returncode)

    def test_clone_with_relative_url_can_pull_afterwards(self) -> None:
        # Regression: clone stored the URL verbatim, so a relative path resolved
        # against the clone's own directory and every later fetch/pull/push died
        # with "not a tmi repository: .../clone/<relpath>".
        self.run_tmi(self.tmp, "init", "--bare", "origin")
        work = self.tmp / "work"
        work.mkdir()
        write_png(work / "a.png", (255, 0, 0, 255))
        self.run_tmi(work, "init")
        self.run_tmi(work, "add", "a.png")
        self.run_tmi(work, "commit", "-m", "base")
        self.run_tmi(work, "remote", "add", "origin", "../origin")
        self.run_tmi(work, "push", "origin", "main")

        # Clone using a *relative* URL from self.tmp.
        self.run_tmi(self.tmp, "clone", "origin", "clone1")
        clone1 = self.tmp / "clone1"

        # New commit upstream, then pull from the clone — must succeed.
        write_png(work / "a.png", (0, 255, 0, 255))
        self.run_tmi(work, "commit", "-a", "-m", "second")
        self.run_tmi(work, "push", "origin", "main")
        pulled = self.run_tmi(clone1, "pull", "origin", "main")
        self.assertIn("fast-forward", pulled.stdout)

    def test_safe_filename_keeps_distinct_non_ascii_names_distinct(self) -> None:
        from tmi.repo import safe_filename

        # Pure-ASCII names survive unchanged.
        self.assertEqual("layer1", safe_filename("layer1"))
        # All-Japanese names previously both collapsed to "_".
        self.assertNotEqual(safe_filename("前景"), safe_filename("背景"))
        # Same input is stable across calls.
        self.assertEqual(safe_filename("前景"), safe_filename("前景"))

    def test_status_reports_merge_in_progress_with_conflicts(self) -> None:
        repo = self.tmp / "repo"
        repo.mkdir()
        write_png(repo / "a.png", (255, 0, 0, 255))
        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "a.png")
        self.run_tmi(repo, "commit", "-m", "base")
        self.run_tmi(repo, "branch", "other")
        write_png(repo / "a.png", (0, 255, 0, 255))
        self.run_tmi(repo, "commit", "-a", "-m", "green")
        self.run_tmi(repo, "switch", "other")
        write_png(repo / "a.png", (0, 0, 255, 255))
        self.run_tmi(repo, "commit", "-a", "-m", "blue")
        self.run_tmi(repo, "switch", "main")

        merged = self.run_tmi(repo, "merge", "other", check=False)
        self.assertEqual(1, merged.returncode)

        status = self.run_tmi(repo, "status")
        self.assertIn("middle of a merge", status.stdout)
        self.assertIn("conflict a.png", status.stdout)

    def test_diff_outputs_do_not_collide_for_same_basename_in_different_dirs(self) -> None:
        repo = self.tmp / "repo"
        repo.mkdir()
        (repo / "left").mkdir()
        (repo / "right").mkdir()
        write_png(repo / "left" / "art.png", (255, 0, 0, 255))
        write_png(repo / "right" / "art.png", (0, 0, 255, 255))

        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "left/art.png")
        self.run_tmi(repo, "add", "right/art.png")
        self.run_tmi(repo, "commit", "-m", "base")
        write_png(repo / "left" / "art.png", (0, 255, 0, 255))
        write_png(repo / "right" / "art.png", (255, 255, 0, 255))

        result = self.run_tmi(repo, "diff", "--out", "diffs")
        wrote = [line.removeprefix("  wrote ") for line in result.stdout.splitlines() if line.startswith("  wrote ")]
        self.assertEqual(4, len(wrote))
        self.assertEqual(4, len(set(wrote)))
        for path in wrote:
            out = Path(path)
            self.assertTrue((out if out.is_absolute() else repo / out).exists(), path)

    def test_add_missing_file_reports_clear_error(self) -> None:
        repo = self.tmp / "repo"
        repo.mkdir()
        self.run_tmi(repo, "init")
        result = self.run_tmi(repo, "add", "ghost.mdp", check=False)
        self.assertNotEqual(0, result.returncode)
        self.assertIn("no such file to add", result.stderr)

    def test_tmi_package_does_not_import_research_at_runtime(self) -> None:
        offenders: list[Path] = []
        for path in (PROJECT_ROOT / "tmi").rglob("*.py"):
            text = path.read_text(encoding="utf-8")
            if "from research" in text or "import research" in text or "research." in text:
                offenders.append(path.relative_to(PROJECT_ROOT))
        self.assertEqual([], offenders)

    def test_diff_takes_git_style_revisions_and_staged(self) -> None:
        # Issue #7: diff compared only the working tree with HEAD.
        repo = self.tmp / "repo"
        repo.mkdir()
        write_png(repo / "a.png", (255, 0, 0, 255))
        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "a.png")
        self.run_tmi(repo, "commit", "-m", "red")
        write_png(repo / "a.png", (0, 255, 0, 255))
        self.run_tmi(repo, "commit", "-a", "-m", "green")

        self.assertIn("M\ta.png", self.run_tmi(repo, "diff", "--name-status", "HEAD~1", "HEAD").stdout)
        self.assertIn("no differences", self.run_tmi(repo, "diff", "--stat", "HEAD").stdout)
        write_png(repo / "a.png", (0, 0, 255, 255))
        self.assertIn("M\ta.png", self.run_tmi(repo, "diff", "--name-status").stdout)
        self.run_tmi(repo, "add", "a.png")
        # Staged: the working tree matches the index, the index differs from HEAD.
        self.assertIn("no differences", self.run_tmi(repo, "diff", "--stat").stdout)
        self.assertIn("M\ta.png", self.run_tmi(repo, "diff", "--staged", "--name-status").stdout)
        self.assertIn("M\ta.png", self.run_tmi(repo, "diff", "--name-status", "HEAD~1", "--", "a.png").stdout)

        # HTML embeds previews and links the originals.
        out = self.tmp / "report"
        self.run_tmi(repo, "diff", "--staged", "--html", "--no-open", "--out", str(out))
        self.assertTrue((out / "a.png-old.png").is_file())
        self.assertTrue((out / "a.png-new.png").is_file())
        self.assertIn('href="a.png-old.png"', (out / "index.html").read_text(encoding="utf-8"))

    def test_ls_layers_paths_and_folder_export_on_a_real_clip(self) -> None:
        # Issue #6, on example/tama.clip (not in git): names repeat, so layers
        # are listed with paths and ids, selectable by path, and a folder
        # exports as its composite.
        src = PROJECT_ROOT / "example" / "tama.clip"
        if not src.exists():
            self.skipTest("example/tama.clip not present")
        repo = self.tmp / "repo"
        repo.mkdir()
        shutil.copy(src, repo / "tama.clip")
        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "tama.clip")
        self.run_tmi(repo, "commit", "-m", "base")
        listing = self.run_tmi(repo, "ls-layers", "tama.clip").stdout
        self.assertIn("人物/ハイライト", listing)
        self.assertIn("色調補正/ハイライト", listing)
        ambiguous = self.run_tmi(repo, "restore", "--output", str(self.tmp / "x.png"), "tama.clip:ハイライト", check=False)
        self.assertIn("人物/ハイライト [clip:", ambiguous.stderr)
        self.run_tmi(repo, "restore", "--output", str(self.tmp / "h.png"), "tama.clip:人物/ハイライト")
        self.run_tmi(repo, "restore", "--output", str(self.tmp / "folder.png"), "tama.clip:人物")
        folder = Image.open(self.tmp / "folder.png")
        self.assertIsNotNone(folder.getbbox())

    def test_diff_with_unknown_path_errors_instead_of_no_differences(self) -> None:
        repo = self.tmp / "repo"
        repo.mkdir()
        write_png(repo / "a.png", (255, 0, 0, 255))
        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "a.png")
        self.run_tmi(repo, "commit", "-m", "base")

        result = self.run_tmi(repo, "diff", "ghost.png", check=False)
        self.assertNotEqual(0, result.returncode)
        self.assertIn("not tracked", result.stderr)

    def test_push_rejects_non_fast_forward_and_force_overrides(self) -> None:
        self.run_tmi(self.tmp, "init", "--bare", "origin")
        a = self.tmp / "A"
        a.mkdir()
        write_png(a / "x.png", (255, 0, 0, 255))
        self.run_tmi(a, "init")
        self.run_tmi(a, "add", "x.png")
        self.run_tmi(a, "commit", "-m", "c1")
        self.run_tmi(a, "remote", "add", "origin", "../origin")
        self.run_tmi(a, "push", "origin", "main")

        self.run_tmi(self.tmp, "clone", "origin", "B")
        b = self.tmp / "B"

        # A advances and pushes (fast-forward, fine).
        write_png(a / "x.png", (0, 255, 0, 255))
        self.run_tmi(a, "commit", "-a", "-m", "c2")
        self.run_tmi(a, "push", "origin", "main")

        # B diverges from the old tip and tries to push — must be rejected.
        write_png(b / "x.png", (0, 0, 255, 255))
        self.run_tmi(b, "commit", "-a", "-m", "c2b")
        rejected = self.run_tmi(b, "push", "origin", "main", check=False)
        self.assertNotEqual(0, rejected.returncode)
        self.assertIn("non-fast-forward", rejected.stderr)

        # --force overrides.
        forced = self.run_tmi(b, "push", "origin", "main", "--force")
        self.assertIn("forced update", forced.stdout)

    def test_mdp_text_layer_is_rasterized(self) -> None:
        repo = self.tmp / "repo"
        repo.mkdir()
        write_mdp2_text_layer(repo / "art.mdp")
        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "art.mdp")
        self.run_tmi(repo, "commit", "-m", "base")
        # The text layer carries pixels, so it can be restored to a PNG.
        self.run_tmi(repo, "restore", "--output", "text.png", "art.mdp:テキスト")
        restored = Image.open(repo / "text.png").convert("RGBA")
        self.assertEqual((128, 128), restored.size)
        self.assertIsNotNone(restored.getbbox())  # non-empty pixels

    def test_layer_pathspec_stages_only_selected_mdp_layer(self) -> None:
        repo = self.tmp / "repo"
        repo.mkdir()
        write_mdp2_layers(repo / "art.mdp", [(255, 0, 0, 255), (0, 0, 255, 255)])

        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "art.mdp")
        self.run_tmi(repo, "commit", "-m", "base")
        # Repaint both layers, but stage only layer1.
        write_mdp2_layers(repo / "art.mdp", [(0, 255, 0, 255), (255, 255, 0, 255)])
        self.run_tmi(repo, "add", "art.mdp:layer1")
        self.run_tmi(repo, "commit", "-m", "stage layer1")

        # layer1 advanced to the new green ...
        self.run_tmi(repo, "restore", "--output", "layer1.png", "art.mdp:layer1")
        self.assertEqual((0, 255, 0, 255), Image.open(repo / "layer1.png").convert("RGBA").getpixel((0, 0)))
        # ... while the unstaged layer2 was carried forward from HEAD (still blue).
        self.run_tmi(repo, "restore", "--output", "layer2.png", "art.mdp:layer2")
        self.assertEqual((0, 0, 255, 255), Image.open(repo / "layer2.png").convert("RGBA").getpixel((0, 0)))

    def test_layer_pathspec_can_stage_emptied_mdp_layer(self) -> None:
        repo = self.tmp / "repo"
        repo.mkdir()
        write_mdp2_layers(repo / "art.mdp", [(255, 0, 0, 255), (0, 0, 255, 255)])

        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "art.mdp")
        self.run_tmi(repo, "commit", "-m", "base")

        # MediBang stores a fully emptied layer as a 4-byte tile_count=0 blob.
        # That layer still has stageable pixels: a transparent raster.
        write_mdp2_layers(repo / "art.mdp", [(0, 255, 0, 255), None])
        status = self.run_tmi(repo, "status")
        self.assertIn("changed layer layer2", status.stdout)

        self.run_tmi(repo, "add", "art.mdp:layer2")
        self.run_tmi(repo, "commit", "-m", "empty layer2")

        self.run_tmi(repo, "restore", "--output", "layer1.png", "art.mdp:layer1")
        self.assertEqual((255, 0, 0, 255), Image.open(repo / "layer1.png").convert("RGBA").getpixel((0, 0)))
        self.run_tmi(repo, "restore", "--output", "layer2.png", "art.mdp:layer2")
        self.assertIsNone(Image.open(repo / "layer2.png").convert("RGBA").getbbox())

    def test_layer_pathspec_rejects_metadata_only_mdp_change_without_empty_commit(self) -> None:
        repo = self.tmp / "repo"
        repo.mkdir()
        write_mdp2_layers(repo / "art.mdp", [(255, 0, 0, 255), (0, 0, 255, 255)])

        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "art.mdp")
        self.run_tmi(repo, "commit", "-m", "base")

        data = (repo / "art.mdp").read_bytes()
        (repo / "art.mdp").write_bytes(data.replace(b'alpha="255"', b'alpha="128"', 1))
        status = self.run_tmi(repo, "status")
        self.assertIn("changed layer layer1 (opacity: 100% -> 50%)", status.stdout)

        rejected = self.run_tmi(repo, "add", "art.mdp:layer1", check=False)
        self.assertNotEqual(0, rejected.returncode)
        self.assertIn("metadata-only/global changes", rejected.stderr)
        self.assertNotIn("staged     art.mdp", self.run_tmi(repo, "status").stdout)

    def test_layer_pathspec_rejects_canvas_change_mixed_with_pixel_change(self) -> None:
        repo = self.tmp / "repo"
        repo.mkdir()
        write_mdp2_layers(repo / "art.mdp", [(255, 0, 0, 255), (0, 0, 255, 255)])

        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "art.mdp")
        self.run_tmi(repo, "commit", "-m", "base")

        write_mdp2_layers(
            repo / "art.mdp",
            [(0, 255, 0, 255), (0, 0, 255, 255)],
            canvas_size=(256, 128),
        )
        rejected = self.run_tmi(repo, "add", "art.mdp:layer1", check=False)
        self.assertNotEqual(0, rejected.returncode)
        self.assertIn("canvas/global settings changed", rejected.stderr)
        self.assertNotIn("staged     art.mdp", self.run_tmi(repo, "status").stdout)

    def test_layer_pathspec_rejects_layer_size_change_mixed_with_pixel_change(self) -> None:
        repo = self.tmp / "repo"
        repo.mkdir()
        write_mdp2_layers(
            repo / "art.mdp",
            [(255, 0, 0, 255), (0, 0, 255, 255)],
            canvas_size=(256, 128),
        )

        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "art.mdp")
        self.run_tmi(repo, "commit", "-m", "base")

        write_mdp2_layers(
            repo / "art.mdp",
            [(0, 255, 0, 255), (0, 0, 255, 255)],
            canvas_size=(256, 128),
            layer_size=(256, 128),
        )
        rejected = self.run_tmi(repo, "add", "art.mdp:layer1", check=False)
        self.assertNotEqual(0, rejected.returncode)
        self.assertIn("metadata-only/global changes", rejected.stderr)
        self.assertNotIn("staged     art.mdp", self.run_tmi(repo, "status").stdout)

    def test_mdp_layer_restore_in_place_rewrites_only_target_layer(self) -> None:
        repo = self.tmp / "repo"
        repo.mkdir()
        write_mdp2_layers(repo / "art.mdp", [(255, 0, 0, 255), (0, 0, 255, 255)])

        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "art.mdp")
        self.run_tmi(repo, "commit", "-m", "base")
        # Repaint the whole file and commit it as the new tip.
        write_mdp2_layers(repo / "art.mdp", [(0, 255, 0, 255), (255, 255, 0, 255)])
        self.run_tmi(repo, "commit", "-a", "-m", "repaint")
        # Roll back only layer1's pixels to the base, writing back into the .mdp.
        self.run_tmi(repo, "restore", "--source", "HEAD~1", "--in-place", "art.mdp:layer1")

        from tmi.image_ops import image_from_bytes
        from tmi.importers import import_file, walk_imported_layers

        layers = {layer.name: layer for layer in walk_imported_layers(import_file(repo / "art.mdp").layers)}
        layer1 = image_from_bytes(layers["layer1"].blob_bytes)
        layer2 = image_from_bytes(layers["layer2"].blob_bytes)
        self.assertEqual((255, 0, 0, 255), layer1.getpixel((0, 0)))  # restored to base red
        self.assertEqual((255, 255, 0, 255), layer2.getpixel((0, 0)))  # untouched yellow

    def test_mdp2_imports_density_text_blend_and_mask_metadata(self) -> None:
        from tmi.importers import import_file, walk_imported_layers

        imported = import_file(PROJECT_ROOT / "example" / "new.mdp")
        layers = {layer.layer_id: layer for layer in walk_imported_layers(imported.layers)}

        self.assertEqual("raster", layers["mdp2:1"].kind)
        self.assertEqual("8bpp", layers["mdp2:1"].source_meta["type"])
        self.assertIsNotNone(layers["mdp2:1"].blob_bytes)
        self.assertEqual("raster", layers["mdp2:2"].kind)
        self.assertEqual("1bpp", layers["mdp2:2"].source_meta["type"])
        self.assertIsNotNone(layers["mdp2:2"].blob_bytes)

        text = layers["mdp2:4"]
        self.assertEqual("text", text.kind)
        self.assertEqual(["ああああ"], text.source_meta["text_lines"])
        self.assertEqual((705, 654), (text.offset_x, text.offset_y))

        blend_sample = import_file(PROJECT_ROOT / "example" / "new2.mdp")
        blend_layers = {layer.layer_id: layer for layer in walk_imported_layers(blend_sample.layers)}
        self.assertEqual("add", blend_layers["mdp2:2"].blend_mode)
        self.assertIs(False, blend_layers["mdp2:2"].source_meta["lumi"])
        self.assertEqual("add2", blend_layers["mdp2:3"].blend_mode)
        self.assertEqual("mask", blend_layers["mdp2:22"].kind)
        self.assertEqual(0, blend_layers["mdp2:22"].source_meta["masking_type"])
        self.assertEqual("stencil", blend_layers["mdp2:23"].kind)
        self.assertEqual(1, blend_layers["mdp2:23"].source_meta["masking_type"])

    def test_mdp2_rectangular_composite_matches_stable_samples(self) -> None:
        import hashlib

        from tmi.formats import mdp2 as mdp2_mod

        expected = {
            "kakenhi-kaguya.mdp": "afbca914779e570d",
            "new.mdp": "fc8a901bfea06aa4",
            "new2.mdp": "d2efc652c5323f86",
        }
        for name, digest in expected.items():
            with self.subTest(name=name):
                mdp = mdp2_mod.open_mdp2(PROJECT_ROOT / "example" / name)
                image = mdp2_mod.render_composite_rgba(mdp)
                self.assertEqual(digest, hashlib.sha256(image.tobytes()).hexdigest()[:16])

    def test_clip_imports_text_vector_and_correct_blend_ids(self) -> None:
        from tmi.importers import import_file, walk_imported_layers

        text_layers = walk_imported_layers(import_file(PROJECT_ROOT / "example" / "text_simple.clip").layers)
        text = next(layer for layer in text_layers if layer.kind == "text")
        self.assertEqual("こんにちは。これはテキスト入力欄です。", text.source_meta["text"])
        self.assertIn("text_layer_attributes_sha256", text.source_meta)

        vector_layers = walk_imported_layers(import_file(PROJECT_ROOT / "example" / "vector_simple.clip").layers)
        vector = next(layer for layer in vector_layers if layer.kind == "vector")
        self.assertEqual(["extrnlid1FA0A73311C5456B98DCF1091D74E309"], vector.source_meta["vector_outer_ids"])

        blend_layers = walk_imported_layers(import_file(PROJECT_ROOT / "example" / "blend_modes.clip").layers)
        by_composite = {layer.source_meta.get("composite_id"): layer.blend_mode for layer in blend_layers}
        # Ids as CLIP STUDIO PAINT 5.1.5 writes them (see _COMPOSITE_MAP).
        self.assertEqual("multiply", by_composite[2])
        self.assertEqual("screen", by_composite[8])
        self.assertEqual("overlay", by_composite[14])
        self.assertEqual("hard_light", by_composite[16])
        self.assertEqual("darker_color", by_composite[6])
        self.assertEqual("lighter_color", by_composite[13])
        self.assertEqual("subtract", by_composite[5])
        self.assertEqual("luminosity", by_composite[26])
        self.assertEqual("divide", by_composite[36])

    def test_clip_composite_ids_match_what_csp_writes(self) -> None:
        """example/csp515/blendids.clip: layer N was set in CSP 5.1.5's layer
        palette to the N-th blend mode of its list (not in git; skip if absent)."""
        from tmi.formats import clip as clip_mod

        path = PROJECT_ROOT / "example" / "csp515" / "blendids.clip"
        if not path.exists():
            self.skipTest("example/csp515/blendids.clip is not available")
        palette_order = [
            "darken", "multiply", "color_burn", "linear_burn", "subtract", "lighten", "screen",
            "color_dodge", "glow_dodge",  # 覆い焼きカラー, 覆い焼き(発光)
            "linear_dodge", "glow_add",  # 加算, 加算(発光)
            "overlay", "soft_light", "hard_light", "difference", "vivid_light", "linear_light",
            "pin_light", "hard_mix", "exclusion", "darker_color", "lighter_color", "divide",
            "hue", "saturation", "color", "luminosity",
        ]
        clip = clip_mod.open_clip(path)
        stored = dict(clip.db.execute("SELECT LayerName, LayerComposite FROM Layer").fetchall())
        for index, expected in enumerate(palette_order, start=1):
            composite = stored[f"レイヤー {index}"]
            self.assertEqual(expected, clip_mod._composite_name(composite), f"レイヤー {index} (id {composite})")

    def test_clip_status_ignores_legacy_outer_id_churn(self) -> None:
        from tmi.importers import ImportedFile, ImportedLayer
        from tmi.repo import Repo

        repo = Repo.create(self.tmp / "repo")
        buf = io.BytesIO()
        Image.new("RGBA", (1, 1), (255, 0, 0, 255)).save(buf, format="PNG")
        png = buf.getvalue()
        blob_ref = repo.store.write("blob", png)
        tree_ref = repo.store.write_json(
            "tree",
            {
                "version": 1,
                "kind": "layer-tree",
                "layers": [
                    {
                        "layer_id": "clip:1",
                        "stable_source_id": "layer-uuid",
                        "name": "paint",
                        "kind": "raster",
                        "visible": True,
                        "opacity": 1.0,
                        "blend_mode": "normal",
                        "clip_to_below": False,
                        "offset_x": 0,
                        "offset_y": 0,
                        "mask_ref": None,
                        "blob_ref": blob_ref,
                        "subtree_ref": None,
                        "pixel_bbox": [0, 0, 1, 1],
                        "source_meta": {
                            "main_id": 1,
                            "type": 1,
                            "folder": 0,
                            "composite_id": 0,
                            "render_outer_id": "extrnlidOLD",
                            "mask_outer_id": "extrnlidOLDMASK",
                            "masking": 0,
                            "pixel_width": 1,
                            "pixel_height": 1,
                            "channel_profile": "color-alpha-bgra",
                            "offscreen_id": 10,
                            "semantic_digest": "legacy-digest-with-outer-id",
                        },
                    }
                ],
            },
        )
        imported = ImportedFile(
            source_format="clip",
            source_bytes=b"",
            source_digest="",
            canvas_meta={},
            layers=[
                ImportedLayer(
                    layer_id="clip:1",
                    stable_source_id="layer-uuid",
                    name="paint",
                    kind="raster",
                    blob_bytes=png,
                    pixel_bbox=[0, 0, 1, 1],
                    source_meta={
                        "main_id": 1,
                        "type": 1,
                        "folder": 0,
                        "composite_id": 0,
                        "render_outer_id": "extrnlidNEW",
                        "mask_outer_id": "extrnlidNEWMASK",
                        "masking": 0,
                        "pixel_width": 1,
                        "pixel_height": 1,
                        "channel_profile": "color-alpha-bgra",
                        "offscreen_id": 99,
                        "semantic_digest": "new-digest-with-outer-id",
                    },
                )
            ],
            composite_png=b"",
            thumbnail_png=b"",
        )

        self.assertEqual([], repo.layer_changes({"source_format": "clip", "tree_ref": tree_ref}, imported))

    def test_clip_status_reports_payload_digest_change_for_metadata_layer(self) -> None:
        from tmi.importers import ImportedFile, ImportedLayer
        from tmi.repo import Repo

        repo = Repo.create(self.tmp / "repo")
        tree_ref = repo.store.write_json(
            "tree",
            {
                "version": 1,
                "kind": "layer-tree",
                "layers": [
                    {
                        "layer_id": "clip:2",
                        "stable_source_id": "adjustment-uuid",
                        "name": "tone",
                        "kind": "unknown",
                        "visible": True,
                        "opacity": 1.0,
                        "blend_mode": "normal",
                        "clip_to_below": False,
                        "offset_x": 0,
                        "offset_y": 0,
                        "mask_ref": None,
                        "blob_ref": None,
                        "subtree_ref": None,
                        "pixel_bbox": None,
                        "source_meta": {
                            "type": 32,
                            "folder": 0,
                            "composite_id": 0,
                            "masking": 0,
                            "render_payload_sha256": "old-payload",
                            "semantic_digest_version": 2,
                            "semantic_digest": "old-semantic",
                        },
                    }
                ],
            },
        )
        imported = ImportedFile(
            source_format="clip",
            source_bytes=b"",
            source_digest="",
            canvas_meta={},
            layers=[
                ImportedLayer(
                    layer_id="clip:2",
                    stable_source_id="adjustment-uuid",
                    name="tone",
                    kind="unknown",
                    blob_bytes=None,
                    source_meta={
                        "type": 32,
                        "folder": 0,
                        "composite_id": 0,
                        "masking": 0,
                        "render_payload_sha256": "new-payload",
                        "semantic_digest_version": 2,
                        "semantic_digest": "new-semantic",
                    },
                )
            ],
            composite_png=b"",
            thumbnail_png=b"",
        )

        self.assertEqual(
            ["changed layer tone (other properties changed)"],
            repo.layer_changes({"source_format": "clip", "tree_ref": tree_ref}, imported),
        )

    def test_clip_status_uses_legacy_source_blob_for_payload_digest(self) -> None:
        from tmiref.clip_io import emit_container
        from tmi.importers import ImportedFile, ImportedLayer, _clip_normalized_exta_payload_sha256
        from tmi.repo import Repo

        repo = Repo.create(self.tmp / "repo")
        outer_id = "extrnlid" + ("A" * 32)
        payload = struct.pack(">Q", 0x28) + outer_id.encode("ascii") + struct.pack(">Q", 7) + b"payload"
        source_ref = repo.store.write("blob", emit_container(24, [(b"Exta", payload)]))
        tree_ref = repo.store.write_json(
            "tree",
            {
                "version": 1,
                "kind": "layer-tree",
                "layers": [
                    {
                        "layer_id": "clip:3",
                        "stable_source_id": "legacy-adjustment",
                        "name": "tone",
                        "kind": "unknown",
                        "visible": True,
                        "opacity": 1.0,
                        "blend_mode": "normal",
                        "clip_to_below": False,
                        "offset_x": 0,
                        "offset_y": 0,
                        "mask_ref": None,
                        "blob_ref": None,
                        "subtree_ref": None,
                        "pixel_bbox": None,
                        "source_meta": {
                            "type": 32,
                            "folder": 0,
                            "composite_id": 0,
                            "render_outer_id": outer_id,
                            "masking": 0,
                            "semantic_digest": "legacy-digest-with-outer-id",
                        },
                    }
                ],
            },
        )

        def imported_with_payload_digest(digest: str) -> ImportedFile:
            return ImportedFile(
                source_format="clip",
                source_bytes=b"",
                source_digest="",
                canvas_meta={},
                layers=[
                    ImportedLayer(
                        layer_id="clip:3",
                        stable_source_id="legacy-adjustment",
                        name="tone",
                        kind="unknown",
                        blob_bytes=None,
                        source_meta={
                            "type": 32,
                            "folder": 0,
                            "composite_id": 0,
                            "masking": 0,
                            "render_payload_sha256": digest,
                            "semantic_digest_version": 2,
                            "semantic_digest": "new-digest",
                        },
                    )
                ],
                composite_png=b"",
                thumbnail_png=b"",
            )

        entry = {"source_format": "clip", "source_blob_ref": source_ref, "tree_ref": tree_ref}
        same_digest = _clip_normalized_exta_payload_sha256(payload)
        self.assertEqual([], repo.layer_changes(entry, imported_with_payload_digest(same_digest)))
        self.assertEqual(
            ["changed layer tone (other properties changed)"],
            repo.layer_changes(entry, imported_with_payload_digest("0" * 64)),
        )

    def test_clip_rectangular_composite_matches_stable_samples(self) -> None:
        import hashlib

        from tmi.formats import clip as clip_mod

        expected = {
            "blend_modes.clip": "ad4f19fef20f2cc7",
            "layer_types.clip": "6954ce17a7bb07b3",
        }
        for name, digest in expected.items():
            with self.subTest(name=name):
                clip = clip_mod.open_clip(PROJECT_ROOT / "example" / name)
                image = clip_mod.render_composite_rgba(clip)
                self.assertEqual(digest, hashlib.sha256(image.tobytes()).hexdigest()[:16])

    def test_clip_layered_composite_matches_canvas_preview_pixel_exact(self) -> None:
        """tmi's own layer compositor must reproduce CSP's CanvasPreview exactly.

        ``CanvasPreview.ImageData`` is the PNG Clip Studio Paint renders for its
        thumbnail; for canvases up to ~2048 px it is stored at full canvas
        resolution, so it is ground truth for the compositor. blend_modes.clip
        is a 1600x1200 sample that stacks one raster layer per blend mode, so an
        exact match proves the blend math, opacity and stacking order all agree
        with CSP — not just tmi's own preview passthrough.
        """
        import numpy as np

        from tmi.formats import clip as clip_mod

        clip = clip_mod.open_clip(PROJECT_ROOT / "example" / "blend_modes.clip")
        preview = clip_mod.render_canvas_preview_rgba(clip, canvas_size=True)
        self.assertIsNotNone(preview)
        composite = clip_mod.render_layered_composite_rgba(clip)
        self.assertEqual(preview.size, composite.size)
        diff = np.abs(
            np.asarray(preview, dtype=np.int16)[..., :3]
            - np.asarray(composite, dtype=np.int16)[..., :3]
        )
        self.assertEqual(int(diff.max()), 0, "layered composite must equal CSP preview")

    def test_clip_folder_layer_mask_clips_base_to_silhouette(self) -> None:
        """Regression: a folder's single-channel layer mask must clip its content.

        tama.clip colours its character inside a folder whose bottom layer is a
        near-full-canvas dark base; a layer mask restricts the folder to the
        character silhouette. CSP stores that mask's value in the offscreen's
        *alpha* plane (R/G/B are a constant fill), so reading the value from R
        left the mask a no-op and the dark base bled across the whole canvas
        (background brightness ~110). Reading it from alpha keeps the background
        the light paper colour and tracks the CSP preview closely.
        """
        import numpy as np

        from tmi.formats import clip as clip_mod

        clip = clip_mod.open_clip(PROJECT_ROOT / "example" / "tama.clip")
        composite = clip_mod.render_layered_composite_rgba(clip)
        preview = clip_mod.render_canvas_preview_rgba(clip)
        self.assertIsNotNone(preview)
        comp_small = composite.resize(preview.size)  # LANCZOS
        ours = np.asarray(comp_small, dtype=np.float32)[..., :3]
        theirs = np.asarray(preview, dtype=np.float32)[..., :3]
        # Corners are background, not the masked-out dark base. Compared with
        # CSP's own preview rather than a fixed "light paper" value: the bottom
        # of tama.clip is green grass once layers sit at LayerOffset +
        # LayerRenderOffscrOffset (they were misplaced before).
        h, w = theirs.shape[:2]
        for y, x in [(10, 10), (10, w - 22), (h - 22, 10)]:
            self.assertLess(
                abs(ours[y:y + 12, x:x + 12].mean() - theirs[y:y + 12, x:x + 12].mean()), 12.0,
                f"corner near ({x},{y}) differs from the CSP preview",
            )
        # ~4 now (preview is half resolution; residual is resampling plus the
        # unsupported colour-balance layer); was ~92 with the dark base bleeding
        # through and ~20 with layers misplaced.
        self.assertLess(float(np.abs(theirs - ours).mean()), 10.0)

    def test_clip_adjustment_filter_math_is_exact(self) -> None:
        """The directly-implemented correction filters compute their textbook
        result: invert = 1-x, posterise quantises to N levels, binarise
        thresholds luminance to black/white."""
        import numpy as np

        from tmiref import clip as clip_mod

        invert = clip_mod._filter_invert(b"")
        np.testing.assert_allclose(
            invert(np.array([[[0.2, 0.5, 0.8]]], np.float32)), [[[0.8, 0.5, 0.2]]], atol=1e-6
        )
        posterize = clip_mod._filter_posterize(struct.pack(">I", 4))  # 4 levels
        np.testing.assert_allclose(
            posterize(np.array([[[0.0, 0.3, 0.6]]], np.float32)),
            [[[0.0, 1 / 3, 2 / 3]]], atol=1e-6,
        )
        binarize = clip_mod._filter_binarize(struct.pack(">I", 128))
        out = binarize(np.array([[[0.1, 0.1, 0.1], [0.9, 0.9, 0.9]]], np.float32))
        np.testing.assert_array_equal(out, [[[0, 0, 0], [1, 1, 1]]])
        # A zero-effect parameter decodes to None so an identity layer is skipped.
        self.assertIsNone(clip_mod._filter_posterize(struct.pack(">I", 1)))

    def test_clip_adjustment_layer_regrades_backdrop_with_opacity(self) -> None:
        """A correction layer re-grades the backdrop already in the surface,
        attenuated by its opacity (256 = full)."""
        from types import SimpleNamespace

        from tmiref import clip as clip_mod

        invert = clip_mod._filter_invert(b"")
        full = clip_mod._CompositeSurface.fixed_canvas(Image.new("RGBA", (2, 2), (200, 100, 50, 255)))
        node = SimpleNamespace(opacity=256, clip=0, mask_mipmap=None, masking=0)
        clip_mod._apply_adjustment_to_surface(None, full, node, invert, [], [], 0)
        self.assertEqual(full.image.getpixel((0, 0)), (55, 155, 205, 255))

        half = clip_mod._CompositeSurface.fixed_canvas(Image.new("RGBA", (2, 2), (200, 100, 50, 255)))
        node_half = SimpleNamespace(opacity=128, clip=0, mask_mipmap=None, masking=0)
        clip_mod._apply_adjustment_to_surface(None, half, node_half, invert, [], [], 0)
        # 50 % between 200 and 55 → 127.5 → 128 (rounded).
        self.assertEqual(half.image.getpixel((0, 0))[0], 128)

    def test_clip_adjustment_layer_clipped_to_base_only_affects_base(self) -> None:
        """A clipping correction layer re-grades only the pixels of its clipping
        base, not the rest of the backdrop."""
        from types import SimpleNamespace

        from tmiref import clip as clip_mod

        surface = clip_mod._CompositeSurface.fixed_canvas(Image.new("RGBA", (4, 1), (200, 100, 50, 255)))
        # Base layer covers only the left half (x=0,1); the adjustment clips to it.
        base_img = Image.new("RGBA", (2, 1), (0, 0, 0, 255))
        base = clip_mod._LayerBuffer(base_img, 0, 0)
        layers = [SimpleNamespace(clip=0), SimpleNamespace(clip=1)]
        node = SimpleNamespace(opacity=256, clip=1, mask_mipmap=None, masking=0)
        clip_mod._apply_adjustment_to_surface(None, surface, node, clip_mod._filter_invert(b""), [base, None], layers, 1)
        self.assertEqual(surface.image.getpixel((0, 0)), (55, 155, 205, 255))  # inside base → inverted
        self.assertEqual(surface.image.getpixel((3, 0)), (200, 100, 50, 255))  # outside base → unchanged

    def test_clip_brightness_contrast_matches_csp_slopes(self) -> None:
        # CSP: brightness first, then slope 100/(100-c) for c > 0 and (100+c)/100
        # for c < 0. The old 1 + c/50 agreed only at c = +50.
        import numpy as np

        from tmiref import clip as clip_mod

        x = np.array([[[0.2, 0.5, 0.8]]], np.float32)
        for b, c, slope in ((0, 50, 2.0), (30, 20, 1.25), (0, -30, 0.7), (-20, 0, 1.0)):
            fn = clip_mod._filter_brightness_contrast(struct.pack(">ii", b, c))
            expected = (x + b / 255.0 - 0.5) * slope + 0.5
            np.testing.assert_allclose(fn(x), expected, atol=1e-6, err_msg=f"b={b} c={c}")

    def test_clip_glow_modes_add_premultiplied_source(self) -> None:
        # 加算(発光) = Cb + Cs·αs and 覆い焼き(発光) = Cb / (1 − Cs·αs), fitted
        # against example/csp515/glow_*.clip; unlike 加算 the half-transparent
        # source is not alpha-mixed back towards the backdrop.
        from tmiref import clip as clip_mod

        src = Image.new("RGBA", (1, 1), (200, 100, 0, 128))
        a = 128 / 255
        for mode, expected in (
            ("glow_add", (min(255, 60 + 200 * a), 60 + 100 * a, 60)),
            ("glow_dodge", (60 / (1 - 200 / 255 * a), 60 / (1 - 100 / 255 * a), 60)),
        ):
            target = Image.new("RGBA", (1, 1), (60, 60, 60, 255))
            clip_mod._blend_onto_at(target, src, mode, 1.0, (0, 0))
            got = target.getpixel((0, 0))
            for g, e in zip(got[:3], expected):
                self.assertAlmostEqual(g, e, delta=1.0, msg=mode)
            self.assertEqual(got[3], 255)

    def test_clip_gradient_fill_parses_and_eases_like_csp(self) -> None:
        # GradationFillInfo as CSP 5.1.5 writes it: 24-byte GradationData header
        # (mixing mode, luminance-correction level), 28-byte stops, then the
        # line's endpoints in GradationSetting. Segments are S-eased, not linear.
        from tmiref import clip as clip_mod
        from tmiref import clip_gradient

        def section(name: str, body: bytes) -> bytes:
            return struct.pack(">I", len(name)) + name.encode("utf-16-be") + body

        stops = b"".join(
            struct.pack(">7I", r << 24, g << 24, b << 24, 0xFFFFFFFF, 0, pos, 0)
            for r, g, b, pos in ((0, 0, 0, 0), (255, 128, 0, 0x8000))
        )
        data = struct.pack(">6I", 24, 28, 2, 16, 0, 3) + stops
        setting = struct.pack(">3I", 0, 0, 0) + struct.pack(">7d", 100, 100, 0, 0.0, 0.0, 64.0, 0.0)
        blob = section("GradationData", struct.pack(">I", len(data)) + data) + section("GradationSetting", setting)

        info = clip_mod._parse_gradation_fill(blob)
        self.assertEqual(info["stops"], [(0.0, (0, 0, 0, 255)), (1.0, (255, 128, 0, 255))])
        self.assertEqual(info["points"], (0.0, 0.0, 64.0, 0.0))
        self.assertEqual((info["mode"], info["level"]), (0, 3))
        self.assertIsNone(info["solid"])

        img = clip_mod._render_gradation_fill(info, (64, 2))
        self.assertEqual(img.getpixel((0, 1))[3], 255)
        mid = img.getpixel((32, 0))[0]
        self.assertAlmostEqual(mid, 255 * float(clip_gradient.ease(32.5 / 64)), delta=1.5)
        # Eased: a tenth of the way along is well below the linear 10 %.
        self.assertLess(img.getpixel((6, 0))[0], 0.1 * 255 * 0.7)

        # 知覚的 (mode 1) mixes black→white far darker in the middle than 通常.
        perceptual = clip_mod._render_gradation_fill({**info, "mode": 1, "level": 0}, (64, 2))
        self.assertLess(perceptual.getpixel((32, 0))[0], mid // 4)

    def test_clip_comic_frame_rebuilds_mask_and_border_from_vector(self) -> None:
        # A コマ枠 folder saves no pixels: its clip mask is the frame polygon and
        # its border is a mitred stroke of total width 2w centred on the edge,
        # both read from the frame's vector record (header 0x60, points 88 B).
        from types import SimpleNamespace

        from tmiref import clip_frame

        pts = [(10.0, 10.0), (50.0, 10.0), (50.0, 50.0), (10.0, 50.0)]
        header = struct.pack(">6I", 0x60, 72, 88, 88, len(pts), 0x1118) + struct.pack(">4i", 6, 6, 54, 54)
        header += struct.pack(">6I", 0, 0, 0, 0, 0, 0) + struct.pack(">d", 1.0) + bytes(12)
        header += struct.pack(">d", 2.0) + bytes(4)
        self.assertEqual(len(header), 0x60)
        inner = header + b"".join(struct.pack(">dd", x, y) + bytes(72) for x, y in pts)
        objs = clip_frame.parse_frame_objects(inner)
        self.assertEqual(len(objs), 1)
        self.assertEqual(objs[0]["points"], pts)
        self.assertEqual((objs[0]["half_width"], objs[0]["color"]), (2.0, (0, 0, 0)))

        canvas = SimpleNamespace(width=64, height=64)
        clip = SimpleNamespace(frame_cache={("objects", 1): objs}, canvas=canvas)
        node = SimpleNamespace(main_id=1)
        mask, mx, my = clip_frame.frame_mask(clip, node)
        self.assertEqual(mask.getpixel((30 - mx, 30 - my)), 255)
        self.assertEqual(mask.getpixel((9 - mx, 30 - my)), 0)
        border, bx, by = clip_frame.frame_border(clip, node)
        self.assertEqual(border.getpixel((9 - bx, 30 - by)), (0, 0, 0, 255))   # on the left edge
        self.assertEqual(border.getpixel((10 - bx, 30 - by)), (0, 0, 0, 255))
        self.assertEqual(border.getpixel((30 - bx, 30 - by))[3], 0)            # interior
        self.assertEqual(border.getpixel((8 - bx, 8 - by)), (0, 0, 0, 255))    # mitred corner

    def test_clip_layer_style_colour_material_and_edge(self) -> None:
        # レイヤーカラー maps dark → main and light → sub colour (miura's magenta
        # fills have main = sub = white and show white in CSP); an alpha-only
        # raster on a mono/grey layer takes black when BlackChecked is set.
        from tmiref import clip_style

        white = 0xFFFFFFFF
        style = {"DrawColorEnable": 1, "LayerType": 2}
        style.update({f"DrawColor{k}{c}": white for k in ("Main", "Sub") for c in ("Red", "Green", "Blue")})
        out = clip_style.apply_layer_style(Image.new("RGBA", (1, 1), (255, 0, 128, 200)), style, None)
        self.assertEqual(out.getpixel((0, 0)), (255, 255, 255, 200))
        paper = dict(style, LayerType=1584)  # the paper layer's DrawColor is its fill
        img = Image.new("RGBA", (1, 1), (255, 0, 128, 255))
        self.assertIs(clip_style.apply_layer_style(img, paper, None), img)

        mono = {"LayerColorTypeIndex": 1, "LayerColorTypeBlackChecked": 1, "LayerColorTypeWhiteChecked": 1}
        out = clip_style.apply_layer_style(Image.new("RGBA", (1, 1), (255, 255, 255, 90)), mono, "single-channel")
        self.assertEqual(out.getpixel((0, 0)), (0, 0, 0, 90))

        # ResizableImageInfo: header u32 ×10 (orig size at [8], [9]), f64 ×7,
        # flags, then the four destination corners as the last 64 bytes.
        blob = struct.pack(">10I", 184, 16, 4, 0, 0, 0, 2, 2, 200, 100) + struct.pack(">7d", 0.5, 0.5, 0, 0, 0, 0, 0)
        blob += bytes(16) + struct.pack(">8d", 10, 20, 110, 20, 10, 70, 110, 70)
        info = clip_style.parse_resizable_image_info(blob)
        self.assertEqual(info["orig_size"], (200, 100))
        self.assertFalse(info["tile"])
        self.assertEqual(info["corners"], [(10, 20), (110, 20), (10, 70), (110, 70)])

        # フチ: LayerEffectInfo section "EffectEdge" = u32 on, f64 width, u32 RGB.
        name = "EffectEdge"
        effect = struct.pack(">I", len(name)) + name.encode("utf-16-be") + struct.pack(">Id3I", 1, 3.5, white, white, white)
        self.assertEqual(clip_style.parse_edge_effect({"LayerEffectAttached": 1, "LayerEffectInfo": effect}), (3.5, (255, 255, 255)))
        self.assertIsNone(clip_style.parse_edge_effect({"LayerEffectAttached": 0, "LayerEffectInfo": effect}))

    def test_clip_vector_layers_are_restroked_like_csp(self) -> None:
        """CSP saves no pixels for ベクター layers; tmi re-strokes them (Catmull-Rom
        centre line, brush pressure curve) and the コマ枠 border of a frame whose
        vector header is 88/92 bytes. example/vector_*.clip are not in git."""
        import numpy as np

        from tmi.formats import clip as clip_mod

        for name, limit in (("vector_simple.clip", 0.2), ("vector_complex.clip", 1.0)):
            path = PROJECT_ROOT / "example" / name
            if not path.exists():
                self.skipTest(f"{name} not present")
            clip = clip_mod.open_clip(path)
            preview = np.asarray(clip_mod.render_canvas_preview_rgba(clip, canvas_size=True), np.int16)[..., :3]
            layered = np.asarray(clip_mod.render_layered_composite_rgba(clip), np.int16)[..., :3]
            self.assertLessEqual(float(np.abs(preview - layered).mean()), limit, name)

    def test_csp515_gradients_match_every_mixing_mode(self) -> None:
        """example/csp515/grad_*.clip: black→white (and red→white) gradients
        saved by CSP 5.1.5 in each 混色モード / 輝度の補正 level (not in git)."""
        import numpy as np

        from tmi.formats import clip as clip_mod

        paths = sorted((PROJECT_ROOT / "example" / "csp515").glob("grad_*.clip"))
        if not paths:
            self.skipTest("CSP 5.1.5 gradient probes not present")
        for path in paths:
            clip = clip_mod.open_clip(path)
            preview = np.asarray(clip_mod.render_canvas_preview_rgba(clip, canvas_size=True), np.int16)[..., :3]
            layered = np.asarray(clip_mod.render_layered_composite_rgba(clip), np.int16)[..., :3]
            self.assertLessEqual(int(np.abs(preview - layered).max()), 3, path.name)

    def test_clip_layer_visibility_reads_only_the_eye_bit(self) -> None:
        # LayerVisibility is a bit field: CSP 5.1.5 stores 3 for a visible
        # correction layer (eye + mask bit) and 2 for a hidden one.
        import sqlite3

        from tmi.formats import clip as clip_mod

        con = sqlite3.connect(":memory:")
        con.execute(
            "CREATE TABLE Layer (MainId, LayerName, LayerType, LayerFolder, LayerVisibility, LayerOpacity,"
            " LayerComposite, LayerClip, LayerMasking, LayerOffsetX, LayerOffsetY, LayerUuid,"
            " LayerRenderMipmap, LayerLayerMaskMipmap, LayerFirstChildIndex, LayerNextIndex)"
        )
        for main_id, vis in ((1, 0), (2, 1), (3, 2), (4, 3)):
            con.execute(
                "INSERT INTO Layer VALUES (?, 'l', 4098, 0, ?, 256, 0, 0, 33, 0, 0, NULL, NULL, NULL, NULL, NULL)",
                (main_id, vis),
            )
        cols = {row[1] for row in con.execute("PRAGMA table_info(Layer)")}
        try:
            seen = {main_id: clip_mod.load_layer(con, main_id).visible for main_id in (1, 2, 3, 4)}
        except sqlite3.OperationalError as exc:  # schema drift in load_layer's SELECT
            self.fail(f"load_layer needs columns beyond {sorted(cols)}: {exc}")
        self.assertEqual({1: False, 2: True, 3: False, 4: True}, seen)

    def test_clip_layer_mask_is_gated_by_visibility_bit_not_layer_masking(self) -> None:
        # CSP 5.1.5: a raster layer with an active mask has LayerMasking 0 and
        # LayerVisibility 3; disabling the mask clears bit 1 (-> 1).
        from types import SimpleNamespace

        from tmiref import clip as clip_mod

        enabled = SimpleNamespace(mask_mipmap=12, masking=0, mask_enabled=True)
        disabled = SimpleNamespace(mask_mipmap=12, masking=0, mask_enabled=False)
        no_mask = SimpleNamespace(mask_mipmap=0, masking=33, mask_enabled=True)
        legacy = SimpleNamespace(mask_mipmap=12, masking=1)  # built without mask_enabled
        self.assertTrue(clip_mod._mask_is_enabled(enabled))
        self.assertFalse(clip_mod._mask_is_enabled(disabled))
        self.assertFalse(clip_mod._mask_is_enabled(no_mask))
        self.assertTrue(clip_mod._mask_is_enabled(legacy))

    def test_clip_layer_is_placed_at_layer_offset_plus_render_offscreen_offset(self) -> None:
        # Real CSP files store e.g. LayerOffset (537,40) with
        # LayerRenderOffscrOffset (-537,-40); the pixels belong at the sum.
        from types import SimpleNamespace
        from unittest import mock

        from tmiref import clip as clip_mod

        layer = SimpleNamespace(render_mipmap=1, offset_x=537, offset_y=40, render_offset_x=-537, render_offset_y=-30)
        buf = clip_mod._LayerBuffer(Image.new("RGBA", (4, 4), (9, 9, 9, 255)), 100, 200)
        with mock.patch.object(clip_mod, "_render_offscreen_buffer", lambda clip, mip: buf):
            placed = clip_mod._render_layer_buffer(None, layer, (0, 0, 2000, 2000))
        self.assertEqual((100, 210), (placed.x, placed.y))

    def test_clip_comic_frame_folder_mask_is_not_applied(self) -> None:
        from types import SimpleNamespace

        from tmiref import clip as clip_mod

        frame = SimpleNamespace(mask_mipmap=7, masking=114, mask_enabled=True, comic_frame=True)
        self.assertFalse(clip_mod._mask_is_enabled(frame))

    def test_clip_clipping_layer_without_base_in_folder_draws_unclipped(self) -> None:
        from types import SimpleNamespace
        from unittest import mock

        from tmiref import clip as clip_mod

        def node(main_id, clip_flag):
            return SimpleNamespace(
                main_id=main_id, name=f"n{main_id}", visible=True, folder=False, composite=0, clip=clip_flag,
                opacity=256, render_mipmap=main_id, offset_x=0, offset_y=0, mask_mipmap=None, masking=0,
                type=1, children=[],
            )

        green = Image.new("RGBA", (4, 4), (0, 200, 0, 255))
        with mock.patch.object(clip_mod, "_render_layer_buffer", lambda clip, n, bounds, region=None: clip_mod._LayerBuffer(green.copy(), 0, 0)), \
             mock.patch.object(clip_mod, "_decode_filter_layer", lambda clip, n: None):
            surface = clip_mod._CompositeSurface.fixed_canvas(Image.new("RGBA", (4, 4), (255, 255, 255, 255)))
            clip_mod._composite_group_surface(None, surface, [node(1, 1)])  # clipped, nothing below
        self.assertEqual((0, 200, 0, 255), surface.image.getpixel((1, 1)))

    def test_csp515_saves_composite_like_canvas_preview(self) -> None:
        """Files saved by CLIP STUDIO PAINT 5.1.5 (example/csp515, not in git):
        tmi's layered composite must match CSP's own CanvasPreview, including
        hidden correction layers and brightness/contrast/tone-curve layers."""
        import numpy as np

        from tmi.formats import clip as clip_mod
        from tmi.importers import import_file, walk_imported_layers

        folder = PROJECT_ROOT / "example" / "csp515"
        paths = sorted(folder.glob("*.clip")) if folder.exists() else []
        if not paths:
            self.skipTest("example/csp515 is not available")
        for path in paths:
            clip = clip_mod.open_clip(path)
            preview = clip_mod.render_canvas_preview_rgba(clip, canvas_size=True)
            if preview is None:
                continue
            layered = clip_mod.render_layered_composite_rgba(clip)
            diff = np.abs(np.asarray(preview, np.int16)[..., :3] - np.asarray(layered, np.int16)[..., :3])
            if path.name.startswith("tone_"):
                # トーン output is binary dots: a threshold one step off flips a
                # pixel fully, so count pixels instead of bounding the difference.
                self.assertLessEqual(float((diff.max(axis=2) > 3).mean()), 0.005, path.name)
            else:
                self.assertLessEqual(int(diff.max()), 3, path.name)
            for layer in walk_imported_layers(import_file(path, render_composite=False).layers):
                if layer.source_meta.get("type") == 4098:
                    self.assertEqual("adjustment", layer.kind, path.name)

    def test_clip_tone_effect_parses_and_screens(self) -> None:
        # EffectTone: u32 on, f64 dpi, u32 shape (1 circle / 2 square / 4 line),
        # u32 ×2, f64 lines per inch, u32 ×2, u32 angle. Cells are integer
        # lattices of period dpi/lpi (at least 2√2); output is binary ink.
        import numpy as np

        from tmiref import clip_tone

        name = "EffectTone"
        body = struct.pack(">Id3Id3I", 1, 72.0, 1, 0, 20, 10.0, 1, 0, 45) + struct.pack(">I", 30) + bytes(20)
        blob = struct.pack(">I", len(name)) + name.encode("utf-16-be") + body
        tone = clip_tone.parse_tone_effect({"LayerEffectAttached": 1, "LayerEffectInfo": blob})
        self.assertEqual((tone["lpi"], tone["angle"], tone["shape"]), (10.0, 45.0, "circle"))
        self.assertIsNone(clip_tone.parse_tone_effect({"LayerEffectAttached": 0, "LayerEffectInfo": blob}))
        self.assertEqual(((5, 5), (-5, 5), False), clip_tone.screen_vectors(10.0, 45.0, 72.0))
        self.assertEqual(((3, 0), (0, 3), True), clip_tone.screen_vectors(30.0, 0.0, 72.0))  # 2.4 px < 2√2

        for grey, expected in ((0, 1.0), (255, 0.0)):
            out = clip_tone.apply_tone(Image.new("RGBA", (40, 40), (grey, grey, grey, 255)), 0, 0, tone, 72.0)
            alpha = np.asarray(out)[..., 3]
            self.assertAlmostEqual(float((alpha == 255).mean()), expected, places=6)
            self.assertTrue(set(np.unique(alpha)) <= {0, 255})
        half = np.asarray(clip_tone.apply_tone(Image.new("RGBA", (50, 50), (128, 128, 128, 255)), 0, 0, tone, 72.0))[..., 3]
        self.assertAlmostEqual(float((half == 255).mean()), 0.5, delta=0.05)

    def test_native_adjustment_regrade_matches_numpy(self) -> None:
        """tmi_accel's regrade_rgba (a per-channel correction filter as a LUT,
        weighted by opacity, mask and clipping base) gives the reference's
        numpy regrade byte for byte."""
        import random
        from types import SimpleNamespace
        from unittest import mock

        import numpy as np

        from tmi import accel
        from tmiref import clip as ref

        rng = random.Random(11)
        w, h = 37, 23

        def noise(n: int) -> bytes:
            return bytes(rng.randrange(256) for _ in range(n))

        curve = bytearray(4 * 130)
        curve[0:2] = struct.pack(">H", 4)
        curve[2:18] = struct.pack(">8H", 0, 0, 20000, 9000, 45000, 58000, 65535, 65535)
        rgb_curve = bytearray(curve)
        for section, (x, y) in ((1, (33000, 49000)), (2, (33000, 16000)), (3, (16000, 41000))):
            off = section * 130
            rgb_curve[off:off + 14] = struct.pack(">7H", 3, 0, 0, x, y, 65535, 65535)
        filters = {
            "brightness+contrast": ref._filter_brightness_contrast(struct.pack(">ii", 37, -21)),
            "contrast": ref._filter_brightness_contrast(struct.pack(">ii", 0, 50)),
            "tone_curve": ref._filter_tone_curve(bytes(curve)),
            "tone_curve r/g/b + master": ref._filter_tone_curve(bytes(rgb_curve)),
            "color_balance": ref._filter_color_balance(struct.pack(">10i", 1, -25, 2, 7, 10, 2, 9, 9, -1, -7)),
            "color_balance no-lum": ref._filter_color_balance(struct.pack(">10i", 0, 0, 0, 0, 30, -30, 20, 0, 0, 0)),
            "invert": ref._filter_invert(b""),
            "posterize": ref._filter_posterize(struct.pack(">I", 5)),
        }
        backdrop = noise(w * h * 4)
        mask = Image.frombytes("L", (w, h), noise(w * h))
        base = ref._LayerBuffer(Image.frombytes("RGBA", (20, 11), noise(20 * 11 * 4)), 30, 15)
        # The clipping base's alpha placed on the surface, as the renderer passes it.
        plane = Image.new("L", (w, h), 0)
        plane.paste(base.image.getchannel("A").crop((0, 0, w - 30, h - 15)), (30, 15))
        layers = [SimpleNamespace(clip=0), SimpleNamespace(clip=1)]
        for name, fn in filters.items():
            self.assertIsNotNone(fn, name)
            self.assertTrue(getattr(fn, "per_channel", True), name)
            for opacity, use_mask, clipped in ((256, False, False), (77, True, False), (200, True, True)):
                node = SimpleNamespace(opacity=opacity, clip=int(clipped), mask_mipmap=None, masking=0)
                surface = ref._CompositeSurface.fixed_canvas(Image.frombytes("RGBA", (w, h), backdrop))
                with mock.patch.object(ref, "_layer_mask_crop", lambda *a: mask if use_mask else None):
                    ref._apply_adjustment_to_surface(None, surface, node, fn, [base, None], layers, 1)
                got = accel.native().regrade_rgba(
                    backdrop, w, h, ref._filter_lut(fn), float(np.float32(opacity / 256.0)),
                    mask.tobytes() if use_mask else None, plane.tobytes() if clipped else None,
                )
                case = f"{name} opacity={opacity} mask={use_mask} clip={clipped}"
                self.assertEqual(surface.image.tobytes(), bytes(got), case)
                self.assertNotEqual(backdrop, bytes(got), case)

    def test_distinct_colour_regrade_matches_per_pixel_path(self) -> None:
        """Channel-mixing filters (hue/saturation, binarize) are evaluated once
        per distinct backdrop colour on large surfaces; the result must be
        byte-identical to running the filter over every pixel."""
        from types import SimpleNamespace
        from unittest import mock

        import numpy as np

        from tmiref import clip as clip_mod

        rng = np.random.default_rng(23)
        w, h = 61, 47
        palette = rng.integers(0, 256, size=(97, 4), dtype=np.uint8)
        few = palette[rng.integers(0, len(palette), size=(h, w))]
        many = rng.integers(0, 256, size=(h, w, 4), dtype=np.uint8)
        mask = Image.fromarray(rng.integers(0, 256, size=(h, w), dtype=np.uint8), "L")
        base = clip_mod._LayerBuffer(Image.fromarray(rng.integers(0, 256, size=(11, 20, 4), dtype=np.uint8), "RGBA"), 30, 15)
        layers = [SimpleNamespace(clip=0), SimpleNamespace(clip=1)]
        filters = {
            "hue/sat +sat": clip_mod._filter_hue_saturation(struct.pack(">iii", -6, 37, 0)),
            "hue/sat -sat +lum": clip_mod._filter_hue_saturation(struct.pack(">iii", 0, -40, 33)),
            "binarize": clip_mod._filter_binarize(struct.pack(">I", 100)),
        }
        for name, fn in filters.items():
            self.assertIsNotNone(fn, name)
            self.assertFalse(getattr(fn, "per_channel", True), name)
            for backdrop_name, backdrop in (("few", few), ("many", many)):
                for opacity, use_mask, clipped in ((256, False, False), (77, False, False), (77, True, False), (200, True, True)):
                    node = SimpleNamespace(opacity=opacity, clip=int(clipped), mask_mipmap=None, masking=0)
                    results = []
                    for threshold in (0, 1 << 40):
                        surface = clip_mod._CompositeSurface.fixed_canvas(Image.fromarray(backdrop, "RGBA"))
                        with mock.patch.object(clip_mod, "_DISTINCT_COLOUR_MIN_PIXELS", threshold), mock.patch.object(
                            clip_mod, "_layer_mask_crop", lambda *a: mask if use_mask else None
                        ):
                            clip_mod._apply_adjustment_to_surface(None, surface, node, fn, [base, None], layers, 1)
                        results.append(surface.image.tobytes())
                    case = f"{name} {backdrop_name} opacity={opacity} mask={use_mask} clip={clipped}"
                    self.assertEqual(results[1], results[0], case)
                    self.assertNotEqual(backdrop.tobytes(), results[0], case)

    def test_scaled_image_material_samples_pixel_centres_bilinearly(self) -> None:
        """A non-tiled 画像素材 maps destination pixel centres to source pixel
        centres and interpolates bilinearly (probe_material_scale.clip: 300 % /
        250 % checkerboards match CSP exactly); scale 1 is a plain copy."""
        from types import SimpleNamespace
        from unittest import mock

        import numpy as np

        from tmiref import clip_style

        src = np.zeros((16, 16, 4), np.uint8)
        src[..., 0] = (np.arange(16) * 10)[None, :]
        src[..., 1] = (np.arange(16) * 10)[:, None]
        src[..., 3] = 255
        node = SimpleNamespace(main_id=1, offset_x=0, offset_y=0)
        for scale in (2.0, 1.0):
            info = {
                "orig_size": (16, 16),
                "tile": False,
                "tile_mode": 0,
                "scale": (scale, scale),
                "angle": 0.0,
                "centre": (0.0, 0.0),
                "corners": [(10.0, 10.0), (10.0 + 16 * scale, 10.0), (10.0, 10.0 + 16 * scale), (10.0 + 16 * scale, 10.0 + 16 * scale)],
            }
            with mock.patch.object(clip_style, "layer_style", lambda *a: {"ResizableImageInfo": b"x", "ResizableOriginalMipmap": 1}), mock.patch.object(
                clip_style, "_full_offscreen", lambda *a: (Image.fromarray(src, "RGBA"), None)
            ), mock.patch.object(clip_style, "parse_resizable_image_info", lambda blob: info):
                buf, _ = clip_style.render_image_material(None, node, (0, 0, 64, 64))
            out = np.asarray(buf.image)
            self.assertEqual((10, 10, 10 + int(16 * scale), 10 + int(16 * scale)), buf.bounds)
            for x, y in ((20, 23), (24, 18), (13, 21)):
                r, g = out[y - buf.y, x - buf.x, :2]
                u = (x + 0.5 - 10) / scale - 0.5
                v = (y + 0.5 - 10) / scale - 0.5
                self.assertLessEqual(abs(int(r) - 10 * u), 1, f"scale={scale} x={x}")
                self.assertLessEqual(abs(int(g) - 10 * v), 1, f"scale={scale} y={y}")

    def test_csp_probes_for_materials_and_vectors_match_preview(self) -> None:
        """CSP-saved probes (example/additional_examples, not in git): 画像素材
        scaled up / down / rotated, the tiled 斑雲1 texture on colour / grey /
        mono canvases, and mouse-drawn vector strokes before and after 線幅修正."""
        import numpy as np

        from tmi.formats import clip as clip_mod

        folder = PROJECT_ROOT / "example" / "additional_examples"
        limits = {
            "probe_material_scale.clip": (0.02, 3),
            "probe_material_madara_color.clip": (0.0, 0),
            "probe_material_madara_grey.clip": (0.0, 0),
            "probe_material_madara_mono.clip": (0.0, 0),
            "probe_vector_mouse.clip": (0.4, 255),
            "probe_vector_mouse_senhaba.clip": (0.4, 255),
        }
        present = [name for name in limits if (folder / name).exists()]
        if not present:
            self.skipTest("CSP material / vector probes not present")
        for name in present:
            clip = clip_mod.open_clip(folder / name)
            preview = np.asarray(clip_mod.render_canvas_preview_rgba(clip, canvas_size=True), np.int16)[..., :3]
            layered = np.asarray(clip_mod.render_layered_composite_rgba(clip), np.int16)[..., :3]
            diff = np.abs(preview - layered)
            mean_limit, max_limit = limits[name]
            self.assertLessEqual(float(diff.mean()), mean_limit, name)
            self.assertLessEqual(int(diff.max()), max_limit, name)

    def test_linear_gradient_bands_match_single_pass(self) -> None:
        """Gradient fills render in parallel row bands; the dither phase is
        counted from the canvas bottom, so any band height gives the same bytes."""
        from unittest import mock

        import numpy as np

        from tmiref import clip_gradient

        stops = [(0.0, (10, 20, 30, 255)), (0.37, (200, 100, 50, 128)), (1.0, (255, 255, 255, 0))]
        for size, mode in (((1100, 1001), 0), ((1031, 1100), 1), ((97, 61), 2)):
            renders = []
            for rows in (1 << 30, 7):
                with mock.patch.object(clip_gradient, "_BAND_ROWS", rows):
                    renders.append(clip_gradient.render_linear_gradient(stops, (-40.0, 900.0, 1000.0, 15.0), size, mode))
            self.assertTrue(np.array_equal(renders[0], renders[1]), f"{size} mode={mode}")

    def test_native_glow_blend_matches_numpy_over_full_alpha_range(self) -> None:
        """加算(発光)/覆い焼き(発光) in tmi_accel: every (αb, αs) pair, including
        a transparent backdrop and opacity 1, matches the reference's numpy."""
        import numpy as np

        from tmiref import composite
        from tmiref import clip as ref

        rng = np.random.default_rng(5)
        a = np.repeat(np.arange(256, dtype=np.uint8), 256).reshape(256, 256)
        target = rng.integers(0, 256, size=(256, 256, 4), dtype=np.uint8)
        source = rng.integers(0, 256, size=(256, 256, 4), dtype=np.uint8)
        target[..., 3] = a
        source[..., 3] = a.T
        for mode in ("glow_add", "glow_dodge"):
            for opacity in (1.0, 0.6):
                out = []
                for blend in (lambda *a: ref._blend_onto(*a, csp=False), composite._blend_onto):
                    image = Image.fromarray(target, "RGBA")
                    blend(image, Image.fromarray(source, "RGBA"), mode, opacity)
                    out.append(image.tobytes())
                self.assertEqual(out[0], out[1], f"{mode} opacity={opacity}")

    def test_banded_numpy_blend_matches_one_pass_reference(self) -> None:
        """The reference's numpy blend runs over row bands on a thread pool;
        every mode must give the bytes of the one-pass numpy blend (frozen
        below as it was before banding), whatever the band height."""
        from unittest import mock

        import numpy as np

        from tmiref import clip as clip_mod

        def reference(target: Image.Image, source: Image.Image, mode: str, opacity: float) -> bytes:
            backdrop = np.asarray(target)
            base = backdrop.astype(np.float32) / 255.0
            src = np.asarray(source, dtype=np.float32) / 255.0
            αb = base[..., 3:4]
            αs = src[..., 3:4] * float(opacity)
            untouched = αs[..., 0] == 0
            Cb = base[..., :3]
            Cs = src[..., :3]
            if mode in clip_mod._GLOW_MODES:
                x = Cs * αs
                if mode == "glow_add":
                    G = np.minimum(1.0, Cb + x)
                else:
                    G = np.where(x >= 1.0, 1.0, np.minimum(1.0, Cb / np.maximum(1e-6, 1.0 - x)))
                αo = αs + αb * (1.0 - αs)
                safe_αo = np.where(αo > 0, αo, 1.0)
                Co = np.where(αo > 0, (αb * G + (1.0 - αb) * x) / safe_αo, 0.0)
                out = np.clip(np.concatenate([Co, αo], axis=-1) * 255.0, 0.0, 255.0).astype(np.uint8)
                out[untouched] = backdrop[untouched]
                return out.tobytes()
            blend_fn = clip_mod._BLEND_FNS.get(mode)
            Bc = Cs if blend_fn is None else np.clip(blend_fn(Cb, Cs), 0.0, 1.0)
            Cs_eff = (1.0 - αb) * Cs + αb * Bc
            αo = αs + αb * (1.0 - αs)
            safe_αo = np.where(αo > 0, αo, 1.0)
            Co = (αs * Cs_eff + (1.0 - αs) * αb * Cb) / safe_αo
            Co = np.where(αo > 0, Co, 0.0)
            out = np.clip(np.concatenate([Co, αo], axis=-1) * 255.0, 0.0, 255.0).astype(np.uint8)
            out[untouched] = backdrop[untouched]
            return out.tobytes()

        rng = np.random.default_rng(31)
        h, w = 53, 64
        target = rng.integers(0, 256, size=(h, w, 4), dtype=np.uint8)
        target[: h // 3, :, 3] = 255
        target[-5:, :, 3] = 0  # transparent backdrop with stray colour
        source = rng.integers(0, 256, size=(h, w, 4), dtype=np.uint8)
        source[..., 3] = np.where(rng.random((h, w)) < 0.3, 0, source[..., 3])
        source[0, 0, 3] = source[-1, -1, 3] = 255  # bbox = the whole source
        modes = sorted(set(clip_mod._BLEND_FNS) | clip_mod._GLOW_MODES | {"no_such_mode"})
        for mode in modes:
            for opacity in (1.0, 0.6, 0.125):
                if mode == clip_mod.BLEND_NORMAL and opacity >= 0.999:
                    continue  # Pillow's alpha_composite, not numpy
                want = reference(Image.fromarray(target, "RGBA"), Image.fromarray(source, "RGBA"), mode, opacity)
                # csp=True (CLIP's rounding) against its own one-pass blend
                want_csp = clip_mod._blend_pixels(target, source, mode, opacity, csp=True).tobytes()
                for band_pixels in (w * 3, w * 7 + 5, 1 << 40):
                    for csp, expected in ((False, want), (True, want_csp)):
                        with mock.patch.object(clip_mod, "_NUMPY_BAND_PIXELS", band_pixels):
                            got = Image.fromarray(target, "RGBA")
                            clip_mod._blend_onto(got, Image.fromarray(source, "RGBA"), mode, opacity, csp=csp)
                        self.assertEqual(expected, got.tobytes(), f"{mode} opacity={opacity} band={band_pixels} csp={csp}")

    def test_banded_numpy_regrade_matches_one_pass(self) -> None:
        """A per-channel correction layer regrades row bands on a thread pool
        in the reference; the bytes must not depend on the bands."""
        from types import SimpleNamespace
        from unittest import mock

        import numpy as np

        from tmiref import clip as clip_mod

        rng = np.random.default_rng(17)
        h, w = 41, 37
        backdrop = rng.integers(0, 256, size=(h, w, 4), dtype=np.uint8)
        mask = Image.fromarray(rng.integers(0, 256, size=(h, w), dtype=np.uint8), "L")
        base = clip_mod._LayerBuffer(Image.fromarray(rng.integers(0, 256, size=(11, 20, 4), dtype=np.uint8), "RGBA"), 9, 15)
        layers = [SimpleNamespace(clip=0), SimpleNamespace(clip=1)]
        curve = bytearray(4 * 130)
        curve[0:2] = struct.pack(">H", 4)
        curve[2:18] = struct.pack(">8H", 0, 0, 20000, 9000, 45000, 58000, 65535, 65535)
        filters = {
            "brightness+contrast": clip_mod._filter_brightness_contrast(struct.pack(">ii", 37, -21)),
            "tone_curve": clip_mod._filter_tone_curve(bytes(curve)),
            "color_balance": clip_mod._filter_color_balance(struct.pack(">10i", 1, -25, 2, 7, 10, 2, 9, 9, -1, -7)),
            "levels": clip_mod._filter_levels(struct.pack(">5H", 0, 20000, 65535, 0, 65535)),
            "invert": clip_mod._filter_invert(b""),
            "posterize": clip_mod._filter_posterize(struct.pack(">I", 5)),
        }
        for name, fn in filters.items():
            self.assertIsNotNone(fn, name)
            self.assertTrue(getattr(fn, "per_channel", True), name)
            for opacity, use_mask, clipped in ((256, False, False), (77, True, False), (200, True, True), (130, False, True)):
                node = SimpleNamespace(opacity=opacity, clip=int(clipped), mask_mipmap=None, masking=0)
                results = []
                for band_pixels in (1 << 40, w * 2, w * 5 + 3):
                    surface = clip_mod._CompositeSurface.fixed_canvas(Image.fromarray(backdrop, "RGBA"))
                    with mock.patch.object(clip_mod, "_NUMPY_BAND_PIXELS", band_pixels), mock.patch.object(
                        clip_mod, "_layer_mask_crop", lambda *a: mask if use_mask else None
                    ):
                        clip_mod._apply_adjustment_to_surface(None, surface, node, fn, [base, None], layers, 1)
                    results.append(surface.image.tobytes())
                case = f"{name} opacity={opacity} mask={use_mask} clip={clipped}"
                self.assertEqual(len(set(results)), 1, case)
                self.assertNotEqual(backdrop.tobytes(), results[0], case)

    def test_prefetched_layer_decode_matches_serial_render(self) -> None:
        """A group's leaf layers decode on a thread pool ahead of compositing;
        the composite is byte for byte the one decoded layer by layer."""
        from unittest import mock

        from tmiref import clip as clip_mod

        names = ["layer_types.clip", "blend_modes.clip", "csp515/structure.clip", "csp515/mask_off.clip",
                 "csp515/adjust_neg.clip", "vector_complex.clip"]
        paths = [PROJECT_ROOT / "example" / name for name in names]
        paths = [p for p in paths if p.exists()]
        if not paths:
            self.skipTest("no example CLIP files")
        for path in paths:
            renders = []
            for prefetch in (clip_mod._prefetch_offscreen_buffers, lambda clip, nodes: None):
                with mock.patch.object(clip_mod, "_prefetch_offscreen_buffers", prefetch):
                    clip = clip_mod.open_clip(path)
                    renders.append(clip_mod.render_layered_composite_rgba(clip).tobytes())
                    self.assertEqual({}, clip.offscreen_prefetch, path.name)
            self.assertEqual(renders[0], renders[1], path.name)

    def test_diff_preview_with_known_bbox_matches(self) -> None:
        from tmi.image_ops import diff_bbox, diff_preview

        old = Image.new("RGBA", (40, 30), (10, 20, 30, 255))
        new = old.copy()
        new.paste((200, 0, 0, 255), (5, 6, 17, 20))
        smaller = Image.new("RGBA", (31, 22), (0, 0, 0, 0))
        for left, right in ((old, new), (smaller, new), (new, smaller)):
            bbox = diff_bbox(left, right)
            self.assertIsNotNone(bbox)
            self.assertEqual(diff_preview(left, right).tobytes(), diff_preview(left, right, bbox=bbox).tobytes())

    def test_side_by_side_pastes_opaque_sides_byte_for_byte(self) -> None:
        """An opaque side is pasted instead of alpha-composited onto the grey
        backdrop; the result must equal the alpha_composite one."""
        import numpy as np

        from tmi.image_ops import side_by_side

        rng = np.random.default_rng(3)
        opaque = rng.integers(0, 256, size=(30, 41, 4), dtype=np.uint8)
        opaque[..., 3] = 255
        translucent = rng.integers(0, 256, size=(30, 41, 4), dtype=np.uint8)
        smaller = rng.integers(0, 256, size=(22, 35, 4), dtype=np.uint8)
        smaller[..., 3] = 255
        for a, b in ((opaque, opaque), (opaque, translucent), (translucent, opaque), (smaller, opaque)):
            left, right = Image.fromarray(a, "RGBA"), Image.fromarray(b, "RGBA")
            w, h = max(left.width, right.width), max(left.height, right.height)
            want = Image.new("RGBA", (w * 2 + 16, h), (32, 32, 32, 255))
            for image, x in ((left, 0), (right, w + 16)):
                padded = Image.new("RGBA", (w, h), (0, 0, 0, 0))
                padded.alpha_composite(image, (0, 0))
                want.alpha_composite(padded, (x, 0))
            self.assertEqual(want.tobytes(), side_by_side(left, right).tobytes())

    def _assert_probe_matches_preview(self, name: str, tol: float) -> None:
        """A probe clip is a grey ramp + one correction layer; CSP's CanvasPreview
        is full-resolution, so the layered composite must reproduce it once the
        filter is implemented. Probe clips were generated in CSP for RE
        (research §15.5); skip if not present."""
        import numpy as np

        from tmi.formats import clip as clip_mod

        path = PROJECT_ROOT / "example" / "additional_examples" / name
        if not path.exists():
            self.skipTest(f"probe clip {name} not present")
        clip = clip_mod.open_clip(path)
        preview = clip_mod.render_canvas_preview_rgba(clip)
        composite = clip_mod.render_layered_composite_rgba(clip)
        self.assertEqual(preview.size, composite.size)
        diff = np.abs(
            np.asarray(preview, dtype=np.int16)[..., :3]
            - np.asarray(composite, dtype=np.int16)[..., :3]
        )
        self.assertLessEqual(float(diff.max()), tol, f"{name}: max channel diff {diff.max()}")

    def test_clip_tone_curve_reproduces_canvas_preview(self) -> None:
        """Tone Curve (Bézier control points) matches CSP to <=1 LSB on a grey
        ramp — validates the decoded curve format and the Bézier evaluation."""
        self._assert_probe_matches_preview("probe_tonecurve.clip", 2)

    def test_clip_tone_curve_scurve_reproduces_canvas_preview(self) -> None:
        """A 4-point S-curve matches CSP to <=1 LSB — confirms the quadratic
        spline (interior points are off-curve controls) generalises beyond 3
        points, where a single high-degree Bézier drifts by ~28."""
        self._assert_probe_matches_preview("probe_tonecurve_Sji.clip", 2)

    def test_clip_brightness_reproduces_canvas_preview(self) -> None:
        """Brightness (additive shift) matches CSP to <=1 LSB on a grey ramp."""
        self._assert_probe_matches_preview("probe_akarusa+50.clip", 2)

    def test_clip_contrast_reproduces_canvas_preview(self) -> None:
        """Contrast (slope = 1 + contrast/50 about mid-grey) matches CSP to
        <=1 LSB on a grey ramp."""
        self._assert_probe_matches_preview("probe_contrust+50.clip", 2)

    def test_clip_levels_reproduces_canvas_preview(self) -> None:
        """Levels with the mid (gamma) slider dragged to 0 → out = in^0.1."""
        self._assert_probe_matches_preview("probe_levelhosei_center_to_0.clip", 2)

    def test_clip_hue_saturation_reproduces_canvas_preview(self) -> None:
        """Hue/Saturation/Lightness works in HSV (lightness, then saturation,
        then hue; negative saturation pulls V to (max+min)/2), on a grey and on
        a colour ramp."""
        self._assert_probe_matches_preview("probe_shikiso+50_saido-30_meido+5.clip", 2)
        self._assert_probe_matches_preview("probe_shikiso+90_saido-50_colored.clip", 2)

    def test_clip_hue_saturation_lightness_and_positive_saturation(self) -> None:
        """Positive lightness blends towards white in HSV (S scales by 1-l),
        negative towards black; positive saturation raises V by s·min(RGB),
        capped at 1, and moves S towards 1 by the share the cap let through —
        so a hue ramp over white (V = 1) is left unchanged. Probed on a hue
        ramp at 100 %, at 50 % over white ("pastel") and at 50 % over #808080
        ("grey")."""
        for name in (
            "probe_shikiso0_saido-40_meido+33_colored.clip",
            "probe_shikiso0_saido-40_meido+33_pastel.clip",
            "probe_shikiso0_saido-40_meido+33_grey.clip",
            "probe_shikiso-6_saido+3_colored.clip",
            "probe_saido+50_colored.clip",
            "probe_saido+50_pastel.clip",
            "probe_saido+50_grey.clip",
            "probe_saido+100_grey.clip",
            "probe_meido-30_pastel.clip",
        ):
            self._assert_probe_matches_preview(name, 2)
        # Three adjustments at once over a 50 % layer: CSP's backdrop keeps
        # precision that tmi's 8-bit one rounds away (two ramp colours that
        # blend to the same 8-bit value come out differently), so a few
        # pixels land 3 levels off.
        for name in (
            "probe_shikiso+30_saido+50_meido-20_pastel.clip",
            "probe_shikiso+30_saido+50_meido-20_grey.clip",
        ):
            self._assert_probe_matches_preview(name, 3)

    def test_clip_color_balance_reproduces_canvas_preview(self) -> None:
        """Colour balance is per-channel levels: black / white points for the
        shadow / highlight ranges and a gamma for midtones, coupled across the
        channels only while preserve-luminosity is on."""
        for name in (
            "probe_shikiso_red+20_yellow+10_blue-10.clip",
            "probe_colorbalance_highlightOnly_blue+50.clip",
            "probe_colorbalance_shadowOnly_red+50.clip",
            "probe_colorbalance_mid_r+30_keeplum.clip",
            "probe_colorbalance_mid_r+30_nolum.clip",
            "probe_colorbalance_mid_g-30_b+20_keeplum.clip",
            "probe_colorbalance_mid_g-30_b+20_nolum.clip",
            "probe_colorbalance_shadowOnly_red+50_nolum.clip",
            "probe_colorbalance_highlightOnly_blue+50_nolum.clip",
        ):
            self._assert_probe_matches_preview(name, 2)

    def test_clip_tone_curve_rgb_sections_reproduce_canvas_preview(self) -> None:
        """Tone-curve sections 1/2/3 are R/G/B and run before the master."""
        self._assert_probe_matches_preview("probe_tonecurve_RGB.clip", 2)
        self._assert_probe_matches_preview("probe_tonecurve_RGB_master.clip", 2)

    def test_clip_offset_blend_matches_full_canvas_source(self) -> None:
        from tmiref import clip as clip_mod

        full_target = Image.new("RGBA", (6, 5), (80, 120, 160, 255))
        rect_target = full_target.copy()
        patch = Image.new("RGBA", (3, 2), (220, 70, 30, 160))
        full_source = Image.new("RGBA", full_target.size, (0, 0, 0, 0))
        full_source.alpha_composite(patch, (2, 1))

        clip_mod._blend_onto(full_target, full_source, "hard_light", 0.5)
        surface = clip_mod._CompositeSurface.fixed_canvas(rect_target)
        clip_mod._blend_buffer_onto_surface(surface, clip_mod._LayerBuffer(patch, 2, 1), "hard_light", 0.5)

        self.assertEqual(full_target.tobytes(), rect_target.tobytes())

    def test_native_blend_matches_numpy_reference(self) -> None:
        """tmi's blend (tmi_accel.blend_rgba, which MDP2 composites with) gives
        the reference's numpy blend in every mode it knows."""
        from tmi import accel
        from tmiref import composite
        from tmiref import clip as ref

        modes = sorted(accel.native().supported_blend_modes())
        for width, height, salt in ((13, 9, 5), (17, 11, 41), (8, 15, 97)):
            target_bytes = bytes(
                (x * 17 + y * 3 + c * 29 + salt) % 256 for y in range(height) for x in range(width) for c in range(4)
            )
            source_bytes = bytes(
                (x * 11 + y * 19 + c * 37 + 5 + salt * 3) % 256 for y in range(height) for x in range(width) for c in range(4)
            )
            for opacity in (0.125, 0.4375, 0.875):
                for mode in modes:
                    source = Image.frombytes("RGBA", (width, height), source_bytes)
                    reference = Image.frombytes("RGBA", (width, height), target_bytes)
                    ref._blend_onto(reference, source, mode, opacity, csp=False)
                    accelerated = Image.frombytes("RGBA", (width, height), target_bytes)
                    composite._blend_onto(accelerated, source, mode, opacity)
                    case = f"{mode} {width}x{height} opacity={opacity}"
                    self.assertEqual(reference.tobytes(), accelerated.tobytes(), case)

    def test_native_clip_blend_matches_numpy_reference(self) -> None:
        """The CLIP renderer's blend (composite_ops' blend op,
        blend::blend_pixel_clip: CSP's 8-bit opacity and rounding) gives the
        reference's numpy blend with csp=True in every mode, at every alpha
        and at opacities that are and are not multiples of 1/256."""
        import numpy as np

        from tmi import accel
        from tmiref import clip as ref

        native = accel.native()
        modes = sorted(accel.native().supported_blend_modes())
        width, height = 256, 16
        rng = np.random.default_rng(12)
        target = rng.integers(0, 256, (height, width, 4), dtype=np.uint8)
        target[: height // 2, :, 3] = 255  # opaque and translucent backdrops
        source = rng.integers(0, 256, (height, width, 4), dtype=np.uint8)
        source[0, :, 3] = np.arange(256, dtype=np.uint8)
        for opacity in (1.0, 0.5, 77 / 256, 0.4375, 0.3):
            for mode in modes:
                want = ref._blend_pixels(target, source, mode, opacity, csp=True)
                op = (1, source.tobytes(), width, height, 0, 0, width, height, 0, 0, mode, opacity)
                got = native.composite_ops(target.tobytes(), width, height, [op])
                self.assertEqual(want.tobytes(), got, f"{mode} opacity={opacity}")

    def test_transparent_source_pixels_leave_the_backdrop_alone(self) -> None:
        """Under a fully transparent source pixel every blend mode leaves the
        backdrop byte for byte as it was — opaque, translucent, or transparent
        with stray colour — in tmi_accel and in the reference's numpy alike.
        So blending a part of a layer gives the pixels the whole layer gives
        there, however far the layer's bounding box reaches."""
        import numpy as np

        from tmi import accel
        from tmiref import composite
        from tmiref import clip as ref

        rng = np.random.default_rng(11)
        h, w = 48, 64
        target = rng.integers(0, 256, size=(h, w, 4), dtype=np.uint8)
        target[..., 3] = np.resize(np.array([0, 1, 37, 128, 200, 254, 255, 255], np.uint8), (h, w))
        source = rng.integers(0, 256, size=(h, w, 4), dtype=np.uint8)
        source[..., 3] = np.where(rng.random((h, w)) < 0.5, 0, source[..., 3])
        source[0, 0, 3] = source[-1, -1, 3] = 255  # the bounding box is the whole source
        transparent = source[..., 3] == 0
        window = (10, 7, 50, 40)  # a part whose bounding box is smaller

        modes = sorted(set(ref._BLEND_FNS) | ref._GLOW_MODES | set(accel.native().supported_blend_modes()))
        for mode in modes:
            for opacity in (1.0, 0.6, 0.125):
                results = []
                # the reference's W3C blend against tmi_accel's (MDP2), and
                # the reference's CLIP blend (csp=True) on its own
                for impl, kw in ((ref, {"csp": False}), (composite, {}), (ref, {"csp": True})):
                    case = f"{mode} opacity={opacity} {impl.__name__} {kw}"
                    whole = Image.fromarray(target, "RGBA")
                    impl._blend_onto(whole, Image.fromarray(source, "RGBA"), mode, opacity, **kw)
                    out = np.asarray(whole)
                    self.assertTrue(np.array_equal(out[transparent], target[transparent]), case)
                    part = Image.fromarray(target, "RGBA")
                    crop = Image.fromarray(source, "RGBA").crop(window)
                    impl._blend_onto_at(part, crop, mode, opacity, window[:2], **kw)
                    self.assertEqual(part.crop(window).tobytes(), whole.crop(window).tobytes(), case)
                    if kw.get("csp") is not True:
                        results.append(whole.tobytes())
                self.assertEqual(len(set(results)), 1, f"{mode} opacity={opacity}: tmi_accel differs from numpy")

    def test_native_clip_tile_decode_matches_python_reference(self) -> None:
        """tmi's tile decoder (tmi_accel) against the reference's Python one:
        every tile encoding, gaps, empty tiles, a repeated block, an unknown
        tile size (warned about, transparent) and an unreadable stream."""
        import contextlib
        import random
        from types import SimpleNamespace

        from tmi.formats import clip as clip_mod
        from tmiref import clip as ref

        rng = random.Random(7)

        def noise(n: int) -> bytes:
            return bytes(rng.randrange(256) for _ in range(n))

        def tile_payload(kind: str, w: int, h: int) -> bytes:
            n = w * h
            if kind == "bit":
                return noise(n // 8)
            if kind == "bit2":
                return noise(n // 4)
            if kind == "alpha":
                return bytes(0 if i % 7 == 0 else b for i, b in enumerate(noise(n)))
            if kind == "gray":
                return noise(n * 2)
            if kind == "clear":
                return bytes(n)
            if kind == "odd":
                return noise(3 * n)  # no such encoding
            # colour: alpha plane with transparent runs over non-zero BGRA
            alpha = bytearray(noise(n))
            alpha[: n // 3] = bytes(n // 3)
            return bytes(alpha) + noise(n * 4)

        def build(tiles: list[tuple[int, str | None, int, int]]):
            def record(name: str, body: bytes) -> bytes:
                encoded = name.encode("utf-16-be")
                return struct.pack(">II", 8 + len(encoded) + len(body), len(name)) + encoded + body

            inner = bytearray(record(clip_mod.END_NAME, b"\0" * 4))
            for block_index, kind, w, h in tiles:
                if kind is None:
                    body = struct.pack(">IIIII", block_index, 0, w, h, 0)
                else:
                    raw = tile_payload(kind, w, h)
                    blob = struct.pack(">I", len(raw)) + zlib.compress(raw)
                    body = struct.pack(">IIIIII", block_index, len(raw), w, h, 1, len(blob)) + blob
                inner += record(clip_mod.BEGIN_NAME, body)
            outer_id = "extrnlid" + "0" * 32
            payload = struct.pack(">Q", 0x28) + outer_id.encode("ascii") + struct.pack(">Q", len(inner)) + bytes(inner)
            raw = b"pad!" + payload
            chunk = clip_mod.Chunk(b"Exta", 0, 4, len(payload))
            return SimpleNamespace(raw=raw, extas={outer_id: chunk}), outer_id

        def decode(clip, outer_id, attr, native: bool):
            """(buffer, warnings printed), unknown-size warnings afresh."""
            clip_mod._warned_tile_sizes.clear()
            err = io.StringIO()
            with contextlib.redirect_stderr(err):
                if native:
                    buf = clip_mod._decode_offscreen(clip, outer_id, attr)
                else:
                    buf = ref._python_offscreen_buffer(clip, outer_id, attr)
            return buf, err.getvalue()

        def as_tuple(decoded):
            buf, warnings = decoded
            if clip_mod._is_empty_buffer(buf):
                return "empty", warnings
            return (buf.x, buf.y, buf.image.size, buf.image.tobytes()), warnings

        attr = SimpleNamespace(tile_cols=3, tile_rows=2, pixel_width=2 * 256 + 100, pixel_height=256 + 30)
        cases = {
            "mixed": [
                (0, "color", 256, 256),
                (1, "bit", 64, 40),
                (2, "bit2", 256, 256),
                (3, "gray", 256, 256),
                (4, "alpha", 256, 256),
                (5, None, 256, 256),
            ],
            "sparse": [(4, "color", 256, 256), (2, "clear", 256, 256)],
            "transparent": [(0, "clear", 256, 256), (1, None, 256, 256)],
            "no-tiles": [],
            "unknown size": [(0, "color", 256, 256), (1, "odd", 256, 256)],
        }
        for name, tiles in cases.items():
            clip, outer_id = build(tiles)
            native = as_tuple(decode(clip, outer_id, attr, native=True))
            self.assertEqual(as_tuple(decode(clip, outer_id, attr, native=False)), native, name)
            self.assertEqual(name == "unknown size", "unknown CLIP tile size" in native[1], name)

        # A repeated block is composited over the first, as Python does.
        clip, outer_id = build([(0, "color", 256, 256), (0, "alpha", 256, 256)])
        self.assertEqual(as_tuple(decode(clip, outer_id, attr, native=False)), as_tuple(decode(clip, outer_id, attr, native=True)))
        clip, outer_id = build([(0, "color", 256, 256)])
        clip.raw = clip.raw[:-4] + bytes(4 if clip.raw[-4:] != bytes(4) else [1, 1, 1, 1])
        for native in (False, True):
            with self.assertRaises(Exception, msg=f"bad zlib checksum, native={native}"):
                decode(clip, outer_id, attr, native=native)

    def test_layer_blob_stores_only_the_painted_rect_and_expands_losslessly(self) -> None:
        from tmi import layer_blob
        from tmi.image_ops import (
            LayerCrop,
            image_from_bytes,
            layer_blob_digest,
            layer_blob_to_png,
        )

        full = Image.new("RGBA", (40, 30), (0, 0, 0, 0))
        full.putpixel((5, 4), (10, 20, 30, 255))
        full.putpixel((20, 25), (200, 100, 50, 128))
        full.putpixel((33, 7), (9, 9, 9, 0))  # colour under zero alpha must survive
        empty = Image.new("RGBA", (40, 30), (0, 0, 0, 0))

        for image in (full, empty):
            crop = LayerCrop.from_full(image)
            blob = crop.encode()
            self.assertEqual(image.tobytes(), image_from_bytes(blob).tobytes())
            self.assertEqual(crop.digest(), layer_blob_digest(blob))
            plain = Image.open(io.BytesIO(layer_blob_to_png(blob)))
            self.assertEqual(image.size, plain.size)
            self.assertEqual(image.tobytes(), plain.convert("RGBA").tobytes())
        crop = LayerCrop.from_full(full)
        self.assertEqual((5, 4, 34, 26), crop.rect)
        header = layer_blob.parse_header(crop.encode())
        self.assertEqual(((40, 30), (5, 4, 34, 26)), (header.size, header.rect))
        self.assertEqual([5, 4, 21, 26], crop.alpha_bbox())

        # Same pixels, same digest — whichever path produced the crop.
        tight = full.crop((5, 4, 34, 26))
        self.assertEqual(
            LayerCrop.from_full(full).digest(),
            LayerCrop.from_tight_crop(full.size, 5, 4, tight).digest(),
        )
        self.assertNotEqual(LayerCrop.from_full(full).digest(), LayerCrop.from_full(empty).digest())

    def test_empty_layer_blobs_keep_their_own_size_under_concurrent_encode(self) -> None:
        # Regression: empty layers encoded one shared 1x1 Image; Pillow's save()
        # keeps encoderinfo on the Image, so concurrent leaf-pool encodes could
        # stamp another layer's "v1 W H" header onto a blob. (Layer blobs are
        # TMIL now, without Pillow; the size must still be each layer's own.)
        from concurrent.futures import ThreadPoolExecutor

        from tmi.image_ops import LayerCrop, image_from_bytes

        sizes = [(w, h) for w in range(1, 41) for h in (1, 7, 13)]
        for _ in range(3):
            with ThreadPoolExecutor(16) as pool:
                blobs = list(pool.map(lambda s: LayerCrop.from_tight_crop(s, 0, 0, None).encode(), sizes))
            self.assertEqual(sizes, [image_from_bytes(b).size for b in blobs])

    def test_clip_composite_reuses_canvas_preview_png_with_identical_pixels(self) -> None:
        from tmi.formats import clip as clip_mod
        from tmi.image_ops import image_from_bytes

        paths = sorted((PROJECT_ROOT / "example").glob("*.clip"))
        if not paths:
            self.skipTest("no example CLIP files")
        reused = 0
        for path in paths:
            clip = clip_mod.open_clip(path)
            image, png = clip_mod.render_composite_with_png(clip)
            expected = clip_mod.render_composite_rgba(clip)
            self.assertEqual(expected.tobytes(), image.tobytes(), path.name)
            if png is not None:
                reused += 1
                self.assertEqual(expected.tobytes(), image_from_bytes(png).tobytes(), path.name)
        self.assertGreater(reused, 0)

    def _reuse_maps(self, imported):
        from tmi.importers import walk_imported_layers

        decoded, pixels = {}, {}
        for layer in walk_imported_layers(imported.layers):
            source = (layer.source_meta or {}).get("pixel_source")
            if layer.pixel_sha256 and source:
                decoded[layer.stable_source_id] = {
                    "pixel_source": source,
                    "pixel_sha256": layer.pixel_sha256,
                    "pixel_bbox": layer.pixel_bbox,
                }
                pixels[layer.stable_source_id] = layer.pixel_sha256
        return decoded, pixels

    def _layer_summary(self, imported):
        from tmi.importers import walk_imported_layers

        return {
            layer.stable_source_id: (layer.pixel_sha256, layer.pixel_bbox, layer.source_meta)
            for layer in walk_imported_layers(imported.layers)
        }

    def test_unchanged_layers_skip_decoding_with_identical_results(self) -> None:
        from unittest import mock

        from tmi import importers
        from tmi.formats import clip as clip_mod
        from tmi.formats import mdp2 as mdp2_mod

        paths = sorted((PROJECT_ROOT / "example").glob("*.clip")) + sorted((PROJECT_ROOT / "example").glob("*.mdp"))
        if not paths:
            self.skipTest("no example files")
        for path in paths:
            fresh = importers.import_file(path, render_composite=False)
            decoded, pixels = self._reuse_maps(fresh)
            if not decoded:
                continue

            decodes: list[str] = []
            real_clip = clip_mod.render_offscreen_crop_from_info
            real_mdp = mdp2_mod.render_layer_crop

            def clip_spy(clip, info):
                out = real_clip(clip, info)
                if out is not None:
                    decodes.append(info[2])
                return out

            def mdp_spy(mdp, layer, *args, **kwargs):
                decodes.append(layer.name)
                return real_mdp(mdp, layer, *args, **kwargs)

            with mock.patch.object(clip_mod, "render_offscreen_crop_from_info", clip_spy), \
                 mock.patch.object(mdp2_mod, "render_layer_crop", mdp_spy):
                again = importers.import_file(
                    path, render_composite=False, reuse_pixels=pixels, reuse_decoded=decoded
                )
            self.assertEqual([], decodes, f"{path.name}: unchanged layers were decoded")
            self.assertEqual(self._layer_summary(fresh), self._layer_summary(again), path.name)
            for layer in importers.walk_imported_layers(again.layers):
                if layer.stable_source_id in decoded:
                    self.assertIsNone(layer.blob_bytes, path.name)

        # A different decoder version must never reuse an older decode.
        path = paths[0]
        fresh = importers.import_file(path, render_composite=False)
        decoded, _ = self._reuse_maps(fresh)
        with mock.patch.object(importers, "PIXEL_SOURCE_VERSION", importers.PIXEL_SOURCE_VERSION + 1):
            bumped = importers.import_file(path, render_composite=False, reuse_decoded=decoded)
        for layer in importers.walk_imported_layers(bumped.layers):
            if layer.stable_source_id in decoded:
                self.assertIsNotNone(layer.blob_bytes, "stale decode reused after a version bump")

    def test_editing_one_clip_layer_redecodes_only_that_layer(self) -> None:
        from tmi import importers
        from tmi.formats import clip as clip_mod

        # tama.clip: the small samples' raster offscreens are all empty.
        src = PROJECT_ROOT / "example" / "tama.clip"
        if not src.exists():
            self.skipTest("example/tama.clip is not available")
        work = self.tmp / "art.clip"
        shutil.copyfile(src, work)
        base = importers.import_file(work, render_composite=False)
        decoded, pixels = self._reuse_maps(base)
        target = next(
            layer for layer in importers.walk_imported_layers(base.layers)
            if layer.stable_source_id in decoded
            and layer.source_meta.get("write_capability") == "raster-pixels"
            and layer.pixel_bbox
        )
        size = (target.source_meta["pixel_width"], target.source_meta["pixel_height"])
        edited = Image.new("RGBA", size, (0, 0, 0, 0))
        edited.paste((12, 200, 90, 255), (0, 0, min(40, size[0]), min(30, size[1])))
        buf = io.BytesIO()
        edited.save(buf, "PNG")
        work.write_bytes(clip_mod.replace_layer_pixels(work, target.stable_source_id, buf.getvalue()))

        reused = importers.import_file(work, render_composite=False, reuse_pixels=pixels, reuse_decoded=decoded)
        fresh = importers.import_file(work, render_composite=False)
        self.assertEqual(self._layer_summary(fresh), self._layer_summary(reused))
        redecoded = {
            layer.stable_source_id
            for layer in importers.walk_imported_layers(reused.layers)
            if layer.blob_bytes is not None
        }
        self.assertEqual({target.stable_source_id}, redecoded)
        self.assertNotEqual(decoded[target.stable_source_id]["pixel_sha256"],
                            self._layer_summary(fresh)[target.stable_source_id][0])

    def test_object_store_skips_deflate_for_incompressible_payloads(self) -> None:
        from tmi.objects import ObjectStore, _zlib_level_for

        store = ObjectStore(self.tmp / "objects")
        noise = os.urandom(1 << 20)  # already-compressed-like data
        text = b"layer " * (1 << 18)  # highly compressible
        for payload, level in ((noise, 0), (text, 1), (b"tiny", 1)):
            self.assertEqual(level, _zlib_level_for(b"blob\0" + payload), len(payload))
            oid = store.write("blob", payload)
            self.assertEqual(payload, store.read_blob(oid))
        self.assertLess(store.path_for(store.write("blob", text)).stat().st_size, len(text) // 10)

    def test_native_png_encode_round_trips_and_is_deterministic(self) -> None:
        import random

        from tmi.image_ops import image_to_png_bytes

        rng = random.Random(3)
        # Wide enough that one image spans several 256 KiB deflate chunks.
        for width, height in ((1, 1), (7, 5), (640, 480)):
            data = bytes(rng.randrange(256) for _ in range(width * height * 4))
            image = Image.frombytes("RGBA", (width, height), data)
            encoded = image_to_png_bytes(image)
            self.assertTrue(encoded.startswith(b"\x89PNG"))
            self.assertEqual(data, Image.open(io.BytesIO(encoded)).convert("RGBA").tobytes())
            self.assertEqual(encoded, image_to_png_bytes(image))
            pillow = image_to_png_bytes(image, parallel=False)
            self.assertEqual(data, Image.open(io.BytesIO(pillow)).convert("RGBA").tobytes())

        gray = Image.new("L", (9, 9), 128)  # non-RGBA keeps Pillow's own encoding
        self.assertEqual("L", Image.open(io.BytesIO(image_to_png_bytes(gray))).mode)

    def test_native_clip_offscreens_match_python_on_example(self) -> None:
        """tama.clip's offscreens decode in tmi (tmi_accel) as in the reference's
        Python tile decoder: the alpha-bbox buffer and the full-size image."""
        from tmi.formats import clip as clip_mod
        from tmiref import clip as ref

        path = PROJECT_ROOT / "example" / "tama.clip"
        if not path.exists():
            self.skipTest("example/tama.clip is not available")

        clip = clip_mod.open_clip(path)
        infos = [clip_mod.layer_offscreen_info(clip, n.render_mipmap) for n in clip_mod.walk_layer_nodes(clip.root.children)]
        infos = [info for info in infos if info and info[2] in clip.extas]
        self.assertTrue(infos)
        for k, info in enumerate(infos):
            native_buf = clip_mod._decode_offscreen(clip, info[2], clip_mod.parse_offscreen_attribute(info[1]))
            python_buf = ref._render_offscreen_buffer_from_info(clip, info)
            if ref._is_empty_buffer(python_buf):
                self.assertTrue(ref._is_empty_buffer(native_buf), info[2])
            else:
                self.assertEqual(
                    (python_buf.x, python_buf.y, python_buf.image.size),
                    (native_buf.x, native_buf.y, native_buf.image.size),
                    info[2],
                )
                self.assertEqual(python_buf.image.tobytes(), native_buf.image.tobytes(), info[2])
            if k % 8 == 0:  # (the Python full-size decode is slow)
                native_full = clip_mod.render_offscreen_from_info(clip, info)
                python_full = ref.render_offscreen_from_info(clip, info)
                self.assertEqual(python_full.image.size, native_full.image.size, info[2])
                self.assertEqual(python_full.image.tobytes(), native_full.image.tobytes(), info[2])

    def test_clip_pass_through_mix_can_clear_fixed_transparent_canvas(self) -> None:
        from types import SimpleNamespace

        from tmiref import clip as clip_mod

        target = clip_mod._CompositeSurface.fixed_canvas(Image.new("RGBA", (4, 4), (0, 0, 0, 0)))
        before = target.copy()
        target.image.alpha_composite(Image.new("RGBA", (2, 2), (255, 0, 0, 255)), (1, 1))

        clip_mod._mix_pass_through_result(None, SimpleNamespace(mask_mipmap=None, masking=0), target, before, 0.0)

        self.assertEqual(bytes(4 * 4 * 4), target.image.tobytes())

    def test_clip_composite_30_folder_is_pass_through(self) -> None:
        from types import SimpleNamespace

        from tmiref import clip as clip_mod

        child = SimpleNamespace(
            name="multiply-child",
            visible=True,
            folder=False,
            composite=2,  # multiply
            clip=0,
            opacity=256,
            render_mipmap=1,
            offset_x=0,
            offset_y=0,
            mask_mipmap=None,
            masking=0,
            main_id=2,
        )
        folder = SimpleNamespace(
            name="through-folder",
            visible=True,
            folder=True,
            composite=30,
            clip=0,
            opacity=256,
            mask_mipmap=None,
            masking=0,
            children=[child],
            main_id=1,
        )

        original = clip_mod._render_layer_buffer
        try:
            def fake_render_layer_buffer(_clip, _layer, _bounds, _region=None):
                return clip_mod._LayerBuffer(Image.new("RGBA", (2, 2), (0, 0, 255, 255)), 0, 0)

            clip_mod._render_layer_buffer = fake_render_layer_buffer
            target = Image.new("RGBA", (2, 2), (255, 0, 0, 255))
            clip_mod._composite_group(None, target, [folder])
        finally:
            clip_mod._render_layer_buffer = original

        self.assertEqual((0, 0, 0, 255), target.getpixel((0, 0)))

    def test_clip_clipping_to_empty_raster_base_hides_source(self) -> None:
        from types import SimpleNamespace

        from tmiref import clip as clip_mod

        def node(name: str, *, clipped: bool = False):
            return SimpleNamespace(
                name=name,
                visible=True,
                folder=False,
                composite=0,
                clip=1 if clipped else 0,
                opacity=256,
                render_mipmap=1,
                offset_x=0,
                offset_y=0,
                mask_mipmap=None,
                masking=0,
                main_id=1 if name == "base" else 2,
            )

        original = clip_mod._render_layer_buffer
        try:
            def fake_render_layer_buffer(_clip, layer, _bounds, _region=None):
                if layer.name == "base":
                    return clip_mod._EMPTY_LAYER_BUFFER
                return clip_mod._LayerBuffer(Image.new("RGBA", (2, 2), (255, 0, 0, 255)), 0, 0)

            clip_mod._render_layer_buffer = fake_render_layer_buffer
            target = Image.new("RGBA", (4, 4), (0, 0, 0, 0))
            clip_mod._composite_group(None, target, [node("base"), node("source", clipped=True)])
        finally:
            clip_mod._render_layer_buffer = original

        self.assertEqual(bytes(4 * 4 * 4), target.tobytes())

    def test_clip_clipping_without_visible_base_hides_source(self) -> None:
        from types import SimpleNamespace

        from tmiref import clip as clip_mod

        def node(name: str, *, clipped: bool = False, visible: bool = True):
            return SimpleNamespace(
                name=name,
                visible=visible,
                folder=False,
                composite=0,
                clip=1 if clipped else 0,
                opacity=256,
                render_mipmap=1,
                offset_x=0,
                offset_y=0,
                mask_mipmap=None,
                masking=0,
                main_id={"base": 1, "source": 2}.get(name, 3),
            )

        original = clip_mod._render_layer_buffer
        try:
            def fake_render_layer_buffer(_clip, _layer, _bounds, _region=None):
                return clip_mod._LayerBuffer(Image.new("RGBA", (2, 2), (255, 0, 0, 255)), 0, 0)

            clip_mod._render_layer_buffer = fake_render_layer_buffer

            # A hidden base hides what is clipped to it.
            target = Image.new("RGBA", (4, 4), (0, 0, 0, 0))
            clip_mod._composite_group(None, target, [node("base", visible=False), node("source", clipped=True)])
            self.assertEqual(bytes(4 * 4 * 4), target.tobytes())

            # No base at all in the group: CSP 5.1.5 draws the layer unclipped
            # (example/csp515/structure.clip) — it is not hidden.
            target = Image.new("RGBA", (4, 4), (0, 0, 0, 0))
            clip_mod._composite_group(None, target, [node("source", clipped=True)])
            self.assertEqual((255, 0, 0, 255), target.getpixel((0, 0)))
        finally:
            clip_mod._render_layer_buffer = original

    def test_clip_metadata_only_layer_selector_reports_whole_file_hint(self) -> None:
        repo = self.tmp / "repo"
        repo.mkdir()
        shutil.copyfile(PROJECT_ROOT / "example" / "text_simple.clip", repo / "art.clip")

        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "art.clip")
        self.run_tmi(repo, "commit", "-m", "base")

        rejected = self.run_tmi(repo, "add", "art.clip:clip:4", check=False)
        self.assertNotEqual(0, rejected.returncode)
        self.assertIn("metadata-only changes", rejected.stderr)

    def test_clip_layer_restore_in_place_invalidates_canvas_preview(self) -> None:
        repo = self.tmp / "repo"
        repo.mkdir()
        shutil.copyfile(PROJECT_ROOT / "example" / "text_simple.clip", repo / "art.clip")

        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "art.clip")
        self.run_tmi(repo, "commit", "-m", "base")

        from tmi.formats import clip as clip_mod
        from tmi.importers import import_file, walk_imported_layers

        def raster_blob() -> bytes:
            layers = walk_imported_layers(import_file(repo / "art.clip", render_composite=False).layers)
            return next(layer.blob_bytes for layer in layers if layer.name == "レイヤー 1")

        def overwrite_canvas_preview(color: tuple[int, int, int, int]) -> None:
            clip = clip_mod.open_clip(repo / "art.clip")
            con = clip_io._clone_sqlite_connection(clip)
            con.execute(
                "UPDATE CanvasPreview SET ImageWidth = 4, ImageHeight = 4, ImageData = ?",
                (png_bytes(color),),
            )
            con.commit()
            container = clip_mod.parse_container(clip.raw)
            payload_by_file_off = {
                chunk.file_off: clip.raw[chunk.payload_off:chunk.payload_off + chunk.payload_size]
                for chunk in container.chunks
            }
            (repo / "art.clip").write_bytes(
                clip_io._emit_container_with_sqli_payload(
                    container,
                    payload_by_file_off,
                    clip_io._serialize_sqlite(con),
                )
            )

        base_blob = raster_blob()
        self.assertIsNotNone(clip_mod.render_canvas_preview_rgba(clip_mod.open_clip(repo / "art.clip")))

        self.run_tmi(repo, "restore", "--in-place", "art.clip:レイヤー 1")
        self.assertIsNotNone(clip_mod.render_canvas_preview_rgba(clip_mod.open_clip(repo / "art.clip")))

        overwrite_canvas_preview((0, 0, 255, 255))
        self.assertEqual(base_blob, raster_blob())
        self.assertIsNotNone(clip_mod.render_canvas_preview_rgba(clip_mod.open_clip(repo / "art.clip")))

        self.run_tmi(repo, "restore", "--in-place", "art.clip:レイヤー 1")
        self.assertIsNone(clip_mod.render_canvas_preview_rgba(clip_mod.open_clip(repo / "art.clip")))
        self.assertEqual(base_blob, raster_blob())

        (repo / "art.clip").write_bytes(
            clip_mod.replace_layer_pixels(repo / "art.clip", "レイヤー 1", png_bytes((255, 0, 0, 255)))
        )
        self.assertIsNone(clip_mod.render_canvas_preview_rgba(clip_mod.open_clip(repo / "art.clip")))
        self.assertNotEqual(base_blob, raster_blob())

        self.run_tmi(repo, "restore", "--in-place", "art.clip:レイヤー 1")
        self.assertIsNone(clip_mod.render_canvas_preview_rgba(clip_mod.open_clip(repo / "art.clip")))
        self.assertEqual(base_blob, raster_blob())

    def test_opaque_binary_file_versions_diffs_and_restores(self) -> None:
        repo = self.tmp / "repo"
        repo.mkdir()
        (repo / "data.bin").write_bytes(b"\x00\x01original\xff")
        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "data.bin")
        self.run_tmi(repo, "commit", "-m", "base")

        (repo / "data.bin").write_bytes(b"\x00\x01CHANGED\xff\xfe")
        status = self.run_tmi(repo, "status")
        self.assertIn("modified   data.bin", status.stdout)
        diff = self.run_tmi(repo, "diff")
        self.assertIn("binary file changed", diff.stdout)

        self.run_tmi(repo, "restore", "--source", "HEAD", "data.bin")
        self.assertEqual(b"\x00\x01original\xff", (repo / "data.bin").read_bytes())

    def test_unknown_image_extension_tracked_as_raster(self) -> None:
        repo = self.tmp / "repo"
        repo.mkdir()
        Image.new("RGBA", (16, 16), (1, 2, 3, 255)).save(repo / "pic.bmp")
        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "pic.bmp")
        self.run_tmi(repo, "commit", "-m", "base")
        status = self.run_tmi(repo, "status")
        self.assertIn("working tree clean", status.stdout)
        Image.new("RGBA", (16, 16), (9, 8, 7, 255)).save(repo / "pic.bmp")
        diff = self.run_tmi(repo, "diff")
        self.assertIn("pic.bmp", diff.stdout)
        self.assertNotIn("binary file changed", diff.stdout)  # it diffs visually

    def test_switch_create_makes_and_checks_out_branch(self) -> None:
        repo = self.tmp / "repo"
        repo.mkdir()
        write_png(repo / "a.png", (255, 0, 0, 255))
        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "a.png")
        self.run_tmi(repo, "commit", "-m", "base")

        created = self.run_tmi(repo, "switch", "-c", "feature")
        self.assertIn("Switched to branch feature", created.stdout)
        status = self.run_tmi(repo, "status")
        self.assertIn("On branch feature", status.stdout)
        # -c on an existing branch is refused.
        dup = self.run_tmi(repo, "switch", "-c", "feature", check=False)
        self.assertNotEqual(0, dup.returncode)
        self.assertIn("already exists", dup.stderr)

    def test_branch_delete_requires_merge_force_and_is_recoverable(self) -> None:
        repo = self.tmp / "repo"
        repo.mkdir()
        write_png(repo / "a.png", (255, 0, 0, 255))
        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "a.png")
        self.run_tmi(repo, "commit", "-m", "base")
        self.run_tmi(repo, "switch", "-c", "feature")
        write_png(repo / "a.png", (0, 255, 0, 255))
        self.run_tmi(repo, "commit", "-a", "-m", "feature-work")
        self.run_tmi(repo, "switch", "main")

        # -d refuses an unmerged branch.
        rejected = self.run_tmi(repo, "branch", "-d", "feature", check=False)
        self.assertNotEqual(0, rejected.returncode)
        self.assertIn("not fully merged", rejected.stderr)
        # -D forces it and reports the lost tip.
        forced = self.run_tmi(repo, "branch", "-D", "feature")
        self.assertIn("deleted branch feature", forced.stdout)
        listing = self.run_tmi(repo, "branch")
        self.assertNotIn("feature", listing.stdout)
        # The commit is still recoverable via the reflog.
        oid = forced.stdout.split("was ")[1].strip().rstrip(")")
        self.run_tmi(repo, "recover", "--as", "rescued", oid)
        self.assertIn("rescued", self.run_tmi(repo, "branch").stdout)

    def test_recover_as_accepts_short_reflog_oid_prefixes(self) -> None:
        repo = self.tmp / "repo"
        repo.mkdir()
        write_png(repo / "a.png", (255, 0, 0, 255))

        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "a.png")
        self.run_tmi(repo, "commit", "-m", "base")
        self.run_tmi(repo, "switch", "-c", "lost")
        write_png(repo / "a.png", (0, 255, 0, 255))
        self.run_tmi(repo, "commit", "-a", "-m", "lost")
        self.run_tmi(repo, "switch", "main")
        forced = self.run_tmi(repo, "branch", "-D", "lost")

        oid = forced.stdout.split("was ")[1].strip().rstrip(")")
        recovered = self.run_tmi(repo, "recover", "--as", "short-rescue", oid[:6])

        self.assertIn(f"created branch short-rescue at {oid[:12]}", recovered.stdout)

    def test_cannot_delete_current_branch(self) -> None:
        repo = self.tmp / "repo"
        repo.mkdir()
        write_png(repo / "a.png", (255, 0, 0, 255))
        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "a.png")
        self.run_tmi(repo, "commit", "-m", "base")
        result = self.run_tmi(repo, "branch", "-D", "main", check=False)
        self.assertNotEqual(0, result.returncode)
        self.assertIn("current branch", result.stderr)

    def test_tag_delete_removes_tag(self) -> None:
        repo = self.tmp / "repo"
        repo.mkdir()
        write_png(repo / "a.png", (255, 0, 0, 255))
        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "a.png")
        self.run_tmi(repo, "commit", "-m", "base")
        self.run_tmi(repo, "tag", "v1")
        self.assertIn("v1", self.run_tmi(repo, "tag").stdout)
        self.run_tmi(repo, "tag", "-d", "v1")
        self.assertNotIn("v1", self.run_tmi(repo, "tag").stdout)
        missing = self.run_tmi(repo, "tag", "-d", "ghost", check=False)
        self.assertNotEqual(0, missing.returncode)

    def test_tmiignore_negation_is_respected_in_status(self) -> None:
        repo = self.tmp / "repo"
        repo.mkdir()
        (repo / ".tmiignore").write_text("*.png\n!keep.png\n", encoding="utf-8")
        write_png(repo / "drop.png", (255, 0, 0, 255))
        write_png(repo / "keep.png", (0, 255, 0, 255))

        self.run_tmi(repo, "init")
        status = self.run_tmi(repo, "status")

        self.assertIn("untracked  keep.png", status.stdout)
        self.assertNotIn("drop.png", status.stdout)

    def test_deleted_files_can_be_staged_committed_and_restored_with_output(self) -> None:
        repo = self.tmp / "repo"
        repo.mkdir()
        (repo / "a.bin").write_bytes(b"a-v1")
        (repo / "b.bin").write_bytes(b"b-v1")

        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "a.bin")
        self.run_tmi(repo, "add", "b.bin")
        self.run_tmi(repo, "commit", "-m", "base")
        (repo / "a.bin").unlink()
        (repo / "b.bin").unlink()

        self.run_tmi(repo, "add", "a.bin")
        self.assertIn("staged     a.bin  delete", self.run_tmi(repo, "status").stdout)
        self.run_tmi(repo, "commit", "-a", "-m", "delete files")

        self.assertIn("working tree clean", self.run_tmi(repo, "status").stdout)
        self.run_tmi(repo, "restore", "--source", "HEAD~1", "--output", "old-a.bin", "a.bin")
        self.assertFalse((repo / "a.bin").exists())
        self.assertEqual(b"a-v1", (repo / "old-a.bin").read_bytes())

    def test_merge_conflict_keeps_non_conflicting_remote_changes_and_clears_on_add(self) -> None:
        repo = self.tmp / "repo"
        repo.mkdir()
        (repo / "a.bin").write_bytes(b"base-a")
        (repo / "b.bin").write_bytes(b"base-b")

        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "a.bin")
        self.run_tmi(repo, "add", "b.bin")
        self.run_tmi(repo, "commit", "-m", "base")
        self.run_tmi(repo, "branch", "other")
        (repo / "a.bin").write_bytes(b"main-a")
        self.run_tmi(repo, "commit", "-a", "-m", "main-a")
        self.run_tmi(repo, "switch", "other")
        (repo / "a.bin").write_bytes(b"other-a")
        (repo / "b.bin").write_bytes(b"other-b")
        self.run_tmi(repo, "commit", "-a", "-m", "other-a-and-b")
        self.run_tmi(repo, "switch", "main")

        merged = self.run_tmi(repo, "merge", "other", check=False)
        self.assertEqual(1, merged.returncode)
        self.assertEqual(b"other-b", (repo / "b.bin").read_bytes())
        blocked = self.run_tmi(repo, "commit", "-m", "too early", check=False)
        self.assertNotEqual(0, blocked.returncode)
        self.assertIn("unresolved merge conflicts", blocked.stderr)

        (repo / "a.bin").write_bytes(b"other-a")
        self.run_tmi(repo, "add", "a.bin")
        status = self.run_tmi(repo, "status")
        self.assertIn("All conflicts resolved", status.stdout)
        self.assertNotIn("Unresolved conflicts", status.stdout)
        self.run_tmi(repo, "commit", "-m", "resolve")

        self.assertEqual(b"other-a", (repo / "a.bin").read_bytes())
        self.assertEqual(b"other-b", (repo / "b.bin").read_bytes())

    def test_layer_conflict_cli_merges_the_rest_and_resolves_per_layer(self) -> None:
        # Issues #1/#2: both sides repaint layer1, only theirs repaints layer2.
        from tmi.formats import mdp2 as mdp2_mod

        red, green, blue, yellow = (255, 0, 0, 255), (0, 255, 0, 255), (0, 0, 255, 255), (255, 255, 0, 255)
        repo = self.tmp / "repo"
        repo.mkdir()
        art = repo / "art.mdp"
        write_mdp2_layers(art, [red, green])
        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "art.mdp")
        self.run_tmi(repo, "commit", "-m", "base")
        self.run_tmi(repo, "branch", "other")
        write_mdp2_layers(art, [blue, green])
        self.run_tmi(repo, "commit", "-a", "-m", "ours")
        self.run_tmi(repo, "switch", "other")
        write_mdp2_layers(art, [yellow, blue])
        self.run_tmi(repo, "commit", "-a", "-m", "theirs")
        self.run_tmi(repo, "switch", "main")

        def pixels() -> list:
            mdp = mdp2_mod.open_mdp2(art)
            return [mdp2_mod.render_layer_rgba(mdp, layer, apply_opacity=False).getpixel((64, 64)) for layer in mdp.layers]

        merged = self.run_tmi(repo, "merge", "other", check=False)
        self.assertEqual(1, merged.returncode, merged.stdout + merged.stderr)
        self.assertIn("layer1: overlapping pixel edits", merged.stdout)
        # The non-conflicting layer is merged; the conflicting one stays ours.
        self.assertEqual([blue, blue], pixels())
        self.assertIn("overlapping pixel edits", self.run_tmi(repo, "status").stdout)
        conflict_dir = repo / ".tmi" / "conflicts"
        pngs = sorted(p.name for p in conflict_dir.rglob("*.png"))
        self.assertEqual(["layer1__OURS.png", "layer1__THEIRS.png"], pngs)

        # MERGE_HEAD is a revision while the merge is pending.
        out = self.tmp / "theirs.png"
        self.run_tmi(repo, "restore", "--source", "MERGE_HEAD", "--output", str(out), "art.mdp:layer1")
        self.assertEqual(yellow, Image.open(out).convert("RGBA").getpixel((64, 64)))

        # Abort puts everything back at HEAD and ends the merge.
        self.run_tmi(repo, "merge", "--abort")
        self.assertEqual([blue, green], pixels())
        self.assertIn("nothing to commit", self.run_tmi(repo, "status").stdout)

        # Again, now resolving: staging the file untouched warns that their
        # side of layer1 is dropped; taking theirs for that layer works.
        self.run_tmi(repo, "merge", "other", check=False)
        self.run_tmi(repo, "restore", "--theirs", "art.mdp:layer1")
        self.assertEqual([yellow, blue], pixels())
        added = self.run_tmi(repo, "add", "art.mdp")
        self.assertNotIn("warning", added.stderr)
        self.run_tmi(repo, "commit", "-m", "resolve")
        self.assertEqual([yellow, blue], pixels())

    def test_adding_an_untouched_conflicted_file_warns_and_overwrites_are_backed_up(self) -> None:
        repo = self.tmp / "repo"
        repo.mkdir()
        art = repo / "art.mdp"
        write_mdp2_layers(art, [(255, 0, 0, 255)])
        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "art.mdp")
        self.run_tmi(repo, "commit", "-m", "base")
        self.run_tmi(repo, "branch", "other")
        write_mdp2_layers(art, [(0, 0, 255, 255)])
        self.run_tmi(repo, "commit", "-a", "-m", "ours")
        self.run_tmi(repo, "switch", "other")
        write_mdp2_layers(art, [(255, 255, 0, 255)])
        self.run_tmi(repo, "commit", "-a", "-m", "theirs")
        self.run_tmi(repo, "switch", "main")
        self.run_tmi(repo, "merge", "other", check=False)
        added = self.run_tmi(repo, "add", "art.mdp")
        self.assertIn("keep our version: layer1", added.stderr)
        self.run_tmi(repo, "commit", "-m", "resolve")

        # Issue #4: restore over uncommitted work keeps a copy.
        work = b"unsaved hours" + art.read_bytes()[13:]
        art.write_bytes(work)
        restored = self.run_tmi(repo, "restore", "art.mdp")
        self.assertIn("tmiki recover --files", restored.stdout)
        self.assertNotEqual(work, art.read_bytes())
        listing = self.run_tmi(repo, "recover", "--files")
        self.assertIn("art.mdp", listing.stdout)
        self.assertIn("restore art.mdp", listing.stdout)
        self.run_tmi(repo, "recover", "--file", "0")
        self.assertEqual(work, art.read_bytes())

    def _single_layer_conflict_repo(self) -> tuple[Path, Path, bytes]:
        """main and other both repaint layer1 of art.mdp; returns (repo, art,
        HEAD's bytes of art.mdp) with main checked out and the merge not yet run."""
        repo = self.tmp / "repo"
        repo.mkdir()
        art = repo / "art.mdp"
        write_mdp2_layers(art, [(255, 0, 0, 255)])
        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "art.mdp")
        self.run_tmi(repo, "commit", "-m", "base")
        self.run_tmi(repo, "branch", "other")
        write_mdp2_layers(art, [(0, 0, 255, 255)])
        self.run_tmi(repo, "commit", "-a", "-m", "ours")
        self.run_tmi(repo, "switch", "other")
        write_mdp2_layers(art, [(255, 255, 0, 255)])
        self.run_tmi(repo, "commit", "-a", "-m", "theirs")
        self.run_tmi(repo, "switch", "main")
        return repo, art, art.read_bytes()

    def test_merge_abort_after_add_unstage_and_in_place_restore_returns_to_head(self) -> None:
        # Issue #13: conflict -> add -> restore --staged -> restore --source
        # MERGE_HEAD --in-place -> merge --abort used to report success while
        # leaving the layer edit in the file, with no backup.
        from tmi.formats import mdp2 as mdp2_mod

        repo, art, head_bytes = self._single_layer_conflict_repo()

        def layer1() -> tuple:
            mdp = mdp2_mod.open_mdp2(art)
            return mdp2_mod.render_layer_rgba(mdp, mdp.layers[0], apply_opacity=False).getpixel((64, 64))

        self.assertEqual(1, self.run_tmi(repo, "merge", "other", check=False).returncode)
        self.run_tmi(repo, "add", "art.mdp")
        self.run_tmi(repo, "restore", "--staged", "art.mdp")
        self.run_tmi(repo, "restore", "--source", "MERGE_HEAD", "--in-place", "art.mdp:layer1")
        self.assertEqual((255, 255, 0, 255), layer1())
        edited = art.read_bytes()

        aborted = self.run_tmi(repo, "merge", "--abort")
        self.assertIn("merge aborted", aborted.stdout)
        self.assertEqual(head_bytes, art.read_bytes())
        self.assertIn("nothing to commit, working tree clean", self.run_tmi(repo, "status").stdout)
        self.assertFalse((repo / ".tmi" / "MERGE_HEAD").exists())
        listing = self.run_tmi(repo, "recover", "--files").stdout
        self.assertIn("art.mdp", listing)
        self.assertIn("(merge --abort)", listing)
        self.run_tmi(repo, "recover", "--file", "0", "--output", str(self.tmp / "kept.mdp"))
        self.assertEqual(edited, (self.tmp / "kept.mdp").read_bytes())
        # And the next merge is not blocked by a leftover change.
        self.assertEqual(1, self.run_tmi(repo, "merge", "other", check=False).returncode)

    def test_conflict_staged_with_add_can_be_reopened_and_still_takes_theirs(self) -> None:
        # Issue #12: after `add` of an untouched conflicted file, restore
        # --theirs said "no unresolved conflict" and restore --staged said
        # "was not staged" while status had just shown it staged.
        from tmi.formats import mdp2 as mdp2_mod

        yellow = (255, 255, 0, 255)
        repo, art, head_bytes = self._single_layer_conflict_repo()

        def layer1() -> tuple:
            mdp = mdp2_mod.open_mdp2(art)
            return mdp2_mod.render_layer_rgba(mdp, mdp.layers[0], apply_opacity=False).getpixel((64, 64))

        self.assertEqual(1, self.run_tmi(repo, "merge", "other", check=False).returncode)
        added = self.run_tmi(repo, "add", "art.mdp")
        self.assertIn("their side is dropped", added.stderr)
        self.assertIn("All conflicts resolved", self.run_tmi(repo, "status").stdout)

        # restore --staged puts the conflict back.
        unstaged = self.run_tmi(repo, "restore", "--staged", "art.mdp")
        self.assertNotIn("was not staged", unstaged.stdout)
        self.assertIn("unstaged art.mdp; its merge conflict is unresolved again", unstaged.stdout)
        status = self.run_tmi(repo, "status").stdout
        self.assertIn("Unresolved conflicts", status)
        self.assertIn("conflict art.mdp", status)
        self.assertIn("layer1: overlapping pixel edits", status)
        blocked = self.run_tmi(repo, "commit", "-m", "too early", check=False)
        self.assertIn("unresolved merge conflicts", blocked.stderr)

        self.run_tmi(repo, "restore", "--theirs", "art.mdp:layer1")
        self.assertEqual(yellow, layer1())
        added = self.run_tmi(repo, "add", "art.mdp")
        self.assertNotIn("warning", added.stderr)
        self.run_tmi(repo, "commit", "-m", "resolve")
        self.assertEqual(yellow, layer1())
        self.assertNotEqual(head_bytes, art.read_bytes())
        self.assertEqual([], list((repo / ".tmi" / "conflicts").iterdir()))

    def test_status_shows_which_side_restore_took_for_a_conflict(self) -> None:
        # Issue #15: after `restore --theirs`, status still listed the bare
        # conflict and said "no unstaged changes".
        repo, art, _head_bytes = self._single_layer_conflict_repo()
        self.assertEqual(1, self.run_tmi(repo, "merge", "other", check=False).returncode)
        status = self.run_tmi(repo, "status").stdout
        self.assertIn("    layer1: overlapping pixel edits\n", status)
        self.assertNotIn("took", status)
        self.assertNotIn("no unstaged changes", status)
        self.assertIn("no other changes", status)

        self.run_tmi(repo, "restore", "--theirs", "art.mdp:layer1")
        status = self.run_tmi(repo, "status").stdout
        self.assertIn("    layer1: overlapping pixel edits -> took theirs\n", status)
        self.assertIn("every conflict has a side; check the file, then 'tmiki add art.mdp'", status)
        self.assertNotIn("edited since", status)
        self.assertIn("Unresolved conflicts", status)

        self.run_tmi(repo, "restore", "--ours", "art.mdp")
        self.assertIn("layer1: overlapping pixel edits -> took ours", self.run_tmi(repo, "status").stdout)

        write_mdp2_layers(art, [(10, 20, 30, 255)])
        status = self.run_tmi(repo, "status").stdout
        self.assertIn("-> took ours", status)
        self.assertIn("(working file edited since then)", status)

        self.run_tmi(repo, "add", "art.mdp")
        self.assertIn("All conflicts resolved", self.run_tmi(repo, "status").stdout)

    def test_take_theirs_after_add_during_merge_and_refusal_points_to_merge_head(self) -> None:
        # Issue #12, the other way out: --ours/--theirs keep working for a
        # conflict already staged with `add` (re-stage afterwards).
        from tmi.formats import mdp2 as mdp2_mod

        yellow, blue = (255, 255, 0, 255), (0, 0, 255, 255)
        repo, art, _head_bytes = self._single_layer_conflict_repo()
        (repo / "other.bin").write_bytes(b"x")
        self.run_tmi(repo, "add", "other.bin")
        self.run_tmi(repo, "commit", "-m", "unrelated")

        def layer1() -> tuple:
            mdp = mdp2_mod.open_mdp2(art)
            return mdp2_mod.render_layer_rgba(mdp, mdp.layers[0], apply_opacity=False).getpixel((64, 64))

        refused = self.run_tmi(repo, "restore", "--theirs", "art.mdp:layer1", check=False)
        self.assertNotEqual(0, refused.returncode)
        self.assertIn("no merge in progress", refused.stderr)

        self.assertEqual(1, self.run_tmi(repo, "merge", "other", check=False).returncode)
        self.run_tmi(repo, "add", "art.mdp")
        self.run_tmi(repo, "restore", "--theirs", "art.mdp:layer1")
        self.assertEqual(yellow, layer1())
        self.assertIn("modified", self.run_tmi(repo, "status").stdout)
        self.run_tmi(repo, "restore", "--ours", "art.mdp")
        self.assertEqual(blue, layer1())

        refused = self.run_tmi(repo, "restore", "--theirs", "other.bin", check=False)
        self.assertNotEqual(0, refused.returncode)
        self.assertIn("tmiki restore --source MERGE_HEAD --in-place other.bin:<layer>", refused.stderr)

    def test_merge_abort_that_cannot_rewrite_a_file_fails_and_keeps_the_merge(self) -> None:
        import stat as stat_mod

        if hasattr(os, "geteuid") and os.geteuid() == 0:
            self.skipTest("root ignores read-only files")
        repo, art, head_bytes = self._single_layer_conflict_repo()
        self.assertEqual(1, self.run_tmi(repo, "merge", "other", check=False).returncode)
        self.run_tmi(repo, "restore", "--theirs", "art.mdp")
        theirs = art.read_bytes()
        os.chmod(art, stat_mod.S_IREAD)
        try:
            failed = self.run_tmi(repo, "merge", "--abort", check=False)
        finally:
            os.chmod(art, stat_mod.S_IREAD | stat_mod.S_IWRITE)
        self.assertNotEqual(0, failed.returncode)
        self.assertNotIn("merge aborted", failed.stdout)
        self.assertIn("could not put 1 file(s) back to HEAD: art.mdp", failed.stderr)
        self.assertEqual(theirs, art.read_bytes())
        self.assertTrue((repo / ".tmi" / "MERGE_HEAD").exists())
        self.assertIn("(merge --abort)", self.run_tmi(repo, "recover", "--files").stdout)

        retried = self.run_tmi(repo, "merge", "--abort")
        self.assertIn("merge aborted", retried.stdout)
        self.assertEqual(head_bytes, art.read_bytes())

    def test_every_working_tree_rewrite_reminds_to_reopen_and_names_its_backups(self) -> None:
        # Issue #14: switch, switch --discard, reset --hard, a clean merge /
        # fast-forward, pull and checkout rewrite working files too; backups
        # they take must say which operation took them, not "(overwrite)".
        notice = "If any of these files is open in your drawing app, reopen it before saving:"
        repo = self.tmp / "repo"
        repo.mkdir()
        art = repo / "a.bin"
        art.write_bytes(b"base")
        (repo / "b.bin").write_bytes(b"b-base")
        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "a.bin")
        self.run_tmi(repo, "add", "b.bin")
        self.run_tmi(repo, "commit", "-m", "base")
        self.run_tmi(repo, "branch", "feature")
        art.write_bytes(b"main-1")
        self.run_tmi(repo, "commit", "-a", "-m", "main-1")

        switched = self.run_tmi(repo, "switch", "feature")
        self.assertIn(notice, switched.stdout)
        self.assertIn(str(art), switched.stdout)
        self.assertEqual(b"base", art.read_bytes())

        # switch --discard backs up both a modified file and a staged new one.
        art.write_bytes(b"unsaved on feature")
        (repo / "new.bin").write_bytes(b"staged only")
        self.run_tmi(repo, "add", "new.bin")
        discarded = self.run_tmi(repo, "switch", "--discard", "main")
        self.assertIn(notice, discarded.stdout)
        self.assertIn("backed up 2 file(s)", discarded.stdout)
        self.assertFalse((repo / "new.bin").exists())
        listing = self.run_tmi(repo, "recover", "--files").stdout
        self.assertIn("a.bin", listing)
        self.assertIn("new.bin", listing)
        self.assertEqual(2, listing.count("(switch --discard main)"), listing)
        self.assertNotIn("(overwrite)", listing)

        art.write_bytes(b"main-2")
        self.run_tmi(repo, "commit", "-a", "-m", "main-2")
        art.write_bytes(b"unsaved on main")
        reset = self.run_tmi(repo, "reset", "--hard", "HEAD~1")
        self.assertIn(notice, reset.stdout)
        self.assertEqual(b"main-1", art.read_bytes())
        self.assertIn("(reset --hard HEAD~1)", self.run_tmi(repo, "recover", "--files").stdout)

        # checkout -- <path> restores through `restore`, but its backup is
        # named after checkout.
        art.write_bytes(b"unsaved before checkout --")
        self.run_tmi(repo, "checkout", "--", "a.bin")
        self.assertEqual(b"main-1", art.read_bytes())
        listing = self.run_tmi(repo, "recover", "--files").stdout
        self.assertIn("(checkout a.bin)", listing)
        self.assertNotIn("(restore a.bin)", listing)

        checked_out = self.run_tmi(repo, "checkout", "feature")
        self.assertIn(notice, checked_out.stdout)
        self.assertEqual(b"base", art.read_bytes())
        (repo / "b.bin").write_bytes(b"b-feature")
        self.run_tmi(repo, "commit", "-a", "-m", "feature-b")
        self.run_tmi(repo, "switch", "main")

        merged = self.run_tmi(repo, "merge", "feature")
        self.assertIn("Merged feature", merged.stdout)
        self.assertIn(notice, merged.stdout)
        self.assertIn(str(repo / "b.bin"), merged.stdout)
        self.assertEqual(b"b-feature", (repo / "b.bin").read_bytes())

        self.run_tmi(repo, "switch", "feature")
        ff = self.run_tmi(repo, "merge", "main")
        self.assertIn("Fast-forward", ff.stdout)
        self.assertIn(notice, ff.stdout)
        self.assertIn(str(art), ff.stdout)

        # pull (fast-forward) from a clone's origin.
        clone = self.tmp / "clone"
        self.run_tmi(self.tmp, "clone", str(repo), str(clone))
        self.run_tmi(repo, "switch", "main")
        art.write_bytes(b"main-3")
        self.run_tmi(repo, "commit", "-a", "-m", "main-3")
        pulled = self.run_tmi(clone, "pull", "origin", "main")
        self.assertIn(notice, pulled.stdout)
        self.assertIn(str(clone / "a.bin"), pulled.stdout)
        self.assertEqual(b"main-3", (clone / "a.bin").read_bytes())

        # Nothing rewritten, no notice.
        self.assertNotIn(notice, self.run_tmi(repo, "merge", "feature").stdout)

    def test_switch_during_merge_requires_discard_and_clears_merge_state(self) -> None:
        repo = self.tmp / "repo"
        repo.mkdir()
        (repo / "a.bin").write_bytes(b"base")

        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "a.bin")
        self.run_tmi(repo, "commit", "-m", "base")
        self.run_tmi(repo, "branch", "other")
        (repo / "a.bin").write_bytes(b"main")
        self.run_tmi(repo, "commit", "-a", "-m", "main")
        self.run_tmi(repo, "switch", "other")
        (repo / "a.bin").write_bytes(b"other")
        self.run_tmi(repo, "commit", "-a", "-m", "other")
        self.run_tmi(repo, "switch", "main")

        merged = self.run_tmi(repo, "merge", "other", check=False)
        self.assertEqual(1, merged.returncode)
        blocked = self.run_tmi(repo, "switch", "other", check=False)
        self.assertNotEqual(0, blocked.returncode)
        self.assertIn("merge in progress", blocked.stderr)

        self.run_tmi(repo, "switch", "other", "--discard")
        status = self.run_tmi(repo, "status")
        self.assertIn("On branch other", status.stdout)
        self.assertNotIn("middle of a merge", status.stdout)
        self.assertFalse((repo / ".tmi" / "MERGE_HEAD").exists())
        self.assertFalse(any((repo / ".tmi" / "conflicts").iterdir()))

    def test_commit_amend_is_rejected_during_pending_merge(self) -> None:
        repo = self.tmp / "repo"
        repo.mkdir()
        (repo / "a.bin").write_bytes(b"base")

        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "a.bin")
        self.run_tmi(repo, "commit", "-m", "base")
        self.run_tmi(repo, "branch", "other")
        (repo / "a.bin").write_bytes(b"main")
        self.run_tmi(repo, "commit", "-a", "-m", "main")
        self.run_tmi(repo, "switch", "other")
        (repo / "a.bin").write_bytes(b"other")
        self.run_tmi(repo, "commit", "-a", "-m", "other")
        self.run_tmi(repo, "switch", "main")

        merged = self.run_tmi(repo, "merge", "other", check=False)
        self.assertEqual(1, merged.returncode)
        amended = self.run_tmi(repo, "commit", "--amend", "-m", "oops", check=False)
        self.assertNotEqual(0, amended.returncode)
        self.assertIn("cannot amend during a merge", amended.stderr)

    def test_commit_all_does_not_auto_resolve_pending_conflicts(self) -> None:
        repo = self.tmp / "repo"
        repo.mkdir()
        (repo / "a.bin").write_bytes(b"base")

        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "a.bin")
        self.run_tmi(repo, "commit", "-m", "base")
        self.run_tmi(repo, "branch", "other")
        (repo / "a.bin").write_bytes(b"main")
        self.run_tmi(repo, "commit", "-a", "-m", "main")
        self.run_tmi(repo, "switch", "other")
        (repo / "a.bin").write_bytes(b"other")
        self.run_tmi(repo, "commit", "-a", "-m", "other")
        self.run_tmi(repo, "switch", "main")

        merged = self.run_tmi(repo, "merge", "other", check=False)
        self.assertEqual(1, merged.returncode)
        committed = self.run_tmi(repo, "commit", "-a", "-m", "should-not-commit", check=False)

        self.assertNotEqual(0, committed.returncode)
        self.assertIn("unresolved merge conflicts", committed.stderr)
        status = self.run_tmi(repo, "status")
        self.assertIn("middle of a merge", status.stdout)
        self.assertIn("conflict a.bin", status.stdout)

    def test_merge_is_rejected_while_another_merge_is_pending(self) -> None:
        repo = self.tmp / "repo"
        repo.mkdir()
        (repo / "a.bin").write_bytes(b"base")

        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "a.bin")
        self.run_tmi(repo, "commit", "-m", "base")
        self.run_tmi(repo, "branch", "other")
        self.run_tmi(repo, "branch", "third")
        (repo / "a.bin").write_bytes(b"main")
        self.run_tmi(repo, "commit", "-a", "-m", "main")
        self.run_tmi(repo, "switch", "other")
        (repo / "a.bin").write_bytes(b"other")
        self.run_tmi(repo, "commit", "-a", "-m", "other")
        self.run_tmi(repo, "switch", "third")
        (repo / "a.bin").write_bytes(b"third")
        self.run_tmi(repo, "commit", "-a", "-m", "third")
        self.run_tmi(repo, "switch", "main")

        merged = self.run_tmi(repo, "merge", "other", check=False)
        self.assertEqual(1, merged.returncode)
        second = self.run_tmi(repo, "merge", "third", check=False)

        self.assertNotEqual(0, second.returncode)
        self.assertIn("merge already in progress", second.stderr)
        status = self.run_tmi(repo, "status")
        self.assertIn("middle of a merge", status.stdout)
        self.assertIn("conflict a.bin", status.stdout)

    def test_branch_and_tag_creation_refuse_to_overwrite_existing_refs(self) -> None:
        repo = self.tmp / "repo"
        repo.mkdir()
        (repo / "a.bin").write_bytes(b"base")

        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "a.bin")
        self.run_tmi(repo, "commit", "-m", "base")
        self.run_tmi(repo, "branch", "feature")
        self.run_tmi(repo, "tag", "v1")
        (repo / "a.bin").write_bytes(b"main2")
        self.run_tmi(repo, "commit", "-a", "-m", "main2")

        branch = self.run_tmi(repo, "branch", "feature", check=False)
        self.assertNotEqual(0, branch.returncode)
        self.assertIn("branch already exists", branch.stderr)
        tag = self.run_tmi(repo, "tag", "v1", check=False)
        self.assertNotEqual(0, tag.returncode)
        self.assertIn("tag already exists", tag.stderr)

    def test_status_does_not_write_progress_to_stderr_when_captured(self) -> None:
        repo = self.tmp / "repo"
        repo.mkdir()
        (repo / "a.bin").write_bytes(b"base")

        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "a.bin")
        self.run_tmi(repo, "commit", "-m", "base")
        status = self.run_tmi(repo, "status")

        self.assertEqual("", status.stderr)

    def test_git_style_status_add_and_diff_summary_options(self) -> None:
        repo = self.tmp / "repo"
        repo.mkdir()
        (repo / "a.bin").write_bytes(b"base")

        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "-A")
        short = self.run_tmi(repo, "status", "-sb")
        self.assertIn("## main", short.stdout)
        self.assertIn("A  a.bin", short.stdout)

        self.run_tmi(repo, "commit", "-m", "base")
        (repo / "a.bin").write_bytes(b"changed")
        name_status = self.run_tmi(repo, "diff", "--name-status")
        stat = self.run_tmi(repo, "diff", "--stat")

        self.assertIn("M\ta.bin", name_status.stdout)
        self.assertIn("1 file changed", stat.stdout)
        self.assertFalse(any((repo / ".tmi" / "diff").iterdir()))

        (repo / "b.bin").write_bytes(b"new")
        (repo / "a.bin").unlink()
        self.run_tmi(repo, "add", "-A")
        staged = self.run_tmi(repo, "status", "-s")
        self.assertIn("D  a.bin", staged.stdout)
        self.assertIn("A  b.bin", staged.stdout)

    def test_checkout_reset_rm_mv_show_remote_and_stash_commands(self) -> None:
        repo = self.tmp / "repo"
        repo.mkdir()
        (repo / "a.bin").write_bytes(b"base")

        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "a.bin")
        self.run_tmi(repo, "commit", "-m", "base")
        self.run_tmi(repo, "checkout", "-b", "side")
        (repo / "a.bin").write_bytes(b"side")
        self.run_tmi(repo, "commit", "-a", "-m", "side")
        self.run_tmi(repo, "checkout", "main")
        self.assertEqual(b"base", (repo / "a.bin").read_bytes())

        (repo / "a.bin").write_bytes(b"main")
        self.run_tmi(repo, "commit", "-a", "-m", "main")
        shown = self.run_tmi(repo, "show", "HEAD:a.bin")
        self.assertEqual("main", shown.stdout)
        commit_show = self.run_tmi(repo, "show", "--name-status")
        self.assertIn("M\ta.bin", commit_show.stdout)

        self.run_tmi(repo, "reset", "--soft", "HEAD~1")
        self.assertIn("M  a.bin", self.run_tmi(repo, "status", "-s").stdout)
        self.run_tmi(repo, "reset", "--mixed")
        self.assertIn(" M a.bin", self.run_tmi(repo, "status", "-s").stdout)
        self.run_tmi(repo, "reset", "--hard")
        self.assertEqual(b"base", (repo / "a.bin").read_bytes())

        self.run_tmi(repo, "mv", "a.bin", "renamed.bin")
        moved = self.run_tmi(repo, "status", "-s")
        self.assertIn("D  a.bin", moved.stdout)
        self.assertIn("A  renamed.bin", moved.stdout)
        self.run_tmi(repo, "commit", "-m", "rename")
        self.run_tmi(repo, "rm", "renamed.bin")
        removed = self.run_tmi(repo, "status", "-s")
        self.assertIn("D  renamed.bin", removed.stdout)
        self.assertFalse((repo / "renamed.bin").exists())
        self.run_tmi(repo, "reset", "--hard")

        (repo / "renamed.bin").write_bytes(b"stash")
        saved = self.run_tmi(repo, "stash", "push", "-m", "try stash")
        self.assertIn("try stash", saved.stdout)
        self.assertEqual(b"base", (repo / "renamed.bin").read_bytes())
        self.assertIn("try stash", self.run_tmi(repo, "stash", "list").stdout)
        self.run_tmi(repo, "stash", "pop")
        self.assertEqual(b"stash", (repo / "renamed.bin").read_bytes())
        self.assertEqual("", self.run_tmi(repo, "stash", "list").stdout)

        self.run_tmi(repo, "remote", "add", "origin", "../origin")
        self.run_tmi(repo, "remote", "rename", "origin", "backup")
        self.run_tmi(repo, "remote", "set-url", "backup", "../elsewhere")
        remotes = self.run_tmi(repo, "remote")
        self.assertIn("backup", remotes.stdout)
        self.assertIn("elsewhere", remotes.stdout)

    def test_stash_preserves_staged_only_changes(self) -> None:
        repo = self.tmp / "repo"
        repo.mkdir()
        (repo / "a.bin").write_bytes(b"base")

        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "a.bin")
        self.run_tmi(repo, "commit", "-m", "base")

        (repo / "a.bin").write_bytes(b"staged")
        self.run_tmi(repo, "add", "a.bin")
        # Put the worktree back to HEAD by hand: `restore` without --source now
        # restores the staged content, like git.
        (repo / "a.bin").write_bytes(b"base")
        self.assertIn("MM a.bin", self.run_tmi(repo, "status", "-s").stdout)

        saved = self.run_tmi(repo, "stash", "push", "-m", "staged only")
        self.assertIn("staged only", saved.stdout)
        self.assertIn("working tree clean", self.run_tmi(repo, "status").stdout)

        self.run_tmi(repo, "gc", "--prune=now")
        self.run_tmi(repo, "stash", "apply")
        self.assertEqual(b"base", (repo / "a.bin").read_bytes())
        self.assertIn("MM a.bin", self.run_tmi(repo, "status", "-s").stdout)

    def test_git_style_log_graph_branch_and_tag_options(self) -> None:
        repo = self.tmp / "repo"
        repo.mkdir()
        (repo / "base.bin").write_bytes(b"base")

        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "base.bin")
        self.run_tmi(repo, "commit", "-m", "base")
        self.run_tmi(repo, "branch", "side")
        self.run_tmi(repo, "tag", "-a", "v1", "-m", "release note")
        (repo / "main.bin").write_bytes(b"main")
        self.run_tmi(repo, "add", "main.bin")
        self.run_tmi(repo, "commit", "-m", "main work")
        self.run_tmi(repo, "switch", "side")
        (repo / "side.bin").write_bytes(b"side")
        self.run_tmi(repo, "add", "side.bin")
        self.run_tmi(repo, "commit", "-m", "side work")
        self.run_tmi(repo, "switch", "main")
        self.run_tmi(repo, "merge", "side")

        graph = self.run_tmi(repo, "log", "--graph", "--oneline", "--decorate", "--all", "-n", "5")
        self.assertIn("* ", graph.stdout)
        self.assertIn("HEAD -> main", graph.stdout)
        self.assertIn("side", graph.stdout)

        names = self.run_tmi(repo, "log", "--name-status", "--grep", "side work")
        self.assertIn("A\tside.bin", names.stdout)

        branches = self.run_tmi(repo, "branch", "-a", "-v", "--contains", "v1")
        self.assertIn("main", branches.stdout)
        self.assertIn("side", branches.stdout)
        self.assertIn("side work", branches.stdout)

        tag_points = self.run_tmi(repo, "tag", "--points-at", "v1")
        tag_lines = self.run_tmi(repo, "tag", "-n1", "v*")
        self.assertIn("v1", tag_points.stdout)
        self.assertIn("release note", tag_lines.stdout)

    def test_log_graph_draws_branches_in_their_own_columns(self) -> None:
        # Issue #31: a branch tip met for the first time gets a new column on
        # the right, a merge fans out (|\) and columns waiting for the same
        # commit close in (|/), as Git draws them.
        repo = self.tmp / "repo"
        repo.mkdir()
        self.run_tmi(repo, "init")
        day = [0]

        def commit(name: str, *extra: str) -> str:
            day[0] += 1
            self.env["TMI_COMMIT_TIMESTAMP"] = f"2026-01-{day[0]:02d}T10:00:00+00:00"
            try:
                if extra:
                    self.run_tmi(repo, *extra)
                else:
                    (repo / f"{name}.bin").write_bytes(name.encode() * day[0])
                    self.run_tmi(repo, "add", f"{name}.bin")
                    self.run_tmi(repo, "commit", "-m", name)
            finally:
                del self.env["TMI_COMMIT_TIMESTAMP"]
            return self.run_tmi(repo, "log", "-n", "1", "--format=%h").stdout.strip()

        f = commit("f")
        self.run_tmi(repo, "switch", "-c", "side")
        c = commit("c")
        self.run_tmi(repo, "switch", "main")
        m = commit("m")
        graph = self.run_tmi(repo, "log", "--graph", "--oneline", "--all").stdout
        self.assertEqual(f"* {m} m\n| * {c} c\n|/\n* {f} f\n", graph)
        full = self.run_tmi(repo, "log", "--graph", "--all", "-n", "2").stdout.splitlines()
        self.assertEqual(f"| * commit {self.run_tmi(repo, 'log', '-n', '1', '--format=%H', 'side').stdout.strip()}", full[6])
        self.assertTrue(full[7].startswith("|/  Author: "), full[7])
        self.assertTrue(full[8].startswith("|   Date:   "), full[8])

        merge = commit("merge", "merge", "side", "-m", "merge side")
        self.run_tmi(repo, "switch", "side")
        c2 = commit("c2")
        self.run_tmi(repo, "switch", "main")
        m2 = commit("m2")
        graph = self.run_tmi(repo, "log", "--graph", "--oneline", "--all").stdout
        self.assertEqual(
            f"* {m2} m2\n| * {c2} c2\n* |   {merge} merge side\n|\\ \\\n| |/\n* | {m} m\n| * {c} c\n|/\n* {f} f\n",
            graph,
        )
        self.assertEqual(
            f"* {m2} m2\n*   {merge} merge side\n|\\\n",
            self.run_tmi(repo, "log", "--graph", "--oneline", "-n", "2").stdout,
        )
        self.assertEqual(
            f"* {m2} m2\n* {merge} merge side\n* {m} m\n* {f} f\n",
            self.run_tmi(repo, "log", "--graph", "--oneline", "--first-parent").stdout,
        )

    def test_restore_output_of_a_whole_file_writes_what_the_name_says(self) -> None:
        # Issue #30: without a layer, --output x.png wrote a JPEG's or GIF's
        # own bytes. A .png path now gets the file's image; the file's own
        # extension (or none, or a non-image one) its bytes; another image
        # format is refused.
        repo = self.tmp / "repo"
        repo.mkdir()
        Image.new("RGB", (30, 20), (200, 40, 40)).save(repo / "f.jpg", quality=90)
        Image.new("P", (12, 8), 3).save(repo / "a.gif")
        (repo / "notes.txt").write_text("hello\n", encoding="utf-8")
        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "f.jpg", "a.gif", "notes.txt")
        self.run_tmi(repo, "commit", "-m", "x")
        jpeg = (repo / "f.jpg").read_bytes()

        def png_size(path: Path) -> tuple[int, int]:
            data = path.read_bytes()
            self.assertTrue(data.startswith(b"\x89PNG\r\n\x1a\n"), f"{path.name} is not a PNG")
            with Image.open(path) as image:
                return image.size

        self.run_tmi(repo, "restore", "--source", "HEAD", "--output", "out/one.png", "f.jpg")
        self.assertEqual((30, 20), png_size(repo / "out" / "one.png"))
        self.run_tmi(repo, "restore", "--output", "out/two.PNG", "a.gif")
        self.assertEqual((12, 8), png_size(repo / "out" / "two.PNG"))
        self.run_tmi(repo, "restore", "--source", "WORKTREE", "--output", "out/three.png", "f.jpg")
        self.assertEqual((30, 20), png_size(repo / "out" / "three.png"))
        for name in ("same.jpg", "same.jpeg", "copy.bak", "noext"):
            restored = self.run_tmi(repo, "restore", "--output", f"out/{name}", "f.jpg")
            self.assertIn("restored", restored.stdout)
            self.assertEqual(jpeg, (repo / "out" / name).read_bytes(), name)
        refused = self.run_tmi(repo, "restore", "--output", "out/x.gif", "f.jpg", check=False)
        self.assertEqual(1, refused.returncode)
        self.assertEqual(
            "tmiki: error: cannot convert f.jpg to .gif; give --output a path ending in .jpg for the file as it is, "
            "or .png for its image\n",
            refused.stderr,
        )
        self.assertFalse((repo / "out" / "x.gif").exists())
        no_image = self.run_tmi(repo, "restore", "--output", "out/n.png", "notes.txt", check=False)
        self.assertEqual(
            "tmiki: error: notes.txt has no image to write as .png; give --output a path ending in .txt for the file as it is\n",
            no_image.stderr,
        )
        no_image = self.run_tmi(repo, "restore", "--source", "WORKTREE", "--output", "out/n.png", "notes.txt", check=False)
        self.assertIn("notes.txt has no image to write as .png", no_image.stderr)
        self.run_tmi(repo, "restore", "--source", "WORKTREE", "--output", "out/n.txt", "notes.txt")
        self.assertEqual((repo / "notes.txt").read_bytes(), (repo / "out" / "n.txt").read_bytes())

    def test_progress_lines_name_the_phases_in_order(self) -> None:
        # Issue #26: TMI_PROGRESS=1 prints the phases (the Rust front door
        # too, without handing the command to Python) and a breakdown.
        repo = self.tmp / "repo"
        repo.mkdir()
        write_mdp2_layers(repo / "art.mdp", [(200, 0, 0, 255), (0, 0, 200, 255)])
        self.run_tmi(repo, "init")
        self.env["TMI_PROGRESS"] = "1"
        try:
            added = self.run_tmi(repo, "add", "art.mdp")
            self.run_tmi(repo, "commit", "-m", "base")
            switched = self.run_tmi(repo, "switch", "-c", "side")
            write_mdp2_layers(repo / "art.mdp", [(0, 200, 0, 255), (0, 0, 200, 255)])
            status = self.run_tmi(repo, "status")
        finally:
            del self.env["TMI_PROGRESS"]

        def phases(stderr: str) -> list[str]:
            # the lines, without the decode pool's ticks (they interleave
            # with the composite's phases) and repeats within a phase
            lines = stderr.splitlines()
            self.assertRegex(lines[-1], r"^tmiki: done in \d+\.\ds \(")
            out: list[str] = []
            for line in lines[:-1]:
                match = re.match(r"^tmiki: \[\s*\d+\.\ds\] (.*)$", line)
                self.assertIsNotNone(match, line)
                msg = match.group(1)
                if msg.startswith("decoding layers ") and not msg.startswith("decoding layers 0/"):
                    continue
                if out and msg.split()[:2] == out[-1].split()[:2] and msg.startswith("writing layer "):
                    continue
                out.append(msg)
            return out

        added_phases = phases(added.stderr)
        self.assertEqual(
            ["parsing art.mdp", "decoding layers 0/2", "rendering composite", "encoding composite PNG",
             "rendering thumbnail", "writing source blob for art.mdp"],
            added_phases[:6],
        )
        self.assertTrue(added_phases[6].startswith("writing layer art.mdp:"), added_phases)
        self.assertEqual(["writing thumbnail for art.mdp"], added_phases[7:])
        self.assertIn("staged art.mdp", added.stdout)
        self.assertEqual(["switching to side"], phases(switched.stderr))
        self.assertEqual(
            ["reading index", "reading HEAD tree", "hashing [1/1] art.mdp", "scanning layers [1/1] art.mdp",
             "scanning working tree for untracked files"],
            phases(status.stderr),
        )

        # Issue #29: the Rust `tmi` handing a command to a Python without tmi
        # printed only Python's "No module named tmi".
        binary = os.environ.get("TMI_BIN")
        base = getattr(sys, "_base_executable", "") or ""
        if not binary or not base:
            self.skipTest("the Rust front door (TMI_BIN) and a base interpreter are needed")
        env = {k: v for k, v in self.env.items() if k not in ("PYTHONPATH", "PYTHONHOME")}
        probe = subprocess.run([base, "-c", "import tmi"], cwd=self.tmp, env=env, capture_output=True)
        if probe.returncode == 0:
            self.skipTest(f"{base} imports tmi")
        env["TMI_PYTHON"] = base
        env["TMI_NATIVE"] = "0"
        result = subprocess.run([binary, "status"], cwd=self.tmp, env=env, text=True, capture_output=True, timeout=30)
        self.assertEqual(1, result.returncode)
        self.assertEqual(
            f"tmiki: error: this command runs in the Python part of tmi, which {base} cannot import; install it from "
            "the tmi source (see README: pip install -e .), or set TMI_PYTHON to a Python that has it\n",
            result.stderr.replace(os.path.realpath(base), base),
        )
        self.assertNotIn("No module named", result.stderr)
        env["TMI_PYTHON"] = str(self.tmp / "no-such-python.exe")
        result = subprocess.run([binary, "status"], cwd=self.tmp, env=env, text=True, capture_output=True, timeout=30)
        self.assertEqual(1, result.returncode)
        self.assertTrue(result.stderr.startswith("tmiki: error: this command runs in Python, but "), result.stderr)

    def test_commit_allow_empty_creates_commit_without_staged_changes(self) -> None:
        repo = self.tmp / "repo"
        repo.mkdir()

        self.run_tmi(repo, "init")
        committed = self.run_tmi(repo, "commit", "--allow-empty", "-m", "empty")
        self.assertIn("empty", committed.stdout)
        self.assertIn("empty", self.run_tmi(repo, "log", "--oneline").stdout)

    def test_add_unchanged_file_does_not_stage_or_create_noop_commit(self) -> None:
        repo = self.tmp / "repo"
        repo.mkdir()
        (repo / "a.bin").write_bytes(b"base")

        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "a.bin")
        self.run_tmi(repo, "commit", "-m", "base")

        added = self.run_tmi(repo, "add", "a.bin")
        self.assertEqual("", added.stdout)
        self.assertIn("working tree clean", self.run_tmi(repo, "status").stdout)

        rejected = self.run_tmi(repo, "commit", "-m", "noop", check=False)
        self.assertNotEqual(0, rejected.returncode)
        self.assertIn("nothing to commit", rejected.stderr)
        self.assertNotIn("noop", self.run_tmi(repo, "log", "--oneline").stdout)

    def test_rm_cached_unstages_new_file_instead_of_staging_delete(self) -> None:
        repo = self.tmp / "repo"
        repo.mkdir()
        (repo / "new.bin").write_bytes(b"new")

        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "new.bin")
        self.run_tmi(repo, "rm", "--cached", "new.bin")

        status = self.run_tmi(repo, "status", "-s")
        self.assertIn("?? new.bin", status.stdout)
        self.assertNotIn("D  new.bin", status.stdout)

        rejected = self.run_tmi(repo, "commit", "-m", "delete nothing", check=False)
        self.assertNotEqual(0, rejected.returncode)
        self.assertIn("nothing to commit", rejected.stderr)

    def test_push_to_checked_out_branch_in_non_bare_repo_is_rejected(self) -> None:
        remote = self.tmp / "remote"
        remote.mkdir()
        (remote / "a.bin").write_bytes(b"base")
        self.run_tmi(remote, "init")
        self.run_tmi(remote, "add", "a.bin")
        self.run_tmi(remote, "commit", "-m", "base")
        self.run_tmi(self.tmp, "clone", "remote", "client")
        client = self.tmp / "client"

        (client / "a.bin").write_bytes(b"client")
        self.run_tmi(client, "commit", "-a", "-m", "client")
        pushed = self.run_tmi(client, "push", "origin", "main", check=False)

        self.assertNotEqual(0, pushed.returncode)
        self.assertIn("checked-out branch", pushed.stderr)
        self.assertEqual(b"base", (remote / "a.bin").read_bytes())
        self.assertIn("working tree clean", self.run_tmi(remote, "status").stdout)

    def test_force_with_lease_rejects_when_tracking_ref_is_stale(self) -> None:
        self.run_tmi(self.tmp, "init", "--bare", "origin")
        a = self.tmp / "A"
        a.mkdir()
        (a / "x.bin").write_bytes(b"one")
        self.run_tmi(a, "init")
        self.run_tmi(a, "add", "x.bin")
        self.run_tmi(a, "commit", "-m", "one")
        self.run_tmi(a, "remote", "add", "origin", "../origin")
        self.run_tmi(a, "push", "origin", "main")

        self.run_tmi(self.tmp, "clone", "origin", "B")
        b = self.tmp / "B"

        (a / "x.bin").write_bytes(b"two")
        self.run_tmi(a, "commit", "-a", "-m", "two")
        self.run_tmi(a, "push", "origin", "main")

        (b / "x.bin").write_bytes(b"two from b")
        self.run_tmi(b, "commit", "-a", "-m", "two from b")
        rejected = self.run_tmi(b, "push", "origin", "main", "--force-with-lease", check=False)

        self.assertNotEqual(0, rejected.returncode)
        self.assertIn("force-with-lease rejected", rejected.stderr)

        forced = self.run_tmi(b, "push", "origin", "main", "--force")
        self.assertIn("forced update", forced.stdout)

    def test_non_overlapping_layer_merge_preserves_an_erase_on_one_side(self) -> None:
        # Regression: the non-overlapping layer merge used alpha_composite, so a
        # region one side *erased* (alpha -> 0) was silently re-filled from base
        # (source-over leaves the opaque backdrop showing). The merge must replace
        # each side's diff region wholesale, alpha included.
        from tmi import merge as merge_mod

        def png(img: Image.Image) -> bytes:
            buf = io.BytesIO()
            img.convert("RGBA").save(buf, "PNG")
            return buf.getvalue()

        base = Image.new("RGBA", (16, 16), (255, 0, 0, 255))
        local = base.copy()
        for y in range(4):
            for x in range(4):
                local.putpixel((x, y), (0, 0, 0, 0))  # erase top-left
        remote = base.copy()
        for y in range(12, 16):
            for x in range(12, 16):
                remote.putpixel((x, y), (0, 0, 255, 255))  # paint bottom-right

        class _Store:
            def __init__(self) -> None:
                self._m: dict[str, bytes] = {}

            def put(self, data: bytes) -> str:
                import hashlib

                key = hashlib.sha256(data).hexdigest()
                self._m[key] = data
                return key

            def read_blob(self, key: str) -> bytes:
                return self._m[key]

        class _Repo:
            def __init__(self) -> None:
                self.store = _Store()

        repo = _Repo()
        b = {"blob_ref": repo.store.put(png(base))}
        l = {"blob_ref": repo.store.put(png(local))}
        r = {"blob_ref": repo.store.put(png(remote))}

        merged_bytes = merge_mod._merge_non_overlapping_pixels(repo, b, l, r)
        self.assertIsNotNone(merged_bytes)
        merged = Image.open(io.BytesIO(merged_bytes)).convert("RGBA")
        self.assertEqual((0, 0, 0, 0), merged.getpixel((0, 0)))       # erase survives
        self.assertEqual((0, 0, 255, 255), merged.getpixel((15, 15)))  # remote paint
        self.assertEqual((255, 0, 0, 255), merged.getpixel((8, 8)))    # base untouched

    def test_layer_merge_compares_pixels_not_png_bytes_inside_folders(self) -> None:
        # Layer blobs are PNG bytes, so the same pixels encoded by Pillow and by
        # tmi_accel get different blob_refs, and a folder's subtree_ref changes
        # with any child blob. Merge must compare pixel digests and child order:
        # a re-encoded layer is unchanged, and an edit inside a folder is not a
        # structural change of the folder.
        from unittest import mock

        from tmi import merge as merge_mod

        trees: dict[str, list[dict]] = {}

        def tree(ref: str, layers: list[dict]) -> str:
            trees[ref] = layers
            return ref

        def side(name: str, a: tuple[str, str], b: tuple[str, str], digest: str) -> dict:
            sub = tree(f"sub-{name}", [
                {"stable_source_id": "a", "name": "A", "blob_ref": a[0], "pixel_sha256": a[1]},
                {"stable_source_id": "b", "name": "B", "blob_ref": b[0], "pixel_sha256": b[1]},
            ])
            top = tree(f"top-{name}", [{"stable_source_id": "f", "name": "F", "subtree_ref": sub}])
            return {"source_format": "clip", "source_digest": digest, "source_blob_ref": f"src-{name}", "tree_ref": top}

        base = side("base", ("a0-pil", "a0"), ("b0-pil", "b0"), "d-base")
        local = side("local", ("a1-pil", "a1"), ("b0-pil", "b0"), "d-local")      # edits A
        remote = side("remote", ("a0-acc", "a0"), ("b1-acc", "b1"), "d-remote")  # re-encodes A, edits B

        class _Store:
            def read_json(self, ref: str, kind: str) -> dict:
                return {"layers": trees[ref]}

            def read_blob(self, ref: str) -> bytes:
                return ref.encode("ascii")

        class _Repo:
            store = _Store()
            repo_dir = self.tmp

            def flatten_layers(self, tree_ref: str, prefix: str = "") -> list[dict]:
                out = []
                for layer in trees[tree_ref]:
                    item = dict(layer)
                    item["path_name"] = f"{prefix}/{layer['name']}" if prefix else layer["name"]
                    out.append(item)
                    if layer.get("subtree_ref"):
                        out.extend(self.flatten_layers(layer["subtree_ref"], item["path_name"]))
                return out

            def materialize_file_from_path(self, relpath: str, path: Path, **_kwargs) -> dict:
                return {"merged_from": path.read_bytes().decode("ascii")}

        written: list[tuple[str, bytes]] = []

        def fake_replace(source_path, source_format, sid, png_bytes):
            written.append((sid, png_bytes))
            return source_path.read_bytes()

        with mock.patch.object(merge_mod, "replace_layer_pixels", fake_replace):
            result = merge_mod.merge_entries(_Repo(), "art.clip", base, local, remote)

        self.assertEqual([], result.conflicts)
        # Remote's real edit (B) is written into local's file, which already
        # holds local's edit of A; remote's re-encode of A is not mistaken for
        # an edit that competes with local's.
        self.assertEqual([("b", b"b1-acc")], written)
        self.assertEqual({"merged_from": "src-local"}, result.entry)

        # Same source bytes on both sides is the same file, whatever the PNG encoder did.
        reencoded = dict(base, tree_ref=remote["tree_ref"], source_blob_ref="src-other")
        self.assertIs(merge_mod.merge_entries(_Repo(), "art.clip", base, local, reencoded).entry, local)

    def _stub_merge_repo(self, trees: dict[str, list[dict]]):
        class _Store:
            def read_json(self, ref: str, kind: str) -> dict:
                return {"layers": trees[ref]}

            def read_blob(self, ref: str) -> bytes:
                return ref.encode("ascii")

        tmp = self.tmp

        class _Repo:
            store = _Store()
            repo_dir = tmp

            def flatten_layers(self, tree_ref: str, prefix: str = "") -> list[dict]:
                return [dict(layer, path_name=layer["name"]) for layer in trees[tree_ref]]

            def materialize_file_from_path(self, relpath: str, path: Path, **_kwargs) -> dict:
                return {"merged_from": path.read_bytes().decode("ascii")}

        return _Repo()

    def test_layer_merge_takes_one_sided_structure_and_property_changes(self) -> None:
        # Issue #3: local edits only `line`'s pixels; remote changes opacity,
        # adds a layer, hides one or renames one. Like git, a change only one
        # side made wins: the merge starts from remote's file (which carries
        # the structure) and writes local's pixel edit into it.
        from unittest import mock

        from tmi import merge as merge_mod

        def layers(line="l0", color=None, extra=()):
            color = color or {}
            return [
                {"stable_source_id": "line", "name": "line", "blob_ref": f"{line}-png", "pixel_sha256": line, "opacity": 256},
                {"stable_source_id": "color", "name": "color", "blob_ref": "c0-png", "pixel_sha256": "c0", "opacity": 256, "visible": True, **color},
                *extra,
            ]

        shadow = {"stable_source_id": "shadow", "name": "shadow", "blob_ref": "s-png", "pixel_sha256": "s", "opacity": 256}
        remotes = {
            "opacity": layers(color={"opacity": 128}),
            "new layer": layers(extra=[shadow]),
            "hidden": layers(color={"visible": False}),
            "renamed": layers(color={"name": "paint"}),
        }
        for label, remote_layers in remotes.items():
            trees = {"base": layers(), "local": layers(line="l1"), "remote": remote_layers}
            entry = lambda name: {"source_format": "clip", "source_digest": name, "source_blob_ref": f"src-{name}", "tree_ref": name}
            written = []

            def fake_replace(source_path, source_format, sid, png_bytes):
                written.append((sid, png_bytes))
                return source_path.read_bytes()

            with mock.patch.object(merge_mod, "replace_layer_pixels", fake_replace):
                result = merge_mod.merge_entries(self._stub_merge_repo(trees), "art.clip", entry("base"), entry("local"), entry("remote"))
            self.assertEqual([], result.conflicts, label)
            self.assertEqual({"merged_from": "src-remote"}, result.entry, label)
            self.assertEqual([("line", b"l1-png")], written, label)

    def test_layer_merge_conflict_keeps_merged_layers_and_names_the_conflict(self) -> None:
        # Issue #1/#2: a conflict on one layer used to throw away every other
        # layer's merge (the working file stayed OURS) and the reason. The
        # partial merge keeps remote's edit of `color` and leaves the
        # conflicting `line` at OURS; the reason names the layer.
        from unittest import mock

        from tmi import merge as merge_mod

        def layers(line, color):
            return [
                {"stable_source_id": "line", "name": "line", "blob_ref": f"{line}-png", "pixel_sha256": line},
                {"stable_source_id": "color", "name": "color", "blob_ref": f"{color}-png", "pixel_sha256": color},
            ]

        trees = {"base": layers("l0", "c0"), "local": layers("l1", "c0"), "remote": layers("l2", "c2")}
        entry = lambda name: {"source_format": "clip", "source_digest": name, "source_blob_ref": f"src-{name}", "tree_ref": name}
        written = []

        def fake_replace(source_path, source_format, sid, png_bytes):
            written.append((sid, png_bytes))
            return source_path.read_bytes()

        with mock.patch.object(merge_mod, "replace_layer_pixels", fake_replace), \
                mock.patch.object(merge_mod, "_merge_non_overlapping_pixels", lambda *a: None):
            result = merge_mod.merge_entries(self._stub_merge_repo(trees), "art.clip", entry("base"), entry("local"), entry("remote"))
        self.assertIsNone(result.entry)
        self.assertEqual(["art.clip:line overlapping pixel edits"], result.conflicts)
        self.assertEqual([("line", "overlapping pixel edits")], [(c.name, c.reason) for c in result.layer_conflicts])
        self.assertEqual({"merged_from": "src-local"}, result.partial_entry)
        self.assertEqual([("color", b"c2-png")], written)

    def test_layer_merge_keeps_a_change_both_sides_made_identically(self) -> None:
        # Regression: the layer merge wrote its result starting from the *base*
        # source and only re-applied the layers it had collected as
        # ``replacements``. A layer that local and remote edited to the *same*
        # new pixels is skipped (local == remote), so it never entered
        # ``replacements`` and silently reverted to base — losing the agreed-upon
        # edit whenever some *other* layer also changed. The write-back must start
        # from local so both-sides-agree edits (and local-only edits) survive.
        from tmi.formats import mdp2 as mdp2_mod

        red, green, blue, yellow = (
            (255, 0, 0, 255),
            (0, 255, 0, 255),
            (0, 0, 255, 255),
            (255, 255, 0, 255),
        )
        repo = self.tmp / "repo"
        repo.mkdir()
        write_mdp2_layers(repo / "art.mdp", [red, green])
        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "art.mdp")
        self.run_tmi(repo, "commit", "-m", "base")
        self.run_tmi(repo, "branch", "other")

        # local edits both layers: layer0 -> blue, layer1 -> yellow.
        write_mdp2_layers(repo / "art.mdp", [blue, yellow])
        self.run_tmi(repo, "commit", "-a", "-m", "local")

        # remote makes the *same* layer0 -> blue edit and leaves layer1 at base.
        self.run_tmi(repo, "switch", "other")
        write_mdp2_layers(repo / "art.mdp", [blue, green])
        self.run_tmi(repo, "commit", "-a", "-m", "remote")

        self.run_tmi(repo, "switch", "main")
        merged = self.run_tmi(repo, "merge", "other", check=False)
        self.assertEqual(0, merged.returncode, merged.stdout + merged.stderr)

        mdp = mdp2_mod.open_mdp2(repo / "art.mdp")
        pixels = {
            layer.layer_id: mdp2_mod.render_layer_rgba(mdp, layer, apply_opacity=False).getpixel((64, 64))
            for layer in mdp.layers
        }
        self.assertEqual(blue, pixels[0])    # agreed edit kept, not reverted to base red
        self.assertEqual(yellow, pixels[1])  # local-only edit kept

    def test_clip_layer_selector_refuses_ambiguous_name_but_resolves_by_id(self) -> None:
        # Regression: write-back matched the first layer whose *name* equalled the
        # selector. CSP names are not unique (copies keep the name), so a bare name
        # could overwrite the wrong layer. It must refuse on a duplicated name yet
        # still resolve a unique clip:<id> / uuid.
        from types import SimpleNamespace
        from tmi.formats import clip as clip_mod

        def node(main_id, name, uuid):
            return SimpleNamespace(main_id=main_id, name=name, uuid=uuid, children=[])

        clip = SimpleNamespace(root=SimpleNamespace(children=[
            node(10, "Line", "aaaa1111"),
            node(11, "Line", "bbbb2222"),
            node(12, "Color", "cccc3333"),
        ]))
        with self.assertRaisesRegex(ValueError, "Rename"):
            clip_mod.find_layer_node(clip, "Line")
        self.assertEqual(10, clip_mod.find_layer_node(clip, "aaaa1111").main_id)
        self.assertEqual(11, clip_mod.find_layer_node(clip, "clip:11").main_id)
        self.assertEqual(12, clip_mod.find_layer_node(clip, "Color").main_id)
        self.assertIsNone(clip_mod.find_layer_node(clip, "nope"))

    def test_repo_layer_selectors_refuse_duplicate_names_and_suggest_renaming(self) -> None:
        from tmi.importers import ImportedLayer, find_layer
        from tmi.repo import Repo

        imported_layers = [
            ImportedLayer("clip:10", "uuid-a", "Line", "raster"),
            ImportedLayer("clip:11", "uuid-b", "Line", "raster"),
            ImportedLayer("clip:12", "uuid-c", "Color", "raster"),
        ]
        with self.assertRaisesRegex(ValueError, "Rename"):
            find_layer(imported_layers, "Line")
        self.assertEqual("clip:11", find_layer(imported_layers, "clip:11").layer_id)
        self.assertEqual("clip:12", find_layer(imported_layers, "Color").layer_id)

        repo = Repo.create(self.tmp / "repo-selector")
        tree_ref = repo.store.write_json(
            "tree",
            {
                "version": 1,
                "kind": "layer-tree",
                "layers": [
                    {"layer_id": "clip:10", "stable_source_id": "uuid-a", "name": "Line"},
                    {"layer_id": "clip:11", "stable_source_id": "uuid-b", "name": "Line"},
                    {"layer_id": "clip:12", "stable_source_id": "uuid-c", "name": "Color"},
                ],
            },
        )
        entry = {"tree_ref": tree_ref}
        with self.assertRaisesRegex(RuntimeError, "Rename"):
            repo.find_committed_layer(entry, "Line")
        self.assertEqual("uuid-b", repo.find_committed_layer(entry, "uuid-b")["stable_source_id"])
        self.assertEqual("Color", repo.find_committed_layer(entry, "Color")["name"])

    def test_mdp2_layer_selector_refuses_ambiguous_name(self) -> None:
        from tmi.formats import mdp2 as mdp2_mod

        write_mdp2_layers(self.tmp / "art.mdp", [(255, 0, 0, 255), (0, 0, 255, 255)])
        mdp = mdp2_mod.open_mdp2(self.tmp / "art.mdp")
        # Unique name and id both resolve.
        self.assertIsNotNone(mdp2_mod._find_layer_by_selector(mdp, "layer1"))
        self.assertIsNotNone(mdp2_mod._find_layer_by_selector(mdp, "mdp2:0"))
        # Force a name collision: write-back must refuse rather than pick one.
        mdp.layers[1].name = mdp.layers[0].name
        with self.assertRaisesRegex(ValueError, "Rename"):
            mdp2_mod._find_layer_by_selector(mdp, mdp.layers[0].name)

    def test_status_stat_cache_skips_rehash_of_unchanged_files(self) -> None:
        import json
        from unittest import mock
        import tmi.repo as repo_mod
        from tmi.repo import Repo

        repo = self.tmp / "repo"
        repo.mkdir()
        write_png(repo / "a.png", (255, 0, 0, 255))
        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "a.png")
        self.run_tmi(repo, "commit", "-m", "base")

        # First status warms the cache (one hash) and persists it.
        Repo.open(repo).status_items()
        cache_path = repo / ".tmi" / "stat_cache"
        self.assertTrue(cache_path.exists())
        entry = json.loads(cache_path.read_text(encoding="utf-8"))["entries"]["a.png"]
        self.assertEqual({"size", "mtime_ns", "digest"}, set(entry))

        # Second status must trust the cache and never read the file again.
        with mock.patch.object(repo_mod, "sha256_file") as spy, \
                mock.patch.object(repo_mod, "read_file_digest") as read_spy:
            items = Repo.open(repo).status_items()
        spy.assert_not_called()
        read_spy.assert_not_called()
        self.assertEqual([], items)

    def test_stat_cache_racy_guard_distrusts_entries_not_older_than_write(self) -> None:
        import json
        from tmi.repo import Repo, _StatCache, sha256_file

        repo = self.tmp / "repo"
        repo.mkdir()
        write_png(repo / "a.png", (255, 0, 0, 255))
        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "a.png")
        self.run_tmi(repo, "commit", "-m", "base")

        opened = Repo.open(repo)
        full = repo / "a.png"
        st = full.stat()
        real = sha256_file(full)
        cache_path = repo / ".tmi" / "stat_cache"

        def seed(written_ns: int) -> None:
            cache_path.write_text(
                json.dumps(
                    {
                        "version": 1,
                        "written_ns": written_ns,
                        "entries": {
                            "a.png": {
                                "size": st.st_size,
                                "mtime_ns": st.st_mtime_ns,
                                "digest": "stale",
                            }
                        },
                    }
                ),
                encoding="utf-8",
            )

        # mtime strictly older than the cache write -> trusted (returns cached digest).
        seed(st.st_mtime_ns + 1)
        self.assertEqual("stale", _StatCache(opened).digest_for("a.png", full))
        # mtime in the same (or newer) window as the write -> racy, re-hashed.
        seed(st.st_mtime_ns)
        self.assertEqual(real, _StatCache(opened).digest_for("a.png", full))

    def test_stat_cache_prunes_entries_for_deleted_files(self) -> None:
        import json
        from tmi.repo import Repo

        repo = self.tmp / "repo"
        repo.mkdir()
        write_png(repo / "a.png", (255, 0, 0, 255))
        write_png(repo / "b.png", (0, 255, 0, 255))
        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "-A")
        self.run_tmi(repo, "commit", "-m", "base")

        Repo.open(repo).status_items()  # warm: both files cached
        cache_path = repo / ".tmi" / "stat_cache"
        self.assertEqual(
            {"a.png", "b.png"},
            set(json.loads(cache_path.read_text(encoding="utf-8"))["entries"]),
        )

        (repo / "b.png").unlink()
        items = Repo.open(repo).status_items()
        self.assertIn(("deleted", "b.png"), [(i.state, i.path) for i in items])
        self.assertEqual(
            {"a.png"},
            set(json.loads(cache_path.read_text(encoding="utf-8"))["entries"]),
        )

    def test_status_tolerates_corrupt_stat_cache(self) -> None:
        import json

        repo = self.tmp / "repo"
        repo.mkdir()
        write_png(repo / "a.png", (255, 0, 0, 255))
        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "a.png")
        self.run_tmi(repo, "commit", "-m", "base")

        cache_path = repo / ".tmi" / "stat_cache"
        cache_path.write_text("{not json", encoding="utf-8")
        status = self.run_tmi(repo, "status")  # must not crash
        self.assertIn("working tree clean", status.stdout)
        json.loads(cache_path.read_text(encoding="utf-8"))  # rebuilt as valid JSON

    def test_stage_all_modified_reuses_status_stat_cache(self) -> None:
        from unittest import mock
        import tmi.repo as repo_mod
        from tmi.repo import Repo

        repo = self.tmp / "repo"
        repo.mkdir()
        write_png(repo / "a.png", (255, 0, 0, 255))
        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "a.png")
        self.run_tmi(repo, "commit", "-m", "base")

        Repo.open(repo).status_items()  # warm the shared cache
        with mock.patch.object(repo_mod, "sha256_file") as spy, \
                mock.patch.object(repo_mod, "read_file_digest") as read_spy:
            staged = Repo.open(repo).stage_all_modified()
        self.assertEqual([], staged)
        spy.assert_not_called()
        read_spy.assert_not_called()

    def test_modified_working_file_is_read_and_hashed_once(self) -> None:
        """status, commit -a, add -u and diff hash a modified working file to
        compare it, and the import takes those bytes instead of reading and
        hashing the file again."""
        import argparse
        import contextlib
        from unittest import mock
        import tmi.cli as cli_mod
        import tmi.importers as importers_mod
        import tmi.repo as repo_mod
        from tmi.diff import WORKTREE, diff_sides, index_side
        from tmi.repo import Repo

        repo = self.tmp / "repo"
        repo.mkdir()
        write_mdp2_layers(repo / "art.mdp", [(255, 0, 0, 255), (0, 0, 255, 255)])
        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "art.mdp")
        self.run_tmi(repo, "commit", "-m", "base")
        write_mdp2_layers(repo / "art.mdp", [(255, 0, 0, 255), (0, 255, 0, 255)])
        (repo / ".tmi" / "stat_cache").unlink(missing_ok=True)

        real_read = repo_mod.read_file_digest

        def run(fn, read_target):
            reads: list[Path] = []

            def counting_read(path):
                reads.append(Path(path))
                return real_read(path)

            with mock.patch.object(read_target, "read_file_digest", side_effect=counting_read), \
                    mock.patch.object(importers_mod, "_sha256", wraps=importers_mod._sha256) as source_hash, \
                    mock.patch.object(repo_mod, "sha256_file", wraps=repo_mod.sha256_file) as streamed:
                result = fn()
            self.assertEqual([repo / "art.mdp"], reads)
            source_hash.assert_not_called()
            streamed.assert_not_called()
            return result

        items = run(lambda: Repo.open(repo).status_items(), repo_mod)
        self.assertIn(("modified", "art.mdp"), [(i.state, i.path) for i in items])
        (repo / ".tmi" / "stat_cache").unlink()
        results = run(lambda: diff_sides(Repo.open(repo), index_side(), WORKTREE, mode="html", out_dir=self.tmp / "out"), repo_mod)
        self.assertEqual(["art.mdp"], [r["path"] for r in results])
        args = argparse.Namespace(specs=[], all=False, update=True, force=False)
        cwd = os.getcwd()
        os.chdir(repo)
        try:
            with contextlib.redirect_stdout(io.StringIO()) as out:
                run(lambda: cli_mod.cmd_add(args), cli_mod)
        finally:
            os.chdir(cwd)
        self.assertEqual("staged art.mdp\n", out.getvalue())
        self.run_tmi(repo, "reset")
        (repo / ".tmi" / "stat_cache").unlink(missing_ok=True)
        staged = run(lambda: Repo.open(repo).stage_all_modified(), repo_mod)
        self.assertEqual(["art.mdp"], staged)

    def test_short_status_skips_layer_decode_but_long_status_keeps_it(self) -> None:
        from unittest import mock
        import tmi.repo as repo_mod
        from tmi.repo import Repo

        repo = self.tmp / "repo"
        repo.mkdir()
        write_mdp2_layers(repo / "art.mdp", [(255, 0, 0, 255), (0, 0, 255, 255)])
        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "art.mdp")
        self.run_tmi(repo, "commit", "-m", "base")
        write_mdp2_layers(repo / "art.mdp", [(0, 255, 0, 255), (0, 0, 255, 255)])

        # Short/porcelain path: never decodes layers, detail stays empty.
        with mock.patch.object(repo_mod, "import_file", wraps=repo_mod.import_file) as spy:
            items = Repo.open(repo).status_items(with_layer_detail=False)
        spy.assert_not_called()
        self.assertIn(("modified", "art.mdp"), [(i.state, i.path) for i in items])
        self.assertEqual("", next(i.detail for i in items if i.path == "art.mdp"))

        # Long path: still decodes layers to build the human detail.
        with mock.patch.object(repo_mod, "import_file", wraps=repo_mod.import_file) as spy:
            Repo.open(repo).status_items(with_layer_detail=True)
        self.assertTrue(spy.called)

    def test_status_finds_nested_untracked_and_excludes_internal_dirs(self) -> None:
        repo = self.tmp / "repo"
        repo.mkdir()
        write_png(repo / "a.png", (255, 0, 0, 255))
        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "a.png")
        self.run_tmi(repo, "commit", "-m", "base")  # populates .tmi/objects

        nested = repo / "assets" / "big"
        nested.mkdir(parents=True)
        write_png(nested / "extra.png", (0, 255, 0, 255))
        short = self.run_tmi(repo, "status", "-s")
        self.assertIn("?? assets/big/extra.png", short.stdout)
        self.assertNotIn(".tmi", short.stdout)

    def test_re_adding_unchanged_file_leaves_index_empty(self) -> None:
        # Pixel-digest change detection: re-adding a byte-for-byte unchanged file
        # must produce no staged entry (re-encoded PNG bytes must not look like a
        # change).
        from tmi.repo import Repo

        repo = self.tmp / "repo"
        repo.mkdir()
        write_mdp2_layers(repo / "art.mdp", [(255, 0, 0, 255), (0, 0, 255, 255)])
        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "art.mdp")
        self.run_tmi(repo, "commit", "-m", "base")

        # Re-add the same unchanged file: nothing should be staged.
        self.run_tmi(repo, "add", "art.mdp")
        index = Repo.open(repo).read_index().get("entries", {})
        self.assertEqual({}, index, "re-adding unchanged file staged something")

    def test_committed_layer_tree_carries_matching_pixel_sha256(self) -> None:
        # Every raster layer dict in the committed tree must store pixel_sha256
        # equal to the digest of its decoded blob (the load-bearing invariant).
        from tmi.image_ops import layer_blob_digest
        from tmi.repo import Repo

        repo = self.tmp / "repo"
        repo.mkdir()
        write_mdp2_layers(repo / "art.mdp", [(255, 0, 0, 255), (0, 0, 255, 255)])
        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "art.mdp")
        self.run_tmi(repo, "commit", "-m", "base")

        opened = Repo.open(repo)
        entry = opened.entries_for_commit(opened.current_commit())["art.mdp"]
        raster_layers = 0
        for layer in opened.flatten_layers(entry["tree_ref"]):
            if not layer.get("blob_ref"):
                continue
            raster_layers += 1
            digest = layer.get("pixel_sha256")
            self.assertIsNotNone(digest, f"layer {layer.get('name')} missing pixel_sha256")
            blob = opened.store.read_blob(layer["blob_ref"])
            self.assertEqual(
                digest,
                layer_blob_digest(blob),
                f"layer {layer.get('name')} pixel_sha256 != decoded blob digest",
            )
        self.assertGreater(raster_layers, 0)

    def test_re_add_reuses_unchanged_layer_blobs_but_not_changed_one(self) -> None:
        # Editing one layer of a multi-layer file and re-adding must keep the
        # unchanged layers' blob_refs byte-identical (proving reuse, no
        # re-encode/re-store) while the edited layer's blob_ref differs.
        from tmi.repo import Repo

        repo = self.tmp / "repo"
        repo.mkdir()
        write_mdp2_layers(repo / "art.mdp", [(255, 0, 0, 255), (0, 0, 255, 255)])
        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "art.mdp")
        self.run_tmi(repo, "commit", "-m", "base")

        opened = Repo.open(repo)
        before = {
            layer["stable_source_id"]: layer.get("blob_ref")
            for layer in opened.flatten_layers(
                opened.entries_for_commit(opened.current_commit())["art.mdp"]["tree_ref"]
            )
        }

        # Edit only layer0 (red -> green); layer1 (blue) stays unchanged.
        write_mdp2_layers(repo / "art.mdp", [(0, 255, 0, 255), (0, 0, 255, 255)])
        self.run_tmi(repo, "add", "art.mdp")
        self.run_tmi(repo, "commit", "-m", "edit layer0")

        opened = Repo.open(repo)
        after = {
            layer["stable_source_id"]: layer.get("blob_ref")
            for layer in opened.flatten_layers(
                opened.entries_for_commit(opened.current_commit())["art.mdp"]["tree_ref"]
            )
        }

        self.assertEqual(before["mdp2:1"], after["mdp2:1"], "unchanged layer blob_ref was not reused")
        self.assertNotEqual(before["mdp2:0"], after["mdp2:0"], "changed layer blob_ref should differ")

    def test_working_tree_diff_decodes_only_changed_layers_with_same_output(self) -> None:
        # Issue #15: `diff --stat` / `diff <rev> --name-status` against the
        # working tree decoded (and rendered) the whole file. Layers whose
        # pixels a stored version already has are now not decoded, the name
        # forms parse nothing, and every output stays the same.
        from unittest import mock

        from tmi import diff as diff_mod
        from tmi.diff import WORKTREE, DiffSide, diff_sides, index_side
        from tmi.formats import mdp2 as mdp2_mod
        from tmi.repo import Repo

        repo = self.tmp / "repo"
        repo.mkdir()
        red, green, blue, white = (255, 0, 0, 255), (0, 255, 0, 255), (0, 0, 255, 255), (255, 255, 255, 255)
        write_mdp2_layers(repo / "art.mdp", [red, green, blue])
        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "art.mdp")
        self.run_tmi(repo, "commit", "-m", "v1")
        write_mdp2_layers(repo / "art.mdp", [red, white, blue])  # layer2 changes in HEAD
        self.run_tmi(repo, "add", "art.mdp")
        self.run_tmi(repo, "commit", "-m", "v2")
        write_mdp2_layers(repo / "art.mdp", [red, white, green])  # layer3 changes in the working tree

        opened = Repo.open(repo)
        head1 = opened.resolve_rev("HEAD~1")
        decodes: list[str] = []
        real = mdp2_mod.render_layer_crop

        def spy(mdp, layer, *args, **kwargs):
            decodes.append(layer.name)
            return real(mdp, layer, *args, **kwargs)

        def run(left, mode, *, reuse: bool = True):
            from contextlib import ExitStack

            decodes.clear()
            with ExitStack() as stack:
                stack.enter_context(mock.patch.object(mdp2_mod, "render_layer_crop", spy))
                if not reuse:
                    stack.enter_context(mock.patch.object(diff_mod, "_working_file_reuse", lambda *a: {}))
                results = diff_sides(opened, left, WORKTREE, mode=mode, out_dir=self.tmp / f"out-{mode}-{reuse}-{left.label}")
            for result in results:
                for layer in result["layers"]:
                    layer.pop("html", None)
                result["outputs"] = [Path(p).name for p in result["outputs"]]
            return results, list(decodes)

        # --name-only / --name-status: nothing is parsed.
        names, decoded = run(DiffSide(head1, "HEAD~1"), "names")
        self.assertEqual([{"path": "art.mdp", "state": "modified", "layers": [], "outputs": []}], names)
        self.assertEqual([], decoded)

        # --stat against the index: only the edited layer is decoded.
        stat, decoded = run(index_side(), "summary")
        self.assertEqual(["layer3"], decoded)
        self.assertEqual(["layer3"], [layer["name"] for layer in stat[0]["layers"]])
        self.assertEqual(run(index_side(), "summary", reuse=False)[0], stat)

        # Against HEAD~1 layer2 differs too; its pixels come from HEAD's blob.
        for mode in ("summary", "side-by-side"):
            fast, decoded = run(DiffSide(head1, "HEAD~1"), mode)
            self.assertEqual(["layer3"], decoded, mode)
            slow, _ = run(DiffSide(head1, "HEAD~1"), mode, reuse=False)
            self.assertEqual(slow, fast, mode)
            self.assertEqual(["layer2", "layer3"], sorted(layer["name"] for layer in fast[0]["layers"]))
    # ---- issue #37: unreadable images ----

    def test_add_dot_says_which_images_tmi_could_not_read(self) -> None:
        repo = self.tmp / "repo"
        repo.mkdir()
        write_png(repo / "a_good.png", (255, 0, 0, 255))
        jpeg = io.BytesIO()
        Image.new("RGB", (16, 16), (10, 200, 30)).save(jpeg, "JPEG")
        (repo / "b_bad.jpg").write_bytes(jpeg.getvalue()[: len(jpeg.getvalue()) // 2])
        (repo / "c_zero.png").write_bytes(b"")
        (repo / "d_text.png").write_bytes(b"this is not a png\n")
        (repo / "e_notes.txt").write_text("hello\n")
        self.run_tmi(repo, "init")

        added = self.run_tmi(repo, "add", ".")

        # one file never stops the rest, and every file that is not read as an
        # image is named, in tmi's words (no Pillow text, no object reprs)
        for name in ("a_good.png", "b_bad.jpg", "c_zero.png", "d_text.png", "e_notes.txt"):
            self.assertIn(f"staged {name}", added.stdout)
        self.assertIn(
            "tmiki: warning: could not read b_bad.jpg as a JPEG image (the data is damaged, incomplete or of a kind "
            "tmi cannot read; was the file still being saved or downloaded?); tracking as opaque binary",
            added.stderr,
        )
        self.assertIn(
            "tmiki: warning: could not read c_zero.png as a PNG image (the file is empty); tracking as opaque binary",
            added.stderr,
        )
        self.assertIn(
            "tmiki: warning: could not read d_text.png as a PNG image (it does not contain PNG data); "
            "tracking as opaque binary",
            added.stderr,
        )
        self.assertNotIn("a_good.png", added.stderr)
        self.assertNotIn("e_notes.txt", added.stderr)  # not an image by name or content: nothing to say
        self.assertNotIn("BytesIO", added.stderr)
        self.assertNotIn("PIL", added.stderr)
        # they are versioned as opaque files, byte for byte
        layers = self.run_tmi(repo, "ls-layers", "b_bad.jpg").stdout
        self.assertIn("blob:0", layers)
        self.assertIn("binary", layers)
        self.run_tmi(repo, "commit", "-m", "all")
        self.assertIn("working tree clean", self.run_tmi(repo, "status").stdout)

    def test_add_dot_reports_the_files_that_cannot_be_added_and_goes_on(self) -> None:
        repo = self.tmp / "repo"
        repo.mkdir()
        self.run_tmi(repo, "init")
        # over the pixel limit: tmi refuses to decode it (an error, not an opaque blob)
        self.run_tmi(repo, "config", "core.maxImagePixels", "100")
        write_png(repo / "a_small.png", (255, 0, 0, 255))
        Image.new("RGBA", (20, 20), (0, 0, 255, 255)).save(repo / "b_big.png")
        Image.new("RGBA", (30, 30), (0, 255, 255, 255)).save(repo / "c_big.png")
        write_png(repo / "d_small.png", (0, 255, 0, 255))

        added = self.run_tmi(repo, "add", ".", check=False)

        self.assertEqual(1, added.returncode)
        self.assertIn("staged a_small.png", added.stdout)
        self.assertIn("staged d_small.png", added.stdout)
        self.assertIn("tmiki: error: 2 files could not be added:", added.stderr)
        self.assertRegex(added.stderr, r"(?m)^  b_big\.png: canvas too large: 400 pixels \(limit 100")
        self.assertRegex(added.stderr, r"(?m)^  c_big\.png: canvas too large: 900 pixels \(limit 100")
        status = self.run_tmi(repo, "status").stdout
        self.assertIn("staged     a_small.png", status)
        self.assertIn("untracked  b_big.png", status)
        # a file named on the command line is named in its error as well
        alone = self.run_tmi(repo, "add", "b_big.png", check=False)
        self.assertEqual(1, alone.returncode)
        self.assertIn("tmiki: error: b_big.png: canvas too large: 400 pixels", alone.stderr)

    # ---- issue #38: previews of files with the same stem ----

    def test_diff_previews_of_files_with_the_same_stem_do_not_overwrite_each_other(self) -> None:
        repo = self.tmp / "repo"
        repo.mkdir()
        write_png(repo / "art.png", (255, 0, 0, 255))
        Image.new("RGB", (8, 8), (255, 0, 0)).save(repo / "art.bmp")
        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", ".")
        self.run_tmi(repo, "commit", "-m", "base")
        write_png(repo / "art.png", (0, 255, 0, 255))
        Image.new("RGB", (8, 8), (0, 0, 255)).save(repo / "art.bmp")

        out = self.tmp / "report"
        self.run_tmi(repo, "diff", "--html", "--no-open", "--out", str(out))
        self.assertEqual(
            ["art.bmp-new.png", "art.bmp-old.png", "art.png-new.png", "art.png-old.png", "index.html"],
            sorted(p.name for p in out.iterdir()),
        )
        self.assertEqual((0, 255, 0, 255), Image.open(out / "art.png-new.png").convert("RGBA").getpixel((0, 0)))
        self.assertEqual((0, 0, 255, 255), Image.open(out / "art.bmp-new.png").convert("RGBA").getpixel((0, 0)))
        self.assertEqual((255, 0, 0, 255), Image.open(out / "art.bmp-old.png").convert("RGBA").getpixel((0, 0)))
        page = (out / "index.html").read_text(encoding="utf-8")
        for href in ("art.png-new.png", "art.png-old.png", "art.bmp-new.png", "art.bmp-old.png"):
            self.assertIn(f'href="{href}"', page)

        # one picture for each file (and each of its layers), none overwritten
        # by another file's, none with the name said twice
        for mode, suffixes in (
            ("--overlay", ("overlay", "layer-overlay")),
            ("--side-by-side", ("side-by-side", "layer-side-by-side")),
            ("--per-layer", ("composite-diff", "layer-diff")),
        ):
            target = self.tmp / f"out{mode}"
            self.run_tmi(repo, "diff", mode, "--out", str(target))
            expected = sorted(f"art.{ext}-{suffix}.png" for ext in ("bmp", "png") for suffix in suffixes)
            self.assertEqual(expected, sorted(p.name for p in target.iterdir()), mode)

    # ---- issue #39: a bare `tmiki stash` ----

    def test_bare_stash_is_stash_push(self) -> None:
        repo = self.tmp / "repo"
        repo.mkdir()
        write_png(repo / "a.png", (255, 0, 0, 255))
        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "a.png")
        self.run_tmi(repo, "commit", "-m", "base")
        write_png(repo / "a.png", (0, 255, 0, 255))

        stashed = self.run_tmi(repo, "stash")

        self.assertIn("Saved working directory and index state stash@{0}: WIP", stashed.stdout)
        self.assertIn("working tree clean", self.run_tmi(repo, "status").stdout)
        self.assertEqual((255, 0, 0, 255), Image.open(repo / "a.png").convert("RGBA").getpixel((0, 0)))
        self.assertIn("stash@{0}: WIP", self.run_tmi(repo, "stash", "list").stdout)
        # with nothing to stash it does what `stash push` does (not a quiet success)
        again = self.run_tmi(repo, "stash", check=False)
        push = self.run_tmi(repo, "stash", "push", check=False)
        self.assertEqual((push.returncode, push.stdout, push.stderr), (again.returncode, again.stdout, again.stderr))
        self.run_tmi(repo, "stash", "pop")
        self.assertEqual((0, 255, 0, 255), Image.open(repo / "a.png").convert("RGBA").getpixel((0, 0)))

    # ---- issue #40: pushing tags ----

    def test_tags_can_be_pushed_and_are_fetched(self) -> None:
        remote = self.tmp / "remote.tmi"
        work = self.tmp / "work"
        work.mkdir()
        self.run_tmi(self.tmp, "init", "--bare", str(remote))
        write_png(work / "a.png", (255, 0, 0, 255))
        self.run_tmi(work, "init")
        self.run_tmi(work, "remote", "add", "origin", str(remote))
        self.run_tmi(work, "add", "a.png")
        self.run_tmi(work, "commit", "-m", "first")
        self.run_tmi(work, "tag", "v1")
        self.run_tmi(work, "tag", "-a", "v2", "-m", "second tag")

        # a tag by name, before any branch is there; again: nothing to do
        pushed = self.run_tmi(work, "push", "origin", "v1")
        self.assertRegex(pushed.stdout, r"pushed tag v1 [0-9a-f]{12} to origin")
        self.assertIn("copied", pushed.stdout)
        self.assertIn("tags are up to date on origin", self.run_tmi(work, "push", "origin", "v1").stdout)
        # --tags sends the others
        every = self.run_tmi(work, "push", "--tags")
        self.assertIn("pushed tag v2", every.stdout)
        self.assertNotIn("pushed tag v1", every.stdout)
        self.assertIn("tags are up to date on origin", self.run_tmi(work, "push", "--tags").stdout)
        self.assertTrue((remote / "refs" / "tags" / "v1").is_file())
        self.assertTrue((remote / "refs" / "tags" / "v2").is_file())
        # a branch push still pushes the branch; an unknown name is not a tag either
        self.assertIn("pushed main", self.run_tmi(work, "push", "origin", "main").stdout)
        missing = self.run_tmi(work, "push", "origin", "nope", check=False)
        self.assertEqual(1, missing.returncode)
        self.assertIn("branch has no commit: nope", missing.stderr)
        both = self.run_tmi(work, "push", "--tags", "origin", "main", check=False)
        self.assertEqual(1, both.returncode)
        self.assertIn("--tags pushes every tag", both.stderr)

        # clone and fetch bring the tags
        copy = self.tmp / "copy"
        self.run_tmi(self.tmp, "clone", str(remote), str(copy))
        self.assertEqual(["v1", "v2"], [line.split()[0] for line in self.run_tmi(copy, "tag").stdout.splitlines()])
        self.run_tmi(work, "tag", "v3")
        self.run_tmi(work, "push", "--tags")
        fetched = self.run_tmi(copy, "fetch")
        self.assertIn("  new tag v3", fetched.stdout)
        self.assertNotIn("new tag v1", fetched.stdout)

        # a tag is never moved: the remote keeps v1 unless --force
        write_png(work / "a.png", (0, 255, 0, 255))
        self.run_tmi(work, "commit", "-a", "-m", "second")
        self.run_tmi(work, "tag", "-d", "v1")
        self.run_tmi(work, "tag", "v1")
        before = (remote / "refs" / "tags" / "v1").read_text()
        moved = self.run_tmi(work, "push", "origin", "v1", check=False)
        self.assertEqual(1, moved.returncode)
        self.assertIn("tag v1 already exists on origin", moved.stderr)
        self.assertEqual(before, (remote / "refs" / "tags" / "v1").read_text())
        forced = self.run_tmi(work, "push", "--force", "origin", "v1")
        self.assertIn("pushed tag v1", forced.stdout)
        self.assertNotEqual(before, (remote / "refs" / "tags" / "v1").read_text())
        lease = self.run_tmi(work, "push", "--force-with-lease", "origin", "v1", check=False)
        self.assertEqual(1, lease.returncode)
        self.assertIn("--force-with-lease does not apply to tags", lease.stderr)

    # ---- issue #41: branch and tag names ----

    def test_branch_and_tag_names_follow_check_ref_format(self) -> None:
        repo = self.tmp / "repo"
        repo.mkdir()
        write_png(repo / "a.png", (255, 0, 0, 255))
        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "a.png")
        self.run_tmi(repo, "commit", "-m", "base")

        bad = {
            "HEAD": "HEAD is reserved",
            "-x": "starts with '-'",
            "main~1": "'~'",
            "a^b": "'^'",
            "a:b": "':'",
            "a?b": "'?'",
            "a*b": "'*'",
            "a[b": "'['",
            "a\\b": "'\\'",
            "rough 1": "a space",
            "tab\there": "control character",
            "a..b": "'..'",
            ".hidden": "starts with '.'",
            "dir/.hidden": "starts with '.'",
            "x.lock": "ends with '.lock'",
            "dir.lock/x": "ends with '.lock'",
            "a@{b": "'@{'",
            "end.": "ends with '.'",
            "a//b": "empty part",
            "trailing/": "empty part",
            "/leading": "empty part",
            "@": "it is '@'",
        }
        for name, why in bad.items():
            for command in (("branch", "--", name), ("tag", "--", name)):
                result = self.run_tmi(repo, *command, check=False)
                self.assertEqual(1, result.returncode, (command, name))
                self.assertIn(f"tmiki: error: invalid ref name: {name} (", result.stderr, (command, name))
                self.assertIn(why, result.stderr, (command, name))
        branches = self.run_tmi(repo, "branch").stdout.splitlines()
        self.assertEqual(1, len(branches))
        self.assertTrue(branches[0].startswith("* main"))
        self.assertEqual("", self.run_tmi(repo, "tag").stdout)
        for command in (("switch", "-c"), ("checkout", "-b")):
            result = self.run_tmi(repo, *command, "bad name", check=False)
            self.assertEqual(1, result.returncode, command)
            self.assertIn("invalid ref name: bad name (it contains a space)", result.stderr)
        remote = self.run_tmi(repo, "remote", "add", "bad name", "somewhere", check=False)
        self.assertEqual(1, remote.returncode)
        self.assertIn("invalid ref name: bad name", remote.stderr)

        # Japanese names, and ordinary punctuation, stay allowed
        for name in ("入稿版", "feat/線画", "release-1.0", "v1.2_rc1", "a+b", "100%"):
            self.assertIn(f"created branch {name} at", self.run_tmi(repo, "branch", name).stdout)
            self.assertIn(f"tagged {name} ->", self.run_tmi(repo, "tag", name).stdout)
        # `main~1` still means main's parent, because no branch can take that name
        write_png(repo / "a.png", (0, 255, 0, 255))
        self.run_tmi(repo, "commit", "-a", "-m", "second")
        parent = self.run_tmi(repo, "log", "--oneline", "-n", "1", "main~1").stdout
        self.assertIn("base", parent)
        self.assertNotIn("second", parent)

    # ---- issue #42: a full disk ----

    def _objects_and_cache(self, repo: Path) -> set[str]:
        return {
            p.relative_to(repo).as_posix()
            for sub in (".tmi/objects", ".tmi/cache")
            for p in (repo / sub).rglob("*")
            if p.is_file()
        }

    def test_a_full_disk_is_reported_in_tmis_words_and_leaves_nothing_behind(self) -> None:
        repo = self.tmp / "repo"
        repo.mkdir()
        write_png(repo / "a.png", (255, 0, 0, 255))
        write_png(repo / "b.png", (0, 255, 0, 255))
        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", "a.png")
        before = self._objects_and_cache(repo)

        # the disk fills up while b.png is written (the 2nd file written fails)
        self.env["TMI_TEST_ENOSPC_AFTER"] = "1"
        try:
            failed = self.run_tmi(repo, "add", "b.png", check=False)
        finally:
            del self.env["TMI_TEST_ENOSPC_AFTER"]
        self.assertEqual(1, failed.returncode)
        self.assertRegex(
            failed.stderr,
            r"tmiki: error: the disk is full while recording b\.png \(at least [^)]+ needed, [^)]+ free\); "
            r"tmi removed what it had written for it\. Free some space and run the command again\n",
        )
        self.assertNotIn("Errno", failed.stderr)
        self.assertNotIn("os error", failed.stderr)
        self.assertEqual(before, self._objects_and_cache(repo))  # what b.png wrote is gone, a.png's stays
        status = self.run_tmi(repo, "status").stdout
        self.assertIn("staged     a.png", status)
        self.assertIn("untracked  b.png", status)
        self.assertEqual([], list((repo / ".tmi").rglob("*.tmp")))

        # in a bulk add it stops at the file that does not fit
        write_png(repo / "c.png", (0, 0, 255, 255))
        self.env["TMI_TEST_ENOSPC_AFTER"] = "1"
        try:
            bulk = self.run_tmi(repo, "add", ".", check=False)
        finally:
            del self.env["TMI_TEST_ENOSPC_AFTER"]
        self.assertEqual(1, bulk.returncode)
        self.assertIn("the disk is full while recording b.png", bulk.stderr)
        self.assertNotIn("could not be added", bulk.stderr)  # no list of failures: it stopped at once
        self.assertEqual(before, self._objects_and_cache(repo))

        # not enough room for the file itself: said before anything is written
        self.env["TMI_TEST_FREE_BYTES"] = "10"
        try:
            precheck = self.run_tmi(repo, "add", "b.png", check=False)
        finally:
            del self.env["TMI_TEST_FREE_BYTES"]
        self.assertEqual(1, precheck.returncode)
        self.assertRegex(
            precheck.stderr,
            r"tmiki: error: not enough disk space to record b\.png: it needs at least \d+ bytes and 10 bytes is free\.",
        )
        self.assertEqual(before, self._objects_and_cache(repo))
        # and with room, all is well
        self.assertIn("staged b.png", self.run_tmi(repo, "add", "b.png").stdout)

    # ---- issue #43: conflicts of formats tmi cannot write back ----

    def test_conflicts_of_formats_without_layers_are_explained_for_the_user(self) -> None:
        repo = self.tmp / "repo"
        repo.mkdir()
        write_gif(repo / "a.gif", None)
        write_tiff(repo / "c.tif", None)
        self.run_tmi(repo, "init")
        self.run_tmi(repo, "add", ".")
        self.run_tmi(repo, "commit", "-m", "base")
        self.run_tmi(repo, "branch", "side")
        self.run_tmi(repo, "switch", "side")
        write_gif(repo / "a.gif", (0, 0))
        write_tiff(repo / "c.tif", (5, 5))
        self.run_tmi(repo, "commit", "-a", "-m", "side")
        self.run_tmi(repo, "switch", "main")
        write_gif(repo / "a.gif", (12, 12))
        write_tiff(repo / "c.tif", (9, 9))
        self.run_tmi(repo, "commit", "-a", "-m", "main")

        merged = self.run_tmi(repo, "merge", "side", check=False)

        self.assertEqual(1, merged.returncode)
        self.assertIn(
            "  conflict a.gif\n    GIF files cannot be merged automatically (tmi cannot write them back); "
            "take one side with --ours or --theirs",
            merged.stdout,
        )
        self.assertIn(
            "  conflict c.tif\n    TIFF files cannot be merged automatically (tmi cannot write them back); "
            "take one side with --ours or --theirs",
            merged.stdout,
        )
        self.assertNotIn("merge writer", merged.stdout)
        # no layers here: no advice about <file>:<layer> or "each conflicting layer"
        self.assertNotIn("<layer>", merged.stdout)
        self.assertNotIn("each conflicting layer", merged.stdout)
        self.assertIn("Each conflicting file still holds our version. To resolve:", merged.stdout)
        self.assertIn("tmiki restore --ours|--theirs <file>", merged.stdout)
        self.assertIn("Both sides of each conflicting file:", merged.stdout)

        # a side taken on purpose is not a surprise when it is added...
        self.run_tmi(repo, "restore", "--ours", "c.tif")
        chosen = self.run_tmi(repo, "add", "c.tif")
        self.assertEqual("", chosen.stderr)
        self.assertIn("staged c.tif", chosen.stdout)
        # ...but adding a file as the merge left it is said to keep our version
        left = self.run_tmi(repo, "add", "a.gif")
        self.assertIn(
            "warning: a.gif was staged exactly as the merge left it, so it keeps our version (their side is dropped)",
            left.stderr,
        )
        self.assertNotIn("conflicting layers", left.stderr)
        self.assertNotIn("merge writer", left.stderr)

    # ---- issue #44: symbolic links ----

    def test_symbolic_links_are_not_tracked_and_tmi_says_so(self) -> None:
        repo = self.tmp / "repo"
        repo.mkdir()
        (repo / "sub").mkdir()
        write_png(repo / "sub" / "s.png", (255, 0, 0, 255))
        try:
            os.symlink(Path("sub") / "s.png", repo / "link.png")
            os.symlink("sub", repo / "linkdir", target_is_directory=True)
        except (OSError, NotImplementedError) as exc:
            self.skipTest(f"cannot create symbolic links here: {exc}")
        self.run_tmi(repo, "init")

        # named on the command line: an error, nothing staged
        for spec, expected in (
            ("link.png", "link.png is a symbolic link; tmi does not track symbolic links or junctions "),
            ("linkdir", "linkdir is a symbolic link; tmi does not track symbolic links or junctions "),
            (
                "linkdir/s.png",
                "linkdir/s.png is beyond the symbolic link linkdir; tmi does not follow symbolic links or junctions ",
            ),
        ):
            named = self.run_tmi(repo, "add", spec, check=False)
            self.assertEqual(1, named.returncode, spec)
            self.assertIn(f"tmiki: error: {expected}", named.stderr, spec)
            self.assertEqual("", named.stdout, spec)
        self.assertNotIn("staged", self.run_tmi(repo, "status").stdout)

        # found by `add .`: skipped, with a warning for each link; the real file is staged once
        bulk = self.run_tmi(repo, "add", ".")
        self.assertIn(
            "warning: link.png is a symbolic link; tmi does not track symbolic links or junctions, so it was skipped",
            bulk.stderr,
        )
        self.assertIn(
            "warning: linkdir is a symbolic link; tmi does not track symbolic links or junctions, so it was skipped",
            bulk.stderr,
        )
        self.assertEqual("staged sub/s.png\n", bulk.stdout)
        self.run_tmi(repo, "commit", "-m", "the real file only")

    # ---- issue #45: README ----

    def test_readme_describes_how_formats_are_recorded(self) -> None:
        readme = (PROJECT_ROOT / "README.md").read_text(encoding="utf-8")
        formats = readme.split("## 対応フォーマット", 1)[1].split("\n## ", 1)[0]
        for heading in ("**レイヤー認識**", "**画像として記録**", "**その他**"):
            self.assertIn(heading, formats)
        images = formats.split("**画像として記録**", 1)[1].split("**その他**", 1)[0]
        for name in ("PNG", "JPEG", "GIF", "BMP", "TIFF", "WebP", "PSD"):
            self.assertIn(name, images)
        # PSD is read as an image (its composite), no longer "tracked as a binary"
        self.assertNotIn("(現状はバイナリとして追跡されます)", formats.split("**画像として記録**", 1)[0])
        # --out names a folder
        self.assertIn("report.html/index.html", readme)



def write_png(path: Path, color: tuple[int, int, int, int]) -> None:
    Image.new("RGBA", (8, 8), color).save(path)


def write_gif(path: Path, mark: tuple[int, int] | None) -> None:
    image = Image.new("RGB", (16, 16), (200, 200, 200))
    if mark:
        image.putpixel(mark, (255, 0, 0))
    image.convert("P").save(path, "GIF")


def write_tiff(path: Path, mark: tuple[int, int] | None) -> None:
    image = Image.new("RGB", (16, 16), (200, 200, 200))
    if mark:
        image.putpixel(mark, (255, 0, 0))
    image.save(path, "TIFF")


def png_bytes(color: tuple[int, int, int, int]) -> bytes:
    buffer = io.BytesIO()
    Image.new("RGBA", (4, 4), color).save(buffer, format="PNG")
    return buffer.getvalue()


def write_mdp2_layers(
    path: Path,
    colors: list[tuple[int, int, int, int] | None],
    *,
    canvas_size: tuple[int, int] = (128, 128),
    layer_size: tuple[int, int] = (128, 128),
) -> None:
    """Write a minimal MDP2 file with one full 128x128 ``32bpp`` layer per color.

    Each layer is a single solid tile, so ``getpixel((0, 0))`` after a round-trip
    is a deterministic check that per-layer write-back rebuilt the right tiles.
    """
    records: list[bytes] = []
    rows: list[str] = []
    for i, color in enumerate(colors):
        if color is None:
            payload = struct.pack("<I", 0)
        else:
            r, g, b, a = Image.new("RGBA", (128, 128), color).split()
            raw = Image.merge("RGBA", (b, g, r, a)).tobytes()  # BGRA, as MDP2 stores it
            comp = zlib.compress(raw)
            payload = struct.pack("<IIIIII", 1, 128, 0, 0, 0, len(comp)) + comp
            payload += b"\x00" * ((-len(payload)) % 4)
        name = f"layer{i}img"
        record_size = 132 + len(payload)
        pac = b"PAC " + struct.pack("<IIII", record_size, 0, len(payload), len(payload))
        pac += b"\x00" * (68 - len(pac)) + name.encode("ascii").ljust(64, b"\x00") + payload
        records.append(pac)
        rows.append(
            f'<Layer name="layer{i + 1}" width="{layer_size[0]}" height="{layer_size[1]}" mode="normal" alpha="255" '
            f'id="{i}" parentId="-1" bin="{name}" type="32bpp" visible="true" ofsx="0" ofsy="0"/>'
        )
    xml = (
        f'<MDP width="{canvas_size[0]}" height="{canvas_size[1]}" dpi="72"><Layers>'
        + "".join(rows)
        + "</Layers></MDP>"
    ).encode("utf-8")
    pac = b"".join(records)
    data = b"mdipack\x00" + struct.pack("<III", 0, len(xml), len(pac)) + xml + pac
    path.write_bytes(data)


def patch_mdp2_xml(path: Path, edit) -> None:
    """Rewrite the XML part of an MDP2 file written by ``write_mdp2_layers``."""
    data = path.read_bytes()
    xml_len, pac_len = struct.unpack_from("<II", data, 12)
    xml = edit(data[20:20 + xml_len])
    path.write_bytes(data[:8] + struct.pack("<III", 0, len(xml), pac_len) + xml + data[20 + xml_len:])


def write_mdp2_text_layer(path: Path) -> None:
    """Write a minimal MediBang MDP2 file with a single ``type="text"`` layer.

    The text layer ships the same 128px BGRA tile stream as pixel layers, which
    is exactly what the importer now rasterises.
    """
    image = Image.new("RGBA", (128, 128), (0, 0, 0, 0))
    for y in range(40, 90):
        for x in range(40, 90):
            image.putpixel((x, y), (10, 20, 200, 255))
    r, g, b, a = image.split()
    raw = Image.merge("RGBA", (b, g, r, a)).tobytes()  # BGRA, as MDP2 stores it
    comp = zlib.compress(raw)
    payload = struct.pack("<IIIIII", 1, 128, 0, 0, 0, len(comp)) + comp
    payload += b"\x00" * ((-len(payload)) % 4)
    record_size = 132 + len(payload)
    pac = b"PAC " + struct.pack("<IIII", record_size, 0, len(payload), 0)
    pac += b"\x00" * (68 - len(pac)) + b"tile0img".ljust(64, b"\x00") + payload
    xml = (
        '<MDP width="128" height="128" dpi="72"><Layers>'
        '<Layer name="テキスト" width="128" height="128" mode="normal" alpha="255" '
        'id="0" parentId="-1" bin="tile0img" type="text" visible="true" ofsx="0" ofsy="0"/>'
        "</Layers></MDP>"
    ).encode("utf-8")
    data = b"mdipack\x00" + struct.pack("<III", 0, len(xml), len(pac)) + xml + pac
    path.write_bytes(data)


def _png_with(image: Image.Image, **params) -> bytes:
    buffer = io.BytesIO()
    image.save(buffer, format="PNG", **params)
    return buffer.getvalue()


def _srgb_icc() -> bytes:
    from PIL import ImageCms

    return ImageCms.ImageCmsProfile(ImageCms.createProfile("sRGB")).tobytes()


def _gradient(size: tuple[int, int], mode: str = "RGBA") -> Image.Image:
    width, height = size
    image = Image.new("RGBA", size)
    image.putdata([((x * 7) % 256, (y * 5) % 256, (x + y) % 256, (x * y) % 256) for y in range(height) for x in range(width)])
    return image if mode == "RGBA" else image.convert(mode)


def _as_pil(image) -> Image.Image:
    """A PIL image of a ``tmi.raster`` image (tmi's image operations return
    those), or the PIL image itself."""
    return image if isinstance(image, Image.Image) else image.to_pil()


class ImageHandoffTests(unittest.TestCase):
    """Images handed between import and diff instead of a PNG round trip, and
    the decode/thumbnail helpers that skip pixel copies, must be what the
    round trip / the copying code gave: same pixels, same ``info``."""

    def setUp(self) -> None:
        root = PROJECT_ROOT / ".test-tmp"
        root.mkdir(exist_ok=True)
        self.tmp = Path(tempfile.mkdtemp(prefix="tmi-handoff-", dir=root))

    def tearDown(self) -> None:
        shutil.rmtree(self.tmp, ignore_errors=True)
        try:
            (PROJECT_ROOT / ".test-tmp").rmdir()
        except OSError:
            pass

    def assertSameImage(self, want: Image.Image, got: Image.Image, label: str = "") -> None:
        self.assertEqual((want.mode, want.size), (got.mode, got.size), label)
        self.assertEqual(want.info, got.info, label)
        self.assertEqual(want.tobytes(), got.tobytes(), label)

    def test_image_from_bytes_equals_convert(self) -> None:
        from tmi.image_ops import LayerCrop, image_from_bytes

        rgba = _gradient((37, 23))
        palette = rgba.convert("RGB").convert("P", palette=Image.Palette.ADAPTIVE, colors=16)
        cases = {
            "rgba": _png_with(rgba),
            "rgba+icc+dpi": _png_with(rgba, icc_profile=_srgb_icc(), dpi=(300, 300)),
            "rgb": _png_with(rgba.convert("RGB")),
            "rgb+trns": _png_with(rgba.convert("RGB"), transparency=(0, 5, 0)),
            "la": _png_with(rgba.convert("LA")),
            "p+trns": _png_with(palette, transparency=3),
        }
        jpeg = io.BytesIO()
        rgba.convert("RGB").save(jpeg, format="JPEG", quality=90)
        cases["jpeg"] = jpeg.getvalue()
        for label, data in cases.items():
            # The pixels of Pillow's decode; no metadata (tmi_accel decodes
            # pixels only, and nothing tmi stores carries metadata a reader
            # uses).
            want = Image.open(io.BytesIO(data)).convert("RGBA")
            want.info = {}
            self.assertSameImage(want, image_from_bytes(data), label)
        full = Image.new("RGBA", (50, 40))
        full.paste(rgba, (3, 4))
        crop = LayerCrop.from_full(full)
        decoded = image_from_bytes(crop.encode())
        self.assertSameImage(full, decoded, "layer blob")

    def test_make_thumbnail_matches_copying_version(self) -> None:
        from tmi.image_ops import image_to_png_bytes, make_thumbnail

        def reference(image: Image.Image, size: int = 384) -> Image.Image:
            thumb = image.copy() if image.mode == "RGBA" else image.convert("RGBA")
            thumb.thumbnail((size, size), Image.Resampling.LANCZOS)
            canvas = Image.new("RGBA", (size, size), (0, 0, 0, 0))
            canvas.alpha_composite(thumb, ((size - thumb.width) // 2, (size - thumb.height) // 2))
            return canvas

        for size, mode in [((900, 500), "RGBA"), ((300, 200), "RGBA"), ((640, 960), "RGB")]:
            image = _gradient(size, mode)
            before = image.tobytes()
            self.assertSameImage(reference(image), make_thumbnail(image), f"{size} {mode}")
            self.assertEqual(image_to_png_bytes(reference(image)), image_to_png_bytes(make_thumbnail(image)))
            self.assertEqual(before, image.tobytes(), "the source image is left alone")
            self.assertEqual(size, image.size)

    def test_decoded_twin_is_what_decoding_gives(self) -> None:
        from tmi.image_ops import LayerCrop, decoded_twin, image_from_bytes, image_to_png_bytes

        image = _gradient((64, 48))
        image.info.update({"icc_profile": _srgb_icc(), "dpi": (72, 72), "Software": "x"})
        for label, png in {
            "tmi encode": image_to_png_bytes(image),
            "tmi encode + text": image_to_png_bytes(image, text=("tmi-composite", "{}")),
            "pillow + icc": _png_with(image, icc_profile=image.info["icc_profile"], dpi=(96, 96)),
        }.items():
            twin = decoded_twin(image, png)
            self.assertIsNotNone(twin, label)
            self.assertSameImage(image_from_bytes(png), twin, label)
            self.assertIn("icc_profile", image.info, "the source image keeps its info")

        # Cases a decode cannot be predicted from the chunks before the data.
        self.assertIsNone(decoded_twin(image, LayerCrop.from_full(image).encode()), "layer blob")
        self.assertIsNone(decoded_twin(image, _png_with(image.convert("RGB"))), "colour type")
        self.assertIsNone(decoded_twin(image.convert("RGB"), _png_with(image.convert("RGB"))), "mode")
        self.assertIsNone(decoded_twin(_gradient((10, 10)), image_to_png_bytes(image)), "size")
        png = image_to_png_bytes(image)
        text = b"tEXt" + b"late\0value"
        trailing = png[:-12] + struct.pack(">I", len(text) - 4) + text + struct.pack(">I", zlib.crc32(text)) + png[-12:]
        self.assertEqual("value", Image.open(io.BytesIO(trailing)).convert("RGBA").info.get("late"))
        self.assertIsNone(decoded_twin(image, trailing), "metadata after the image data")

    def test_import_composite_image_equals_a_decode_of_the_composite_png(self) -> None:
        from tmi.image_ops import image_from_bytes
        from tmi.importers import import_file

        paths = []
        png = self.tmp / "a.png"
        image = _gradient((90, 70))
        image.save(png, icc_profile=_srgb_icc(), dpi=(350, 350))
        paths.append(png)
        mdp = self.tmp / "a.mdp"
        write_mdp2_layers(mdp, [(255, 0, 0, 128), (0, 0, 255, 255)])
        paths.append(mdp)
        example = PROJECT_ROOT / "example" / "layer_types.clip"
        if example.exists():
            from tmi.formats import clip as C

            paths.append(example)  # CanvasPreview
            written = self.tmp / "w.clip"
            written.write_bytes(example.read_bytes())
            from tmi.image_ops import image_to_png_bytes

            clip = C.open_clip(example)
            for node in C.walk_layer_nodes(clip.root.children):
                info = None if node.folder else C.layer_offscreen_info(clip, node.render_mipmap)
                if not info or C.parse_offscreen_attribute(info[1]).channel_profile != "color-alpha-bgra":
                    continue
                rendered = C.render_layer_rgba(clip, node)
                if rendered is not None:
                    break
            # The same pixels written back: no CanvasPreview any more.
            pixels = image_to_png_bytes(rendered.image)
            written.write_bytes(C.replace_layer_pixels(written, f"clip:{node.main_id}", pixels, invalidate_preview_on_noop=True))
            self.assertIsNone(C.render_canvas_preview_rgba(C.open_clip(written)))
            paths.append(written)  # tmi's layered render
        for path in paths:
            imported = import_file(path, composite_image=True)
            self.assertIsNotNone(imported.composite_image, path.name)
            self.assertSameImage(image_from_bytes(imported.composite_png), imported.composite_image, path.name)
            self.assertIsNone(import_file(path).composite_image, "only kept when asked for")

    def test_diff_output_is_unchanged_by_the_composite_handoff(self) -> None:
        from unittest import mock
        import tmi.importers as importers_mod
        from tmi.diff import WORKTREE, diff_sides, index_side
        from tmi.repo import Repo

        repo = self.tmp / "repo"
        repo.mkdir()
        env = dict(os.environ, PYTHONPATH=str(PROJECT_ROOT), TMI_AUTHOR_NAME="T", TMI_AUTHOR_EMAIL="t@example.com")
        image = _gradient((120, 80))
        image.save(repo / "a.png", icc_profile=_srgb_icc(), dpi=(350, 350))
        write_mdp2_layers(repo / "b.mdp", [(255, 0, 0, 128), (0, 0, 255, 255)])
        for args in (["init"], ["add", "a.png", "b.mdp"], ["commit", "-m", "base"]):
            subprocess.run([sys.executable, "-m", "tmi", *args], cwd=repo, env=env, check=True, capture_output=True)
        image.paste((0, 200, 0, 255), (10, 10, 30, 40))
        image.save(repo / "a.png", icc_profile=_srgb_icc(), dpi=(350, 350))
        write_mdp2_layers(repo / "b.mdp", [(255, 0, 0, 128), (0, 255, 0, 255)])

        def run(mode: str, label: str) -> list:
            # Render afresh each time: a render-cache hit has no image to hand.
            shutil.rmtree(repo / ".tmi" / "cache", ignore_errors=True)
            return diff_sides(Repo.open(repo), index_side(), WORKTREE, mode=mode, out_dir=self.tmp / label)

        for mode in ("html", "side-by-side", "overlay", "per-layer"):
            handed = run(mode, f"{mode}-handed")
            with mock.patch.object(importers_mod, "decoded_twin", return_value=None):
                decoded = run(mode, f"{mode}-decoded")
            strip = lambda results: [  # noqa: E731
                {**r, "outputs": [Path(o).name for o in r["outputs"]]} for r in results
            ]
            self.assertEqual(strip(decoded), strip(handed), mode)
            for name in sorted(p.name for p in (self.tmp / f"{mode}-decoded").iterdir()):
                self.assertEqual(
                    (self.tmp / f"{mode}-decoded" / name).read_bytes(),
                    (self.tmp / f"{mode}-handed" / name).read_bytes(),
                    f"{mode}: {name}",
                )


class DiffHtmlPreviewTests(unittest.TestCase):
    """`diff --html` previews are made from canvases downscaled once and
    encoded side by side; bboxes are worked out within the changed bytes.
    Previews only need to look the same; every bbox must be exact."""

    def setUp(self) -> None:
        root = PROJECT_ROOT / ".test-tmp"
        root.mkdir(exist_ok=True)
        self.tmp = Path(tempfile.mkdtemp(prefix="tmi-preview-", dir=root))

    def tearDown(self) -> None:
        shutil.rmtree(self.tmp, ignore_errors=True)
        try:
            (PROJECT_ROOT / ".test-tmp").rmdir()
        except OSError:
            pass

    @staticmethod
    def _pair(size: tuple[int, int], *, alpha: bool = True) -> tuple[Image.Image, Image.Image]:
        from PIL import ImageDraw

        old = _gradient(size)
        if not alpha:
            old.putalpha(255)
        new = old.copy()
        draw = ImageDraw.Draw(new)
        w, h = size
        draw.line([(w // 4, h // 3), (w // 2, h // 2), (3 * w // 4, h // 3)], fill=(200, 30, 60, 255), width=max(2, w // 50))
        return old, new

    @staticmethod
    def _psnr(want: Image.Image, got: Image.Image) -> float:
        import numpy as np

        def on_dark(image: Image.Image):
            canvas = Image.new("RGBA", image.size, (10, 10, 10, 255))
            canvas.alpha_composite(_as_pil(image).convert("RGBA"))
            return np.asarray(canvas.convert("RGB"), dtype=np.float64)

        mse = float(np.mean((on_dark(want) - on_dark(got)) ** 2))
        return float("inf") if mse == 0 else 10 * np.log10(255 ** 2 / mse)

    @staticmethod
    def _full_size_previews(old: Image.Image, new: Image.Image, bbox, edge: int) -> dict[str, Image.Image]:
        """The previews as they were made before: each built at full size,
        then downscaled with plain Lanczos."""
        from tmi.image_ops import diff_preview, overlay, side_by_side

        def shrink(image: Image.Image) -> Image.Image:
            image = _as_pil(image)
            scale = edge / max(image.size)
            if scale >= 1:
                return image
            size = (max(1, round(image.width * scale)), max(1, round(image.height * scale)))
            return image.resize(size, Image.Resampling.LANCZOS)

        return {
            "old_png": shrink(old),
            "new_png": shrink(new),
            "side_by_side_png": shrink(side_by_side(old, new)),
            "overlay_png": shrink(overlay(old, new)),
            "diff_preview_png": shrink(diff_preview(old, new, bbox=bbox)),
        }

    def test_parallel_preview_encode_equals_encoding_one_after_another(self) -> None:
        from unittest import mock

        from tmi import diff as diff_mod

        images = {
            "b": Image.effect_noise((300, 200), 40).convert("RGBA"),
            "a": _gradient((257, 129)),
            "c": Image.new("RGBA", (64, 64), (0, 0, 0, 0)),
            "d": _gradient((40, 700), "RGB"),
        }
        serial = {key: diff_mod._preview_bytes(image) for key, image in images.items()}
        for _ in range(3):
            parallel = diff_mod._encode_previews(images)
            self.assertEqual(list(images), list(parallel))
            self.assertEqual(serial, parallel)
        self.assertEqual({}, diff_mod._encode_previews({}))

    def test_fast_diff_bbox_equals_diff_bbox(self) -> None:
        import random

        from PIL import ImageDraw

        from tmi import diff as diff_mod
        from tmi.image_ops import diff_bbox

        rng = random.Random(7)
        base = _gradient((211, 157))
        cases = [(base, base.copy())]
        for _ in range(12):
            edited = base.copy()
            draw = ImageDraw.Draw(edited)
            for _ in range(rng.randint(1, 3)):
                x, y = rng.randrange(211), rng.randrange(157)
                colour = tuple(rng.randrange(256) for _ in range(4))
                draw.rectangle((x, y, x + rng.randint(0, 30), y + rng.randint(0, 20)), fill=colour)
            cases.append((base, edited))
        # RGB that changes under alpha 0 only: bytes differ, nothing visible.
        clear = Image.new("RGBA", (50, 40), (0, 0, 0, 0))
        hidden = clear.copy()
        hidden.paste((255, 0, 0, 0), (10, 10, 20, 20))
        cases.append((clear, hidden))
        cases.append((Image.new("RGBA", (50, 40), (1, 2, 3, 255)), Image.new("RGBA", (50, 40), (3, 2, 1, 255))))
        cases.append((base, base.crop((0, 0, 200, 150))))  # sizes differ
        cases.append((base.convert("RGB"), base.copy()))  # modes differ
        corner = base.copy()
        corner.putpixel((210, 156), (0, 0, 0, 0))
        cases.append((base, corner))
        for i, (left, right) in enumerate(cases):
            self.assertEqual(diff_bbox(left, right), diff_mod._diff_bbox(left, right), i)
            self.assertEqual(diff_bbox(right, left), diff_mod._diff_bbox(right, left), i)

    def test_composite_previews_look_like_full_size_ones(self) -> None:
        import math
        from unittest import mock

        from tmi import diff as diff_mod
        from tmi.image_ops import diff_bbox

        # (canvas, edge): within the edge; the canvas fits but the pair does
        # not; both shrink; a tall canvas; opaque canvases.
        for size, edge, alpha in (((300, 200), 2048, True), ((200, 120), 300, True), ((640, 480), 256, True),
                                  ((120, 500), 200, True), ((640, 480), 256, False), ((1001, 333), 777, False)):
            old, new = self._pair(size, alpha=alpha)
            bbox = diff_bbox(old, new)
            with mock.patch.object(diff_mod, "HTML_COMPOSITE_EDGE", edge):
                got = diff_mod._composite_preview_images(old, new, bbox)
            want = self._full_size_previews(old, new, bbox, edge)
            self.assertEqual(list(want), list(got))
            fits = 2 * size[0] + 16 <= edge and size[1] <= edge
            for key in want:
                label = f"{size} edge {edge} alpha {alpha}: {key}"
                self.assertEqual((want[key].mode, want[key].size), (got[key].mode, got[key].size), label)
                if fits:
                    self.assertEqual(want[key].tobytes(), got[key].tobytes(), label)
                elif key in ("old_png", "new_png") or (key == "side_by_side_png" and not alpha):
                    self.assertEqual(want[key].tobytes(), got[key].tobytes(), label)
                elif key != "diff_preview_png":
                    # Translucent pixels: resampled, then laid on the gutter
                    # colour / blended, rounding differs from the other order.
                    self.assertGreater(self._psnr(want[key], got[key]), 40, label)
            # The outline is drawn on the small "new" preview, around the
            # scaled bbox, and touches nothing else.
            outlined, plain = got["diff_preview_png"], got["new_png"]
            sx, sy = plain.width / size[0], plain.height / size[1]
            x0, y0 = int(bbox[0] * sx), int(bbox[1] * sy)
            self.assertEqual((255, 0, 0, 255), outlined.getpixel((x0, y0)), size)
            ring = Image.new("L", plain.size, 0)
            from PIL import ImageDraw

            ImageDraw.Draw(ring).rectangle((x0 - 1, y0 - 1, math.ceil(bbox[2] * sx) + 1, math.ceil(bbox[3] * sy) + 1), fill=255)
            black = Image.new("RGBA", plain.size, (0, 0, 0, 0))
            self.assertEqual(
                Image.composite(black, _as_pil(plain), ring).tobytes(),
                Image.composite(black, _as_pil(outlined), ring).tobytes(),
                size,
            )

    def test_composite_previews_of_canvases_of_different_sizes(self) -> None:
        from unittest import mock

        from tmi import diff as diff_mod
        from tmi.image_ops import diff_bbox

        old, new = _gradient((300, 200)), _gradient((320, 180))
        bbox = diff_bbox(old, new)
        with mock.patch.object(diff_mod, "HTML_COMPOSITE_EDGE", 256):
            got = diff_mod._composite_preview_images(old, new, bbox)
        want = self._full_size_previews(old, new, bbox, 256)
        for key in want:
            self.assertEqual(want[key].size, got[key].size, key)
            self.assertGreater(self._psnr(want[key], got[key]), 30, key)

    def test_html_diff_keeps_bboxes_and_shows_layers_side_by_side_and_overlaid(self) -> None:
        from tmi.diff import WORKTREE, diff_sides, index_side
        from tmi.diff_html import render_report
        from tmi.repo import Repo

        repo = self.tmp / "repo"
        repo.mkdir()
        env = dict(os.environ, PYTHONPATH=str(PROJECT_ROOT), TMI_AUTHOR_NAME="T", TMI_AUTHOR_EMAIL="t@example.com")
        write_mdp2_layers(repo / "b.mdp", [(255, 0, 0, 128), (0, 0, 255, 255), (0, 0, 0, 0)])
        for args in (["init"], ["add", "b.mdp"], ["commit", "-m", "base"]):
            subprocess.run([sys.executable, "-m", "tmi", *args], cwd=repo, env=env, check=True, capture_output=True)
        write_mdp2_layers(repo / "b.mdp", [(255, 0, 0, 128), (0, 255, 0, 255), (9, 9, 9, 255)])

        html = diff_sides(Repo.open(repo), index_side(), WORKTREE, mode="html", out_dir=self.tmp / "html")
        plain = diff_sides(Repo.open(repo), index_side(), WORKTREE, mode="side-by-side", out_dir=self.tmp / "sbs")
        self.assertEqual([r["bbox"] for r in plain], [r["bbox"] for r in html])
        self.assertEqual(
            [[(l["name"], l["state"], l.get("bbox")) for l in r["layers"]] for r in plain],
            [[(l["name"], l["state"], l.get("bbox")) for l in r["layers"]] for r in html],
        )
        self.assertEqual(
            ["old_png", "new_png", "side_by_side_png", "overlay_png", "diff_preview_png", "size", "full_old", "full_new"],
            list(html[0]["html"]),
        )
        layers = html[0]["layers"]
        self.assertEqual(2, len(layers))
        for layer in layers:
            self.assertEqual(["side_by_side_png", "overlay_png"], list(layer["html"]), layer["name"])
        text = render_report(html, self.tmp / "report" / "index.html").read_text(encoding="utf-8")
        self.assertEqual(5 + 2 * len(layers), text.count("base64,"))


class SkippedWorkTests(unittest.TestCase):
    """Work the import no longer repeats or does one step after another must
    leave the same results: entries, objects, previews, status lines."""

    def setUp(self) -> None:
        root = PROJECT_ROOT / ".test-tmp"
        root.mkdir(exist_ok=True)
        self.tmp = Path(tempfile.mkdtemp(prefix="tmi-skip-", dir=root))

    def tearDown(self) -> None:
        shutil.rmtree(self.tmp, ignore_errors=True)
        try:
            (PROJECT_ROOT / ".test-tmp").rmdir()
        except OSError:
            pass

    def _tmi_written_clip(self) -> Path | None:
        """example/layer_types.clip as tmi writes it: no CanvasPreview, so the
        import renders and encodes the composite itself."""
        example = PROJECT_ROOT / "example" / "layer_types.clip"
        if not example.exists():
            return None
        from tmi.formats import clip as C
        from tmi.image_ops import image_to_png_bytes

        clip = C.open_clip(example)
        for node in C.walk_layer_nodes(clip.root.children):
            info = None if node.folder else C.layer_offscreen_info(clip, node.render_mipmap)
            if info and C.parse_offscreen_attribute(info[1]).channel_profile == "color-alpha-bgra":
                rendered = C.render_layer_rgba(clip, node)
                if rendered is not None:
                    break
        path = self.tmp / "w.clip"
        path.write_bytes(example.read_bytes())
        path.write_bytes(C.replace_layer_pixels(
            path, f"clip:{node.main_id}", image_to_png_bytes(rendered.image), invalidate_preview_on_noop=True,
        ))
        return path

    def test_previews_equal_encoding_one_after_another(self) -> None:
        """The thumbnail is made while the composite encodes (and, for a
        raster image, both beside the layer blob)."""
        from tmi.image_ops import LayerCrop, image_from_bytes, image_to_png_bytes, make_thumbnail
        from tmi.importers import import_file

        png = self.tmp / "a.png"
        image = _gradient((300, 200))
        image.save(png, icc_profile=_srgb_icc(), dpi=(350, 350))
        mdp = self.tmp / "a.mdp"
        write_mdp2_layers(mdp, [(255, 0, 0, 128), (0, 0, 255, 255)])
        paths = [png, mdp] + [p for p in [self._tmi_written_clip()] if p is not None]
        for path in paths:
            imported = import_file(path)
            composite = image_from_bytes(imported.composite_png)
            self.assertEqual(image_to_png_bytes(make_thumbnail(composite)), imported.thumbnail_png, path.name)
        opened = Image.open(png).convert("RGBA")
        imported = import_file(png)
        self.assertEqual(image_to_png_bytes(opened), imported.composite_png)
        self.assertEqual(image_to_png_bytes(make_thumbnail(opened)), imported.thumbnail_png)
        self.assertEqual(LayerCrop.from_full(opened).encode(), imported.layers[0].blob_bytes)
        if len(paths) > 2:
            from tmi.formats import clip as C
            from tmi.formats import clip_partial as P

            clip = C.open_clip(paths[2])
            self.assertEqual(P.encode_layered_composite(*P.render_full(clip)), import_file(paths[2]).composite_png)

    def test_exta_digests_hash_views_like_the_copied_payload(self) -> None:
        example = PROJECT_ROOT / "example" / "layer_types.clip"
        if not example.exists():
            self.skipTest("example/layer_types.clip missing")
        import hashlib

        from tmi.formats import clip as C
        from tmi.importers import (
            PIXEL_SOURCE_VERSION,
            _clip_exta_payload_sha256,
            _clip_normalized_exta_payload_sha256,
            _pixel_source_key,
            _clip_exta_pieces,
        )

        clip = C.open_clip(example)
        self.assertTrue(clip.extas)
        for outer_id, chunk in clip.extas.items():
            payload = clip.raw[chunk.payload_off:chunk.payload_off + chunk.payload_size]
            self.assertEqual(_clip_normalized_exta_payload_sha256(payload), _clip_exta_payload_sha256(clip, outer_id))
            normalized = payload[:8] + payload[48:] if payload[8:16] == b"extrnlid" else payload
            h = hashlib.sha256(f"tmi-pixel-source v{PIXEL_SOURCE_VERSION} clip \0".encode("utf-8"))
            for part in (b"attr", normalized):
                h.update(len(part).to_bytes(8, "big"))
                h.update(part)
            self.assertEqual(h.hexdigest(), _pixel_source_key("clip", "", b"attr", _clip_exta_pieces(clip, outer_id)))

    def test_early_source_write_stores_what_one_batch_stored(self) -> None:
        from tmi.importers import import_file
        from tmi.objects import canonical_json_bytes
        from tmi.repo import Repo

        sources = [self.tmp / "a.png", self.tmp / "a.mdp"]
        _gradient((64, 48)).save(sources[0])
        write_mdp2_layers(sources[1], [(255, 0, 0, 128), (0, 0, 255, 255)])
        clip = self._tmi_written_clip()
        if clip is not None:
            sources.append(clip)
        for source in sources:
            stores = []
            entries = []
            for label in ("stage_file", "one batch"):
                root = self.tmp / f"{source.name}-{label}"
                root.mkdir()
                target = root / source.name
                target.write_bytes(source.read_bytes())
                repo = Repo.create(root)
                if label == "stage_file":
                    repo.stage_file(str(target))
                    entries.append(repo.read_index()["entries"][source.name])
                else:
                    entries.append(repo.materialize_import(source.name, import_file(target, cache=repo.render_cache)))
                stores.append({p.relative_to(root).as_posix(): p.read_bytes() for p in (root / ".tmi" / "objects").rglob("*") if p.is_file()})
            self.assertEqual(canonical_json_bytes(entries[1]), canonical_json_bytes(entries[0]), source.name)
            self.assertEqual(sorted(stores[1]), sorted(stores[0]), source.name)
            self.assertEqual(stores[1], stores[0], source.name)

    def test_status_compares_digests_without_encoding_layers(self) -> None:
        from unittest import mock
        import tmi.image_ops as image_ops
        from tmi.importers import import_file
        from tmi.repo import Repo

        root = self.tmp / "repo"
        root.mkdir()
        art = root / "art.mdp"
        write_mdp2_layers(art, [(255, 0, 0, 255), (0, 0, 255, 255), (0, 255, 0, 255)])
        repo = Repo.create(root)
        repo.stage_file(str(art))
        with mock.patch.dict(os.environ, {"TMI_AUTHOR_NAME": "T", "TMI_AUTHOR_EMAIL": "t@example.com"}):
            repo.commit("base")
        write_mdp2_layers(art, [(255, 0, 0, 255), (9, 9, 9, 255), (0, 255, 0, 64)])
        entry = repo.entries_for_commit(repo.current_commit())["art.mdp"]

        with mock.patch.object(image_ops.LayerCrop, "encode", autospec=True, side_effect=image_ops.LayerCrop.encode) as encode:
            fast = repo._import_to_compare(art, entry, None, None)
        encode.assert_not_called()
        full = import_file(art, render_composite=False)
        self.assertEqual(repo.layer_changes(entry, full), repo.layer_changes(entry, fast))
        self.assertEqual(2, len(repo.layer_changes(entry, fast)))

        # A layer stored without a pixel digest (older commits) is compared by
        # blob, so its blob is still encoded.
        tree = repo.store.read_json(entry["tree_ref"], "tree")
        legacy_sid = tree["layers"][1]["stable_source_id"]
        tree["layers"][1] = {k: v for k, v in tree["layers"][1].items() if k != "pixel_sha256"}
        legacy = dict(entry, tree_ref=repo.store.write_json("tree", tree))
        fast = repo._import_to_compare(art, legacy, None, None)
        blobs = {layer.stable_source_id: layer.blob_bytes for layer in fast.layers}
        self.assertIsNotNone(blobs[legacy_sid])
        self.assertEqual([legacy_sid], [sid for sid, blob in blobs.items() if blob is not None])
        self.assertEqual(repo.layer_changes(legacy, full), repo.layer_changes(legacy, fast))

        # status itself: the staged entry's unstaged changes and HEAD's.
        with mock.patch.object(image_ops.LayerCrop, "encode", autospec=True, side_effect=image_ops.LayerCrop.encode) as encode:
            items = [(i.state, i.path, i.detail) for i in repo.status_items()]
        encode.assert_not_called()
        self.assertEqual("modified", items[0][0])


class IncrementalCompositeTests(unittest.TestCase):
    """A CLIP file tmi wrote has no CanvasPreview, so tmi composites it; from
    the previous version's composite only the changed area is re-rendered
    (tmi/formats/clip_partial.py). Every check here is byte equality with the
    whole-canvas render. Example files are not in git; missing ones skip."""

    # Small files covering: blend modes, clipping, masks, pass-through
    # folders, correction layers, トーン, gradient fills, vectors, comic
    # frames, glow modes.
    FILES = [
        "layer_types.clip",
        "vector_complex.clip",
        "blend_modes.clip",
        "csp515/structure.clip",
        "csp515/mask_off.clip",
        "csp515/tone_lpi10.clip",
        "csp515/glow_add.clip",
        "csp515/adjust_neg.clip",
        "csp515/fill_grad.clip",
        "csp515/v2.clip",
        "additional_examples/probe_shikiso+90_saido-50_colored.clip",
    ]

    def setUp(self) -> None:
        root = PROJECT_ROOT / ".test-tmp"
        root.mkdir(exist_ok=True)
        self.tmp = Path(tempfile.mkdtemp(prefix="tmi-inc-", dir=root))

    def tearDown(self) -> None:
        shutil.rmtree(self.tmp, ignore_errors=True)
        try:
            (PROJECT_ROOT / ".test-tmp").rmdir()
        except OSError:
            pass

    # ── helpers ──

    def _examples(self, names: list[str]) -> list[Path]:
        paths = [PROJECT_ROOT / "example" / name for name in names]
        paths = [p for p in paths if p.exists()]
        if not paths:
            self.skipTest("no example CLIP files")
        return paths

    @staticmethod
    def _colour_leaves(clip):
        from tmi.formats import clip as C

        out = []
        for node in C.walk_layer_nodes(clip.root.children):
            info = None if node.folder else C.layer_offscreen_info(clip, node.render_mipmap)
            if info and info[2] in clip.extas and C.parse_offscreen_attribute(info[1]).channel_profile == "color-alpha-bgra":
                out.append(node)
        return out

    def _write(self, raw: bytes, main_id: int, edit) -> bytes:
        """``raw`` with layer ``main_id``'s pixels passed through ``edit`` (an
        in-place ImageDraw callback), via tmi's own write-back."""
        from PIL import ImageDraw

        from tmi.formats import clip as C
        from tmi.image_ops import image_to_png_bytes

        clip = C.open_clip_bytes(raw)
        node = next(n for n in C.walk_layer_nodes(clip.root.children) if n.main_id == main_id)
        image = C.render_layer_rgba(clip, node).image.to_pil()
        edit(ImageDraw.Draw(image), image)
        path = self.tmp / "w.clip"
        path.write_bytes(raw)
        return C.replace_layer_pixels(path, f"clip:{main_id}", image_to_png_bytes(image), invalidate_preview_on_noop=True)

    def _tmi_written(self, raw: bytes) -> bytes:
        """The file as tmi writes it: no CanvasPreview, pixels unchanged."""
        from tmi.formats import clip as C

        leaves = self._colour_leaves(C.open_clip_bytes(raw))
        out = self._write(raw, leaves[0].main_id, lambda draw, image: None)
        self.assertIsNone(C.render_canvas_preview_rgba(C.open_clip_bytes(out)))
        return out

    @staticmethod
    def _patch_layers(raw: bytes, updates: dict[int, dict]) -> bytes:
        """``raw`` with Layer columns rewritten (metadata only)."""
        from tmi.formats import clip as C

        clip = C.open_clip_bytes(raw)
        con = clip_io._clone_sqlite_connection(clip)
        for main_id, cols in updates.items():
            sets = ", ".join(f"{k} = ?" for k in cols)
            con.execute(f"UPDATE Layer SET {sets} WHERE MainId = ?", (*cols.values(), main_id))
        con.commit()
        container = C.parse_container(raw)
        payloads = {c.file_off: raw[c.payload_off:c.payload_off + c.payload_size] for c in container.chunks}
        return clip_io._emit_container_with_sqli_payload(container, payloads, clip_io._serialize_sqlite(con))

    @staticmethod
    def _effect_blob(edge_width: float | None = None, tone_lpi: float | None = None) -> bytes:
        def section(name: str) -> bytes:
            return struct.pack(">I", len(name)) + name.encode("utf-16-be")

        blob = b""
        if edge_width is not None:
            blob += section("EffectEdge") + struct.pack(">Id3I", 1, edge_width, 0xFFFFFFFF, 0x30000000, 0)
        if tone_lpi is not None:
            blob += section("EffectTone") + struct.pack(">Id3Id4I", 1, 72.0, 1, 0, 20, tone_lpi, 1, 0, 45, 30) + bytes(20)
        return blob

    def _base_png(self, base_raw: bytes) -> bytes:
        from tmi.formats import clip as C
        from tmi.formats import clip_partial as P

        cache = self.__dict__.setdefault("_base_pngs", {})
        if base_raw not in cache:
            cache.clear()
            cache[base_raw] = P.encode_layered_composite(*P.render_full(C.open_clip_bytes(base_raw)))
        return cache[base_raw]

    def _assert_incremental(
        self, base_raw: bytes, new_raw: bytes, *, expect_used: bool | None = True, label: str = ""
    ) -> bool:
        """Incremental == full render whenever the incremental path is taken;
        ``expect_used`` True / False also demands that it is / is not taken
        (None: either). Returns whether it was."""
        from tmi.formats import clip as C
        from tmi.formats import clip_partial as P

        base_png = self._base_png(base_raw)
        full, _is_base = P.render_full(C.open_clip_bytes(new_raw))
        new = C.open_clip_bytes(new_raw)
        image = P.render_incremental(new, P.CompositeBase(lambda: base_raw, lambda: base_png))
        if expect_used is False:
            self.assertIsNone(image, f"{label}: expected a full render")
        if expect_used:
            self.assertIsNotNone(image, f"{label}: incremental path not taken ({P.last_fallback})")
        if image is None:
            return False
        self.assertEqual(image.size, full.size)
        if image.tobytes() != full.tobytes():
            import numpy as np

            diff = np.argwhere(np.any(np.asarray(image) != np.asarray(full), axis=-1))
            self.fail(f"{label}: {len(diff)} pixels differ, first {diff[:5].tolist()}")
        return True

    # ── tests ──

    def test_region_render_is_the_crop_of_the_full_render(self) -> None:
        import random

        from tmi.formats import clip as C

        rng = random.Random(5)
        for path in self._examples(self.FILES + ["tama.clip"]):
            clip = C.open_clip(path)
            full = C.render_layered_composite_rgba(clip)
            w, h = full.size
            ids = frozenset(n.main_id for n in C.walk_layer_nodes(clip.root.children))
            # Thin strips cut through the middle of most layers, so a window
            # holds only part of a blended layer's bounding box.
            windows = [(0, 0, 1, 1), (w - 300, h - 1, w, h), (0, 0, w, h), (w // 2, 0, w // 2 + 3, h), (0, h // 2, w, h // 2 + 2)]
            for _ in range(3):
                ww, hh = rng.randint(1, w // 2), rng.randint(1, h // 2)
                x, y = rng.randint(0, w - ww), rng.randint(0, h - hh)
                windows.append((x, y, x + ww, y + hh))
            for k, window in enumerate(windows):
                # Odd windows decode just their tiles; even ones whole offscreens.
                clip.decodable_layers = ids if k % 2 else None
                part = C.render_layered_composite_rgba(clip, region=window)
                self.assertEqual(part.tobytes(), full.crop(window).tobytes(), f"{path.name} {window}")

    def _check_pixel_edits_equal_full_render(self, name: str) -> None:
        """Pixel edits take the incremental path — including ones that paint
        on an empty layer or across the edge of a layer that blends in
        another mode or below full opacity, so that its bounding box grows —
        and give exactly the whole-canvas render. One generated test per
        example file (``test_incremental_composite_after_pixel_edits_*``) so
        a parallel run spreads them over workers."""
        from tmi.formats import clip as C

        from tmi.formats import clip_partial as P

        paths = self._examples([name])
        if not paths:
            self.skipTest(f"example/{name} not present")
        used = total = 0
        fallbacks = []
        for path in paths:
            base = self._tmi_written(path.read_bytes())
            clip = C.open_clip_bytes(base)
            leaves = self._colour_leaves(clip)
            w, h = clip.canvas.width, clip.canvas.height
            first, last = leaves[0].main_id, leaves[-1].main_id
            blended = [n.main_id for n in leaves if (n.composite or 0) != 0 or (n.opacity or 0) < 256]

            def square(x, y, colour):
                return lambda draw, image: draw.rectangle((x, y, x + 40, y + 30), fill=colour)

            def erase(draw, image):
                box = image.getchannel("A").getbbox() or (0, 0, 64, 64)
                draw.rectangle((box[0], box[1], box[0] + 70, box[1] + 50), fill=(0, 0, 0, 0))

            def across_edge(draw, image):
                box = image.getchannel("A").getbbox() or (image.width // 2, image.height // 2) * 2
                draw.rectangle((max(0, box[0] - 20), max(0, box[1] - 15), box[0] + 20, box[1] + 15), fill=(30, 160, 90, 255))

            cases = {
                "square": self._write(base, first, square(w // 3, h // 3, (200, 40, 40, 255))),
                "translucent on the tile seam": self._write(base, last, square(250, 250, (40, 90, 220, 128))),
                "erase": self._write(base, last, erase),
                "canvas corner": self._write(base, first, square(w - 20, h - 10, (10, 200, 10, 255))),
            }
            if first != last:
                two = self._write(base, first, square(10, 10, (250, 250, 0, 255)))
                cases["two layers far apart"] = self._write(two, last, square(w - 60, h - 50, (0, 250, 250, 200)))
            if blended:
                cases["across a blended layer's edge"] = self._write(base, blended[0], across_edge)
            if w * h > 4_000_000:
                # A large canvas costs seconds per full render: three cases.
                keep = ("square", "two layers far apart", "across a blended layer's edge")
                cases = {k: cases[k] for k in keep if k in cases}
            for label, new in cases.items():
                if self._assert_incremental(base, new, expect_used=None, label=f"{path.name}: {label}"):
                    used += 1
                else:
                    fallbacks.append(f"{path.name}: {label}: {P.last_fallback}")
                total += 1
        self.assertEqual(fallbacks, [], f"incremental path taken for only {used} of {total} edits")

    def test_outline_and_tone_effects_rerender_around_an_edit(self) -> None:
        """フチ reads neighbouring pixels: the outline of an edit (and of the
        folder holding it) reaches past the changed tiles."""
        from tmi.formats import clip as C

        for path in self._examples(["csp515/structure.clip", "csp515/v2.clip", "csp515/mask_off.clip"]):
            raw = self._tmi_written(path.read_bytes())
            clip = C.open_clip_bytes(raw)
            columns = {row[1] for row in clip.db.execute("PRAGMA table_info(Layer)")}
            if not {"LayerEffectAttached", "LayerEffectInfo"} <= columns:
                continue
            leaf = self._colour_leaves(clip)[-1]
            parents = [n for n in C.walk_layer_nodes(clip.root.children) if n.folder and any(c.main_id == leaf.main_id for c in n.children)]
            updates = {leaf.main_id: {"LayerEffectAttached": 1, "LayerEffectInfo": self._effect_blob(edge_width=9.5, tone_lpi=10.0)}}
            for folder in parents:
                updates[folder.main_id] = {"LayerEffectAttached": 1, "LayerEffectInfo": self._effect_blob(edge_width=4.0)}
            base = self._patch_layers(raw, updates)
            # Paint right next to a tile seam so the outline crosses into
            # unchanged tiles.
            for x, y in ((250, 250), (512 - 3, 300)):
                new = self._write(base, leaf.main_id, lambda d, im, x=x, y=y: d.rectangle((x, y, x + 5, y + 4), fill=(0, 0, 0, 255)))
                self._assert_incremental(base, new, label=f"{path.name}: edge at {x},{y}")

    def test_property_and_structure_edits_render_the_whole_canvas(self) -> None:
        from tmi.formats import clip as C

        for path in self._examples(["csp515/structure.clip"]):
            base = self._tmi_written(path.read_bytes())
            clip = C.open_clip_bytes(base)
            leaf = self._colour_leaves(clip)[-1]
            visibility = clip.db.execute("SELECT LayerVisibility FROM Layer WHERE MainId = ?", (leaf.main_id,)).fetchone()[0]
            for label, cols in {
                "opacity": {"LayerOpacity": 100},
                "visibility": {"LayerVisibility": (visibility or 0) ^ 1},
                "blend mode": {"LayerComposite": 2},
            }.items():
                new = self._patch_layers(base, {leaf.main_id: cols})
                self._assert_incremental(base, new, expect_used=False, label=label)

    def test_base_composite_must_be_this_renderers_layered_render(self) -> None:
        from tmi.formats import clip as C
        from tmi.formats import clip_partial as P
        from tmi.image_ops import image_to_png_bytes

        for path in self._examples(["csp515/v2.clip"]):
            base = self._tmi_written(path.read_bytes())
            leaf = self._colour_leaves(C.open_clip_bytes(base))[0]
            new = self._write(base, leaf.main_id, lambda d, im: d.rectangle((5, 5, 20, 20), fill=(1, 2, 3, 255)))
            image = C.render_layered_composite_rgba(C.open_clip_bytes(base))
            for label, png in {
                "untagged (e.g. a CanvasPreview)": image_to_png_bytes(image),
                "another renderer version": image_to_png_bytes(
                    image, text=(P.COMPOSITE_TEXT_KEY, '{"renderer":"layered/v0"}')
                ),
            }.items():
                got = P.render_incremental(C.open_clip_bytes(new), P.CompositeBase(lambda: base, lambda png=png: png))
                self.assertIsNone(got, label)

    def test_composite_with_an_undecodable_layer_is_no_base(self) -> None:
        """Windows decode only some tiles, so a render in which a layer failed
        to decode (and dropped out) must not be built on."""
        from tmi.formats import clip as C
        from tmi.formats import clip_partial as P

        for path in self._examples(["csp515/v2.clip"]):
            raw = bytearray(self._tmi_written(path.read_bytes()))
            clip = C.open_clip_bytes(bytes(raw))
            leaf = self._colour_leaves(clip)[0]
            outer = C.layer_offscreen_info(clip, leaf.render_mipmap)[2]
            _header, inner, records = C._offscreen_tile_index(clip, outer)
            chunk = clip.extas[outer]
            data_start = chunk.payload_off + 56 + records[0][1] + 46 + 24 + 4  # past the zlib size prefix
            raw[data_start:data_start + 16] = bytes(16)
            broken = C.open_clip_bytes(bytes(raw))
            image, is_base = P.render_full(broken)
            self.assertFalse(is_base)
            self.assertIsNone(P.read_composite_meta(P.encode_layered_composite(image, is_base)))

    def test_verify_switch_raises_when_the_base_composite_is_wrong(self) -> None:
        from tmi.formats import clip as C
        from tmi.formats import clip_partial as P

        for path in self._examples(["csp515/v2.clip"]):
            base = self._tmi_written(path.read_bytes())
            leaf = self._colour_leaves(C.open_clip_bytes(base))[0]
            new = self._write(base, leaf.main_id, lambda d, im: d.rectangle((5, 5, 20, 20), fill=(1, 2, 3, 255)))
            stale, is_base = P.render_full(C.open_clip_bytes(base))
            stale = stale.to_pil()
            stale.putpixel((stale.width - 1, stale.height - 1), (0, 0, 0, 255))  # far from the edit
            base_obj = P.CompositeBase(lambda: base, lambda: P.encode_layered_composite(stale, is_base))
            old = os.environ.get("TMI_VERIFY_PARTIAL")
            os.environ["TMI_VERIFY_PARTIAL"] = "1"
            try:
                with self.assertRaises(RuntimeError):
                    C.render_composite_with_png(C.open_clip_bytes(new), base_obj)
            finally:
                if old is None:
                    os.environ.pop("TMI_VERIFY_PARTIAL", None)
                else:
                    os.environ["TMI_VERIFY_PARTIAL"] = old

    def test_commit_and_merge_build_on_the_previous_composite(self) -> None:
        """End to end: a commit -a after a tmi-written layer edit and a merge
        of two branches editing different layers both take the incremental
        path, and store exactly the whole-canvas render."""
        from unittest import mock

        from tmi.formats import clip as C
        from tmi.formats import clip_partial as P
        from tmi.image_ops import image_from_bytes
        from tmi.repo import Repo

        paths = self._examples(["csp515/structure.clip"])
        repo_root = self.tmp / "repo"
        repo_root.mkdir()
        repo = Repo.create(repo_root)
        target = repo_root / "art.clip"
        base = self._tmi_written(paths[0].read_bytes())
        leaves = self._colour_leaves(C.open_clip_bytes(base))
        self.assertGreaterEqual(len(leaves), 2)
        a, b = leaves[0].main_id, leaves[-1].main_id
        target.write_bytes(base)

        used: list[bool] = []
        real = P.render_incremental

        def spy(*args, **kwargs):
            image = real(*args, **kwargs)
            used.append(image is not None)
            return image

        def check_head_composite() -> None:
            entry = repo.entries_for_commit(repo.current_commit())["art.clip"]
            stored = image_from_bytes(repo.store.read_blob(entry["composite_ref"]))
            source = repo.store.read_blob(entry["source_blob_ref"])
            full = C.render_layered_composite_rgba(C.open_clip_bytes(source))
            self.assertEqual(stored.tobytes(), full.tobytes())

        with mock.patch.object(P, "render_incremental", spy):
            repo.stage_file(str(target))
            repo.commit("base")
            repo.create_branch("feature")
            target.write_bytes(self._write(target.read_bytes(), b, lambda d, im: d.rectangle((300, 200, 340, 260), fill=(9, 9, 200, 255))))
            repo.stage_all_modified()
            repo.commit("main edit")
            self.assertEqual(used[-1:], [True], "commit -a did not build on HEAD's composite")
            check_head_composite()
            repo.switch("feature")
            target.write_bytes(self._write(target.read_bytes(), a, lambda d, im: d.rectangle((900, 700, 960, 720), fill=(200, 9, 9, 255))))
            repo.stage_all_modified()
            repo.commit("feature edit")
            repo.switch("main")
            used.clear()
            result = repo.merge("feature")
            self.assertEqual(result["mode"], "merge")
            self.assertIn(True, used, "merge did not build on the start side's composite")
            check_head_composite()


class BandedCompositeTests(unittest.TestCase):
    """A large whole-canvas composite is rendered as bands (tmi_accel; the
    reference ``tmiref.clip._render_banded`` on a thread pool), and
    generated layers (vectors, materials, comic frames) are drawn once per
    render or per window. Every check is byte equality with the reference's
    one-pass render (or Pillow): its bands and windows, tmi's whole render
    and windows. Example files are not in git; missing ones skip."""

    FILES = IncrementalCompositeTests.FILES + [
        "vector_simple.clip",
        "csp515/adjust_tc.clip",
        "csp515/tone_lpi10_line.clip",
        "additional_examples/probe_material_madara_color.clip",
        "additional_examples/probe_material_interp.clip",
        "additional_examples/probe_material_scale.clip",
        "additional_examples/probe_material_greyflash_mono.clip",
        "additional_examples/probe_vector_mouse.clip",
        "additional_examples/probe_vector_thin.clip",
    ]

    def _examples(self, names: list[str]) -> list[Path]:
        paths = [PROJECT_ROOT / "example" / name for name in names]
        paths = [p for p in paths if p.exists()]
        if not paths:
            self.skipTest("no example CLIP files")
        return paths

    @staticmethod
    def _one_pass(clip):
        """The reference renderer's one-pass render of ``clip``."""
        from unittest import mock

        from tmiref import clip as R

        with mock.patch.object(R, "_BAND_MIN_PIXELS", 1 << 62):
            return R.render_layered_composite_rgba(clip)

    @staticmethod
    def _bands(clip, workers: int = 4):
        """The reference's banded render of ``clip`` (None if it declined),
        with bands of 256 rows even on the small example canvases."""
        from unittest import mock

        from tmiref import clip as R

        with mock.patch.object(R, "_BAND_MIN_PIXELS", 0), mock.patch.object(R, "_BAND_TARGET_PIXELS", 1), \
                mock.patch.object(R, "_BAND_WORKERS", workers):
            return R._render_banded(clip, (255, 255, 255, 255))

    def _check_bands(self, name: str) -> None:
        import random

        from tmi.formats import clip as C
        from tmiref import clip as R

        for path in self._examples([name]):
            one = self._one_pass(C.open_clip(path))
            for workers in (4, 1):
                banded = self._bands(C.open_clip(path), workers)
                self.assertIsNotNone(banded, f"{path.name}: bands declined")
                self.assertEqual(banded.tobytes(), one.tobytes(), f"{path.name}: {workers} worker(s)")
            self.assertEqual(C.render_layered_composite_rgba(C.open_clip(path)).tobytes(), one.tobytes(), path.name)
            # Windows: tmi's, in one call and one by one, and the reference's
            # in a render session (vector strokes drawn per window, materials
            # resampled per window, frame shapes shared).
            clip = C.open_clip(path)
            w, h = one.size
            rng = random.Random(len(name))
            windows = [(0, 0, w, 1), (w // 2, 0, w // 2 + 1, h)]
            for _ in range(4):
                ww, hh = rng.randint(1, w // 2), rng.randint(1, h // 2)
                x, y = rng.randint(0, w - ww), rng.randint(0, h - hh)
                windows.append((x, y, x + ww, y + hh))
            for window, part in zip(windows, C.render_layered_regions(clip, windows)):
                self.assertEqual(part.tobytes(), one.crop(window).tobytes(), f"{path.name} {window}")
            for window in windows[:3]:
                part = C.render_layered_composite_rgba(clip, region=window)
                self.assertEqual(part.tobytes(), one.crop(window).tobytes(), f"{path.name} {window}")
            with R.render_session(clip):
                for window in windows:
                    part = R.render_layered_composite_rgba(clip, region=window)
                    self.assertEqual(part.tobytes(), one.crop(window).tobytes(), f"reference {path.name} {window}")

    def test_banded_render_equals_one_pass_tama(self) -> None:
        from unittest import mock

        from tmi.formats import clip as C
        from tmiref import clip as R

        for path in self._examples(["tama.clip"]):
            one = self._one_pass(C.open_clip(path))
            # The default layout (16 bands of 256 rows on 8 workers here).
            with mock.patch.object(R, "_BAND_MIN_PIXELS", 0):
                banded = R._render_banded(C.open_clip(path), (255, 255, 255, 255))
            self.assertIsNotNone(banded)
            self.assertEqual(banded.tobytes(), one.tobytes())
            # tmi's render (tmi_accel bands tama.clip too).
            self.assertEqual(C.render_layered_composite_rgba(C.open_clip(path)).tobytes(), one.tobytes())

    def test_banded_render_falls_back_when_a_layer_does_not_decode(self) -> None:
        """A layer whose stored pixels fail to decode drops out of the one-pass
        render whole; the reference's bands then start over in one pass (a
        band decodes only its tiles), including when the broken tile lies off
        the canvas, where no band looks; tmi's render (and its decode
        failures, ``render_full``) agrees. (tmi_accel's own bands: the damaged
        tama.clip of tests/test_render_parity.py.)"""
        from tmi.formats import clip as C
        from tmi.formats import clip_partial as P

        inc = IncrementalCompositeTests()
        inc.tmp = Path(tempfile.mkdtemp(prefix="tmi-band-", dir=PROJECT_ROOT / ".test-tmp")) \
            if (PROJECT_ROOT / ".test-tmp").exists() else Path(tempfile.mkdtemp(prefix="tmi-band-"))
        try:
            for path in self._examples(["csp515/v2.clip"]):
                raw = bytearray(inc._tmi_written(path.read_bytes()))
                clip = C.open_clip_bytes(bytes(raw))
                leaf = inc._colour_leaves(clip)[0]
                outer = C.layer_offscreen_info(clip, leaf.render_mipmap)[2]
                _header, _inner, records = C._offscreen_tile_index(clip, outer)
                chunk = clip.extas[outer]
                first_block = records[0][0]
                data_start = chunk.payload_off + 56 + records[0][1] + 46 + 24 + 4
                raw[data_start:data_start + 16] = bytes(16)
                broken = bytes(raw)
                one = self._one_pass(C.open_clip_bytes(broken))
                self.assertIsNone(self._bands(C.open_clip_bytes(broken)))
                image, is_base = P.render_full(C.open_clip_bytes(broken))
                self.assertFalse(is_base)
                self.assertEqual(image.tobytes(), one.tobytes())
                # Move the broken tile off the canvas.
                cols = C.parse_offscreen_attribute(C.layer_offscreen_info(clip, leaf.render_mipmap)[1]).tile_cols
                tile_y = (first_block // cols) * C.TILE_SIZE
                shifted = inc._patch_layers(broken, {leaf.main_id: {"LayerOffsetY": -(tile_y + C.TILE_SIZE)}})
                one = self._one_pass(C.open_clip_bytes(shifted))
                self.assertIsNone(self._bands(C.open_clip_bytes(shifted)))
                self.assertEqual(C.render_layered_composite_rgba(C.open_clip_bytes(shifted)).tobytes(), one.tobytes())
        finally:
            shutil.rmtree(inc.tmp, ignore_errors=True)

    def test_composite_ops_over_is_pillow_alpha_composite_for_every_alpha_pair(self) -> None:
        """tmi_accel's compositing kernel (what render_clip blends runs of
        layers with) is Pillow's alpha_composite for every alpha pair."""
        import numpy as np

        from tmi import accel

        composite_ops = accel.native().composite_ops
        # Pixel i: source alpha i >> 16, backdrop alpha (i >> 8) & 255, and
        # the source channels running through all 256 values against
        # backdrop values that vary with them.
        i = np.arange(1 << 24, dtype=np.uint32)
        src = np.empty((4096, 4096, 4), np.uint8)
        dst = np.empty_like(src)
        s, d = src.reshape(-1, 4), dst.reshape(-1, 4)
        sa, da, sc = i >> 16, (i >> 8) & 255, i & 255
        s[:, 0], s[:, 1], s[:, 2], s[:, 3] = sc, 255 - sc, (sc * 7) & 255, sa
        d[:, 0], d[:, 1], d[:, 2], d[:, 3] = (sc * 13 + da) & 255, (sc * 5 + 3) & 255, da ^ sc, da
        del i, sa, da, sc
        ref = Image.alpha_composite(Image.fromarray(dst, "RGBA"), Image.fromarray(src, "RGBA")).tobytes()
        out = composite_ops(dst.tobytes(), 4096, 4096, [(0, src.tobytes(), 4096, 4096, 0, 0, 4096, 4096, 0, 0, None, 1.0)])
        self.assertTrue(out == ref)
        # CLEAR is what re-compositing onto a transparent canvas does.
        clear = composite_ops(dst.tobytes(), 4096, 4096, [(2, None, 0, 0, 0, 0, 4096, 4096, 0, 0, None, 1.0)])
        ref = Image.alpha_composite(Image.new("RGBA", (4096, 4096)), Image.fromarray(dst, "RGBA")).tobytes()
        self.assertTrue(clear == ref)

    def test_opaque_alpha_composite_is_a_paste(self) -> None:
        """Pillow's alpha_composite returns a source pixel of alpha 255
        as it is over any backdrop, so ``_blend_onto_at`` (tmi's and the
        reference's) pastes opaque sources."""
        import numpy as np

        from tmiref import composite
        from tmiref import clip as R

        rng = np.random.default_rng(4)
        v = np.arange(256, dtype=np.uint8)
        src = np.empty((256, 256, 4), np.uint8)
        src[..., 0], src[..., 1], src[..., 2], src[..., 3] = v[None, :], v[:, None], 255 - v[None, :], 255
        dst = rng.integers(0, 256, size=(256, 256, 4), dtype=np.uint8)
        dst[..., 3] = v[:, None]
        out = Image.alpha_composite(Image.fromarray(dst, "RGBA"), Image.fromarray(src, "RGBA"))
        self.assertEqual(out.tobytes(), src.tobytes())
        # And through _blend_onto_at, with and without the shortcut.
        reference = Image.fromarray(dst, "RGBA")
        reference.alpha_composite(Image.fromarray(src[:100, :90], "RGBA"), (20, 30))
        for impl in (composite, R):
            target = Image.fromarray(dst, "RGBA")
            impl._blend_onto_at(target, Image.fromarray(src[:100, :90], "RGBA"), "normal", 1.0, (20, 30))
            self.assertEqual(target.tobytes(), reference.tobytes(), impl.__name__)

    def test_layer_style_per_distinct_colour_matches_per_pixel(self) -> None:
        """Expression / layer colour is evaluated once per distinct colour on
        large layers; the bytes equal running it over every pixel."""
        from unittest import mock

        import numpy as np

        from tmiref import clip as C
        from tmiref import clip_style

        rng = np.random.default_rng(31)
        few = rng.integers(0, 256, size=(61, 4), dtype=np.uint8)[rng.integers(0, 61, size=(40, 70))]
        many = rng.integers(0, 256, size=(40, 70, 4), dtype=np.uint8)
        grey = np.repeat(rng.integers(0, 256, size=(40, 70, 1), dtype=np.uint8), 4, axis=2)
        lc = {"DrawColorEnable": 1, "DrawColorMainRed": 0x80000000, "DrawColorMainGreen": 0x20000000,
              "DrawColorMainBlue": 0xF0000000, "DrawColorSubRed": 0xFFFFFFFF, "DrawColorSubGreen": 0x40000000,
              "DrawColorSubBlue": 0}
        styles = []
        for ct in (0, 1, 2):
            for black, white in ((0, 0), (1, 0), (0, 1), (1, 1)):
                for colour in ({}, lc):
                    styles.append({"LayerColorTypeIndex": ct, "LayerColorTypeBlackChecked": black,
                                   "LayerColorTypeWhiteChecked": white, **colour})
        for name, px in (("few", few), ("many", many), ("grey", grey)):
            for style in styles:
                for profile in ("color-alpha-bgra", "gray-alpha", "single-channel", None):
                    results = []
                    for threshold in (0, 1 << 40):
                        with mock.patch.object(C, "_DISTINCT_COLOUR_MIN_PIXELS", threshold):
                            out = clip_style.apply_layer_style(Image.fromarray(px, "RGBA"), style, profile)
                        results.append(out.tobytes())
                    self.assertEqual(results[0], results[1], f"{name} {style} {profile}")

    def test_mask_plane_tiles_match_the_whole_plane(self) -> None:
        """Masks are kept as their painted tiles (``_MaskPlane``); any crop of
        it equals the crop of the plane built whole as before."""
        import random

        from tmiref import clip as C

        checked = 0
        rng = random.Random(3)
        for path in self._examples(["csp515/mask_off.clip", "csp515/structure.clip", "layer_types.clip", "tama.clip"]):
            clip = C.open_clip(path)
            for node in C.walk_layer_nodes(clip.root.children):
                info = C.layer_offscreen_info(clip, node.mask_mipmap) if node.mask_mipmap else None
                if not info or info[2] not in clip.extas:
                    continue
                attr = C.parse_offscreen_attribute(info[1])
                init = C._mask_init_alpha(attr)
                whole = Image.new("L", (attr.tile_cols * C.TILE_SIZE, attr.tile_rows * C.TILE_SIZE), init)
                for tile in C.iter_tiles(clip.raw, clip.extas[info[2]]):
                    if tile.data is not None:
                        col, row = tile.block_index % attr.tile_cols, tile.block_index // attr.tile_cols
                        whole.paste(C._tile_to_image(tile).getchannel("A"), (col * C.TILE_SIZE, row * C.TILE_SIZE))
                whole = whole.crop((0, 0, attr.pixel_width, attr.pixel_height))
                plane, got_init = C._render_mask_attenuation(clip, info)
                self.assertEqual((plane.size, got_init), (whole.size, init))
                boxes = [(0, 0, plane.width, plane.height)]
                for _ in range(5):
                    x0, y0 = rng.randrange(plane.width), rng.randrange(plane.height)
                    boxes.append((x0, y0, rng.randint(x0 + 1, plane.width), rng.randint(y0 + 1, plane.height)))
                for box in boxes:
                    self.assertEqual(plane.crop(box).tobytes(), whole.crop(box).tobytes(), f"{path.name} {box}")
                checked += 1
        if not checked:
            self.skipTest("no layer masks in the example files")

    def test_tile_alpha_matches_tile_to_image(self) -> None:
        from types import SimpleNamespace

        from tmiref import clip as C

        rng = __import__("random").Random(8)
        for w, h in ((256, 256), (64, 40)):
            n = w * h
            for bpt in (n // 8, n // 4, n, 2 * n, 5 * n):
                data = bytes(rng.randrange(256) for _ in range(bpt))
                tile = SimpleNamespace(tile_w=w, tile_h=h, bytes_per_tile=bpt, data=data)
                self.assertEqual(C._tile_alpha(tile).tobytes(), C._tile_to_image(tile).getchannel("A").tobytes(), (w, h, bpt))

    def test_render_memo_computes_each_key_once(self) -> None:
        import threading
        import time

        from tmiref import clip as C

        memo = C._RenderMemo(banded=True)
        calls = []

        def slow():
            calls.append(1)
            time.sleep(0.05)
            return object()

        def failing():
            calls.append(2)
            raise ValueError("boom")

        results, errors = [], []

        def worker():
            results.append(memo.get("a", slow))
            try:
                memo.get("b", failing)
            except ValueError as exc:
                errors.append(exc)

        threads = [threading.Thread(target=worker) for _ in range(8)]
        for t in threads:
            t.start()
        for t in threads:
            t.join()
        self.assertEqual(sorted(calls), [1, 2])
        self.assertEqual(len({id(r) for r in results}), 1)
        self.assertEqual(len(errors), 8)
        self.assertEqual(len({id(e) for e in errors}), 1)

    def test_rendered_images_are_read_only_views(self) -> None:
        """A render hands out a ``tmi.raster.Rgba`` over tmi_accel's bytes
        (no copy). Those are immutable: an edit of a PIL copy, or of the
        pixel-sharing twin of one (image_ops.share_pixels), never reaches
        them."""
        from tmi.formats import clip as C
        from tmi.image_ops import share_pixels
        from tmi.raster import Rgba

        raw = bytes(range(256)) * 4
        image = C._rgba_image(raw, (16, 16))
        self.assertIsInstance(image, Rgba)
        self.assertIs(raw, image.tobytes())
        pil = image.to_pil()
        pil.paste((1, 2, 3, 4), (0, 0, 4, 4))
        self.assertEqual((1, 2, 3, 4), pil.getpixel((0, 0)))
        twin = share_pixels(pil)
        twin.paste((9, 9, 9, 9), (0, 0, 16, 16))
        self.assertEqual((0, 1, 2, 3), image.getpixel((0, 0)))
        self.assertEqual(raw, bytes(range(256)) * 4)


def _add_banded_tests() -> None:
    for name in BandedCompositeTests.FILES:
        slug = re.sub(r"[^0-9A-Za-z]+", "_", Path(name).stem).strip("_").lower()

        def test(self: BandedCompositeTests, name: str = name) -> None:
            self._check_bands(name)

        test.__name__ = f"test_banded_render_and_windows_equal_one_pass_{slug}"
        test.__doc__ = f"example/{name}: bands and windows equal the one-pass render."
        setattr(BandedCompositeTests, test.__name__, test)


_add_banded_tests()


def _add_pixel_edit_tests() -> None:
    for name in IncrementalCompositeTests.FILES + ["tama.clip"]:
        slug = re.sub(r"[^0-9A-Za-z]+", "_", Path(name).stem).strip("_").lower()

        def test(self: IncrementalCompositeTests, name: str = name) -> None:
            self._check_pixel_edits_equal_full_render(name)

        test.__name__ = f"test_incremental_composite_after_pixel_edits_equals_full_render_{slug}"
        test.__doc__ = f"Pixel edits on example/{name} render incrementally and equal the full render."
        setattr(IncrementalCompositeTests, test.__name__, test)


_add_pixel_edit_tests()


# Run one CLI command in a fresh interpreter, then record which modules it loaded.
_IMPORT_PROBE = """
import json, sys
out, args = sys.argv[1], sys.argv[2:]
from tmi.cli import main
try:
    code = main(args)
except SystemExit as exc:
    code = exc.code
sys.stdout.flush()
with open(out, "w", encoding="utf-8") as f:
    json.dump({"code": code, "modules": sorted(sys.modules)}, f)
"""

# Pixel, format and transport code: commands that only read refs, the index or
# commit metadata must start without it (numpy + Pillow alone cost ~160 ms).
_HEAVY_MODULES = (
    "numpy",
    "PIL",
    "tmi_accel",
    "zstandard",
    "tmi.accel",
    "tmi.chunking",
    "tmi.diff",
    "tmi.formats",
    "tmi.image_ops",
    "tmi.importers",
    "tmi.merge",
    "tmi.native",
    "tmi.transport",
    "urllib.request",
)


class GeneratedFrontDoorTablesTests(unittest.TestCase):
    """The Rust front door prints argparse's -h texts and casefolds --author
    from tables generated from this Python (RUST_MIGRATION.md 3b-1); they
    must match tmi/cli.py as it is now."""

    def _recorded_version(self, rel: str) -> str:
        text = (PROJECT_ROOT / rel).read_text(encoding="utf-8")
        match = re.search(r'PYTHON_VERSION: &str = "([0-9.]+)"', text)
        assert match, rel
        return match.group(1)

    def test_help_text_is_current(self) -> None:
        version = self._recorded_version("rust/crates/tmi-cli/src/help_text.rs")
        if version != f"{sys.version_info.major}.{sys.version_info.minor}":
            self.skipTest(f"help_text.rs is generated with Python {version}")
        result = subprocess.run(
            [sys.executable, str(PROJECT_ROOT / "tools" / "gen_cli_help.py"), "--check"],
            capture_output=True, text=True, timeout=120,
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_cp932_table_is_current(self) -> None:
        result = subprocess.run(
            [sys.executable, str(PROJECT_ROOT / "tools" / "gen_cp932.py"), "--check"],
            capture_output=True, text=True, timeout=120,
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_diff_html_template_is_current(self) -> None:
        # The Rust diff --html (RUST_MIGRATION.md 4a) writes tmi/diff_html.py's
        # template text from rust/crates/tmi-diffmerge/src/html_text.rs.
        result = subprocess.run(
            [sys.executable, str(PROJECT_ROOT / "tools" / "gen_diff_html.py"), "--check"],
            capture_output=True, text=True, timeout=120,
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_casefold_table(self) -> None:
        version = self._recorded_version("rust/crates/tmi-cli/src/casefold_table.rs")
        if version != f"{sys.version_info.major}.{sys.version_info.minor}":
            self.skipTest(f"casefold_table.rs is generated with Python {version}")
        text = (PROJECT_ROOT / "rust/crates/tmi-cli/src/casefold_table.rs").read_text(encoding="utf-8")
        rows = re.findall(r'\(0x([0-9a-f]+), "((?:\\u\{[0-9a-f]+\})+)"\)', text)
        table = {int(cp, 16): "".join(chr(int(h, 16)) for h in re.findall(r"\\u\{([0-9a-f]+)\}", s)) for cp, s in rows}
        for cp in range(0x110000):
            if 0xD800 <= cp <= 0xDFFF:
                continue
            ch = chr(cp)
            self.assertEqual(table.get(cp, ch), ch.casefold(), hex(cp))


class LightStartupTests(unittest.TestCase):
    """Commands that never touch pixels must not import numpy, Pillow,
    tmi_accel or the format parsers."""

    @classmethod
    def setUpClass(cls) -> None:
        root = PROJECT_ROOT / ".test-tmp"
        root.mkdir(exist_ok=True)
        cls.tmp = Path(tempfile.mkdtemp(prefix="tmi-startup-", dir=root))
        cls.env = os.environ.copy()
        cls.env["PYTHONPATH"] = str(PROJECT_ROOT) + os.pathsep + cls.env.get("PYTHONPATH", "")
        cls.env["TMI_AUTHOR_NAME"] = "Tester"
        cls.env["TMI_AUTHOR_EMAIL"] = "tester@example.com"
        cls.env.pop("TMI_PROGRESS", None)
        cls.repo = cls.tmp / "repo"
        cls.repo.mkdir()
        write_png(cls.repo / "a.png", (255, 0, 0, 255))
        write_mdp2_layers(cls.repo / "b.mdp", [(0, 0, 255, 255), (0, 255, 0, 128)])
        for args in (["init"], ["add", "."], ["commit", "-m", "first"], ["tag", "v1"], ["status"]):
            subprocess.run(
                [sys.executable, "-m", "tmi", *args],
                cwd=cls.repo, env=cls.env, check=True, capture_output=True, timeout=60,
            )

    @classmethod
    def tearDownClass(cls) -> None:
        shutil.rmtree(cls.tmp, ignore_errors=True)
        try:
            (PROJECT_ROOT / ".test-tmp").rmdir()
        except OSError:
            pass

    def _probe(self, *args: str) -> tuple[str, list[str]]:
        import json

        out = self.tmp / "modules.json"
        result = subprocess.run(
            [sys.executable, "-c", _IMPORT_PROBE, str(out), *args],
            cwd=self.repo, env=self.env, text=True, capture_output=True, timeout=60,
        )
        self.assertEqual(0, result.returncode, result.stderr)
        report = json.loads(out.read_text(encoding="utf-8"))
        self.assertIn(report["code"], (0, None), f"tmi {' '.join(args)}\n{result.stdout}\n{result.stderr}")
        heavy = [
            name for name in report["modules"]
            if any(name == mod or name.startswith(mod + ".") for mod in _HEAVY_MODULES)
        ]
        return result.stdout, heavy

    def test_metadata_commands_do_not_import_pixel_code(self) -> None:
        from tmi import __version__

        cases = [
            (["--help"], "usage: tmiki"),
            (["--version"], f"tmiki {__version__}\ntmi_accel "),
            (["status"], "working tree clean"),
            (["status", "--short"], ""),
            (["log"], "first"),
            (["log", "--oneline"], "first"),
            (["branch"], "* main"),
            (["tag"], "v1"),
            (["show", "HEAD"], "first"),
            (["ls-layers", "b.mdp"], ""),
            (["config", "--list"], ""),
            (["reflog"], "commit"),
            (["hook", "list"], ""),
        ]
        for args, expected in cases:
            with self.subTest(command=" ".join(args)):
                stdout, heavy = self._probe(*args)
                self.assertIn(expected, stdout)
                self.assertEqual([], heavy)

    def test_diff_that_finds_no_change_does_not_import_pixel_code(self) -> None:
        # tmi.diff itself loads; Pillow, image_ops and the importers only
        # once a changed file is compared.
        cases = [
            (["diff"], "no differences"),
            (["diff", "v1"], "no differences"),
            (["diff", "--stat", "v1", "HEAD"], "no differences"),
            (["diff", "--html", "--no-open", "v1"], "no differences"),
            (["diff", "--name-only", "v1", "HEAD"], ""),
        ]
        for args, expected in cases:
            with self.subTest(command=" ".join(args)):
                stdout, heavy = self._probe(*args)
                self.assertIn(expected, stdout)
                self.assertEqual(["tmi.diff"], heavy)


# What a command on PNG / JPEG files must not load since stage 2b: the pixels
# are decoded, compared, previewed and written back by tmi_accel.
_PILLOW_FREE = ("PIL", "numpy", "zstandard", "tmi.formats")


class PillowFreeRasterTests(unittest.TestCase):
    """PNG / JPEG files go through tmi_accel (tmi.raster, RUST_MIGRATION.md
    2b): add, status, diff (files, commits, --html), restore (export and
    in-place write-back) and merge (write-back) import neither Pillow nor
    numpy, nor the CLIP / MDP2 readers."""

    def setUp(self) -> None:
        root = PROJECT_ROOT / ".test-tmp"
        root.mkdir(exist_ok=True)
        self.tmp = Path(tempfile.mkdtemp(prefix="tmi-pillowfree-", dir=root))
        self.env = os.environ.copy()
        self.env["PYTHONPATH"] = str(PROJECT_ROOT) + os.pathsep + self.env.get("PYTHONPATH", "")
        self.env["TMI_AUTHOR_NAME"] = "Tester"
        self.env["TMI_AUTHOR_EMAIL"] = "tester@example.com"
        self.env.pop("TMI_PROGRESS", None)
        self.repo = self.tmp / "repo"
        self.repo.mkdir()

    def tearDown(self) -> None:
        shutil.rmtree(self.tmp, ignore_errors=True)
        try:
            (PROJECT_ROOT / ".test-tmp").rmdir()
        except OSError:
            pass

    def tmi(self, *args: str) -> None:
        subprocess.run([sys.executable, "-m", "tmi", *args], cwd=self.repo, env=self.env, check=True,
                       capture_output=True, timeout=120)

    def probe(self, *args: str) -> str:
        import json

        out = self.tmp / "modules.json"
        result = subprocess.run(
            [sys.executable, "-c", _IMPORT_PROBE, str(out), *args],
            cwd=self.repo, env=self.env, text=True, capture_output=True, timeout=120,
        )
        self.assertEqual(0, result.returncode, result.stderr)
        report = json.loads(out.read_text(encoding="utf-8"))
        self.assertIn(report["code"], (0, None), f"tmi {' '.join(args)}\n{result.stdout}\n{result.stderr}")
        heavy = sorted({name.split(".")[0] if not name.startswith("tmi.") else name
                        for name in report["modules"]
                        if any(name == mod or name.startswith(mod + ".") for mod in _PILLOW_FREE)})
        self.assertEqual([], heavy, f"tmi {' '.join(args)}")
        return result.stdout

    def _edit(self, name: str, colour, box, **save) -> None:
        image = Image.open(self.repo / name)
        image.load()
        image = image.convert(image.mode)
        image.paste(colour, box)
        image.save(self.repo / name, **save)

    def test_raster_commands_load_no_pillow(self) -> None:
        icc = _srgb_icc()  # (it carries its creation time)
        Image.new("RGB", (64, 48), (200, 220, 240)).save(self.repo / "a.png", dpi=(350, 350), icc_profile=icc)
        Image.new("RGB", (64, 48), (90, 120, 30)).save(self.repo / "b.jpg", quality=85, dpi=(300, 300))
        self.tmi("init")
        self.assertIn("staged a.png", self.probe("add", "a.png", "b.jpg"))
        self.tmi("commit", "-m", "v1")
        self.tmi("tag", "v1")
        self._edit("a.png", (255, 0, 0), (0, 0, 16, 16), dpi=(350, 350))
        self._edit("b.jpg", (0, 0, 255), (8, 8, 30, 30), quality=85)
        self.assertIn("modified", self.probe("status"))
        self.assertIn("bbox=[0, 0, 16, 16]", self.probe("diff", "a.png"))
        self.probe("diff", "--html", "--no-open")
        self.probe("diff", "--stat")
        self.probe("add", "a.png", "b.jpg")
        self.tmi("commit", "-m", "v2")
        self.probe("diff", "v1", "HEAD")
        self.probe("diff", "--html", "--no-open", "v1", "HEAD")
        self.probe("restore", "--source", "v1", "--output", str(self.tmp / "out.png"), "a.png:a")
        self.assertEqual((200, 220, 240), Image.open(self.tmp / "out.png").convert("RGB").getpixel((0, 0)))
        self.probe("restore", "--in-place", "--source", "v1", "b.jpg:b")
        self.probe("restore", "--in-place", "--source", "v1", "a.png:a")
        restored = Image.open(self.repo / "a.png")
        self.assertEqual(("RGB", (200, 220, 240), icc), (restored.mode, restored.getpixel((0, 0)), restored.info.get("icc_profile")))
        self.tmi("restore", "--source", "HEAD", "a.png", "b.jpg")
        # A merge that writes the other side's pixels into ours.
        self.tmi("branch", "other")
        self._edit("a.png", (0, 255, 0), (40, 30, 64, 48), dpi=(350, 350))
        self.tmi("commit", "-a", "-m", "ours")
        self.tmi("switch", "other")
        self._edit("a.png", (0, 0, 0), (20, 0, 30, 10), dpi=(350, 350))
        self.tmi("commit", "-a", "-m", "theirs")
        self.tmi("switch", "main")
        self.assertIn("Merged other", self.probe("merge", "other"))
        merged = Image.open(self.repo / "a.png").convert("RGB")
        self.assertEqual([(0, 255, 0), (0, 0, 0), (255, 0, 0)], [merged.getpixel(p) for p in ((50, 40), (25, 5), (5, 5))])

    def test_mdp2_commands_load_no_pillow(self) -> None:
        """MDP2 files (RUST_MIGRATION.md 2c): the reader, layer decode,
        composite and layer write-back run in tmi_accel, so add, status,
        diff, restore (layer write-back) and merge load neither Pillow nor
        numpy, nor the CLIP reader."""
        global _PILLOW_FREE
        saved = _PILLOW_FREE
        _PILLOW_FREE = ("PIL", "numpy", "zstandard", "tmi.formats.clip", "tmi.formats.composite")
        try:
            red, green, blue, yellow = (255, 0, 0, 255), (0, 255, 0, 255), (0, 0, 255, 255), (255, 255, 0, 255)
            art = self.repo / "art.mdp"
            write_mdp2_layers(art, [red, green])
            self.tmi("init")
            self.assertIn("staged art.mdp", self.probe("add", "art.mdp"))
            self.tmi("commit", "-m", "v1")
            self.tmi("tag", "v1")
            write_mdp2_layers(art, [blue, green])
            self.assertIn("modified", self.probe("status"))
            self.probe("diff", "art.mdp")
            self.probe("diff", "--html", "--no-open")
            self.probe("commit", "-a", "-m", "v2")
            self.probe("restore", "--in-place", "--source", "v1", "art.mdp:layer1")
            self.tmi("restore", "--source", "HEAD", "art.mdp")
            self.tmi("branch", "other")
            write_mdp2_layers(art, [blue, yellow])
            self.tmi("commit", "-a", "-m", "ours")
            self.tmi("switch", "other")
            write_mdp2_layers(art, [red, green])
            self.tmi("commit", "-a", "-m", "theirs")
            self.tmi("switch", "main")
            self.probe("merge", "other")
        finally:
            _PILLOW_FREE = saved

    def test_without_pillow_the_verdict_is_the_same(self) -> None:
        """Whether a file is an image or an opaque blob is tmi's own rule
        (PLUGINS.md P0e: tmi_accel's decoders read it or not), so it is the
        same with Pillow blocked as with Pillow installed. A GIF is an
        image; what Pillow reads but tmi does not (a TGA, a TIFF in LAB, a
        TIFF in YCbCr) is an opaque blob in both, and so is a PNG that does
        not decode."""
        write_png(self.repo / "a.png", (255, 0, 0, 255))
        write_mdp2_layers(self.repo / "b.mdp", [(0, 0, 255, 255)])
        Image.new("RGB", (8, 8), (1, 2, 3)).save(self.repo / "c.gif")
        Image.new("RGB", (8, 8), (4, 5, 6)).save(self.repo / "d.tga")
        Image.new("LAB", (8, 8)).save(self.repo / "e.tif")
        Image.new("RGB", (8, 8), (7, 8, 9)).save(self.repo / "f.tif", compression="jpeg")
        Image.new("RGB", (8, 8), (1, 1, 1)).save(self.repo / "noext", format="BMP")
        (self.repo / "broken.png").write_bytes((self.repo / "a.png").read_bytes()[:40])
        names = ["a.png", "b.mdp", "c.gif", "d.tga", "e.tif", "f.tif", "noext", "broken.png"]
        for name in ("d.tga", "e.tif", "f.tif"):  # Pillow does read these
            Image.open(self.repo / name).load()
        self.tmi("init")

        def verdicts(block: str) -> dict:
            import json

            result = subprocess.run([sys.executable, "-c", _VERDICT_PROBE, block, *names], cwd=self.repo, env=self.env,
                                    text=True, capture_output=True, timeout=120)
            self.assertEqual(0, result.returncode, result.stderr)
            return json.loads(result.stdout)

        with_pillow, without = verdicts("keep"), verdicts("block")
        self.assertEqual(with_pillow, without)
        self.assertEqual(
            {"a.png": "png", "b.mdp": "mdp2", "c.gif": "gif", "d.tga": "binary", "e.tif": "binary", "f.tif": "binary",
             "noext": "bmp", "broken.png": "binary"},
            without,
        )
        for name in names:  # and `add` works with Pillow blocked, for every one of them
            result = subprocess.run([sys.executable, "-c", _NO_PILLOW_PROBE, "add", name], cwd=self.repo, env=self.env,
                                    text=True, capture_output=True, timeout=120)
            self.assertEqual(0, result.returncode, f"{name}: {result.stderr}")
            self.assertNotIn("Traceback", result.stderr)

    def _edit_clip_layer(self, path: Path, main_id: int, colour, box) -> None:
        """Paint ``box`` of layer ``main_id`` with tmi's own write-back (in
        this process; the probes run the CLI in fresh ones)."""
        from tmi.formats import clip as C
        from tmi.image_ops import image_to_png_bytes

        clip = C.open_clip(path)
        node = next(n for n in C.walk_layer_nodes(clip.root.children) if n.main_id == main_id)
        image = C.render_layer_rgba(clip, node).image.to_pil()
        clip.db.close()
        image.paste(colour, box)
        path.write_bytes(C.replace_layer_pixels(path, f"clip:{main_id}", image_to_png_bytes(image)))

    def test_clip_commands_load_no_pillow(self) -> None:
        """CLIP files (RUST_MIGRATION.md 2d): renders, decoded layers and the
        CanvasPreview are tmi.raster images, the incremental composite
        decodes and patches its base in tmi_accel, so add, status, diff
        (--html too), commit -a (whole and incremental layered renders),
        log, show, restore (layer and folder export, layer write-back),
        switch, merge and stash load neither Pillow nor numpy."""
        from tmi.formats import clip as C

        global _PILLOW_FREE
        saved = _PILLOW_FREE
        _PILLOW_FREE = ("PIL", "numpy", "zstandard")
        source = next((p for p in (PROJECT_ROOT / "example" / "csp515" / "v2.clip", PROJECT_ROOT / "example" / "tama.clip")
                       if p.exists()), None)
        if source is None:
            self.skipTest("example CLIP files missing")
        try:
            art = self.repo / "art.clip"
            shutil.copyfile(source, art)
            clip = C.open_clip(art)
            leaves = [n.main_id for n in C.walk_layer_nodes(clip.root.children)
                      if not n.folder and (info := C.layer_offscreen_info(clip, n.render_mipmap)) and info[2] in clip.extas
                      and C.parse_offscreen_attribute(info[1]).channel_profile == "color-alpha-bgra"]
            folders = [n.main_id for n in C.walk_layer_nodes(clip.root.children) if n.folder and n.children]
            self.assertIsNotNone(C.render_canvas_preview_rgba(clip))
            clip.db.close()
            a, b = leaves[0], leaves[-1]
            self.tmi("init")
            self.assertIn("staged art.clip", self.probe("add", "art.clip"))  # the CanvasPreview
            self.probe("commit", "-m", "v1")
            self.tmi("tag", "v1")
            self._edit_clip_layer(art, a, (200, 30, 30, 255), (10, 10, 60, 40))  # no CanvasPreview any more
            self.assertIn("modified", self.probe("status"))
            self.probe("diff", "art.clip")
            self.probe("diff", "--html", "--no-open")
            self.probe("commit", "-a", "-m", "v2")  # a whole layered render
            self.probe("log")
            self.probe("show", "HEAD")
            self.probe("diff", "--html", "--no-open", "v1", "HEAD")
            self.probe("restore", "--source", "v1", "--output", str(self.tmp / "layer.png"), f"art.clip:clip:{a}")
            if folders:
                self.probe("restore", "--source", "v1", "--output", str(self.tmp / "folder.png"), f"art.clip:clip:{folders[0]}")
            self.probe("restore", "--in-place", "--source", "v1", f"art.clip:clip:{a}")
            self.probe("commit", "-a", "-m", "v3")  # incremental, on v2's layered render
            self.tmi("branch", "other")
            self._edit_clip_layer(art, a, (30, 200, 30, 255), (100, 100, 140, 130))
            self.tmi("commit", "-a", "-m", "ours")
            self.probe("switch", "other")
            self._edit_clip_layer(art, b, (30, 30, 200, 255), (300, 200, 340, 260))
            self.tmi("commit", "-a", "-m", "theirs")
            self.probe("switch", "main")
            self.assertIn("Merged other", self.probe("merge", "other"))
            self._edit_clip_layer(art, b, (90, 90, 90, 255), (0, 0, 8, 8))
            self.probe("stash", "push")
            self.probe("stash", "pop")
            self.assertIn("modified", self.probe("status"))
        finally:
            _PILLOW_FREE = saved


# Run the CLI in a fresh interpreter in which ``import PIL`` fails, as if
# Pillow were not installed.
_NO_PILLOW_PROBE = """
import sys
sys.modules["PIL"] = None
from tmi.cli import main
raise SystemExit(main(sys.argv[1:]))
"""


# Print the source format `import_file` records for each path, with ``import
# PIL`` failing (argv[1] == "block") or not.
_VERDICT_PROBE = """
import json, sys
if sys.argv[1] == "block":
    sys.modules["PIL"] = None
from pathlib import Path
from tmi.importers import import_file
print(json.dumps({p: import_file(Path(p), render_composite=False).source_format for p in sys.argv[2:]}))
"""


# Run the CLI in a fresh interpreter in which ``import tmi_accel`` fails (or
# finds a module of the given FEATURES), as if it were not installed.
_NO_ACCEL_PROBE = """
import sys, types
features = sys.argv[1]
if features == "-":
    sys.modules["tmi_accel"] = None
else:
    sys.modules["tmi_accel"] = types.SimpleNamespace(FEATURES=features.split(",") if features else [])
from tmi.cli import main
raise SystemExit(main(sys.argv[2:]))
"""


class AccelRequiredTests(unittest.TestCase):
    """tmi needs tmi_accel (RUST_MIGRATION.md 1e): without it, or with a
    build that lacks a required feature, a command that touches pixels fails
    at its first pixel operation with an error saying how to install it, and
    no fallback (tracking a drawing as an opaque file) takes over. Commands
    that never touch pixels do not need it. The module is not uninstalled:
    the probe hides it."""

    @classmethod
    def setUpClass(cls) -> None:
        root = PROJECT_ROOT / ".test-tmp"
        root.mkdir(exist_ok=True)
        cls.tmp = Path(tempfile.mkdtemp(prefix="tmi-noaccel-", dir=root))
        cls.env = os.environ.copy()
        cls.env["PYTHONPATH"] = str(PROJECT_ROOT) + os.pathsep + cls.env.get("PYTHONPATH", "")
        cls.env["TMI_AUTHOR_NAME"] = "Tester"
        cls.env["TMI_AUTHOR_EMAIL"] = "tester@example.com"
        cls.repo = cls.tmp / "repo"
        cls.repo.mkdir()
        write_png(cls.repo / "a.png", (255, 0, 0, 255))
        for args in (["init"], ["add", "a.png"], ["commit", "-m", "first"]):
            subprocess.run([sys.executable, "-m", "tmi", *args], cwd=cls.repo, env=cls.env, check=True,
                           capture_output=True, timeout=60)
        write_png(cls.repo / "b.png", (0, 0, 255, 255))
        write_mdp2_layers(cls.repo / "c.mdp", [(0, 0, 255, 255), (0, 255, 0, 128)])

    @classmethod
    def tearDownClass(cls) -> None:
        shutil.rmtree(cls.tmp, ignore_errors=True)
        try:
            (PROJECT_ROOT / ".test-tmp").rmdir()
        except OSError:
            pass

    def _run(self, features: str, *args: str) -> subprocess.CompletedProcess:
        return subprocess.run([sys.executable, "-c", _NO_ACCEL_PROBE, features, *args], cwd=self.repo, env=self.env,
                              text=True, capture_output=True, timeout=60)

    def test_pixel_commands_fail_with_install_instructions(self) -> None:
        for args in (["add", "b.png"], ["add", "c.mdp"]):
            with self.subTest(command=" ".join(args)):
                result = self._run("-", *args)
                self.assertEqual(1, result.returncode, result.stderr)
                self.assertIn("tmiki: error: tmi needs its native module tmi_accel", result.stderr)
                self.assertIn("pip install ./rust/tmi-accel", result.stderr)
                self.assertIn("Rust toolchain", result.stderr)
                self.assertNotIn("Traceback", result.stderr)
                self.assertNotIn("opaque", result.stderr)
        # Nothing was staged on the way.
        status = subprocess.run([sys.executable, "-m", "tmi", "status", "--short"], cwd=self.repo, env=self.env,
                                text=True, capture_output=True, timeout=60)
        self.assertIn("?? b.png", status.stdout)

    def test_an_older_build_is_named_too_old(self) -> None:
        from tmi import accel

        result = self._run("concurrent-calls,tmil-v1", "add", "b.png")
        self.assertEqual(1, result.returncode, result.stderr)
        self.assertIn("the installed tmi_accel is older than this tmi", result.stderr)
        missing = [name for name in accel.REQUIRED_FEATURES if name not in ("concurrent-calls", "tmil-v1")]
        self.assertIn(", ".join(missing), result.stderr)
        self.assertNotIn("Traceback", result.stderr)

    def test_metadata_commands_run_without_it(self) -> None:
        for args, expected in ((["status"], "b.png"), (["log", "--oneline"], "first"), (["branch"], "main")):
            with self.subTest(command=" ".join(args)):
                result = self._run("-", *args)
                self.assertEqual(0, result.returncode, result.stderr)
                self.assertIn(expected, result.stdout)

    def test_the_error_is_not_an_exception_fallbacks_catch(self) -> None:
        from unittest import mock

        from tmi import accel

        previous = accel._native
        accel._native = None
        try:
            with mock.patch.dict(sys.modules, {"tmi_accel": None}):
                with self.assertRaises(accel.AccelUnavailableError) as caught:
                    try:
                        accel.native()
                    except Exception:  # what the importer's fallbacks catch
                        self.fail("AccelUnavailableError was caught as an Exception")
            self.assertNotIsInstance(caught.exception, Exception)
        finally:
            accel._native = previous


if __name__ == "__main__":
    unittest.main()
