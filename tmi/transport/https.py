"""HTTPS transport for tmihub.

Authentication: a personal access token is attached as ``Authorization:
Bearer <token>`` on every request. The token is resolved from, in order:

  1. env ``TMIHUB_TOKEN``
  2. tmi user config key ``tmihub.token`` (``tmiki config --global tmihub.token ...``)

Public repositories accept anonymous reads; pushes (and private reads) require
a token. Missing/invalid tokens surface as a clear RuntimeError.

Object transfer is split into batches (``BATCH_MAX_OBJECTS`` objects or
``BATCH_MAX_BYTES`` bytes, overridable via env ``TMIHUB_BATCH_OBJECTS`` /
``TMIHUB_BATCH_BYTES``), each one an ordinary ``/objects/download`` or
``/objects/upload`` request carrying a subset of oids. Bodies are streamed
through temp files rather than held in memory, and every received object lands
in the store as soon as it is unpacked, so an interrupted transfer resumes by
simply running it again: only the oids still missing are requested.
"""

from __future__ import annotations

import http.client
import json
import io
import os
import time
import urllib.request
import urllib.error
import uuid
import zipfile
import zlib
import tempfile
from typing import Any, Callable, Iterable
from urllib.parse import urlsplit

from .. import config as _config
from ..objects import ObjectStore
from ..lockfile import UNCHECKED, Unchecked
from . import ProgressFn


# Per-request batch limits. Each batch is one request, so a dropped connection
# only costs the batch in flight.
BATCH_MAX_OBJECTS = 256
BATCH_MAX_BYTES = 64 * 1024 * 1024
# Seconds a request may make no progress (the connect, each send and each
# receive: urlopen's socket timeout) before it fails with "timed out";
# overridable via env ``TMIHUB_TIMEOUT``. A transfer that keeps moving is
# never cut off, however long it takes.
TIMEOUT = 300
_COPY_CHUNK = 1024 * 1024
# The most JSON one reply may hold (env ``TMIHUB_MAX_REPLY_BYTES``), the most
# of an error reply that is kept, and the most one download may deliver
# (env ``TMIHUB_MAX_DOWNLOAD_BYTES``).
MAX_REPLY_BYTES = 64 * 1024 * 1024
MAX_ERROR_BYTES = 64 * 1024
MAX_DOWNLOAD_BYTES = 4 * 1024 * 1024 * 1024
# A reply is given up when it brings fewer than PACE_MIN_BYTES bytes in
# PACE_WINDOW seconds (env ``TMIHUB_PACE_MIN_BYTES`` / ``TMIHUB_PACE_WINDOW``).
PACE_WINDOW = 60
PACE_MIN_BYTES = 1024

# What went wrong, in tmi's words rather than urllib's, json's or zipfile's
# (issue #29). The Rust front door words the same failures the same way
# (tmi-cli transport_errors.rs).
UNEXPECTED_REPLY = (
    "tmihub sent a reply tmi cannot read (not the JSON it expects); the server may have a problem, "
    "or the remote URL may not point at a tmihub repository"
)
BROKEN_DOWNLOAD = (
    "tmihub sent a broken download (not a readable zip archive); run the command again, "
    "and if it keeps failing the server may have a problem"
)


# A reply that stopped coming (after TMIHUB_TIMEOUT seconds without data,
# or cut off).
NO_ANSWER = (
    "tmihub stopped answering (TMIHUB_TIMEOUT sets how many seconds tmi waits, 300 by default); "
    "check your network connection and run the command again"
)


def broke_off(detail: Any) -> str:
    return f"the connection to tmihub broke off ({detail}); run the command again"


class ReplyError(RuntimeError):
    """A tmihub reply tmi cannot read, or one that stopped coming."""


# These texts are printed by the Rust executable too (tmi-transport
# https.rs); keep them byte for byte the same.
def plain_http_token(netloc: str) -> str:
    return (
        f"refusing to send the tmihub token over plain http to {netloc}; use an https:// remote URL, "
        "or remove the token (TMIHUB_TOKEN, 'tmiki config --global --unset tmihub.token')"
    )


def too_big_reply(limit: int) -> str:
    return f"tmihub sent a reply larger than {limit} bytes; tmi will not read it (TMIHUB_MAX_REPLY_BYTES raises the limit)"


