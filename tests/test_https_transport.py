"""HttpsTransport batching / streaming / resume (issue #10).

The HTTP layer is replaced by a tiny in-process fake of tmihub's object
endpoints (``/objects/list``, ``/objects/download``, ``/objects/upload``) so
these tests need no server. One test also goes through a real loopback
``http.server`` to check that the streamed upload body reaches the wire intact.
"""

from __future__ import annotations

import io
import json
import os
import shutil
import sys
import tempfile
import threading
import unittest
import zipfile
import zlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from unittest import mock

from tmi.objects import ObjectStore
from tmi.transport import https as https_mod
from tmi.transport.https import HttpsTransport


PROJECT_ROOT = Path(__file__).resolve().parents[1]


def _zip_of(objects: dict[str, bytes]) -> bytes:
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, 'w', compression=zipfile.ZIP_DEFLATED) as zf:
        for oid, raw in objects.items():
            zf.writestr(f"{oid[:2]}/{oid[2:]}", raw)
    return buf.getvalue()


def _unzip_multipart(body: bytes, content_type: str) -> dict[str, bytes]:
    boundary = content_type.split("boundary=", 1)[1].encode()
    head, rest = body.split(b"\r\n\r\n", 1)
    assert head.startswith(b"--" + boundary), head
    zip_data, tail = rest.rsplit(b"\r\n--" + boundary + b"--\r\n", 1)
    assert tail == b"", tail
    with zipfile.ZipFile(io.BytesIO(zip_data)) as zf:
        return {name.replace("/", ""): zf.read(name) for name in zf.namelist()}


class _FakeHub:
    """Serves the three object endpoints from an in-memory oid -> raw dict."""

    def __init__(self, objects: dict[str, bytes] | None = None) -> None:
        self.objects = dict(objects or {})
        self.requests: list[tuple[str, object]] = []
        self.upload_bodies: list[object] = []
        self.fail_download_after: int | None = None

    def open(self, req, timeout: float = 300.0):
        endpoint = req.full_url.rsplit("/api/u/r", 1)[1]
        self.requests.append((endpoint, req.data))
        if endpoint == "/objects/list":
            return io.BytesIO(json.dumps({"oids": sorted(self.objects)}).encode())
        if endpoint == "/objects/download":
            downloads = sum(1 for e, _ in self.requests if e == endpoint)
            if self.fail_download_after is not None and downloads > self.fail_download_after:
                raise RuntimeError("tmihub request failed (connection reset)")
            oids = json.loads(req.data.decode())["oids"]
            return io.BytesIO(_zip_of({oid: self.objects[oid] for oid in oids}))
        if endpoint == "/objects/upload":
            self.upload_bodies.append(req.data)
            body = req.data.read()
            self.objects.update(_unzip_multipart(body, req.get_header("Content-type")))
            assert int(req.get_header("Content-length")) == len(body)
            return io.BytesIO(b"{}")
        raise AssertionError(f"unexpected endpoint {endpoint}")


