"""What a remote lists as refs is data other people write.

A value that is not a 64-digit lower-case hex object id (it would reach ref
files, messages and the pre-push hook's ``TMI_REMOTE_COMMIT``) and a name tmi
could not write (a bidi control, a control character, a Windows device name)
refuse the whole operation before any ref is written. Control characters in
what is printed are escaped (tmi/textsafe.py). The Rust side's twins are
rust/crates/tmi-transport/tests/ref_values.rs and tmi-repo's unit tests;
conformance compares the two.
"""
from __future__ import annotations

import shutil
import tempfile
import unittest
from pathlib import Path

from tmi.repo import Repo, ref_name_problem
from tmi.textsafe import escape
from tmi.transport import checked_refs

OK = "0123456789abcdef" * 4

BAD_VALUES = [
    'abc"&calc&"',
    "\x1b]0;pwned\x07",
    OK.upper(),
    OK[:-1],
    OK + "0",
    "z" * 64,
]


def raw_ref(repo: Repo, name: str, value: str) -> None:
    path = repo.repo_dir.joinpath(*name.split("/"))
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(value + "\n", encoding="utf-8")


class RefValueTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = Path(tempfile.mkdtemp(prefix="tmi-refvals-"))
        self.addCleanup(shutil.rmtree, self.tmp, True)

    def test_checked_refs_rules(self) -> None:
        self.assertEqual(checked_refs({"refs/heads/main": OK, "refs/heads/e": ""}), {"refs/heads/main": OK, "refs/heads/e": ""})
        for value in BAD_VALUES:
            with self.assertRaisesRegex(RuntimeError, "invalid object id"):
                checked_refs({"refs/heads/x": value})
        for name in ["refs/heads/CON", "refs/heads/nul.txt", "refs/tags/x/COM1", "refs/heads/a\x1bb",
                     "refs/heads/a\x85b", "refs/heads/a\x7fb", "refs/heads/a‮b", "refs/heads/a⁩"]:
            with self.assertRaisesRegex(RuntimeError, "invalid ref name"):
                checked_refs({name: OK})

    def test_error_text_has_no_raw_control_characters(self) -> None:
        with self.assertRaises(RuntimeError) as cm:
            checked_refs({"refs/heads/x": "\x1b]0;pwned\x07"})
        self.assertNotIn("\x1b", str(cm.exception))
        self.assertNotIn("\x07", str(cm.exception))

    def _remote(self, name: str, **refs: str) -> Repo:
        repo = Repo.create(self.tmp / name)
        for ref, value in refs.items():
            raw_ref(repo, "refs/heads/" + ref, value)
        return repo

    def test_bad_object_id_refuses_clone_fetch_and_push(self) -> None:
        for i, bad in enumerate(BAD_VALUES):
            case = self.tmp / f"case{i}"
            src = Repo.create(case / "src")
            good = self._some_oid(src)
            raw_ref(src, "refs/heads/aaa", good)
            raw_ref(src, "refs/heads/zzz", bad)
            fetcher = Repo.create(case / "fetcher")
            fetcher.add_remote("origin", str(case / "src"))
            with self.assertRaisesRegex(RuntimeError, "invalid object id"):
                fetcher.fetch("origin")
            self.assertIsNone(fetcher.read_ref("refs/remotes/origin/aaa"), bad)
            raw_ref(fetcher, "refs/heads/zzz", good)
            with self.assertRaisesRegex(RuntimeError, "invalid object id"):
                fetcher.push("origin", "zzz", force=True)

    def _some_oid(self, repo: Repo) -> str:
        return repo.store.write_json("tree", {"entries": [], "kind": "repository", "version": 1})

    def test_bad_ref_name_refuses_the_whole_fetch(self) -> None:
        for i, name in enumerate(["refs/heads/a‮b", "refs/heads/x⁦y", "refs/tags/t‏"]):
            case = self.tmp / f"name{i}"
            src = Repo.create(case / "src")
            good = self._some_oid(src)
            raw_ref(src, "refs/heads/aaa", good)
            raw_ref(src, "refs/heads/aab", good)
            raw_ref(src, name, good)
            fetcher = Repo.create(case / "fetcher")
            fetcher.add_remote("origin", str(case / "src"))
            with self.assertRaisesRegex(RuntimeError, "invalid ref name") as cm:
                fetcher.fetch("origin")
            self.assertNotIn("‮", str(cm.exception))
            self.assertIsNone(fetcher.read_ref("refs/remotes/origin/aaa"), name)
            self.assertIsNone(fetcher.read_ref("refs/remotes/origin/aab"), name)


class RefNameTests(unittest.TestCase):
    def test_refused(self) -> None:
        for name in ["CON", "nul", "aux.txt", "prn", "COM1", "lpt9.x", "a/CONIN$", "x/com²", "a\x85b",
                     "a\x9fb", "a‎b", "a‏b", "a‪b", "a‮b", "a⁦b", "a⁩b", "a\x7fb"]:
            self.assertIsNotNone(ref_name_problem(name), repr(name))
        for name in ["refs/heads/con", "refs/heads/dir/NUL.txt", "refs/tags/lpt1"]:
            self.assertEqual(ref_name_problem(name), "a part of it is a reserved Windows device name")

    def test_accepted(self) -> None:
        for name in ["main", "feature/x", "console", "com10", "コン", "a⁥b", "v1.0"]:
            self.assertIsNone(ref_name_problem(name), repr(name))


class EscapeTests(unittest.TestCase):
    def test_escape(self) -> None:
        self.assertEqual(escape("a\x1b]0;x\x07b"), "a\\x1b]0;x\\x07b")
        self.assertEqual(escape("a\tb\nc"), "a\tb\\x0ac")
        self.assertEqual(escape("a\tb\nc", True), "a\tb\nc")
        self.assertEqual(escape("\x7f\x9b‮⁦", True), "\\x7f\\x9b\\u202e\\u2066")
        self.assertEqual(escape("日本語 ok\\"), "日本語 ok\\")


if __name__ == "__main__":
    unittest.main()