def too_big_download(limit: int) -> str:
    return (
        f"tmihub sent a download larger than {limit} bytes; tmi stopped receiving it "
        "(TMIHUB_MAX_DOWNLOAD_BYTES raises the limit)"
    )


def too_slow(min_bytes: int, window: int) -> str:
    return (
        f"tmihub is sending data too slowly (under {min_bytes} bytes in {window} seconds); tmi gave up "
        "(TMIHUB_PACE_WINDOW and TMIHUB_PACE_MIN_BYTES set the limit); run the command again"
    )


def redirected(code: int, location: str | None) -> str:
    shown = "".join(c for c in (location or "") if " " <= c <= "~")[:200]
    where = f" to {shown}" if shown else ""
    return f"tmihub answered with a redirect ({code}){where}; tmi does not follow redirects (check the remote URL)"


class _NoRedirect(urllib.request.HTTPRedirectHandler):
    """A 3xx answer is an error: the token must not travel to another place."""

    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


_OPENER = urllib.request.build_opener(_NoRedirect)


def _origin(url: str) -> tuple[str, str, int, str]:
    parts = urlsplit(url)
    scheme = parts.scheme.lower()
    try:
        port = parts.port
    except ValueError:
        port = None
    return scheme, (parts.hostname or "").lower(), port or (443 if scheme == "https" else 80), parts.netloc


def _declared_length(response: Any) -> int:
    headers = getattr(response, "headers", None)
    length = headers.get("Content-Length") if headers is not None else None
    return int(length) if length and length.strip().isdigit() else 0


def _is_loopback(host: str) -> bool:
    return host in ("localhost", "127.0.0.1", "::1")


def read_limited(source: Any, limit: int, what: str, *, truncate: bool = False) -> bytes:
    """Up to ``limit`` bytes of ``source``: more is the error ``what(limit)``
    (or, with ``truncate``, just cut off), and a body that brings fewer than
    PACE_MIN_BYTES bytes in PACE_WINDOW seconds is given up."""
    return b"".join(_chunks(source, limit, what, truncate, whole=True))


def _chunks(source: Any, limit: int, what: Any, truncate: bool = False, *, whole: bool = False):
    window = _env_limit("TMIHUB_PACE_WINDOW", PACE_WINDOW)
    min_bytes = _env_limit("TMIHUB_PACE_MIN_BYTES", PACE_MIN_BYTES)
    # read1: one receive at most, so a trickle cannot hide inside one read
    live = source if isinstance(source, http.client.HTTPResponse) else getattr(source, "fp", None)
    read = live.read1 if isinstance(live, http.client.HTTPResponse) else source.read
    total = 0
    since = time.monotonic()
    got = 0
    while True:
        chunk = read(_COPY_CHUNK)
        if not chunk:
            # read1 ends quietly where read() raised: a whole body shorter than
            # its Content-Length broke off (IncompleteRead only takes len() of
            # it). A short download is left to the zip check, as read(n) did.
            if whole and isinstance(live, http.client.HTTPResponse) and not live.chunked and live.length:
                raise http.client.IncompleteRead(range(total), live.length)
            return
        total += len(chunk)
        if total > limit:
            if truncate:
                yield chunk[: len(chunk) - (total - limit)]
                return
            raise ReplyError(what(limit))
        got += len(chunk)
        if time.monotonic() - since >= window:
            if got < min_bytes:
                raise ReplyError(too_slow(min_bytes, window))
            since = time.monotonic()
            got = 0
        yield chunk


# What http.client / the socket raise while a reply is awaited or read
# (urlopen wraps only the connect and the sending of the request in URLError).
_RECEIVE_ERRORS = (TimeoutError, http.client.IncompleteRead, http.client.RemoteDisconnected)


def _receive_error(exc: BaseException) -> ReplyError:
    if isinstance(exc, TimeoutError):
        return ReplyError(NO_ANSWER)
    return ReplyError(broke_off(exc))


def unreachable(url: str, reason: Any) -> RuntimeError:
    host = urlsplit(url).netloc or url
    return RuntimeError(f"cannot reach tmihub at {host}: {reason}; check the remote URL and your network connection")


