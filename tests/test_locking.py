"""Two tmi processes on one repository (issues #35 and #36).

Every write to the index or a ref goes through a lock file (tmi/lockfile.py,
tmi-repo's lockfile.rs): ``.tmi/index.lock`` for a command that changes the
index, ``<ref>.lock`` with a compare-and-swap for every ref update, the remote
ref of a push included. A second process finds the lock and stops at once
with a message naming it; a command that raced another one and lost is never
reported as a success.

The races are made deterministic with locks the test creates itself and with
hooks (a pre-commit / pre-push hook runs a second tmi in the middle of the
first one), and the real thing is stress-tested with processes started
together. ``python tools/run_tests.py --variants rustcli`` runs the same
tests through the Rust executable.
"""
from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path

from tests.test_cli_regressions import tmi_command, write_png

PROJECT_ROOT = Path(__file__).resolve().parents[1]
LOCK_WORDS = "Another tmi process seems to be running"


class LockTestCase(unittest.TestCase):
    def setUp(self) -> None:
        root = PROJECT_ROOT / ".test-tmp"
        root.mkdir(exist_ok=True)
        self.tmp = Path(tempfile.mkdtemp(prefix="tmi-lock-", dir=root))
        self.env = os.environ.copy()
        self.env["PYTHONPATH"] = str(PROJECT_ROOT) + os.pathsep + self.env.get("PYTHONPATH", "")
        self.env["TMI_AUTHOR_NAME"] = "Tester"
        self.env["TMI_AUTHOR_EMAIL"] = "tester@example.com"
        self.env["TMI_CONFIG_HOME"] = str(self.tmp / "userconfig")
        self.env.setdefault("TMI_PYTHON", sys.executable)

    def tearDown(self) -> None:
        shutil.rmtree(self.tmp, ignore_errors=True)
        try:
            (PROJECT_ROOT / ".test-tmp").rmdir()
        except OSError:
            pass

    def tmi(self, cwd: Path, *args: str, check: bool = True) -> subprocess.CompletedProcess[str]:
        result = subprocess.run([*tmi_command(), *args], cwd=cwd, env=self.env, text=True,
                                capture_output=True, timeout=120)
        if check and result.returncode:
            self.fail(f"tmi {' '.join(args)} failed ({result.returncode}):\n{result.stdout}\n{result.stderr}")
        return result

    def tmi_together(self, jobs: list[tuple[Path, tuple[str, ...]]]) -> list[subprocess.CompletedProcess[str]]:
        """Start every job, then wait for all of them."""
        procs = [subprocess.Popen([*tmi_command(), *args], cwd=cwd, env=self.env, text=True,
                                  stdout=subprocess.PIPE, stderr=subprocess.PIPE) for cwd, args in jobs]
        out = []
        for proc in procs:
            stdout, stderr = proc.communicate(timeout=180)
            out.append(subprocess.CompletedProcess(proc.args, proc.returncode, stdout, stderr))
        return out

    def repo(self, name: str = "repo") -> Path:
        repo = self.tmp / name
        repo.mkdir()
        self.tmi(repo, "init")
        return repo

    @staticmethod
    def staged(repo: Path) -> set[str]:
        index = json.loads((repo / ".tmi" / "index").read_text(encoding="utf-8"))
        return set(index.get("entries", {}))

    @staticmethod
    def locks(repo: Path) -> list[str]:
        base = repo / ".tmi" if (repo / ".tmi").is_dir() else repo  # a bare repository is its own .tmi
        return sorted(p.relative_to(base).as_posix() for p in base.rglob("*.lock"))

    def write_hook(self, repo: Path, name: str, source: str) -> None:
        hooks = repo / ".tmi" / "hooks"
        hooks.mkdir(exist_ok=True)
        hook = hooks / (name + ".py" if os.name == "nt" else name)
        hook.write_text(f"#!{sys.executable}\n{source}\n", encoding="utf-8")
        if os.name != "nt":
            hook.chmod(0o755)

    def run_tmi_hook(self, cwd: Path, *args: str) -> str:
        """Source of a hook that runs tmi in ``cwd`` and exits with its code."""
        cmd = [*tmi_command(), *args]
        # LOCKTEST_INNER: the tmi the hook starts runs this hook too; it must not start another
        return (f"import os, subprocess, sys\n"
                f"if os.environ.get('LOCKTEST_INNER'):\n    sys.exit(0)\n"
                f"env = dict(os.environ, LOCKTEST_INNER='1')\n"
                f"r = subprocess.run({cmd!r}, cwd={str(cwd)!r}, env=env, capture_output=True, text=True)\n"
                f"sys.stderr.write(r.stderr)\nsys.exit(r.returncode)")