class HttpsTransportBatchTests(unittest.TestCase):
    def setUp(self) -> None:
        root = PROJECT_ROOT / ".test-tmp"
        root.mkdir(exist_ok=True)
        self.tmp = Path(tempfile.mkdtemp(prefix="tmi-https-", dir=root))
        self.remote = ObjectStore(self.tmp / "remote")
        self.local = ObjectStore(self.tmp / "local")
        self.oids = [self.remote.write("blob", f"object {i}".encode() * 50) for i in range(10)]
        env = mock.patch.dict(os.environ)
        env.start()
        self.addCleanup(env.stop)
        for name in ("TMIHUB_BATCH_OBJECTS", "TMIHUB_BATCH_BYTES", "TMIHUB_TOKEN"):
            os.environ.pop(name, None)

    def tearDown(self) -> None:
        shutil.rmtree(self.tmp, ignore_errors=True)
        try:
            (PROJECT_ROOT / ".test-tmp").rmdir()
        except OSError:
            pass

    def _raw(self, store: ObjectStore) -> dict[str, bytes]:
        return {oid: store.path_for(oid).read_bytes() for oid in store.iter_object_ids()}

    def _transport(self, hub: _FakeHub) -> HttpsTransport:
        transport = HttpsTransport("http://hub.invalid/api/u/r")
        transport._open = hub.open  # type: ignore[method-assign]
        return transport

    def test_pull_splits_into_batches(self) -> None:
        hub = _FakeHub(self._raw(self.remote))
        progress: list[str] = []
        with mock.patch.object(https_mod, "BATCH_MAX_OBJECTS", 4):
            received = self._transport(hub).pull_objects(self.local, on_progress=progress.append)
        self.assertEqual(received, 10)
        self.assertEqual(sorted(self.local.iter_object_ids()), sorted(self.oids))
        downloads = [json.loads(data)["oids"] for e, data in hub.requests if e == "/objects/download"]
        self.assertEqual([len(b) for b in downloads], [4, 4, 2])
        self.assertTrue(any("batch 3/3" in msg for msg in progress), progress)

    def test_batch_limit_from_env(self) -> None:
        hub = _FakeHub(self._raw(self.remote))
        with mock.patch.dict(os.environ, {"TMIHUB_BATCH_OBJECTS": "3"}):
            self._transport(hub).pull_objects(self.local)
        downloads = [e for e, _ in hub.requests if e == "/objects/download"]
        self.assertEqual(len(downloads), 4)

    def test_interrupted_pull_resumes_with_only_missing_objects(self) -> None:
        hub = _FakeHub(self._raw(self.remote))
        hub.fail_download_after = 1  # first batch succeeds, second dies
        with mock.patch.object(https_mod, "BATCH_MAX_OBJECTS", 4):
            with self.assertRaises(RuntimeError):
                self._transport(hub).pull_objects(self.local)
            # The completed batch is already in the store, intact.
            have_after_failure = set(self.local.iter_object_ids())
            self.assertEqual(len(have_after_failure), 4)
            for oid in have_after_failure:
                self.local.read(oid)

            hub.fail_download_after = None
            hub.requests.clear()
            received = self._transport(hub).pull_objects(self.local)
        self.assertEqual(received, 6)
        self.assertEqual(sorted(self.local.iter_object_ids()), sorted(self.oids))
        requested = [oid for e, data in hub.requests if e == "/objects/download"
                     for oid in json.loads(data)["oids"]]
        self.assertEqual(sorted(requested), sorted(set(self.oids) - have_after_failure))

    def test_pull_response_is_streamed_not_read_whole(self) -> None:
        hub = _FakeHub(self._raw(self.remote))
        reads: list[int] = []

        class _Response(io.BytesIO):
            def read(self, size: int = -1) -> bytes:
                reads.append(size)
                return super().read(size)

        def opener(req, timeout=300.0):
            response = hub.open(req, timeout)
            if req.full_url.endswith("/objects/download"):
                return _Response(response.getvalue())
            return response

        transport = HttpsTransport("http://hub.invalid/api/u/r")
        transport._open = opener  # type: ignore[method-assign]
        transport.pull_objects(self.local)
        self.assertTrue(reads)
        self.assertNotIn(-1, reads)
        self.assertEqual(len(self.local.iter_object_ids()), 10)

    def test_push_batches_by_count_and_bytes_and_streams_body(self) -> None:
        hub = _FakeHub()
        local = self.remote  # push from the populated store
        sizes = [local.path_for(oid).stat().st_size for oid in sorted(self.oids)]
        byte_limit = sizes[0] + sizes[1]  # roughly two objects per batch
        with mock.patch.object(https_mod, "BATCH_MAX_OBJECTS", 100), \
             mock.patch.object(https_mod, "BATCH_MAX_BYTES", byte_limit):
            pushed = self._transport(hub).push_objects(local)
        self.assertEqual(pushed, 10)
        self.assertEqual(hub.objects, self._raw(local))
        self.assertGreaterEqual(len(hub.upload_bodies), 5)
        for body in hub.upload_bodies:
            self.assertNotIsInstance(body, (bytes, bytearray, memoryview))
            self.assertTrue(hasattr(body, "read"))

    def test_push_resumes_after_partial_upload(self) -> None:
        local = self.remote
        already = dict(list(self._raw(local).items())[:7])
        hub = _FakeHub(already)
        with mock.patch.object(https_mod, "BATCH_MAX_OBJECTS", 2):
            pushed = self._transport(hub).push_objects(local)
        self.assertEqual(pushed, 3)
        self.assertEqual(hub.objects, self._raw(local))
        uploads = [e for e, _ in hub.requests if e == "/objects/upload"]
        self.assertEqual(len(uploads), 2)

    def test_oversized_object_goes_alone(self) -> None:
        with mock.patch.object(https_mod, "BATCH_MAX_BYTES", 10):
            batches = https_mod._batches(["a", "b", "c"], {"a": 1, "b": 100, "c": 1}.__getitem__)
        self.assertEqual(batches, [["a"], ["b"], ["c"]])

    def test_streamed_upload_over_real_http(self) -> None:
        received: dict[str, object] = {}

        class Handler(BaseHTTPRequestHandler):
            def do_POST(self) -> None:
                length = int(self.headers["Content-Length"])
                received["length"] = length
                received["chunked"] = self.headers.get("Transfer-Encoding")
                received["objects"] = _unzip_multipart(self.rfile.read(length), self.headers["Content-Type"])
                self.send_response(200)
                self.send_header("Content-Length", "2")
                self.end_headers()
                self.wfile.write(b"{}")

            def log_message(self, *args) -> None:
                pass

        server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            transport = HttpsTransport(f"http://127.0.0.1:{server.server_address[1]}/api/u/r")
            transport._upload_batch(self.remote, self.oids[:3])
        finally:
            server.shutdown()
            server.server_close()
        self.assertIsNone(received["chunked"])
        self.assertEqual(received["objects"], {oid: self.remote.path_for(oid).read_bytes() for oid in self.oids[:3]})