def _json_object(response: Any) -> dict[str, Any]:
    """The decoded JSON object of a reply."""
    limit = _env_limit("TMIHUB_MAX_REPLY_BYTES", MAX_REPLY_BYTES)
    if _declared_length(response) > limit:
        raise ReplyError(too_big_reply(limit))
    try:
        body = read_limited(response, limit, too_big_reply)
    except _RECEIVE_ERRORS as exc:
        raise _receive_error(exc) from exc
    try:
        data = json.loads(body.decode("utf-8"))
    except (ValueError, UnicodeDecodeError) as exc:
        raise ReplyError(UNEXPECTED_REPLY) from exc
    if not isinstance(data, dict):
        raise ReplyError(UNEXPECTED_REPLY)
    return data


def _env_limit(name: str, default: int) -> int:
    try:
        value = int(os.environ.get(name, ""))
    except ValueError:
        return default
    return value if value > 0 else default


def _batches(oids: Iterable[str], size_of: Callable[[str], int] | None = None) -> list[list[str]]:
    """Split ``oids`` into batches bounded by count and (when known) bytes.

    An object larger than the byte limit still goes out, alone in its batch.
    """
    max_objects = _env_limit("TMIHUB_BATCH_OBJECTS", BATCH_MAX_OBJECTS)
    max_bytes = _env_limit("TMIHUB_BATCH_BYTES", BATCH_MAX_BYTES)
    batches: list[list[str]] = []
    batch: list[str] = []
    batch_bytes = 0
    for oid in oids:
        size = size_of(oid) if size_of else 0
        if batch and (len(batch) >= max_objects or batch_bytes + size > max_bytes):
            batches.append(batch)
            batch, batch_bytes = [], 0
        batch.append(oid)
        batch_bytes += size
    if batch:
        batches.append(batch)
    return batches


class _MultipartBody(io.RawIOBase):
    """``head + <file> + tail`` as a readable stream with a known length.

    Handed to urllib as request ``data`` so the zip is sent in chunks straight
    from disk instead of being copied into one bytes object.
    """

    def __init__(self, head: bytes, path: str, tail: bytes) -> None:
        super().__init__()
        self._parts = [io.BytesIO(head), open(path, 'rb'), io.BytesIO(tail)]
        self.length = len(head) + os.path.getsize(path) + len(tail)

    def readable(self) -> bool:
        return True

    def readinto(self, buffer) -> int:
        while self._parts:
            n = self._parts[0].readinto(buffer)
            if n:
                return n
            self._parts.pop(0).close()
        return 0

    def close(self) -> None:
        for part in self._parts:
            part.close()
        self._parts = []
        super().close()


def _resolve_token() -> str | None:
    token = os.environ.get("TMIHUB_TOKEN")
    if token:
        return token.strip()
    try:
        value = _config.get_value(_config.read_user_config(), "tmihub.token")
    except Exception:
        value = None
    return str(value).strip() if value else None


