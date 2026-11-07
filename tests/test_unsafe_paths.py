"""What tmi accepts from a remote, and writes to a working tree.

A hostile remote writes trees tmi's own commands never write (tests/forge.py):
a path that leaves the working tree (``../x``, ``a/../../x``, a drive, a UNC
path), one that lands in ``.tmi`` (``.tmi/hooks/post-commit.bat``: a hook that
the next commit runs), a Windows device name, a ``:``, a trailing dot. Each
must be refused when it is received (clone, fetch, pull: before a ref moves,
before the tree is stored) and again when it is written to a working tree
(switch, checkout, reset --hard, merge: before HEAD moves or a file is
written), with nothing created outside the clone and nothing in ``.tmi``'s
hooks. A symbolic link or junction on the way is not written through.

The Rust side's twins are rust/crates/tmi-transport/tests/unsafe_paths.rs;
conformance scenarios ``unsafe-paths`` and ``transport-unsafe-paths`` compare
the two on the same attacks. ``python tools/run_tests.py --variants rustcli``
runs this file through the Rust executable.
"""
from __future__ import annotations

import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

from tests.forge import forge_branch, store_has
from tests.test_cli_regressions import tmi_command, write_png
from tmi import recpath

PROJECT_ROOT = Path(__file__).resolve().parents[1]

ATTACKS = [
    "../escape.txt",
    "sub/../../escape2.txt",
    ".tmi/hooks/post-commit.bat",
    ".TMI/hooks/post-commit.bat",
    ".tmi./hooks/post-commit.bat",
    "TMI~1/hooks/post-commit.bat",
    "sub/.tmi/hooks/post-commit.bat",
    ".git/config",
    "c:/escape.txt",
    "c:escape.txt",
    "//server/share/escape.txt",
    "\\\\server\\share\\escape.txt",
    "sub\\..\\..\\escape.txt",
    "/escape.txt",
    "CON",
    "sub/nul.txt",
    "COM1.png",
    "file.txt:stream",
    "trailing.",
    "trailing /x",
    "a//b",
    "a\x1b[2Jb",
]
# the ones that reach the write paths in the slower tests (each is one process)
ATTACKS_FEW = ["../escape.txt", "sub/../../escape2.txt", ".tmi/hooks/post-commit.bat", ".TMI/hooks/x.bat", "c:/escape.txt"]
REFUSED = "invalid path"
GOOD_NAMES = ["a.clip", "dir/sub/x.png", "日本語/ファイル.mdp", "a b/c d.png", ".tmiattributes", ".tmiplugins/ora.wasm",
              ".gitignore", ".tmiignore", "console.png", "com10.png", "-x.png", "x~1.png", "tmi~x/y", "a~1/b"]


def listing(root: Path, skip: tuple[str, ...] = ()) -> list[str]:
    """Every file under ``root`` (posix, relative) except ``.tmi/objects`` and
    the directories named in ``skip``."""
    out = []
    for dirpath, dirnames, filenames in os.walk(root):
        rel_dir = Path(dirpath).relative_to(root).as_posix()
        if rel_dir in skip or rel_dir == ".tmi/objects" or rel_dir.startswith(".tmi/objects/"):
            dirnames[:] = []
            continue
        for name in filenames:
            out.append(name if rel_dir == "." else f"{rel_dir}/{name}")
    return sorted(out)