class IndexLockTests(LockTestCase):
    def prepared(self) -> Path:
        repo = self.repo()
        write_png(repo / "a.png", (255, 0, 0, 255))
        self.tmi(repo, "add", "a.png")
        self.tmi(repo, "commit", "-m", "base")
        self.tmi(repo, "branch", "other")
        write_png(repo / "b.png", (0, 255, 0, 255))
        write_png(repo / "c.png", (0, 0, 255, 255))
        self.tmi(repo, "add", "b.png")
        return repo

    def test_held_index_lock_stops_every_command_that_writes_the_index(self) -> None:
        repo = self.prepared()
        lock = repo / ".tmi" / "index.lock"
        lock.write_text("12345\n", encoding="utf-8")
        before = {name: (repo / ".tmi" / name).read_bytes() for name in ("index", "HEAD")}
        main_before = (repo / ".tmi" / "refs" / "heads" / "main").read_bytes()
        commands = [
            ("add", "c.png"),
            ("add", "-A"),
            ("commit", "-m", "x"),
            ("commit", "-a", "-m", "x"),
            ("commit", "--amend", "-m", "x"),
            ("restore", "--staged", "b.png"),
            ("reset",),
            ("rm", "--cached", "b.png"),
            ("mv", "a.png", "z.png"),
            ("switch", "other", "--discard"),
            ("checkout", "other"),
            ("merge", "other"),
            ("stash", "push"),
            ("stash", "clear"),
        ]
        for args in commands:
            with self.subTest(args=args):
                result = self.tmi(repo, *args, check=False)
                self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
                self.assertIn("tmiki: error: unable to create '.tmi/index.lock': File exists.", result.stderr)
                self.assertIn(LOCK_WORDS, result.stderr)
                self.assertIn("remove the file", result.stderr)
                # nothing written, and the other process's lock is left alone
                self.assertEqual((repo / ".tmi" / "index").read_bytes(), before["index"])
                self.assertEqual((repo / ".tmi" / "HEAD").read_bytes(), before["HEAD"])
                self.assertEqual((repo / ".tmi" / "refs" / "heads" / "main").read_bytes(), main_before)
                self.assertEqual(lock.read_text(encoding="utf-8"), "12345\n")
                self.assertEqual((repo / "a.png").exists(), True)
                self.assertEqual((repo / "z.png").exists(), False)
        # reading is not blocked, and the lock is no ref or branch
        self.assertEqual(self.tmi(repo, "log", "--oneline").returncode, 0)
        self.assertEqual(self.tmi(repo, "status").returncode, 0)
        lock.unlink()
        self.assertEqual(self.tmi(repo, "add", "c.png").returncode, 0)
        self.assertEqual(self.staged(repo), {"b.png", "c.png"})

    def test_no_lock_is_left_behind(self) -> None:
        repo = self.prepared()
        self.tmi(repo, "add", "c.png")
        self.assertEqual(self.locks(repo), [])
        failed = self.tmi(repo, "add", "does-not-exist.png", check=False)
        self.assertNotEqual(failed.returncode, 0)
        self.assertEqual(self.locks(repo), [])
        failed = self.tmi(repo, "merge", "no-such-branch", check=False)
        self.assertNotEqual(failed.returncode, 0)
        self.assertEqual(self.locks(repo), [])
        self.tmi(repo, "commit", "-m", "two files")
        self.assertEqual(self.locks(repo), [])
        self.assertEqual(self.tmi(repo, "commit", "-m", "nothing", check=False).returncode, 1)
        self.assertEqual(self.locks(repo), [])

    def test_index_lock_is_released_while_a_hook_runs(self) -> None:
        # a pre-commit hook may stage files (`tmiki add`): the commit takes them
        repo = self.prepared()
        self.write_hook(repo, "pre-commit", self.run_tmi_hook(repo, "add", "c.png"))
        self.tmi(repo, "commit", "-m", "with hook")
        shown = self.tmi(repo, "show", "--stat", "HEAD", check=False)
        self.assertEqual(self.staged(repo), set())
        self.assertEqual(self.locks(repo), [])
        status = self.tmi(repo, "status").stdout
        self.assertIn("working tree clean", status, shown.stdout)

    def test_parallel_adds_never_lose_a_file_silently(self) -> None:
        for round_ in range(3):
            with self.subTest(round=round_):
                repo = self.repo(f"adds{round_}")
                names = [f"f{i}.png" for i in range(8)]
                for i, name in enumerate(names):
                    write_png(repo / name, (i * 30, 10, 10, 255))
                results = self.tmi_together([(repo, ("add", name)) for name in names])
                staged = self.staged(repo)
                for name, result in zip(names, results):
                    if result.returncode == 0:
                        self.assertIn(name, staged, f"{name}: reported staged but is not\n{result.stdout}")
                    else:
                        self.assertNotIn(name, staged)
                        self.assertIn("index.lock", result.stderr)
                        self.assertIn(LOCK_WORDS, result.stderr)
                self.assertTrue(any(r.returncode == 0 for r in results), "no add won the lock")
                self.assertEqual(self.locks(repo), [])
                # the ones that were stopped can simply be run again
                for name in names:
                    self.tmi(repo, "add", name)
                self.assertEqual(self.staged(repo), set(names))

    def test_parallel_commits_never_lose_a_commit_silently(self) -> None:
        for round_ in range(3):
            with self.subTest(round=round_):
                repo = self.repo(f"commits{round_}")
                write_png(repo / "base.png", (1, 2, 3, 255))
                self.tmi(repo, "add", "base.png")
                self.tmi(repo, "commit", "-m", "base")
                for i in range(6):
                    write_png(repo / f"n{i}.png", (i * 30, 10, 10, 255))
                self.tmi(repo, "add", *(f"n{i}.png" for i in range(6)))
                messages = [f"commit-{i}" for i in range(6)]
                results = self.tmi_together([(repo, ("commit", "-m", m)) for m in messages])
                history = self.tmi(repo, "log", "--oneline").stdout
                won = [m for m, r in zip(messages, results) if r.returncode == 0]
                self.assertTrue(won, "no commit won the lock")
                for message, result in zip(messages, results):
                    if result.returncode == 0:
                        self.assertIn(message, history, f"reported committed but lost\n{result.stdout}")
                    else:
                        self.assertNotIn(message, history)
                        self.assertTrue(LOCK_WORDS in result.stderr or "nothing to commit" in result.stderr,
                                        result.stderr)
                self.assertEqual(len(won), history.count("commit-"))
                self.assertEqual(self.locks(repo), [])