class HttpsTransport:
    def __init__(self, url: str) -> None:
        # e.g. http://127.0.0.1:5000/api/user/repo
        self.url = url.rstrip('/')
        self.token = _resolve_token()
        # The token is for this scheme, host and port only.
        self.origin = _origin(self.url)

    def _open(self, req: urllib.request.Request, timeout: float | None = None):
        if self.token:
            target = _origin(req.full_url)
            if target[:3] == self.origin[:3]:
                if target[0] != "https" and not _is_loopback(target[1]):
                    raise RuntimeError(plain_http_token(target[3]))
                req.add_header("Authorization", f"Bearer {self.token}")
        if timeout is None:
            timeout = _env_limit("TMIHUB_TIMEOUT", TIMEOUT)
        try:
            return _OPENER.open(req, timeout=timeout)
        except urllib.error.HTTPError as exc:
            if 300 <= exc.code < 400:
                raise RuntimeError(redirected(exc.code, exc.headers.get("Location"))) from exc
            # tmihub answers errors with a JSON ``{"error": ...}`` body; surface
            # it so the user sees the server's reason rather than a bare HTTPError.
            detail = ""
            try:
                detail = json.loads(read_limited(exc, MAX_ERROR_BYTES, too_big_reply, truncate=True).decode("utf-8")).get("error", "")
            except Exception:
                pass
            suffix = f": {detail}" if detail else ""
            if exc.code in (401, 403):
                hint = (
                    "set a token with 'tmiki config --global tmihub.token <PAT>' "
                    "or the TMIHUB_TOKEN environment variable"
                )
                raise RuntimeError(
                    f"tmihub denied access ({exc.code}){suffix}. {hint}"
                ) from exc
            if exc.code == 409:
                # Non-fast-forward ref update. tmihub does not accept force pushes
                # over HTTPS, so --force cannot help here either.
                raise RuntimeError(
                    f"tmihub rejected the update ({exc.code}){suffix}. "
                    "Fetch and merge first (HTTPS remotes do not accept force pushes)."
                ) from exc
            raise RuntimeError(
                f"tmihub request failed ({exc.code}){suffix}"
            ) from exc
        except urllib.error.URLError as exc:
            # No answer at all: refused, unknown host, TLS, timeout.
            raise unreachable(self.url, exc.reason) from exc
        except _RECEIVE_ERRORS as exc:
            raise _receive_error(exc) from exc

    def list_refs(self) -> dict[str, str]:
        req = urllib.request.Request(f"{self.url}/refs", method="GET")
        with self._open(req) as response:
            refs = _json_object(response).get("refs", {})
        if not refs:
            return {}
        if not isinstance(refs, dict):
            raise ReplyError(UNEXPECTED_REPLY)
        out: dict[str, str] = {}
        for name, value in refs.items():
            if value and not isinstance(value, str):
                raise ReplyError(UNEXPECTED_REPLY)
            out[name] = value or ""
        return out

    def _server_oids(self) -> list[str]:
        req = urllib.request.Request(f"{self.url}/objects/list", method="GET")
        with self._open(req) as response:
            oids = _json_object(response).get("oids", [])
        if not isinstance(oids, list) or not all(isinstance(oid, str) for oid in oids):
            raise ReplyError(UNEXPECTED_REPLY)
        return oids

    def pull_objects(self, into_store: ObjectStore, on_progress: ProgressFn | None = None) -> int:
        local_oids = set(into_store.iter_object_ids())
        missing_oids = [oid for oid in self._server_oids() if oid not in local_oids]
        return self.fetch_objects(into_store, missing_oids, on_progress=on_progress)

    def fetch_objects(
        self,
        into_store: ObjectStore,
        oids: Iterable[str],
        on_progress: ProgressFn | None = None,
    ) -> int:
        """Download exactly ``oids`` (those not already local) in batches.

        Kept separate from ``pull_objects`` so a lazy (LFS-style) fetch of a
        handful of large blobs can reuse the same batched, resumable path.
        """
        missing_oids = [oid for oid in dict.fromkeys(oids) if not into_store.path_for(oid).exists()]
        if not missing_oids:
            return 0
        if on_progress:
            on_progress(f"fetching {len(missing_oids)} remote objects via HTTP...")

        # Remote object sizes are unknown up front, so pull batches are bounded
        # by count only.
        batches = _batches(missing_oids)
        received = 0
        done = 0
        for index, batch in enumerate(batches, start=1):
            if on_progress:
                on_progress(
                    f"fetching objects [{done}/{len(missing_oids)}] (batch {index}/{len(batches)})"
                )
            received += self._download_batch(into_store, batch)
            done += len(batch)
        if on_progress:
            on_progress(f"fetching objects [{done}/{len(missing_oids)}] done")
        return received

    def _download_batch(self, into_store: ObjectStore, oids: list[str]) -> int:
        payload = json.dumps({"oids": oids}).encode('utf-8')
        req = urllib.request.Request(f"{self.url}/objects/download", data=payload, method="POST")
        req.add_header('Content-Type', 'application/json')

        received = 0
        # Spool the zip to disk (zipfile needs a seekable file) instead of
        # reading the whole response into memory.
        with tempfile.TemporaryFile() as spool:
            with self._open(req) as response:
                limit = _env_limit("TMIHUB_MAX_DOWNLOAD_BYTES", MAX_DOWNLOAD_BYTES)
                if _declared_length(response) > limit:
                    raise ReplyError(too_big_download(limit))
                try:
                    for chunk in _chunks(response, limit, too_big_download):
                        spool.write(chunk)
                except _RECEIVE_ERRORS as exc:
                    raise _receive_error(exc) from exc
            spool.seek(0)
            try:
                zf = zipfile.ZipFile(spool, 'r')
            except (zipfile.BadZipFile, EOFError) as exc:
                raise RuntimeError(BROKEN_DOWNLOAD) from exc
            with zf:
                for item in zf.namelist():
                    parts = item.split('/')
                    if len(parts) == 2 and len(parts[0]) == 2 and len(parts[1]) == 62:
                        oid = parts[0] + parts[1]
                        if into_store.path_for(oid).exists():
                            continue
                        try:
                            with zf.open(item) as obj_file:
                                data = obj_file.read()
                        except (zipfile.BadZipFile, zlib.error, EOFError, NotImplementedError, RuntimeError) as exc:
                            # a damaged member, an unknown compression, an encrypted one
                            raise RuntimeError(BROKEN_DOWNLOAD) from exc
                        # write_raw verifies the bytes hash to `oid` and lands them
                        # atomically — a lying/corrupting server or a truncated
                        # download must never poison the local object store.
                        # Each object is stored as soon as it is unpacked, so a
                        # rerun after an interruption only asks for the rest.
                        try:
                            into_store.write_raw(oid, data)
                        except ValueError as exc:
                            raise RuntimeError(f"remote sent corrupt object data: {exc}") from exc
                        received += 1
        return received

    def push_objects(self, from_store: ObjectStore, on_progress: ProgressFn | None = None) -> int:
        server_oids = set(self._server_oids())
        missing_oids = [oid for oid in from_store.iter_object_ids() if oid not in server_oids]

        if not missing_oids:
            return 0
        if on_progress:
            on_progress(f"pushing {len(missing_oids)} local objects via HTTP...")

        batches = _batches(missing_oids, lambda oid: from_store.path_for(oid).stat().st_size)
        done = 0
        for index, batch in enumerate(batches, start=1):
            if on_progress:
                on_progress(
                    f"pushing objects [{done}/{len(missing_oids)}] (batch {index}/{len(batches)})"
                )
            # Each batch is committed on the server once its request returns,
            # so a rerun after an interruption sees those oids in /objects/list
            # and only sends what is still missing.
            self._upload_batch(from_store, batch)
            done += len(batch)
        if on_progress:
            on_progress(f"pushing objects [{done}/{len(missing_oids)}] done")
        return len(missing_oids)

    def _upload_batch(self, from_store: ObjectStore, oids: list[str]) -> None:
        fd, tmp_path = tempfile.mkstemp(suffix=".zip")
        os.close(fd)
        try:
            with zipfile.ZipFile(tmp_path, 'w', compression=zipfile.ZIP_DEFLATED) as zf:
                for oid in oids:
                    path = from_store.path_for(oid)
                    arcname = f"{oid[:2]}/{oid[2:]}"
                    zf.write(path, arcname)

            boundary = uuid.uuid4().hex
            head = (
                f"--{boundary}\r\n"
                f"Content-Disposition: form-data; name=\"file\"; filename=\"objects.zip\"\r\n"
                f"Content-Type: application/zip\r\n\r\n"
            ).encode('utf-8')
            tail = f"\r\n--{boundary}--\r\n".encode('utf-8')

            with _MultipartBody(head, tmp_path, tail) as body:
                req = urllib.request.Request(f"{self.url}/objects/upload", data=body, method="POST")
                req.add_header('Content-Type', f'multipart/form-data; boundary={boundary}')
                req.add_header('Content-Length', str(body.length))
                with self._open(req):
                    pass
        finally:
            os.unlink(tmp_path)

    def update_ref(self, ref_name: str, value: str, expected: str | None | Unchecked = UNCHECKED) -> None:
        body: dict[str, Any] = {"ref_name": ref_name, "value": value}
        if not isinstance(expected, Unchecked):
            # compare-and-swap: the server must refuse (409) unless the ref is
            # still `expected` (null: it must not exist) when it applies this
            body["expected"] = expected
        payload = json.dumps(body).encode('utf-8')
        req = urllib.request.Request(f"{self.url}/update_ref", data=payload, method="POST")
        req.add_header('Content-Type', 'application/json')
        self._open(req)

    def head_branch(self) -> str | None:
        # Best-effort: callers fall back to a default branch when this is None,
        # so swallow both connection errors and the RuntimeError _open raises.
        # A reply that is not the JSON object expected is still an error.
        try:
            req = urllib.request.Request(f"{self.url}/head", method="GET")
            with self._open(req) as response:
                return _json_object(response).get("head_branch")
        except ReplyError:
            raise
        except (urllib.error.URLError, RuntimeError):
            return None
