"""A small stand-in for tmihub's HTTPS object API, for tests and tools.

Serves ``/api/<user>/<repo>/...`` from bare tmi repositories under a root
directory (``<root>/<user>/<repo>``), with the endpoints tmi's
``HttpsTransport`` uses (tmi/transport/https.py):

    GET  refs              {"refs": {"refs/heads/main": oid, ...}}
    GET  head              {"head_branch": "main" | null}
    GET  objects/list      {"oids": [...]}
    POST objects/download  {"oids": [...]} -> a zip of "<oid[:2]>/<oid[2:]>"
    POST objects/upload    multipart/form-data, field "file": such a zip
    POST update_ref        {"ref_name": ..., "value": ..., "expected": ...};
                           409 unless a fast-forward (tmihub takes no force
                           pushes) and, when "expected" is present, unless the
                           ref is still that value (null: still missing)

With ``token`` set, every request must carry ``Authorization: Bearer
<token>`` (401 otherwise); with ``public=True`` reads need none. Errors
answer ``{"error": ...}`` as tmihub does. Requests are recorded in
``requests`` (method, endpoint, oid count).

``faults`` (endpoint -> mode) makes the answers of an endpoint go wrong,
for tests of a network that misbehaves:

    "hang"   answers nothing and keeps the connection open
    "stall"  sends the headers and half the body, then keeps the
             connection open without sending more
    "close"  sends the headers and half the body, then closes
    "slow"   sends the whole answer in ``slow_pieces`` pieces,
             ``slow_delay`` seconds apart

A hung or stalled connection is let go when the hub stops.

    python tools/fake_tmihub.py ROOT [--port N] [--token T]
"""
from __future__ import annotations

import argparse
import io
import json
import sys
import threading
import time
import zipfile
import zlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path


def _object_id(object_type: bytes, payload: bytes) -> str:
    import hashlib

    h = hashlib.sha256()
    h.update(object_type + b"\0")
    h.update(payload)
    return h.hexdigest()