def tmi_command() -> list[str]:
    """``python -m tmi``, or the executable ``TMI_BIN`` names (the Rust front
    door; ``python tools/run_tests.py --variants rustcli``)."""
    binary = os.environ.get("TMI_BIN")
    return [binary] if binary else [sys.executable, "-m", "tmi"]


class HttpsCliTests(unittest.TestCase):
    """clone / push / fetch / pull through the CLI against a loopback tmihub
    (tools/fake_tmihub.py), so the rustcli variant runs the Rust transport."""

    def setUp(self) -> None:
        from tools.fake_tmihub import Hub

        root = PROJECT_ROOT / ".test-tmp"
        root.mkdir(exist_ok=True)
        self.tmp = Path(tempfile.mkdtemp(prefix="tmi-https-cli-", dir=root))
        # no proxy from the environment running the tests (see test_proxy_choice_is_urllibs)
        self.env = {k: v for k, v in os.environ.items()
                    if k not in ("TMIHUB_TOKEN", "TMIHUB_BATCH_OBJECTS", "TMIHUB_BATCH_BYTES", "TMIHUB_TIMEOUT",
                                 "REQUEST_METHOD")
                    and not k.lower().endswith("_proxy")}
        self.env["PYTHONPATH"] = str(PROJECT_ROOT) + os.pathsep + self.env.get("PYTHONPATH", "")
        self.env["TMI_AUTHOR_NAME"] = "Tester"
        self.env["TMI_AUTHOR_EMAIL"] = "tester@example.com"
        self.env["TMI_CONFIG_HOME"] = str(self.tmp / "userconfig")
        self.env.setdefault("TMI_PYTHON", sys.executable)
        (self.tmp / "hub" / "u").mkdir(parents=True)
        self.tmi(self.tmp / "hub" / "u", "init", "--bare", "r")
        self.hub = Hub(self.tmp / "hub")
        self.Hub = Hub

    def tearDown(self) -> None:
        self.hub.server.server_close()
        shutil.rmtree(self.tmp, ignore_errors=True)
        try:
            (PROJECT_ROOT / ".test-tmp").rmdir()
        except OSError:
            pass

    def tmi(self, cwd: Path, *args: str, check: bool = True, env: dict | None = None):
        import subprocess

        result = subprocess.run([*tmi_command(), *args], cwd=cwd, env={**self.env, **(env or {})},
                                text=True, capture_output=True, timeout=120)
        if check and result.returncode:
            self.fail(f"tmi {' '.join(args)} failed ({result.returncode}):\n{result.stdout}\n{result.stderr}")
        return result

    def _source(self, url: str) -> Path:
        from tools.conformance import _png

        src = self.tmp / "src"
        src.mkdir()
        self.tmi(src, "init")
        for i in range(5):
            _png(src / f"p{i}.png", (i * 40, 10, 10, 255))
        self.tmi(src, "add", *(f"p{i}.png" for i in range(5)))
        self.tmi(src, "commit", "-m", "first")
        self.tmi(src, "remote", "add", "origin", url)
        return src

    def test_clone_push_fetch_pull_round_trip(self) -> None:
        from tools.conformance import _png

        with self.hub:
            url = self.hub.url("u", "r")
            src = self._source(url)
            pushed = self.tmi(src, "push", env={"TMIHUB_BATCH_OBJECTS": "3"})
            self.assertIn("pushed main", pushed.stdout)
            uploads = [r for r in self.hub.requests if r[1] == "objects/upload"]
            self.assertGreater(len(uploads), 1)
            self.assertTrue(all(0 < n <= 3 for _, _, n in uploads), uploads)
            self.hub.requests.clear()
            cloned = self.tmi(self.tmp, "clone", url, "copy", env={"TMIHUB_BATCH_OBJECTS": "4"})
            self.assertIn(f"cloned {url} into", cloned.stdout)
            downloads = [n for _, e, n in self.hub.requests if e == "objects/download"]
            self.assertTrue(downloads and all(n <= 4 for n in downloads), downloads)
            copy = self.tmp / "copy"
            self.assertEqual((copy / "p3.png").read_bytes(), (src / "p3.png").read_bytes())
            self.assertIn("working tree clean", self.tmi(copy, "status").stdout)

            _png(src / "p0.png", (1, 2, 3, 255))
            self.tmi(src, "commit", "-a", "-m", "second")
            self.tmi(src, "push")
            fetched = self.tmi(copy, "fetch")
            self.assertIn("fetched", fetched.stdout)
            self.assertIn("origin/main", fetched.stdout)
            pulled = self.tmi(copy, "pull")
            self.assertIn("pull fast-forward", pulled.stdout)
            self.assertEqual((copy / "p0.png").read_bytes(), (src / "p0.png").read_bytes())
            self.assertIn("pull already-up-to-date", self.tmi(copy, "pull").stdout)

    def test_tags_are_pushed_to_the_hub_and_cloned(self) -> None:
        """Issue #40: `push <remote> <tag>` and `push --tags` over HTTPS; clone
        and fetch bring the tags; a tag is never moved."""
        from tools.conformance import _png

        with self.hub:
            url = self.hub.url("u", "r")
            src = self._source(url)
            self.tmi(src, "tag", "v1")
            self.tmi(src, "tag", "-a", "v2", "-m", "second tag")
            self.tmi(src, "push")
            pushed = self.tmi(src, "push", "origin", "v1")
            self.assertRegex(pushed.stdout, r"pushed tag v1 [0-9a-f]{12} to origin")
            self.assertIn("pushed tag v2", self.tmi(src, "push", "--tags").stdout)
            self.assertIn("tags are up to date on origin", self.tmi(src, "push", "--tags").stdout)
            self.assertEqual({"refs/tags/v1", "refs/tags/v2"}, {r for r in self.hub._refs(self.tmp / "hub" / "u" / "r") if "/tags/" in r})

            self.tmi(self.tmp, "clone", url, "copy")
            copy = self.tmp / "copy"
            self.assertEqual(["v1", "v2"], [line.split()[0] for line in self.tmi(copy, "tag").stdout.splitlines()])
            self.tmi(src, "tag", "v3")
            self.tmi(src, "push", "--tags")
            self.assertIn("  new tag v3", self.tmi(copy, "fetch").stdout)

            # the hub has v1 at the first commit: a v1 on another commit is not sent
            _png(src / "p0.png", (1, 2, 3, 255))
            self.tmi(src, "commit", "-a", "-m", "second")
            self.tmi(src, "tag", "-d", "v1")
            self.tmi(src, "tag", "v1")
            moved = self.tmi(src, "push", "origin", "v1", check=False)
            self.assertEqual(1, moved.returncode)
            self.assertIn("tag v1 already exists on origin", moved.stderr)

    def test_force_push_is_refused_by_the_hub(self) -> None:
        from tools.conformance import _png

        with self.hub:
            url = self.hub.url("u", "r")
            src = self._source(url)
            self.tmi(src, "push")
            self.tmi(self.tmp, "clone", url, "other")
            other = self.tmp / "other"
            _png(other / "p1.png", (9, 9, 9, 255))
            self.tmi(other, "commit", "-a", "-m", "other")
            self.tmi(other, "push")
            _png(src / "p2.png", (8, 8, 8, 255))
            self.tmi(src, "commit", "-a", "-m", "mine")
            plain = self.tmi(src, "push", check=False)
            self.assertEqual(plain.returncode, 1)
            self.assertIn("non-fast-forward", plain.stderr)
            forced = self.tmi(src, "push", "--force", check=False)
            self.assertEqual(forced.returncode, 1)
            self.assertIn("tmihub rejected the update (409)", forced.stderr)

    def test_update_ref_is_a_compare_and_swap(self) -> None:
        """A client that says which value it checked (``expected``) is refused
        when the ref moved since, even if its value would fast-forward the ref
        (issue #36)."""
        from tmi.transport.https import HttpsTransport
        from tools.conformance import _png

        with self.hub:
            url = self.hub.url("u", "r")
            src = self._source(url)
            main = src / ".tmi" / "refs" / "heads" / "main"
            self.tmi(src, "push")
            c1 = main.read_text(encoding="utf-8").strip()
            _png(src / "p0.png", (7, 7, 7, 255))
            self.tmi(src, "commit", "-a", "-m", "second")
            c2 = main.read_text(encoding="utf-8").strip()
            self.tmi(src, "push")  # the objects of c2 are on the hub now
            hub_main = self.tmp / "hub" / "u" / "r" / "refs" / "heads" / "main"
            hub_main.write_text(c1 + "\n", encoding="utf-8")  # put the hub's main back at c1
            transport = HttpsTransport(url)
            with self.assertRaises(RuntimeError) as caught:
                transport.update_ref("refs/heads/main", c2, expected="f" * 64)  # not what main is
            self.assertIn("tmihub rejected the update (409)", str(caught.exception))
            self.assertEqual(hub_main.read_text(encoding="utf-8").strip(), c1)
            with self.assertRaises(RuntimeError):
                transport.update_ref("refs/heads/main", c2, expected=None)  # "must not exist"
            transport.update_ref("refs/heads/main", c2, expected=c1)
            self.assertEqual(hub_main.read_text(encoding="utf-8").strip(), c2)

    def test_push_sends_the_value_it_checked(self) -> None:
        """The push (Python's or the Rust executable's) names the remote tip it
        read, so a hub that compares can refuse a push that raced another."""
        from tools.conformance import _png

        with self.hub:
            url = self.hub.url("u", "r")
            src = self._source(url)
            self.tmi(src, "push")
            first = self.hub.ref_updates[-1]
            self.assertEqual((first["ref_name"], first["expected"]), ("refs/heads/main", None))
            c1 = (src / ".tmi" / "refs" / "heads" / "main").read_text(encoding="utf-8").strip()
            _png(src / "p0.png", (7, 7, 7, 255))
            self.tmi(src, "commit", "-a", "-m", "second")
            self.tmi(src, "push")
            self.assertEqual(self.hub.ref_updates[-1]["expected"], c1)
    def test_push_that_lost_a_race_is_rejected(self) -> None:
        from tools.conformance import _png

        with self.hub:
            url = self.hub.url("u", "r")
            src = self._source(url)
            self.tmi(src, "push")
            self.tmi(self.tmp, "clone", url, "other")
            other = self.tmp / "other"
            _png(other / "p1.png", (9, 9, 9, 255))
            self.tmi(other, "commit", "-a", "-m", "other")
            _png(src / "p2.png", (8, 8, 8, 255))
            self.tmi(src, "commit", "-a", "-m", "mine")
            # mine's pre-push hook pushes other's commit after mine read the hub's main
            hook = src / ".tmi" / "hooks" / ("pre-push.py" if os.name == "nt" else "pre-push")
            cmd = [*tmi_command(), "push"]
            hook.write_text(
                f"#!{sys.executable}\nimport os, subprocess, sys\n"
                f"if os.environ.get('LOCKTEST_INNER'):\n    sys.exit(0)\n"
                f"r = subprocess.run({cmd!r}, cwd={str(other)!r}, env=dict(os.environ, LOCKTEST_INNER='1'), capture_output=True)\n"
                f"sys.exit(r.returncode)\n", encoding="utf-8")
            if os.name != "nt":
                hook.chmod(0o755)
            result = self.tmi(src, "push", check=False)
            self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
            self.assertNotIn("pushed main", result.stdout)
            self.assertIn("rejected", result.stderr)
            hub_main = (self.tmp / "hub" / "u" / "r" / "refs" / "heads" / "main").read_text(encoding="utf-8").strip()
            other_main = (other / ".tmi" / "refs" / "heads" / "main").read_text(encoding="utf-8").strip()
            self.assertEqual(hub_main, other_main)

    def test_token_is_sent_and_required(self) -> None:
        hub = self.Hub(self.tmp / "hub", token="s3cret")
        with hub:
            url = hub.url("u", "r")
            denied = self.tmi(self.tmp, "clone", url, "a", check=False)
            self.assertEqual(denied.returncode, 1)
            self.assertIn("tmihub denied access (401): authentication required", denied.stderr)
            self.tmi(self.tmp, "clone", url, "b", env={"TMIHUB_TOKEN": "s3cret"})
            self.tmi(self.tmp, "config", "--global", "tmihub.token", "s3cret")
            self.tmi(self.tmp / "b", "fetch")

    def test_corrupt_object_from_the_hub_is_refused(self) -> None:
        with self.hub:
            url = self.hub.url("u", "r")
            src = self._source(url)
            self.tmi(src, "push")
            bare = self.tmp / "hub" / "u" / "r"
            victim = next(p for p in sorted((bare / "objects").glob("*/*")) if p.is_file())
            victim.write_bytes(zlib.compress(b"blob\0not what the name says"))
            result = self.tmi(self.tmp, "clone", url, "c", check=False)
            self.assertEqual(result.returncode, 1)
            self.assertIn("remote sent corrupt object data: object hash mismatch for", result.stderr)

    def _misbehaving_hub(self, replies: dict[str, bytes]):
        """A loopback server answering ``/api/u/r/<endpoint>`` with
        ``replies[endpoint]`` (status 200), 404 for anything else."""

        class Handler(BaseHTTPRequestHandler):
            def _reply(self) -> None:
                length = int(self.headers.get("Content-Length") or 0)
                if length:
                    self.rfile.read(length)
                body = replies.get(self.path.removeprefix("/api/u/r/"))
                self.send_response(200 if body is not None else 404)
                self.send_header("Content-Length", str(len(body or b"")))
                self.end_headers()
                self.wfile.write(body or b"")

            do_GET = do_POST = _reply

            def log_message(self, *args) -> None:
                pass

        server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        return f"http://127.0.0.1:{server.server_address[1]}/api/u/r"

    def test_network_failures_are_reported_in_tmis_words(self) -> None:
        # Issue #29: no Python / json / zipfile / urllib internals.
        repo = self.tmp / "repo"
        repo.mkdir()
        self.tmi(repo, "init")
        oid = "ab" * 32

        not_json = self._misbehaving_hub({"objects/list": b"", "refs": b"<html>oops</html>"})
        self.tmi(repo, "remote", "add", "empty", not_json)
        result = self.tmi(repo, "fetch", "empty", check=False)
        self.assertEqual(1, result.returncode)
        self.assertEqual(f"tmiki: error: {https_mod.UNEXPECTED_REPLY}\n", result.stderr)
        result = self.tmi(self.tmp, "clone", not_json, "c1", check=False)
        self.assertEqual(f"tmiki: error: {https_mod.UNEXPECTED_REPLY}\n", result.stderr)

        wrong_shape = self._misbehaving_hub({"objects/list": b"[1, 2]"})
        self.tmi(repo, "remote", "add", "shape", wrong_shape)
        result = self.tmi(repo, "fetch", "shape", check=False)
        self.assertEqual(f"tmiki: error: {https_mod.UNEXPECTED_REPLY}\n", result.stderr)

        broken_zip = self._misbehaving_hub({
            "objects/list": json.dumps({"oids": [oid]}).encode(),
            "objects/download": b"PK\x03\x04 this is not a zip archive",
            "refs": b'{"refs": {}}',
        })
        self.tmi(repo, "remote", "add", "zip", broken_zip)
        result = self.tmi(repo, "fetch", "zip", check=False)
        self.assertEqual(1, result.returncode)
        self.assertEqual(f"tmiki: error: {https_mod.BROKEN_DOWNLOAD}\n", result.stderr)

        import socket

        with socket.socket() as probe:
            probe.bind(("127.0.0.1", 0))
            port = probe.getsockname()[1]
        self.tmi(repo, "remote", "add", "down", f"http://127.0.0.1:{port}/api/u/r")
        result = self.tmi(repo, "fetch", "down", check=False)
        self.assertEqual(1, result.returncode)
        self.assertTrue(result.stderr.startswith(f"tmiki: error: cannot reach tmihub at 127.0.0.1:{port}: "), result.stderr)
        self.assertTrue(result.stderr.endswith("; check the remote URL and your network connection\n"), result.stderr)
        self.assertNotIn("urlopen", result.stderr)
    # -- a network that misbehaves (issue #28) --
    # TMIHUB_TIMEOUT bounds each connect / send / receive without progress.
    # A request that reached the server is never run again (the Rust
    # executable used to hand a timed-out command to Python, which repeated
    # it all): the hub sees each request once.

    def _requests(self) -> list[str]:
        return [endpoint for _, endpoint, _ in self.hub.requests]

    def test_fetch_from_a_misbehaving_hub(self) -> None:
        import socket
        import time

        with self.hub:
            url = self.hub.url("u", "r")
            src = self._source(url)
            self.tmi(src, "push")
            copy = self.tmp / "copy"
            copy.mkdir()
            self.tmi(copy, "init")
            self.tmi(copy, "remote", "add", "origin", url)

            def fetch(fault: dict[str, str], timeout: str = "1", remote: str = "origin"):
                self.hub.requests.clear()
                self.hub.faults = fault
                started = time.monotonic()
                result = self.tmi(copy, "fetch", remote, check=False,
                                  env={"TMIHUB_TIMEOUT": timeout, "TMI_DEBUG_FORWARD": "1"})
                return result, time.monotonic() - started

            # the first request is never answered: no answer (issue #29's
            # words for "timed out"), not repeated
            result, elapsed = fetch({"objects/list": "hang"})
            self.assertEqual(result.returncode, 1, result.stderr)
            self.assertIn(f"tmiki: error: {https_mod.NO_ANSWER}", result.stderr)
            self.assertNotIn("forwarding", result.stderr)
            self.assertEqual(self._requests(), ["objects/list"])
            self.assertLess(elapsed, 20)

            # the download stops halfway with the connection open
            result, elapsed = fetch({"objects/download": "stall"})
            self.assertEqual(result.returncode, 1, result.stderr)
            self.assertIn(f"tmiki: error: {https_mod.NO_ANSWER}", result.stderr)
            self.assertNotIn("forwarding", result.stderr)
            self.assertEqual(self._requests(), ["objects/list", "objects/download"])
            self.assertLess(elapsed, 20)

            # a zip cut short: http.client reads what came, zipfile refuses it
            result, _ = fetch({"objects/download": "close"})
            self.assertEqual(result.returncode, 1, result.stderr)
            self.assertIn(f"tmiki: error: {https_mod.BROKEN_DOWNLOAD}", result.stderr)
            self.assertNotIn("forwarding", result.stderr)
            self.assertEqual(self._requests(), ["objects/list", "objects/download"])

            # a JSON answer cut short: response.read() raises IncompleteRead
            result, _ = fetch({"objects/list": "close"})
            self.assertEqual(result.returncode, 1, result.stderr)
            self.assertRegex(result.stderr, r"tmiki: error: the connection to tmihub broke off "
                                            r"\(IncompleteRead\(\d+ bytes read, \d+ more expected\)\); run the command again")
            self.assertEqual(self._requests(), ["objects/list"])

            # nothing listens: urllib's reason in tmi's words (the executable
            # hands this one to Python, as no request reached a server).
            # Windows retries a refused connect for about 2 s.
            with socket.socket() as probe:
                probe.bind(("127.0.0.1", 0))
                port = probe.getsockname()[1]
            self.tmi(copy, "remote", "add", "dead", f"http://127.0.0.1:{port}/api/u/r")
            result, _ = fetch({}, timeout="30", remote="dead")
            self.assertEqual(result.returncode, 1, result.stderr)
            self.assertRegex(result.stderr, rf"tmiki: error: cannot reach tmihub at 127\.0\.0\.1:{port}: \[(WinError|Errno) \d+\] .+; "
                                            r"check the remote URL and your network connection")
            if os.environ.get("TMI_BIN"):
                self.assertIn("forwarding to Python: network error before the first request", result.stderr)

            # a slow download that never pauses for the timeout: 10 pieces
            # 0.3 s apart, 3 s in all, with a 2 s timeout
            self.hub.slow_delay = 0.3
            result, _ = fetch({"objects/download": "slow"}, timeout="2")
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertRegex(result.stdout, r"fetched [1-9]\d* objects from origin")
            self.assertEqual(self._requests(), ["objects/list", "objects/download", "refs"])

    def test_push_with_no_answer_to_the_upload_fails_once(self) -> None:
        with self.hub:
            url = self.hub.url("u", "r")
            src = self._source(url)
            hook = src / ".tmi" / "hooks" / ("pre-push.py" if os.name == "nt" else "pre-push")
            hook.write_text(f"#!{sys.executable}\nopen({str(self.tmp / 'hook-runs')!r}, 'a').write('x')\n",
                            encoding="utf-8")
            if os.name != "nt":
                hook.chmod(0o755)
            self.hub.faults["objects/upload"] = "hang"
            result = self.tmi(src, "push", check=False, env={"TMIHUB_TIMEOUT": "1", "TMI_DEBUG_FORWARD": "1"})
            self.assertEqual(result.returncode, 1, result.stderr)
            self.assertIn(f"tmiki: error: {https_mod.NO_ANSWER}", result.stderr)
            self.assertNotIn("forwarding", result.stderr)
            # the pre-push hook ran once, each request went out once
            self.assertEqual(self._requests(), ["refs", "objects/list", "objects/upload"])
            self.assertEqual((self.tmp / "hook-runs").read_text(), "x")

    # -- proxies (issue #27) --

    _PROBE = (
        "import sys, urllib.request as u\n"
        "req = u.Request(sys.argv[1])\n"
        "calls = []\n"
        "req.set_proxy = lambda *a: calls.append(a)\n"
        "meth = getattr(u.ProxyHandler(), req.type + '_open', None)\n"
        "try:\n"
        "    meth and meth(req)\n"
        "except Exception:\n"
        "    calls.append('error')\n"
        "print('proxy' if calls else 'direct')\n"
    )

    def test_proxy_choice_is_urllibs(self) -> None:
        """The executable hands a command to Python exactly when urlopen
        would send its requests through a proxy (ProxyHandler with
        getproxies() and proxy_bypass(), asked in a child Python)."""
        import subprocess

        if not os.environ.get("TMI_BIN"):
            self.skipTest("the Rust executable's choice (rustcli variant)")
        with self.hub:
            url = self.hub.url("u", "r")
            src = self._source(url)
            self.tmi(src, "push")
            self.tmi(self.tmp, "clone", url, "copy")
            copy = self.tmp / "copy"
            proxy = "http://127.0.0.1:9"
            port = str(self.hub.port)
            scenarios = [
                {},
                {"NO_PROXY": "localhost"},
                {"npm_config_https_proxy": proxy, "YARN_HTTPS_PROXY": proxy, "ELECTRON_GET_USE_PROXY": "1",
                 "GLOBAL_AGENT_NO_PROXY": "x", "DOCKER_HTTPS_PROXY": proxy},
                {"ALL_PROXY": proxy},
                {"HTTPS_PROXY": proxy},
                {"HTTP_PROXY": proxy},
                {"Http_Proxy": "127.0.0.1:9"},
                {"http_proxy": ""},
                {"http_proxy": proxy, "no_proxy": "127.0.0.1"},
                {"http_proxy": proxy, "NO_PROXY": "*"},
                {"http_proxy": proxy, "no_proxy": " .0.0.1, example.com"},
                {"http_proxy": proxy, "no_proxy": f"127.0.0.1:{port}"},
                {"http_proxy": proxy, "no_proxy": "127.0.0.1:1"},
                {"http_proxy": proxy, "no_proxy": "localhost"},
                {"HTTP_PROXY": proxy, "REQUEST_METHOD": "GET"},
                {"http_proxy": "http:/no-authority", "no_proxy": "*"},
            ]
            for extra in scenarios:
                with self.subTest(env=extra):
                    env = {**self.env, **extra}
                    probe = subprocess.run([sys.executable, "-c", self._PROBE, url], env=env,
                                           capture_output=True, text=True, timeout=60)
                    expected = probe.stdout.strip()
                    self.assertIn(expected, ("proxy", "direct"), probe.stderr)
                    result = self.tmi(copy, "fetch", check=False, env={**extra, "TMI_DEBUG_FORWARD": "1"})
                    forwarded = "forwarding to Python: urllib would use a proxy" in result.stderr
                    self.assertEqual(forwarded, expected == "proxy", result.stderr)
                    if expected == "direct":
                        self.assertEqual(result.returncode, 0, result.stderr)


if __name__ == "__main__":
    unittest.main()