class RefLockTests(LockTestCase):
    def prepared(self) -> Path:
        repo = self.repo()
        write_png(repo / "a.png", (255, 0, 0, 255))
        self.tmi(repo, "add", "a.png")
        self.tmi(repo, "commit", "-m", "base")
        write_png(repo / "b.png", (0, 255, 0, 255))
        self.tmi(repo, "add", "b.png")
        return repo

    def test_held_ref_lock_stops_the_commit_and_loses_nothing(self) -> None:
        repo = self.prepared()
        heads = repo / ".tmi" / "refs" / "heads"
        lock = heads / "main.lock"
        lock.write_text("", encoding="utf-8")
        main_before = (heads / "main").read_bytes()
        result = self.tmi(repo, "commit", "-m", "second", check=False)
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("tmiki: error: unable to create '.tmi/refs/heads/main.lock': File exists.", result.stderr)
        self.assertIn(LOCK_WORDS, result.stderr)
        self.assertEqual((heads / "main").read_bytes(), main_before)
        self.assertEqual(self.staged(repo), {"b.png"})  # still staged
        self.assertTrue(lock.exists())
        lock.unlink()
        self.tmi(repo, "commit", "-m", "second")
        self.assertIn("second", self.tmi(repo, "log", "--oneline").stdout)

    def test_ref_lock_stops_branch_and_tag_updates(self) -> None:
        repo = self.prepared()
        (repo / ".tmi" / "refs" / "heads" / "feature.lock").write_text("", encoding="utf-8")
        (repo / ".tmi" / "refs" / "tags").mkdir(exist_ok=True)
        (repo / ".tmi" / "refs" / "tags" / "v1.lock").write_text("", encoding="utf-8")
        result = self.tmi(repo, "branch", "feature", check=False)
        self.assertEqual(result.returncode, 1)
        self.assertIn("unable to create '.tmi/refs/heads/feature.lock'", result.stderr)
        result = self.tmi(repo, "tag", "v1", check=False)
        self.assertEqual(result.returncode, 1)
        self.assertIn("unable to create '.tmi/refs/tags/v1.lock'", result.stderr)
        self.assertEqual(len(self.tmi(repo, "branch").stdout.strip().splitlines()), 1)
        # the lock files are not branches or tags
        self.assertNotIn("feature", self.tmi(repo, "branch").stdout)
        self.assertNotIn("v1", self.tmi(repo, "tag").stdout)

    def test_creating_and_deleting_refs_leaves_no_lock(self) -> None:
        repo = self.prepared()
        self.tmi(repo, "branch", "topic")
        self.tmi(repo, "tag", "v1")
        self.tmi(repo, "tag", "-a", "v2", "-m", "annotated")
        self.assertEqual(self.locks(repo), [])
        self.tmi(repo, "tag", "-d", "v1")
        self.tmi(repo, "branch", "-D", "topic")
        self.assertEqual(self.locks(repo), [])
        self.assertNotIn("topic", self.tmi(repo, "branch").stdout)
        self.assertNotIn("v1", self.tmi(repo, "tag").stdout)

    def test_a_name_ending_in_dot_lock_is_not_a_branch(self) -> None:
        repo = self.prepared()
        result = self.tmi(repo, "branch", "topic.lock", check=False)
        self.assertEqual(result.returncode, 1)
        self.assertIn("invalid ref name", result.stderr)
        self.assertEqual(self.locks(repo), [])

    def test_commit_that_lost_a_race_is_not_reported_as_a_success(self) -> None:
        # The pre-commit hook runs a second commit while the first waits for
        # it: when the first one moves main it finds main is not where it
        # read it, and fails instead of forgetting the second commit.
        repo = self.prepared()
        self.write_hook(repo, "pre-commit", self.run_tmi_hook(repo, "commit", "-m", "inner"))
        result = self.tmi(repo, "commit", "--allow-empty", "-m", "outer", check=False)
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("refs/heads/main was changed by another tmi process", result.stderr)
        self.assertIn("nothing was written", result.stderr)
        log = self.tmi(repo, "log", "--oneline").stdout
        self.assertIn("inner", log)
        self.assertNotIn("outer", log)
        self.assertEqual(self.locks(repo), [])