class UnsafePathTests(unittest.TestCase):
    def setUp(self) -> None:
        root = PROJECT_ROOT / ".test-tmp"
        root.mkdir(exist_ok=True)
        self.tmp = Path(tempfile.mkdtemp(prefix="tmi-unsafe-", dir=root))
        self.env = os.environ.copy()
        self.env["PYTHONPATH"] = str(PROJECT_ROOT) + os.pathsep + self.env.get("PYTHONPATH", "")
        self.env["TMI_AUTHOR_NAME"] = "Tester"
        self.env["TMI_AUTHOR_EMAIL"] = "tester@example.com"
        self.env["TMI_CONFIG_HOME"] = str(self.tmp / "userconfig")
        self.env.setdefault("TMI_PYTHON", sys.executable)
        self.n = 0

    def tearDown(self) -> None:
        shutil.rmtree(self.tmp, ignore_errors=True)
        try:
            (PROJECT_ROOT / ".test-tmp").rmdir()
        except OSError:
            pass

    def tmi(self, cwd: Path, *args: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run([*tmi_command(), *args], cwd=cwd, env=self.env, text=True, capture_output=True, timeout=120)

    def case(self) -> Path:
        self.n += 1
        case = self.tmp / f"case{self.n}"
        case.mkdir()
        return case

    def make_src(self, case: Path) -> Path:
        src = case / "src"
        src.mkdir()
        self.assertEqual(self.tmi(src, "init").returncode, 0)
        write_png(src / "a.png", (255, 0, 0, 255))
        self.assertEqual(self.tmi(src, "add", "a.png").returncode, 0)
        self.assertEqual(self.tmi(src, "commit", "-m", "first").returncode, 0)
        return src

    def assert_refused(self, result: subprocess.CompletedProcess[str], attack: str) -> None:
        self.assertNotEqual(result.returncode, 0, f"{attack!r} was accepted:\n{result.stdout}\n{result.stderr}")
        self.assertIn(REFUSED, result.stderr, f"{attack!r}:\n{result.stdout}\n{result.stderr}")
        self.assertNotIn("\x1b", result.stderr, "an escape sequence of a remote reached the terminal")
        self.assertNotIn("Traceback", result.stderr)

    # ---- the rule ----

    def test_rule_refuses_and_accepts(self) -> None:
        for path in ATTACKS + ["", "a.", "a /b", "aux .png", "NUL.tar.gz", "COM\u00b9", "CONIN$", "GIT~1/x", "tmi~12", "a<b", "a|b",
                               "a?b", "a*b", 'a"b']:
            with self.subTest(path=path):
                self.assertIsNotNone(recpath.record_path_problem(path))
        for path in GOOD_NAMES:
            with self.subTest(path=path):
                self.assertIsNone(recpath.record_path_problem(path))

    def test_every_example_file_name_is_accepted(self) -> None:
        for folder in ("example", ".test-images"):
            base = PROJECT_ROOT / folder
            if not base.is_dir():
                continue
            for path in base.rglob("*"):
                if path.is_file():
                    rel = path.relative_to(base).as_posix()
                    self.assertIsNone(recpath.record_path_problem(rel), rel)

    def test_messages_do_not_pass_control_characters(self) -> None:
        message = recpath.invalid_path_message("a\x1b[2Jb\u202e", "it contains a control character")
        self.assertNotIn("\x1b", message)
        self.assertNotIn("\u202e", message)

    # ---- receiving ----

    def test_clone_refuses_every_attack(self) -> None:
        for attack in ATTACKS:
            with self.subTest(attack=attack):
                case = self.case()
                src = self.make_src(case)
                commit = forge_branch(src, [("good.txt", b"x"), (attack, b"payload")], branch="main")
                tree = None
                work = case / "work"
                work.mkdir()
                result = self.tmi(work, "clone", str(src), "dst")
                self.assert_refused(result, attack)
                dst = work / "dst"
                self.assertEqual([f for f in listing(work) if not f.startswith("dst/.tmi/")], [], "a file was written")
                self.assertFalse((case / "escape.txt").exists() or (case / "escape2.txt").exists())
                self.assertFalse((dst / ".git").exists())
                hooks = dst / ".tmi" / "hooks"
                self.assertEqual(list(hooks.iterdir()) if hooks.exists() else [], [], "a hook was planted")
                self.assertFalse((dst / ".tmi" / "refs" / "heads" / "main").exists(), "a branch was made")
                import json, zlib
                raw = zlib.decompress((src / ".tmi" / "objects" / commit[:2] / commit[2:]).read_bytes())
                tree = json.loads(raw.split(b"\x00", 1)[1])["tree_ref"]
                self.assertFalse(store_has(dst, tree), "the forged tree was stored")

    def test_fetch_and_pull_refuse_before_a_ref_moves(self) -> None:
        for attack in ATTACKS_FEW + ["file.txt:stream", "TMI~1/hooks/post-commit.bat"]:
            with self.subTest(attack=attack):
                case = self.case()
                src = self.make_src(case)
                work = case / "work"
                work.mkdir()
                self.assertEqual(self.tmi(work, "clone", str(src), "dst").returncode, 0)
                dst = work / "dst"
                ref = dst / ".tmi" / "refs" / "remotes" / "origin" / "main"
                before_ref = ref.read_text(encoding="utf-8")
                before = listing(dst)
                forge_branch(src, [("good.txt", b"x"), (attack, b"payload")], branch="main")
                result = self.tmi(dst, "fetch")
                self.assert_refused(result, attack)
                self.assertEqual(ref.read_text(encoding="utf-8"), before_ref, "a remote-tracking ref moved")
                self.assertEqual(listing(dst), before, "something was written")
                self.assertEqual([f for f in listing(work) if not f.startswith("dst/")], [])
                result = self.tmi(dst, "pull")
                self.assert_refused(result, attack)
                self.assertEqual(listing(dst), before)
                self.assertFalse(any((dst / ".tmi" / "hooks").iterdir()))

    def test_push_into_a_repository_refuses_too(self) -> None:
        # the same gate on the objects a local remote receives
        case = self.case()
        src = self.make_src(case)
        bare = case / "bare.tmi"
        self.assertEqual(self.tmi(case, "init", "--bare", "bare.tmi").returncode, 0)
        self.assertEqual(self.tmi(src, "remote", "add", "origin", str(bare)).returncode, 0)
        forge_branch(src, [("../escape.txt", b"payload")], branch="evil")
        result = self.tmi(src, "push", "origin", "evil")
        self.assert_refused(result, "../escape.txt")
        self.assertFalse((bare / "refs" / "heads" / "evil").exists())

    # ---- writing ----

    def test_a_tree_already_in_the_store_is_refused_when_written(self) -> None:
        for attack in ATTACKS_FEW:
            with self.subTest(attack=attack):
                case = self.case()
                src = self.make_src(case)
                work = case / "work"
                work.mkdir()
                self.assertEqual(self.tmi(work, "clone", str(src), "base").returncode, 0)
                base = work / "base"
                # objects as an older tmi would have stored them
                oid = forge_branch(base, [("good.txt", b"x"), (attack, b"payload")], branch="evil")
                for n, args in enumerate([("switch", "evil"), ("switch", "--discard", "evil"), ("checkout", oid),
                                          ("checkout", "--discard", oid), ("reset", "--hard", oid), ("merge", "evil")]):
                    with self.subTest(args=args):
                        repo = work / f"repo{n}"
                        shutil.copytree(base, repo)
                        before = listing(repo)
                        head = (repo / ".tmi" / "HEAD").read_text(encoding="utf-8")
                        result = self.tmi(repo, *args)
                        self.assert_refused(result, attack)
                        self.assertEqual((repo / ".tmi" / "HEAD").read_text(encoding="utf-8"), head, "HEAD moved")
                        self.assertEqual([f for f in listing(repo) if not f.startswith(".tmi/logs/") and f != ".tmi/stat_cache"], before, "the working tree changed")
                        self.assertEqual([f for f in listing(work) if not f.startswith(("repo", "base"))], [], "a file outside")
                        self.assertFalse((work / "escape.txt").exists() or (work / "escape2.txt").exists())
                        self.assertFalse(any((repo / ".tmi" / "hooks").iterdir()), "a hook was planted")
                        shutil.rmtree(repo, ignore_errors=True)

    def make_link(self, link: Path, target: Path) -> bool:
        try:
            if os.name == "nt":
                return subprocess.run(["cmd", "/C", "mklink", "/J", str(link), str(target)], capture_output=True).returncode == 0
            os.symlink(target, link)
            return True
        except OSError:
            return False

    def test_a_link_on_the_way_is_not_written_through(self) -> None:
        case = self.case()
        repo = case / "repo"
        repo.mkdir()
        self.assertEqual(self.tmi(repo, "init").returncode, 0)
        (repo / "d").mkdir()
        write_png(repo / "d" / "a.png", (1, 2, 3, 255))
        self.assertEqual(self.tmi(repo, "add", "d/a.png").returncode, 0)
        self.assertEqual(self.tmi(repo, "commit", "-m", "one").returncode, 0)
        self.assertEqual(self.tmi(repo, "branch", "b").returncode, 0)
        self.assertEqual(self.tmi(repo, "switch", "b").returncode, 0)
        write_png(repo / "d" / "new.png", (9, 9, 9, 255))
        self.assertEqual(self.tmi(repo, "add", "d/new.png").returncode, 0)
        self.assertEqual(self.tmi(repo, "commit", "-m", "two").returncode, 0)
        self.assertEqual(self.tmi(repo, "switch", "main").returncode, 0)
        outside = case / "outside"
        outside.mkdir()
        shutil.rmtree(repo / "d")
        if not self.make_link(repo / "d", outside):
            self.skipTest("cannot make a symbolic link or junction here")
        try:
            for args in (("switch", "--discard", "b"), ("reset", "--hard", "b"), ("checkout", "--discard", "b")):
                with self.subTest(args=args):
                    result = self.tmi(repo, *args)
                    self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
                    self.assertIn("symbolic link or junction", result.stderr)
                    self.assertEqual(os.listdir(outside), [], "a file was written through the link")
        finally:
            if os.name == "nt":
                os.rmdir(repo / "d")
            else:
                (repo / "d").unlink()

    # ---- what tmi records itself ----

    def test_add_makes_records_by_the_same_rule(self) -> None:
        case = self.case()
        repo = case / "repo"
        repo.mkdir()
        self.assertEqual(self.tmi(repo, "init").returncode, 0)
        # a legitimate name with an unusual look is recorded
        (repo / "dir with space").mkdir()
        write_png(repo / "dir with space" / "x y.png", (1, 2, 3, 255))
        write_png(repo / "日本語.png", (1, 2, 3, 255))
        result = self.tmi(repo, "add", "dir with space/x y.png", "日本語.png")
        self.assertEqual(result.returncode, 0, result.stderr)
        # a name the rule refuses on every OS (an NTFS short name of .tmi) is not
        (repo / "GIT~1").write_bytes(b"data")
        result = self.tmi(repo, "add", "GIT~1")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("cannot record GIT~1", result.stderr)
        import json
        index = json.loads((repo / ".tmi" / "index").read_text(encoding="utf-8"))
        self.assertNotIn("GIT~1", index["entries"])
        self.assertIn("日本語.png", index["entries"])


if __name__ == "__main__":
    unittest.main()
