"""A hostile tmihub, through the CLI (Python, or the Rust executable under
``--variants rustcli``): it redirects to another host, wants the token over
plain http, sends a reply, an error body or a download that never ends, or
sends one byte at a time. tmi must keep its token, its memory and its disk,
and give up. The Rust twins are rust/crates/tmi-transport/tests/https_hostile.rs.
"""

from __future__ import annotations

import json
import os
import shutil
import socketserver
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from pathlib import Path

from tests.test_https_transport import PROJECT_ROOT, tmi_command


class _Server(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True

    def __init__(self, host: str, answer) -> None:
        self.answer = answer
        self.heads: list[str] = []
        self.sent = 0
        self.stopping = False
        super().__init__((host, 0), _Handler)
        self.host = host
        self.port = self.server_address[1]
        threading.Thread(target=self.serve_forever, daemon=True).start()

    def url(self) -> str:
        return f"http://{self.host}:{self.port}/api/u/r"

    def close(self) -> None:
        self.stopping = True
        self.shutdown()
        self.server_close()


class _Handler(socketserver.StreamRequestHandler):
    def handle(self) -> None:
        head = b""
        while not head.endswith(b"\r\n\r\n"):
            byte = self.rfile.read(1)
            if not byte:
                return
            head += byte
        text = head.decode("latin-1")
        length = 0
        for line in text.split("\r\n"):
            if line.lower().startswith("content-length:"):
                length = int(line.split(":", 1)[1])
        if length:
            self.rfile.read(length)
        self.server.heads.append(text)
        method, path, _ = text.split(" ", 2)
        endpoint = "/".join(path.split("?")[0].strip("/").split("/")[3:])
        try:
            self.server.answer(self, endpoint)
        except OSError:
            pass

    def json(self, status: str, obj) -> None:
        body = json.dumps(obj).encode()
        self.wfile.write(
            f"HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {len(body)}\r\n"
            "Connection: close\r\n\r\n".encode() + body
        )

    def endless(self, status: str, prefix: bytes, fill: bytes, total: int) -> None:
        self.wfile.write(f"HTTP/1.1 {status}\r\nConnection: close\r\n\r\n".encode() + prefix)
        chunk = fill * (1 << 20)
        while self.server.sent < total and not self.server.stopping:
            self.wfile.write(chunk)
            self.wfile.flush()
            self.server.sent += len(chunk)


class HostileHubTests(unittest.TestCase):
    def setUp(self) -> None:
        root = PROJECT_ROOT / ".test-tmp"
        root.mkdir(exist_ok=True)
        self.tmp = Path(tempfile.mkdtemp(prefix="tmi-hostile-", dir=root))
        (self.tmp / "scratch").mkdir()
        self.env = {k: v for k, v in os.environ.items()
                    if not k.startswith("TMIHUB_") and k != "REQUEST_METHOD" and not k.lower().endswith("_proxy")}
        self.env["PYTHONPATH"] = str(PROJECT_ROOT) + os.pathsep + self.env.get("PYTHONPATH", "")
        self.env["TMI_CONFIG_HOME"] = str(self.tmp / "userconfig")
        self.env.setdefault("TMI_PYTHON", sys.executable)
        self.servers: list[_Server] = []

    def tearDown(self) -> None:
        for server in self.servers:
            server.close()
        shutil.rmtree(self.tmp, ignore_errors=True)
        try:
            (PROJECT_ROOT / ".test-tmp").rmdir()
        except OSError:
            pass

    def serve(self, answer, host: str = "127.0.0.1") -> _Server:
        server = _Server(host, answer)
        self.servers.append(server)
        return server

    def clone(self, url: str, env: dict | None = None, timeout: float = 60):
        return subprocess.run([*tmi_command(), "clone", url, "copy"], cwd=self.tmp / "scratch",
                              env={**self.env, **(env or {})}, text=True, capture_output=True, timeout=timeout)

    # -- 1. redirects --------------------------------------------------

    def test_a_redirect_is_not_followed_and_the_token_stays_home(self) -> None:
        target = self.serve(lambda h, ep: h.json("404 Not Found", {"error": "x"}))
        hub = self.serve(lambda h, ep: h.wfile.write(
            f"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{target.port}/api/u/r/refs\r\n"
            "Content-Length: 0\r\nConnection: close\r\n\r\n".encode()))
        result = self.clone(hub.url(), {"TMIHUB_TOKEN": "s3cret"})
        self.assertNotEqual(0, result.returncode)
        self.assertIn("tmihub answered with a redirect (302) to ", result.stderr)
        self.assertIn(f"127.0.0.1:{target.port}", result.stderr)
        self.assertIn("tmi does not follow redirects", result.stderr)
        self.assertTrue(hub.heads and all("Authorization: Bearer s3cret" in h for h in hub.heads))
        self.assertEqual([], target.heads, "the redirect target was contacted")

    # -- 2. the token is for one place, and never over plain http --------

    def test_the_token_is_not_sent_over_plain_http_to_another_machine(self) -> None:
        # 127.0.0.2 is this machine, but not one of the loopback names tmi trusts
        hub = self.serve(lambda h, ep: h.json("200 OK", {"refs": {}}), "127.0.0.2")
        result = self.clone(hub.url(), {"TMIHUB_TOKEN": "s3cret"})
        self.assertNotEqual(0, result.returncode)
        self.assertIn("refusing to send the tmihub token over plain http to 127.0.0.2", result.stderr)
        self.assertEqual([], hub.heads, "something was sent")

    def test_the_token_still_goes_to_the_loopback_hub_it_was_set_for(self) -> None:
        hub = self.serve(lambda h, ep: h.json("200 OK", {"refs": {}}))
        self.clone(hub.url(), {"TMIHUB_TOKEN": "s3cret"})
        self.assertTrue(hub.heads and all("Authorization: Bearer s3cret" in h for h in hub.heads))

    # -- 3. bodies without end -------------------------------------------

    def test_a_reply_that_never_ends_is_cut_off(self) -> None:
        hub = self.serve(lambda h, ep: h.endless("200 OK", b'{"refs": {"refs/heads/main": "', b"a", 256 << 20))
        result = self.clone(hub.url(), {"TMIHUB_MAX_REPLY_BYTES": "1000000"})
        self.assertIn("tmihub sent a reply larger than 1000000 bytes", result.stderr)
        self.assertLess(hub.sent, 100 << 20, "the client kept reading")

    def test_an_error_body_that_never_ends_is_cut_off(self) -> None:
        hub = self.serve(lambda h, ep: h.endless("500 Internal Server Error", b"{", b"x", 512 << 20))
        result = self.clone(hub.url())
        self.assertIn("tmihub request failed (500)", result.stderr)
        self.assertLess(hub.sent, 100 << 20, f"the client read {hub.sent >> 20} MiB of an error body")

    def test_a_download_that_never_ends_is_cut_off_and_its_temp_file_removed(self) -> None:
        oid = "ab" * 32

        def answer(h, ep):
            if ep == "refs":
                h.json("200 OK", {"refs": {"refs/heads/main": oid}})
            elif ep == "objects/list":
                h.json("200 OK", {"oids": [oid]})
            elif ep == "objects/download":
                h.endless("200 OK", b"", b"\0", 512 << 20)
            else:
                h.json("404 Not Found", {"error": "x"})

        hub = self.serve(answer)
        temp = self.tmp / "temp"
        temp.mkdir()
        result = self.clone(hub.url(), {"TMIHUB_MAX_DOWNLOAD_BYTES": "3000000",
                                        "TMP": str(temp), "TEMP": str(temp), "TMPDIR": str(temp)})
        self.assertIn("tmihub sent a download larger than 3000000 bytes", result.stderr)
        self.assertLess(hub.sent, 100 << 20, "the client kept reading")
        self.assertEqual([], list(temp.iterdir()), "temporary files left")

    # -- 4. no end to the waiting ----------------------------------------

    def test_a_reply_trickled_one_byte_at_a_time_is_given_up(self) -> None:
        def answer(h, ep):
            h.wfile.write(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n")
            while not h.server.stopping:
                h.wfile.write(b" ")
                h.wfile.flush()
                time.sleep(0.1)

        hub = self.serve(answer)
        started = time.monotonic()
        result = self.clone(hub.url(), {"TMIHUB_PACE_WINDOW": "1", "TMIHUB_PACE_MIN_BYTES": "1024"}, timeout=30)
        self.assertIn("tmihub is sending data too slowly", result.stderr)
        self.assertLess(time.monotonic() - started, 20)

    def test_a_slow_but_steady_reply_is_not_given_up(self) -> None:
        def answer(h, ep):
            body = b'{"refs": {}}'
            h.wfile.write(f"HTTP/1.1 200 OK\r\nContent-Length: {len(body)}\r\nConnection: close\r\n\r\n".encode())
            for i in range(len(body)):
                h.wfile.write(body[i:i + 1])
                h.wfile.flush()
                time.sleep(0.03)

        hub = self.serve(answer)
        result = self.clone(hub.url(), {"TMIHUB_PACE_WINDOW": "1", "TMIHUB_PACE_MIN_BYTES": "1"})
        self.assertNotIn("too slowly", result.stderr)


if __name__ == "__main__":
    unittest.main()