class LocalRemoteTests(LockTestCase):
    def remote_and_clones(self, count: int) -> tuple[Path, list[Path]]:
        remote = self.tmp / "remote.tmi"
        self.tmi(self.tmp, "init", "--bare", str(remote))
        seed = self.repo("seed")
        write_png(seed / "base.png", (9, 9, 9, 255))
        self.tmi(seed, "add", "base.png")
        self.tmi(seed, "commit", "-m", "base")
        self.tmi(seed, "remote", "add", "origin", str(remote))
        self.tmi(seed, "push", "origin", "main")
        clones = []
        for i in range(count):
            self.tmi(self.tmp, "clone", str(remote), f"w{i}")
            clone = self.tmp / f"w{i}"
            write_png(clone / f"mine{i}.png", (i * 40, 100, 5, 255))
            self.tmi(clone, "add", f"mine{i}.png")
            self.tmi(clone, "commit", "-m", f"work-{i}")
            clones.append(clone)
        return remote, clones

    def remote_log(self, remote: Path) -> str:
        chk = self.tmp / f"chk{len(list(self.tmp.glob('chk*')))}"
        self.tmi(self.tmp, "clone", str(remote), chk.name)
        return self.tmi(chk, "log", "--oneline").stdout

    def test_push_that_lost_a_race_is_rejected_not_reported_as_pushed(self) -> None:
        remote, (wa, wb) = self.remote_and_clones(2)
        # wa's pre-push hook runs wb's push after wa has read the remote ref
        self.write_hook(wa, "pre-push", self.run_tmi_hook(wb, "push", "origin", "main"))
        result = self.tmi(wa, "push", "origin", "main", check=False)
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertNotIn("pushed main", result.stdout)
        self.assertIn("updates were rejected: remote origin/main is at ", result.stderr)
        self.assertIn("(non-fast-forward). Fetch and merge first", result.stderr)
        log = self.remote_log(remote)
        self.assertIn("work-1", log)
        self.assertNotIn("work-0", log)
        # the loser's tracking ref did not move
        tracking = (wa / ".tmi" / "refs" / "remotes" / "origin" / "main").read_text(encoding="utf-8").strip()
        remote_main = (remote / "refs" / "heads" / "main").read_text(encoding="utf-8").strip()
        self.assertNotEqual(tracking, remote_main)
        self.assertEqual(self.locks(remote), [])
        # fetch, merge and push again: nothing is lost
        self.tmi(wa, "pull", "origin", "main")
        self.tmi(wa, "push", "origin", "main")
        log = self.remote_log(remote)
        self.assertIn("work-0", log)
        self.assertIn("work-1", log)

    def test_tag_push_that_lost_a_race_is_rejected(self) -> None:
        remote, (wa, wb) = self.remote_and_clones(2)
        for clone in (wa, wb):
            self.tmi(clone, "tag", "v1")
        # wa's pre-push hook pushes wb's v1 after wa read that the remote has none
        self.write_hook(wa, "pre-push", self.run_tmi_hook(wb, "push", "origin", "v1"))
        result = self.tmi(wa, "push", "origin", "v1", check=False)
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("updates were rejected: tag v1 was changed on origin while pushing", result.stderr)
        wb_tag = (wb / ".tmi" / "refs" / "tags" / "v1").read_text(encoding="utf-8").strip()
        remote_tag = (remote / "refs" / "tags" / "v1").read_text(encoding="utf-8").strip()
        self.assertEqual(remote_tag, wb_tag)
        self.assertEqual(self.locks(remote), [])
        # --force replaces it (the one update that does not compare)
        forced = self.tmi(wa, "push", "origin", "v1", "--force")
        self.assertEqual(forced.returncode, 0)
        wa_tag = (wa / ".tmi" / "refs" / "tags" / "v1").read_text(encoding="utf-8").strip()
        self.assertEqual((remote / "refs" / "tags" / "v1").read_text(encoding="utf-8").strip(), wa_tag)

    def test_push_stops_at_a_held_remote_ref_lock(self) -> None:
        remote, (wa,) = self.remote_and_clones(1)
        lock = remote / "refs" / "heads" / "main.lock"
        lock.write_text("", encoding="utf-8")
        result = self.tmi(wa, "push", "origin", "main", check=False)
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("refs/heads/main.lock': File exists.", result.stderr)
        self.assertIn(LOCK_WORDS, result.stderr)
        self.assertNotIn("work-0", self.remote_log(remote))
        lock.unlink()
        self.tmi(wa, "push", "origin", "main")
        self.assertIn("work-0", self.remote_log(remote))

    def test_forced_push_still_overwrites(self) -> None:
        remote, (wa, wb) = self.remote_and_clones(2)
        self.tmi(wb, "push", "origin", "main")
        self.tmi(wa, "push", "origin", "main", "--force")
        log = self.remote_log(remote)
        self.assertIn("work-0", log)
        self.assertNotIn("work-1", log)

    def test_parallel_pushes_never_lose_a_commit_silently(self) -> None:
        for round_ in range(2):
            with self.subTest(round=round_):
                shutil.rmtree(self.tmp / "remote.tmi", ignore_errors=True)
                for leftover in list(self.tmp.iterdir()):
                    if leftover.name != "userconfig":
                        shutil.rmtree(leftover, ignore_errors=True)
                remote, clones = self.remote_and_clones(5)
                results = self.tmi_together([(c, ("push", "origin", "main")) for c in clones])
                log = self.remote_log(remote)
                self.assertTrue(any(r.returncode == 0 for r in results), "no push won")
                for i, result in enumerate(results):
                    if result.returncode == 0:
                        self.assertIn(f"work-{i}", log, f"reported pushed but lost\n{result.stdout}")
                    else:
                        self.assertNotIn(f"work-{i}", log)
                        self.assertTrue("updates were rejected" in result.stderr or LOCK_WORDS in result.stderr,
                                        result.stderr)
                self.assertEqual(self.locks(remote), [])

    def hold_open(self, path: Path, seconds: float) -> tuple[str, Path]:
        """Source of a pre-push hook that leaves ``path`` open (as Python's
        open() opens it: no FILE_SHARE_DELETE) in a process of its own for
        ``seconds``, and the file that process removes when it has closed it."""
        ready, done = self.tmp / "holder-ready", self.tmp / "holder-done"
        holder = (f"import pathlib, time\nf = open({str(path)!r}, 'rb')\n"
                  f"pathlib.Path({str(ready)!r}).touch()\ntime.sleep({seconds})\nf.close()\n"
                  f"pathlib.Path({str(done)!r}).touch()\n")
        hook = (f"import os, pathlib, subprocess, sys, time\n"
                f"subprocess.Popen([sys.executable, '-c', {holder!r}])\n"
                f"while not pathlib.Path({str(ready)!r}).exists():\n    time.sleep(0.01)\n")
        return hook, done

    @unittest.skipUnless(os.name == "nt", "Windows sharing semantics")
    def test_push_waits_for_a_reader_of_the_remote_ref(self) -> None:
        # A tmi reading the remote ref (a concurrent push's list_refs) has it
        # open when this push renames main.lock over it: Windows refuses the
        # rename while the handle is open; the push waits, it does not fail
        # with "[WinError 5] Access is denied" (the parallel push test's flake).
        remote, (wa,) = self.remote_and_clones(1)
        hook, done = self.hold_open(remote / "refs" / "heads" / "main", 1.0)
        self.write_hook(wa, "pre-push", hook)
        result = self.tmi(wa, "push", "origin", "main", check=False)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("work-0", self.remote_log(remote))
        self.assertEqual(self.locks(remote), [])

    @unittest.skipUnless(os.name == "nt", "Windows sharing semantics")
    def test_push_stops_with_a_message_when_the_remote_ref_stays_open(self) -> None:
        remote, (wa,) = self.remote_and_clones(1)
        main = remote / "refs" / "heads" / "main"
        before = main.read_bytes()
        hook, done = self.hold_open(main, 6)
        self.write_hook(wa, "pre-push", hook)
        result = self.tmi(wa, "push", "origin", "main", check=False)
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("tmiki: error: unable to update 'remote.tmi/refs/heads/main': another program keeps the file "
                      "open", result.stderr)
        self.assertNotIn("WinError", result.stderr)
        self.assertNotIn("pushed main", result.stdout)
        self.assertEqual(self.locks(remote), [])
        for _ in range(1000):
            if done.exists():
                break
            time.sleep(0.01)
        self.assertEqual(main.read_bytes(), before)
        (wa / ".tmi" / "hooks" / "pre-push.py").unlink()
        self.tmi(wa, "push", "origin", "main")
        self.assertIn("work-0", self.remote_log(remote))