class Hub:
    def __init__(self, root: Path, token: str | None = None, public: bool = False, port: int = 0,
                 compression: int = zipfile.ZIP_DEFLATED) -> None:
        self.root = Path(root)
        self.compression = compression  # of the zips objects/download sends
        self.token = token
        self.public = public
        self.requests: list[tuple[str, str, int]] = []
        self.ref_updates: list[dict] = []  # the JSON bodies of update_ref requests
        self.lock = threading.Lock()
        self.faults: dict[str, str] = {}
        self.slow_pieces = 10
        self.slow_delay = 0.25
        self._released = threading.Event()
        hub = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *args) -> None:  # quiet
                pass

            def do_GET(self) -> None:
                hub._handle(self, "GET")

            def do_POST(self) -> None:
                hub._handle(self, "POST")

        class Server(ThreadingHTTPServer):
            daemon_threads = True

            def handle_error(self, request, client_address) -> None:
                # a client that gave up on a hung or stalled answer
                if not isinstance(sys.exc_info()[1], ConnectionError):
                    super().handle_error(request, client_address)

        self.server = Server(("127.0.0.1", port), Handler)
        self.thread: threading.Thread | None = None

    # -- lifecycle --
    @property
    def port(self) -> int:
        return self.server.server_address[1]

    def url(self, user: str, repo: str) -> str:
        return f"http://127.0.0.1:{self.port}/api/{user}/{repo}"

    def start(self) -> "Hub":
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        return self

    def stop(self) -> None:
        self._released.set()
        self.server.shutdown()
        self.server.server_close()

    def __enter__(self) -> "Hub":
        return self.start()

    def __exit__(self, *exc) -> None:
        self.stop()

    # -- repository access --
    def _repo(self, user: str, name: str) -> Path | None:
        path = self.root / user / name
        return path if (path / "objects").is_dir() and (path / "refs").is_dir() else None

    @staticmethod
    def _objects(repo: Path) -> list[str]:
        out = []
        for sub in (repo / "objects").iterdir():
            if sub.is_dir() and len(sub.name) == 2:
                out += [sub.name + p.name for p in sub.iterdir() if p.is_file() and len(p.name) == 62]
        return sorted(out)

    @staticmethod
    def _refs(repo: Path) -> dict[str, str]:
        out = {}
        for path in sorted((repo / "refs").rglob("*")):
            if path.is_file():
                value = path.read_text(encoding="utf-8").strip()
                if value:
                    out[path.relative_to(repo).as_posix()] = value
        return out

    @staticmethod
    def _read(repo: Path, oid: str) -> tuple[bytes, bytes]:
        raw = zlib.decompress((repo / "objects" / oid[:2] / oid[2:]).read_bytes())
        kind, payload = raw.split(b"\0", 1)
        return kind, payload

    def _is_ancestor(self, repo: Path, ancestor: str, descendant: str) -> bool:
        seen, stack = set(), [descendant]
        while stack:
            cur = stack.pop()
            if cur in seen:
                continue
            seen.add(cur)
            try:
                kind, payload = self._read(repo, cur)
            except OSError:
                continue
            if kind == b"commit":
                stack += json.loads(payload).get("parents") or []
        return ancestor in seen

    # -- HTTP --
    def _handle(self, req: BaseHTTPRequestHandler, method: str) -> None:
        length = int(req.headers.get("Content-Length") or 0)
        body = req.rfile.read(length) if length else b""
        parts = req.path.split("?", 1)[0].strip("/").split("/")
        if len(parts) < 4 or parts[0] != "api":
            return self._send(req, 404, {"error": "not found"})
        user, name, endpoint = parts[1], parts[2], "/".join(parts[3:])
        reading = method == "GET" or endpoint == "objects/download"
        if self.token is not None and not (reading and self.public):
            if req.headers.get("Authorization") != f"Bearer {self.token}":
                with self.lock:
                    self.requests.append((method, endpoint, -1))
                return self._send(req, 401, {"error": "authentication required"})
        repo = self._repo(user, name)
        if repo is None:
            return self._send(req, 404, {"error": f"no repository {user}/{name}"})
        with self.lock:
            try:
                status, answer, count = self._dispatch(repo, method, endpoint, body, req.headers)
            except Exception as exc:  # a bug here should show, not hang
                status, answer, count = 500, {"error": f"{type(exc).__name__}: {exc}"}, 0
            self.requests.append((method, endpoint, count))
        self._send(req, status, answer, self.faults.get(endpoint))

    def _dispatch(self, repo: Path, method: str, endpoint: str, body: bytes, headers) -> tuple[int, object, int]:
        if method == "GET" and endpoint == "refs":
            refs = self._refs(repo)
            return 200, {"refs": refs}, len(refs)
        if method == "GET" and endpoint == "head":
            text = (repo / "HEAD").read_text(encoding="utf-8").strip()
            branch = text[len("ref: refs/heads/"):] if text.startswith("ref: refs/heads/") else None
            return 200, {"head_branch": branch}, 0
        if method == "GET" and endpoint == "objects/list":
            oids = self._objects(repo)
            return 200, {"oids": oids}, len(oids)
        if method == "POST" and endpoint == "objects/download":
            oids = json.loads(body)["oids"]
            buf = io.BytesIO()
            with zipfile.ZipFile(buf, "w", compression=self.compression) as zf:
                for oid in oids:
                    path = repo / "objects" / oid[:2] / oid[2:]
                    if path.is_file():
                        zf.writestr(f"{oid[:2]}/{oid[2:]}", path.read_bytes())
            return 200, buf.getvalue(), len(oids)
        if method == "POST" and endpoint == "objects/upload":
            boundary = headers["Content-Type"].split("boundary=", 1)[1].encode()
            head, rest = body.split(b"\r\n\r\n", 1)
            if not head.startswith(b"--" + boundary):
                return 400, {"error": "bad multipart body"}, 0
            data, tail = rest.rsplit(b"\r\n--" + boundary + b"--\r\n", 1)
            stored = 0
            with zipfile.ZipFile(io.BytesIO(data)) as zf:
                for item in zf.namelist():
                    raw = zf.read(item)
                    oid = item.replace("/", "")
                    kind, payload = zlib.decompress(raw).split(b"\0", 1)
                    if _object_id(kind, payload) != oid:
                        return 400, {"error": f"object hash mismatch for {oid}"}, stored
                    target = repo / "objects" / oid[:2] / oid[2:]
                    if not target.exists():
                        target.parent.mkdir(parents=True, exist_ok=True)
                        target.write_bytes(raw)
                        stored += 1
            return 200, {"stored": stored}, stored
        if method == "POST" and endpoint == "update_ref":
            request = json.loads(body)
            self.ref_updates.append(request)
            ref, value = request["ref_name"], request["value"]
            if not ref.startswith("refs/") or ".." in ref.split("/"):
                return 400, {"error": f"invalid ref name: {ref}"}, 0
            path = repo.joinpath(*ref.split("/"))
            old = path.read_text(encoding="utf-8").strip() if path.is_file() else None
            # Compare-and-swap (issue #36): a client that sends "expected" (the
            # ref's value it checked, null for "no such ref") has the update
            # refused unless the ref is still that, even when the new value
            # would fast-forward it. The check and the write are one step here
            # (self.lock); a real server must make them atomic too.
            if "expected" in request and (old or None) != request["expected"]:
                return 409, {"error": f"{ref} is not at the expected value (it moved since the push read it)"}, 0
            if old and old != value and not self._is_ancestor(repo, old, value):
                return 409, {"error": f"non-fast-forward update of {ref}"}, 0
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(value + "\n", encoding="utf-8", newline="\n")
            return 200, {"ref_name": ref, "value": value}, 0
        return 404, {"error": f"unknown endpoint {method} {endpoint}"}, 0

    def _send(self, req: BaseHTTPRequestHandler, status: int, answer: object, fault: str | None = None) -> None:
        if isinstance(answer, bytes):
            data, ctype = answer, "application/zip"
        else:
            data, ctype = json.dumps(answer).encode("utf-8"), "application/json"
        if fault == "hang":
            self._released.wait(120)
            req.close_connection = True
            return
        req.send_response(status)
        req.send_header("Content-Type", ctype)
        req.send_header("Content-Length", str(len(data)))
        req.end_headers()
        if fault in ("stall", "close"):
            req.wfile.write(data[: len(data) // 2])
            req.wfile.flush()
            if fault == "stall":
                self._released.wait(120)
            req.close_connection = True
            return
        if fault == "slow":
            req.wfile.flush()
            step = -(-len(data) // self.slow_pieces)
            for at in range(0, len(data), step):
                time.sleep(self.slow_delay)
                req.wfile.write(data[at:at + step])
                req.wfile.flush()
            return
        req.wfile.write(data)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("root", type=Path)
    parser.add_argument("--port", type=int, default=0)
    parser.add_argument("--token")
    parser.add_argument("--public", action="store_true")
    args = parser.parse_args()
    hub = Hub(args.root, token=args.token, public=args.public, port=args.port)
    print(f"serving {args.root} at http://127.0.0.1:{hub.port}/api/<user>/<repo>", flush=True)
    try:
        hub.server.serve_forever()
    except KeyboardInterrupt:
        pass
    return 0


if __name__ == "__main__":
    sys.exit(main())