class LibraryTests(unittest.TestCase):
    """The Python API (tmi.repo.Repo, tmi.lockfile)."""

    def setUp(self) -> None:
        root = PROJECT_ROOT / ".test-tmp"
        root.mkdir(exist_ok=True)
        self.tmp = Path(tempfile.mkdtemp(prefix="tmi-lock-lib-", dir=root))

    def tearDown(self) -> None:
        shutil.rmtree(self.tmp, ignore_errors=True)
        try:
            (PROJECT_ROOT / ".test-tmp").rmdir()
        except OSError:
            pass

    def test_write_ref_compares_under_the_lock(self) -> None:
        from tmi.lockfile import RefMovedError
        from tmi.repo import Repo

        repo = Repo.create(self.tmp / "r")
        ref = "refs/heads/topic"
        repo.write_ref(ref, "a" * 64, expected=None)
        self.assertEqual(repo.read_ref(ref), "a" * 64)
        with self.assertRaises(RefMovedError) as caught:
            repo.write_ref(ref, "b" * 64, expected=None)  # it exists now
        self.assertEqual(caught.exception.actual, "a" * 64)
        with self.assertRaises(RefMovedError):
            repo.write_ref(ref, "b" * 64, expected="c" * 64)
        self.assertEqual(repo.read_ref(ref), "a" * 64)
        repo.write_ref(ref, "b" * 64, expected="a" * 64)
        repo.write_ref(ref, "c" * 64)  # no comparison
        self.assertEqual(repo.read_ref(ref), "c" * 64)
        self.assertFalse((repo.repo_dir / "refs" / "heads" / "topic.lock").exists())

    @unittest.skipUnless(os.name == "nt", "Windows sharing semantics")
    def test_reading_a_ref_while_it_is_being_renamed(self) -> None:
        # A rename in flight holds the file with DELETE access; Python's open()
        # (no FILE_SHARE_DELETE) then fails with EACCES, which made a
        # concurrent push's list_refs / read_ref die with "[Errno 13]
        # Permission denied: '.../refs/heads/main'". tmi's reads share it.
        import _winapi

        from tmi import lockfile
        from tmi.repo import Repo

        repo = Repo.create(self.tmp / "r")
        repo.write_ref("refs/heads/main", "a" * 64)
        path = repo.repo_dir / "refs" / "heads" / "main"
        delete_access, share_all = 0x00010000, 0x7
        handle = _winapi.CreateFile(str(path), delete_access, share_all, 0, _winapi.OPEN_EXISTING, 0, 0)
        try:
            with self.assertRaises(PermissionError):
                path.read_text(encoding="utf-8")
            self.assertEqual(lockfile.read_text(path), "a" * 64 + "\n")
            self.assertEqual(repo.read_ref("refs/heads/main"), "a" * 64)
            self.assertEqual(repo.all_refs(), {"refs/heads/main": "a" * 64})
        finally:
            _winapi.CloseHandle(handle)

    def test_read_text_reads_as_path_read_text_does(self) -> None:
        from tmi import lockfile

        path = self.tmp / "f"
        path.write_bytes("a\r\nb\rc\né".encode("utf-8"))
        self.assertEqual(lockfile.read_text(path), path.read_text(encoding="utf-8"))
        with self.assertRaises(FileNotFoundError):
            lockfile.read_text(self.tmp / "missing")

    @unittest.skipUnless(os.name == "nt", "Windows sharing semantics")
    def test_write_ref_waits_for_a_reader_then_gives_up_with_the_message(self) -> None:
        import threading

        from tmi import lockfile
        from tmi.repo import Repo

        repo = Repo.create(self.tmp / "r")
        repo.write_ref("refs/heads/main", "a" * 64)
        path = repo.repo_dir / "refs" / "heads" / "main"
        reader = open(path, "rb")
        threading.Timer(0.2, reader.close).start()
        repo.write_ref("refs/heads/main", "b" * 64, expected="a" * 64)
        self.assertEqual(repo.read_ref("refs/heads/main"), "b" * 64)
        with open(path, "rb"):
            started = time.monotonic()
            with self.assertRaises(RuntimeError) as caught:
                repo.write_ref("refs/heads/main", "c" * 64, expected="b" * 64)
            self.assertGreaterEqual(time.monotonic() - started, lockfile.REPLACE_RETRY_SECONDS)
        self.assertEqual(str(caught.exception), lockfile.busy_message(".tmi/refs/heads/main"))
        self.assertEqual(repo.read_ref("refs/heads/main"), "b" * 64)
        self.assertFalse((repo.repo_dir / "refs" / "heads" / "main.lock").exists())

    def test_index_lock_is_exclusive_and_released(self) -> None:
        from tmi import lockfile
        from tmi.repo import Repo

        repo = Repo.create(self.tmp / "r")
        self.assertTrue(repo.lock_index())
        self.assertFalse(repo.lock_index())  # this process already holds it
        self.assertTrue((repo.repo_dir / "index.lock").exists())
        other = Repo.open_path(self.tmp / "r")
        lockfile.release_all()
        self.assertFalse((repo.repo_dir / "index.lock").exists())
        (repo.repo_dir / "index.lock").write_text("1\n")
        with self.assertRaises(RuntimeError) as caught:
            other.lock_index()
        self.assertIn("index.lock", str(caught.exception))
        with self.assertRaises(RuntimeError):
            other.write_index({"version": 1, "entries": {}})
        self.assertTrue((repo.repo_dir / "index.lock").exists())  # not ours: untouched


if __name__ == "__main__":
    unittest.main()
